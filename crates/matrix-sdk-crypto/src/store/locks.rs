// Copyright 2023 The Matrix.org Foundation C.I.C.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Collection of small helpers that implement store-based locks.
//!
//! Those locks are implemented as one value in the key-value crypto store, that
//! exists if and only if the lock has been taken. For this to work correctly,
//! we rely on multiple assumptions:
//!
//! - the store must allow concurrent reads and writes from multiple processes.
//!   For instance, for
//! sqlite, this means that it is running in [WAL](https://www.sqlite.org/wal.html) mode.
//! - the two operations used in the store implementation,
//!   `insert_custom_value_if_missing` and
//! `remove_custom_value`, must be atomic / implemented in a transaction.

use std::{
    sync::{Arc, Mutex as StdMutex},
    time::Duration,
};

use tokio::{sync::Mutex, task::JoinHandle, time::sleep};
use tracing::{instrument, trace};

use super::DynCryptoStore;
use crate::CryptoStoreError;

/// Small state machine to handle wait times.
#[derive(Clone, Debug)]
enum WaitingTime {
    /// Some time to wait, in milliseconds.
    Some(u32),
    /// Stop waiting when seeing this value.
    Stop,
}

/// A guard on the crypto store lock.
///
/// The lock will be automatically released a short period of time after all the
/// guards have dropped.
#[derive(Debug)]
pub struct CryptoStoreLockGuard {
    num_holders: Arc<StdMutex<u32>>,
}

impl Drop for CryptoStoreLockGuard {
    fn drop(&mut self) {
        let mut num_holders = self.num_holders.lock().unwrap();
        assert!(*num_holders > 0);
        *num_holders -= 1;
    }
}

/// A store-based lock for the `CryptoStore`.
#[derive(Clone, Debug)]
pub struct CryptoStoreLock {
    /// The store we're using to lock.
    store: Arc<DynCryptoStore>,

    /// Number of holders of the lock in this process.
    ///
    /// If greater than 0, this means we've already acquired this lock, in this
    /// process, and the store lock mustn't be touched.
    ///
    /// When the number of holders is decreased to 0, then the lock must be
    /// released in the store.
    num_holders: Arc<StdMutex<u32>>,

    /// The key used in the key/value mapping for the lock entry.
    lock_key: String,

    /// A specific value to identify the lock's holder.
    lock_holder: String,

    /// Backoff time, in milliseconds.
    backoff: Arc<Mutex<WaitingTime>>,

    /// If set, a task that will renew the lock's lease regularly as long as
    /// they are some guards holding onto it.
    extend_lease: Arc<Mutex<Option<JoinHandle<()>>>>,
}

impl CryptoStoreLock {
    const LEASE_MS: u32 = 2000;

    const EXTEND_LEASE_EVERY_MS: u64 = 200;

    /// Initial backoff, in milliseconds. This is the time we wait the first
    /// time, if taking the lock initially failed.
    const INITIAL_BACKOFF_MS: u32 = 10;

    /// Maximal backoff, in milliseconds. This is the maximum amount of time
    /// we'll wait for the lock, *between two attempts*.
    pub const MAX_BACKOFF_MS: u32 = 1000;

    /// Create a new store-based lock implemented as a value in the
    /// crypto-store.
    ///
    /// # Parameters
    ///
    /// - `lock_key`: key in the key-value store to store the lock's state.
    /// - `lock_holder`: identify the lock's holder with this given value.
    pub fn new(store: Arc<DynCryptoStore>, lock_key: String, lock_holder: String) -> Self {
        Self {
            store,
            lock_key,
            lock_holder,
            backoff: Arc::new(Mutex::new(WaitingTime::Some(Self::INITIAL_BACKOFF_MS))),
            num_holders: Arc::new(StdMutex::new(0)),
            extend_lease: Arc::new(Mutex::new(None)),
        }
    }

    /// Try to lock once, returns whether the lock was obtained or not.
    #[instrument(skip(self), fields(?self.lock_key, ?self.lock_holder))]
    pub async fn try_lock_once(&self) -> Result<Option<CryptoStoreLockGuard>, CryptoStoreError> {
        // Hold onto the task lock for the entire lifetime of this function, to avoid
        // multiple reentrant calls.
        let mut task = self.extend_lease.lock().await;

        {
            let mut holders = self.num_holders.lock().unwrap();

            // If another thread obtained the lock, make sure to only superficially increase
            // the number of holders, and carry on.
            if *holders > 0 {
                trace!("We already had the lock, incrementing holder count");
                *holders += 1;
                let guard = CryptoStoreLockGuard { num_holders: self.num_holders.clone() };
                return Ok(Some(guard));
            }
        }

        let acquired = self
            .store
            .try_take_leased_lock(Self::LEASE_MS, &self.lock_key, &self.lock_holder)
            .await?;

        if acquired {
            *self.num_holders.lock().unwrap() += 1;

            let key = self.lock_key.clone();
            let holder = self.lock_holder.clone();
            let store = self.store.clone();
            let num_holders = self.num_holders.clone();

            *task = Some(tokio::spawn(async move {
                loop {
                    {
                        let num_holders = num_holders.lock().unwrap();
                        if *num_holders == 0 {
                            tracing::info!("exiting the lease extension loop");
                            break;
                        }
                    }

                    if let Err(err) =
                        store.try_take_leased_lock(Self::LEASE_MS, &key, &holder).await
                    {
                        tracing::error!("error when extending lock lease: {err:#}");
                        break;
                    }

                    sleep(Duration::from_millis(Self::EXTEND_LEASE_EVERY_MS)).await;
                }
            }));

            let guard = CryptoStoreLockGuard { num_holders: self.num_holders.clone() };
            Ok(Some(guard))
        } else {
            Ok(None)
        }
    }

    /// Attempt to take the lock, with exponential backoff if the lock has
    /// already been taken before.
    ///
    /// The `max_backoff` parameter is the maximum time (in milliseconds) that
    /// should be waited for, between two attempts. When that time is
    /// reached a second time, the lock will stop attempting to get the lock
    /// and will return a timeout error upon locking. If not provided,
    /// will wait for [`Self::MAX_BACKOFF_MS`].
    #[instrument(skip(self), fields(?self.lock_key, ?self.lock_holder))]
    pub async fn spin_lock(
        &self,
        max_backoff: Option<u32>,
    ) -> Result<CryptoStoreLockGuard, CryptoStoreError> {
        let max_backoff = max_backoff.unwrap_or(Self::MAX_BACKOFF_MS);

        // Note: reads/writes to the backoff are racy across threads in theory, but the
        // lock in `try_lock_once` should sequentialize it all.

        loop {
            if let Some(guard) = self.try_lock_once().await? {
                // Reset backoff before returning, for the next attempt to lock.
                *self.backoff.lock().await = WaitingTime::Some(Self::INITIAL_BACKOFF_MS);
                return Ok(guard);
            }

            // Exponential backoff! Multiply by 2 the time we've waited before, cap it to
            // max_backoff.
            let mut backoff = self.backoff.lock().await;

            let wait = match &mut *backoff {
                WaitingTime::Some(ref mut val) => {
                    let wait = *val;
                    *val = val.saturating_mul(2);
                    if *val >= max_backoff {
                        *backoff = WaitingTime::Stop;
                    }
                    wait
                }
                WaitingTime::Stop => {
                    // We've reached the maximum backoff, abandon.
                    return Err(LockStoreError::LockTimeout.into());
                }
            };

            tracing::debug!("Waiting {wait} before re-attempting to take the lock");
            sleep(Duration::from_millis(wait.into())).await;
        }
    }
}

/// Error related to the locking API of the crypto store.
#[derive(Debug, thiserror::Error)]
pub enum LockStoreError {
    /// A lock value was to be removed, but it didn't contain the expected lock
    /// value.
    #[error("a lock value was to be removed, but it didn't contain the expected lock value")]
    IncorrectLockValue,

    /// A lock value was to be removed, but it was missing in the database.
    #[error("a lock value was to be removed, but it was missing in the database")]
    MissingLockValue,

    /// Spent too long waiting for a database lock.
    #[error("a lock timed out")]
    LockTimeout,
}
