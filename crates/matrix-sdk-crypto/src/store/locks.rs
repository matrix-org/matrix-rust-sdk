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
//! This is a per-process lock that may be used only for very specific use
//! cases, where multiple processes might concurrently write to the same
//! database at the same time; this would invalidate crypto store caches, so
//! that should be done mindfully. Such a lock can be acquired multiple times by
//! the same process, and it remains active as long as there's at least one user
//! in a given process.
//!
//! The lock is implemented using time-based leases to values inserted in a
//! crypto store. The store maintains the lock identifier (key), who's the
//! current holder (value), and an expiration timestamp on the side; see also
//! `CryptoStore::try_take_leased_lock` for more details.
//!
//! The lock is initially acquired for a certain period of time (namely, the
//! duration of a lease, aka `LEASE_DURATION_MS`), and then a "heartbeat" task
//! renews the lease to extend its duration, every so often (namely, every
//! `EXTEND_LEASE_EVERY_MS`). Since the tokio scheduler might be busy, the
//! extension request should happen way more frequently than the duration of a
//! lease, in case a deadline is missed. The current values have been chosen to
//! reflect that, with a ratio of 1:10 as of 2023-06-23.
//!
//! Releasing the lock happens naturally, by not renewing a lease. It happens
//! automatically after the duration of the last lease, at most.

use std::{
    sync::{atomic::AtomicU32, Arc},
    time::Duration,
};

use matrix_sdk_common::executor::JoinHandle;
use tokio::{sync::Mutex, time::sleep};
use tracing::instrument;

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
    num_holders: Arc<AtomicU32>,
}

impl Drop for CryptoStoreLockGuard {
    fn drop(&mut self) {
        self.num_holders.fetch_sub(1, atomic::Ordering::SeqCst);
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
    num_holders: Arc<AtomicU32>,

    /// A mutex to control an attempt to take the lock, to avoid making it
    /// reentrant.
    locking_attempt: Arc<Mutex<()>>,

    /// Current renew task spawned by `try_lock_once`.
    renew_task: Arc<Mutex<Option<JoinHandle<()>>>>,

    /// The key used in the key/value mapping for the lock entry.
    lock_key: String,

    /// A specific value to identify the lock's holder.
    lock_holder: String,

    /// Backoff time, in milliseconds.
    backoff: Arc<Mutex<WaitingTime>>,
}

impl CryptoStoreLock {
    /// Amount of time a lease of the lock should last, in milliseconds.
    pub const LEASE_DURATION_MS: u32 = 500;

    /// Period of time between two attempts to extend the lease. We'll
    /// re-request a lease for an entire duration of `LEASE_DURATION_MS`
    /// milliseconds, every `EXTEND_LEASE_EVERY_MS`, so this has to
    /// be an amount safely low compared to `LEASE_DURATION_MS`, to make sure
    /// that we can miss a deadline without compromising the lock.
    const EXTEND_LEASE_EVERY_MS: u64 = 50;

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
            num_holders: Arc::new(0.into()),
            locking_attempt: Arc::new(Mutex::new(())),
            renew_task: Default::default(),
        }
    }

    /// Try to lock once, returns whether the lock was obtained or not.
    #[instrument(skip(self), fields(?self.lock_key, ?self.lock_holder))]
    pub async fn try_lock_once(&self) -> Result<Option<CryptoStoreLockGuard>, CryptoStoreError> {
        // Hold onto the locking attempt mutex for the entire lifetime of this
        // function, to avoid multiple reentrant calls.
        let mut _attempt = self.locking_attempt.lock().await;

        // If another thread obtained the lock, make sure to only superficially increase
        // the number of holders, and carry on.
        if self.num_holders.load(atomic::Ordering::SeqCst) > 0 {
            // Note: between the above load and the fetch_add below, another thread may
            // decrement `num_holders`. That's fine because that means the lock
            // was taken by at least one thread, and after this call it will be
            // taken by at least one thread.
            tracing::trace!("We already had the lock, incrementing holder count");
            self.num_holders.fetch_add(1, atomic::Ordering::SeqCst);
            let guard = CryptoStoreLockGuard { num_holders: self.num_holders.clone() };
            return Ok(Some(guard));
        }

        let acquired = self
            .store
            .try_take_leased_lock(Self::LEASE_DURATION_MS, &self.lock_key, &self.lock_holder)
            .await?;

        if !acquired {
            tracing::trace!("Couldn't acquire the lock immediately.");
            return Ok(None);
        }

        tracing::trace!("Acquired the lock, spawning the lease extension task.");

        // This is the first time we've acquired the lock. We're going to spawn the task
        // that will renew the lease.

        // Clone data to be owned by the task.
        let this = self.clone();

        let mut renew_task = self.renew_task.lock().await;

        // Cancel the previous task, if any. That's safe to do, because:
        // - either the task was done,
        // - or it was still running, but taking a lock in the db has to be an atomic
        //   operation
        // running in a transaction.

        if let Some(_prev) = renew_task.take() {
            #[cfg(not(target_arch = "wasm32"))]
            _prev.abort();
        }

        // Restart a new one.
        *renew_task = Some(matrix_sdk_common::executor::spawn(async move {
            loop {
                {
                    // First, check if there are still users of this lock.
                    //
                    // This is not racy, because:
                    // - the `locking_attempt` mutex makes sure we don't have unexpected
                    // interactions with the non-atomic sequence above in `try_lock_once`
                    // (check > 0, then add 1).
                    // - other entities holding onto the `num_holders` atomic will only
                    // decrease it over time.

                    let _guard = this.locking_attempt.lock().await;

                    // If there are no more users, we can quit.
                    if this.num_holders.load(atomic::Ordering::SeqCst) == 0 {
                        tracing::info!("exiting the lease extension loop");

                        // Cancel the lease with another 0ms lease.
                        // If we don't get the lock, that's (weird but) fine.
                        let _ = this
                            .store
                            .try_take_leased_lock(0, &this.lock_key, &this.lock_holder)
                            .await;

                        // Exit the loop.
                        break;
                    }
                }

                sleep(Duration::from_millis(Self::EXTEND_LEASE_EVERY_MS)).await;

                if let Err(err) = this
                    .store
                    .try_take_leased_lock(
                        Self::LEASE_DURATION_MS,
                        &this.lock_key,
                        &this.lock_holder,
                    )
                    .await
                {
                    tracing::error!("error when extending lock lease: {err:#}");
                    // Exit the loop.
                    break;
                }
            }
        }));

        self.num_holders.fetch_add(1, atomic::Ordering::SeqCst);

        let guard = CryptoStoreLockGuard { num_holders: self.num_holders.clone() };
        Ok(Some(guard))
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

    /// Returns the value in the database that represents the holder's
    /// identifier.
    pub fn lock_holder(&self) -> &str {
        &self.lock_holder
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

    /// The generation counter is missing, and should always be present.
    #[error("missing generation counter in the store")]
    MissingGeneration,

    /// Unexpected format for the generation counter. Is someone tampering the
    /// database?
    #[error("invalid format of the generation counter")]
    InvalidGenerationFormat,
}

#[cfg(test)]
mod tests {
    use assert_matches::assert_matches;
    use matrix_sdk_test::async_test;
    use tokio::spawn;

    use super::*;
    use crate::store::{IntoCryptoStore as _, MemoryStore};

    async fn release_lock(guard: Option<CryptoStoreLockGuard>) {
        drop(guard);
        sleep(Duration::from_millis(CryptoStoreLock::EXTEND_LEASE_EVERY_MS)).await;
    }

    #[async_test]
    async fn test_simple_lock_unlock() -> Result<(), CryptoStoreError> {
        let mem_store = MemoryStore::new();
        let dyn_store = mem_store.into_crypto_store();

        let lock = CryptoStoreLock::new(dyn_store, "key".to_owned(), "first".to_owned());

        // The lock plain works when used with a single holder.
        let acquired = lock.try_lock_once().await?;
        assert!(acquired.is_some());
        assert_eq!(lock.num_holders.load(atomic::Ordering::SeqCst), 1);

        // Releasing works.
        release_lock(acquired).await;
        assert_eq!(lock.num_holders.load(atomic::Ordering::SeqCst), 0);

        // Spin locking on the same lock always works, assuming no concurrent access.
        let acquired = lock.spin_lock(None).await.unwrap();

        // Releasing still works.
        release_lock(Some(acquired)).await;
        assert_eq!(lock.num_holders.load(atomic::Ordering::SeqCst), 0);

        Ok(())
    }

    #[async_test]
    async fn test_self_recovery() -> Result<(), CryptoStoreError> {
        let mem_store = MemoryStore::new();
        let dyn_store = mem_store.into_crypto_store();

        let lock = CryptoStoreLock::new(dyn_store.clone(), "key".to_owned(), "first".to_owned());

        // When a lock is acquired...
        let acquired = lock.try_lock_once().await?;
        assert!(acquired.is_some());
        assert_eq!(lock.num_holders.load(atomic::Ordering::SeqCst), 1);

        // But then forgotten... (note: no need to release the guard)
        drop(lock);

        // And when rematerializing the lock with the same key/value...
        let lock = CryptoStoreLock::new(dyn_store.clone(), "key".to_owned(), "first".to_owned());

        // We still got it.
        let acquired = lock.try_lock_once().await?;
        assert!(acquired.is_some());
        assert_eq!(lock.num_holders.load(atomic::Ordering::SeqCst), 1);

        Ok(())
    }

    #[async_test]
    async fn test_multiple_holders_same_process() -> Result<(), CryptoStoreError> {
        let mem_store = MemoryStore::new();
        let dyn_store = mem_store.into_crypto_store();

        let lock = CryptoStoreLock::new(dyn_store, "key".to_owned(), "first".to_owned());

        // Taking the lock twice...
        let acquired = lock.try_lock_once().await?;
        assert!(acquired.is_some());

        let acquired2 = lock.try_lock_once().await?;
        assert!(acquired2.is_some());

        assert_eq!(lock.num_holders.load(atomic::Ordering::SeqCst), 2);

        // ...means we can release it twice.
        release_lock(acquired).await;
        assert_eq!(lock.num_holders.load(atomic::Ordering::SeqCst), 1);

        release_lock(acquired2).await;
        assert_eq!(lock.num_holders.load(atomic::Ordering::SeqCst), 0);

        Ok(())
    }

    #[async_test]
    async fn test_multiple_processes() -> Result<(), CryptoStoreError> {
        let mem_store = MemoryStore::new();
        let dyn_store = mem_store.into_crypto_store();

        let lock1 = CryptoStoreLock::new(dyn_store.clone(), "key".to_owned(), "first".to_owned());
        let lock2 = CryptoStoreLock::new(dyn_store, "key".to_owned(), "second".to_owned());

        // When the first process takes the lock...
        let acquired1 = lock1.try_lock_once().await?;
        assert!(acquired1.is_some());

        // The second can't take it immediately.
        let acquired2 = lock2.try_lock_once().await?;
        assert!(acquired2.is_none());

        let lock2_clone = lock2.clone();
        let handle = spawn(async move { lock2_clone.spin_lock(Some(1000)).await });

        sleep(Duration::from_millis(100)).await;

        drop(acquired1);

        // lock2 in the background manages to get the lock at some point.
        let _acquired2 = handle
            .await
            .expect("join handle is properly awaited")
            .expect("lock was obtained after spin-locking");

        // Now if lock1 tries to get the lock with a small timeout, it will fail.
        assert_matches!(
            lock1.spin_lock(Some(200)).await,
            Err(CryptoStoreError::Lock(LockStoreError::LockTimeout))
        );

        Ok(())
    }
}
