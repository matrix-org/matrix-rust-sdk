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

use std::{sync::Arc, time::Duration};

use tokio::{sync::Mutex, time::sleep};
use tracing::{instrument, trace, warn};

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
    num_holders: Arc<Mutex<u32>>,

    /// The key used in the key/value mapping for the lock entry.
    lock_key: String,

    /// A specific value to identify the lock's holder.
    lock_holder: String,

    /// Backoff time, in milliseconds.
    backoff: Arc<Mutex<WaitingTime>>,
}

impl CryptoStoreLock {
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
            num_holders: Arc::new(Mutex::new(0)),
        }
    }

    /// Try to lock once, returns whether the lock was obtained or not.
    #[instrument(skip(self), fields(?self.lock_key, ?self.lock_holder))]
    pub async fn try_lock_once(&self) -> Result<bool, CryptoStoreError> {
        // Hold the num_holders lock for the entire's function lifetime, to avoid
        // internal races if called in a reentrant manner.
        let mut holders = self.num_holders.lock().await;

        // If another thread obtained the lock, make sure to only superficially increase
        // the number of holders, and carry on.
        if *holders > 0 {
            trace!("We already had the lock, incrementing holder count");
            *holders += 1;
            return Ok(true);
        }

        let inserted = self
            .store
            .insert_custom_value_if_missing(&self.lock_key, self.lock_holder.as_bytes().to_vec())
            .await?;

        if inserted {
            trace!("Successfully acquired lock through db write");
            *holders += 1;
            return Ok(true);
        }

        // Double-check that we were not interrupted last time we tried to take the
        // lock, and forgot to release it; in that case, we *still* hold it.
        let previous = self.store.get_custom_value(&self.lock_key).await?;
        if previous.as_deref() == Some(self.lock_holder.as_bytes()) {
            warn!(
                "Crypto-store lock {} was already taken by {}; let's pretend we just acquired it.",
                self.lock_key, self.lock_holder
            );
            *holders += 1;
            return Ok(true);
        }

        if let Some(prev_holder) = previous {
            trace!("Lock is already taken by {}", String::from_utf8_lossy(&prev_holder));
        }

        Ok(false)
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
    pub async fn spin_lock(&self, max_backoff: Option<u32>) -> Result<(), CryptoStoreError> {
        let max_backoff = max_backoff.unwrap_or(Self::MAX_BACKOFF_MS);

        // Note: reads/writes to the backoff are racy across threads in theory, but the
        // lock in `try_lock_once` should sequentialize it all.

        loop {
            if self.try_lock_once().await? {
                // Reset backoff before returning, for the next attempt to lock.
                *self.backoff.lock().await = WaitingTime::Some(Self::INITIAL_BACKOFF_MS);
                return Ok(());
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

    /// Release the lock taken previously with [`Self::try_lock_once()`].
    ///
    /// Will return an error if the lock wasn't taken.
    #[instrument(skip(self), fields(?self.lock_key, ?self.lock_holder))]
    pub async fn unlock(&self) -> Result<(), CryptoStoreError> {
        // Keep the lock for the whole's function lifetime, to avoid races with other
        // threads trying to acquire/release  the lock at the same time.
        let mut holders = self.num_holders.lock().await;

        if *holders > 1 {
            // There's at least one other holder, so just decrease the number of holders.
            *holders -= 1;
            trace!("not releasing, because another thread holds onto it");
            return Ok(());
        }

        // Here, holders == 1 (or 0, but that is supposed to trigger the
        // `MissingLockValue` error then).

        let read = self
            .store
            .get_custom_value(&self.lock_key)
            .await?
            .ok_or(CryptoStoreError::from(LockStoreError::MissingLockValue))?;

        if read != self.lock_holder.as_bytes() {
            return Err(LockStoreError::IncorrectLockValue.into());
        }

        let removed = self.store.remove_custom_value(&self.lock_key).await?;
        if removed {
            *holders = 0;
            trace!("successfully released");
            Ok(())
        } else {
            Err(LockStoreError::MissingLockValue.into())
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

#[cfg(test)]
mod tests {
    use assert_matches::assert_matches;
    use matrix_sdk_test::async_test;
    use tokio::spawn;

    use super::*;
    use crate::store::{IntoCryptoStore as _, MemoryStore};

    #[async_test]
    async fn test_simple_lock_unlock() -> Result<(), CryptoStoreError> {
        let mem_store = MemoryStore::new();
        let dyn_store = mem_store.into_crypto_store();

        let lock = CryptoStoreLock::new(dyn_store, "key".to_owned(), "first".to_owned());

        // The lock plain works when used with a single holder.
        let acquired = lock.try_lock_once().await?;
        assert!(acquired);
        assert_eq!(*lock.num_holders.lock().await, 1);

        // Releasing works.
        assert!(lock.unlock().await.is_ok());
        assert_eq!(*lock.num_holders.lock().await, 0);

        // Releasing another time is an error.
        assert_matches!(
            lock.unlock().await,
            Err(CryptoStoreError::Lock(LockStoreError::MissingLockValue))
        );

        // Spin locking on the same lock always works, assuming no concurrent access.
        assert!(lock.spin_lock(None).await.is_ok());

        // Releasing works again.
        assert!(lock.unlock().await.is_ok());

        // Releasing another time is still an error.
        assert_matches!(
            lock.unlock().await,
            Err(CryptoStoreError::Lock(LockStoreError::MissingLockValue))
        );

        Ok(())
    }

    #[async_test]
    async fn test_self_recovery() -> Result<(), CryptoStoreError> {
        let mem_store = MemoryStore::new();
        let dyn_store = mem_store.into_crypto_store();

        let lock = CryptoStoreLock::new(dyn_store.clone(), "key".to_owned(), "first".to_owned());

        // When a lock is acquired...
        let acquired = lock.try_lock_once().await?;
        assert!(acquired);
        assert_eq!(*lock.num_holders.lock().await, 1);

        // But then forgotten...
        drop(lock);

        // The DB still knows about it...
        let prev = dyn_store.get_custom_value("key").await?;
        assert_eq!(String::from_utf8_lossy(prev.as_ref().unwrap()), "first");

        // And when rematerializing the lock with the same key/value...
        let lock = CryptoStoreLock::new(dyn_store.clone(), "key".to_owned(), "first".to_owned());

        // We still got it.
        let acquired = lock.try_lock_once().await?;
        assert!(acquired);
        assert_eq!(*lock.num_holders.lock().await, 1);

        // And can release it.
        assert!(lock.unlock().await.is_ok());

        Ok(())
    }

    #[async_test]
    async fn test_multiple_holders_same_process() -> Result<(), CryptoStoreError> {
        let mem_store = MemoryStore::new();
        let dyn_store = mem_store.into_crypto_store();

        let lock = CryptoStoreLock::new(dyn_store, "key".to_owned(), "first".to_owned());

        // Taking the lock twice...
        let acquired = lock.try_lock_once().await?;
        assert!(acquired);

        let acquired = lock.try_lock_once().await?;
        assert!(acquired);

        assert_eq!(*lock.num_holders.lock().await, 2);

        // ...means we can release it twice.
        assert!(lock.unlock().await.is_ok());

        assert!(lock.unlock().await.is_ok());

        // Releasing another time is still an error.
        assert_matches!(
            lock.unlock().await,
            Err(CryptoStoreError::Lock(LockStoreError::MissingLockValue))
        );

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
        assert!(acquired1);

        // The second can't take it immediately.
        let acquired2 = lock2.try_lock_once().await?;
        assert!(!acquired2);

        let lock2_clone = lock2.clone();
        let handle = spawn(async move { lock2_clone.spin_lock(Some(1000)).await });

        sleep(Duration::from_millis(100)).await;

        lock1.unlock().await?;

        // lock2 in the background managed to get the lock.
        assert!(handle.await.is_ok());

        // Now if lock1 tries to get the lock with a small timeout, it will fail.
        assert_matches!(
            lock1.spin_lock(Some(200)).await,
            Err(CryptoStoreError::Lock(LockStoreError::LockTimeout))
        );

        Ok(())
    }
}
