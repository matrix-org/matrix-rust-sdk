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

use tokio::time::sleep;

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

    /// The key used in the key/value mapping for the lock entry.
    lock_key: String,

    /// A specific value to identify the lock's holder.
    lock_holder: String,

    /// Backoff time, in milliseconds.
    backoff: WaitingTime,

    /// Maximum backoff time, between two attempts.
    max_backoff: u32,
}

impl CryptoStoreLock {
    /// Initial backoff, in milliseconds. This is the time we wait the first
    /// time, if taking the lock initially failed.
    const INITIAL_BACKOFF_MS: u32 = 10;

    /// Maximal backoff, in milliseconds. This is the maximum amount of time
    /// we'll wait for the lock, *between two attempts*.
    const MAX_BACKOFF_MS: u32 = 1000;

    /// Create a new store-based lock implemented as a value in the
    /// crypto-store.
    ///
    /// # Parameters
    ///
    /// - `lock_key`: key in the key-value store to store the lock's state.
    /// - `lock_holder`: identify the lock's holder with this given value.
    /// - `max_backoff`: maximum time (in milliseconds) that should be waited
    ///   for, between two
    /// attempts. When that time is reached a second time, the lock will stop
    /// attempting to get the lock and will return a timeout error upon
    /// locking. If not provided, will wait for `Self::MAX_BACKOFF_MS`.
    pub fn new(
        store: Arc<DynCryptoStore>,
        lock_key: String,
        lock_holder: String,
        max_backoff: Option<u32>,
    ) -> Self {
        let max_backoff = max_backoff.unwrap_or(Self::MAX_BACKOFF_MS);
        Self {
            store,
            lock_key,
            lock_holder,
            max_backoff,
            backoff: WaitingTime::Some(Self::INITIAL_BACKOFF_MS),
        }
    }

    /// Attempt to take the lock, with exponential backoff if the lock has
    /// already been taken before.
    pub async fn lock(&mut self) -> Result<(), CryptoStoreError> {
        loop {
            let inserted = self
                .store
                .insert_custom_value_if_missing(
                    &self.lock_key,
                    self.lock_holder.as_bytes().to_vec(),
                )
                .await?;

            if inserted {
                // Reset backoff before returning, for the next attempt to lock.
                self.backoff = WaitingTime::Some(Self::INITIAL_BACKOFF_MS);
                return Ok(());
            }

            // Double-check that we were not interrupted last time we tried to take the
            // lock, and forgot to release it; in that case, we *still* hold it.
            let previous = self.store.get_custom_value(&self.lock_key).await?;
            if previous.as_deref() == Some(self.lock_holder.as_bytes()) {
                // At this point, the only possible value for backoff is the initial one, but
                // better be safe than sorry.
                tracing::warn!(
                    "Crypto-store lock {} was already taken by {}; let's pretend we just acquired it.",
                    self.lock_key,
                    self.lock_holder
                );
                self.backoff = WaitingTime::Some(Self::INITIAL_BACKOFF_MS);
                return Ok(());
            }

            let wait = match self.backoff {
                WaitingTime::Some(val) => val,
                WaitingTime::Stop => {
                    // We've reached the maximum backoff, abandon.
                    return Err(LockStoreError::LockTimeout.into());
                }
            };

            // Exponential backoff! Multiply by 2 the time we've waited before, cap it to
            // max_backoff.
            let next_value = wait.saturating_mul(2);
            self.backoff = if next_value >= self.max_backoff {
                WaitingTime::Stop
            } else {
                WaitingTime::Some(next_value)
            };

            sleep(Duration::from_millis(wait.into())).await;
        }
    }

    /// Release the lock taken previously with [`Self::lock()`].
    ///
    /// Will return an error if the lock wasn't taken.
    pub async fn unlock(&mut self) -> Result<(), CryptoStoreError> {
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
