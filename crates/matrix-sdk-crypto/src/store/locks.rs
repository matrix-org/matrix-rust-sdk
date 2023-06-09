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
//! Those locks are implemented as one value in the key-value crypto store, that exists if and only
//! if the lock has been taken. For this to work correctly, we rely on multiple assumptions:
//!
//! - the store must allow concurrent reads and writes from multiple processes. For instance, for
//! sqlite, this means that it is running in [WAL](https://www.sqlite.org/wal.html) mode.
//! - the two operations used in the store implementation, `insert_custom_value_if_missing` and
//! `remove_custom_value`, must be atomic / implemented in a transaction.

use super::DynCryptoStore;
use crate::CryptoStoreError;
use std::{sync::Arc, time::Duration};
use tokio::{runtime::Handle, task::spawn_blocking, time::sleep};

/// A store-based lock for the `CryptoStore`.
#[derive(Debug, Clone)]
pub struct CryptoStoreLock {
    store: Arc<DynCryptoStore>,
    lock_key: String,
    backoff: u32,
}

impl CryptoStoreLock {
    /// Initial backoff, in milliseconds. This is the time we wait the first time, if taking the
    /// lock initially failed.
    const INITIAL_BACKOFF_MS: u32 = 10;
    /// Maximal backoff, in milliseconds. This is the maximum amount of time we'll wait for the
    /// lock, *between two attempts*.
    const MAX_BACKOFF_MS: u32 = 2000;
    // TODO generate a random value per instance of the lock?
    const SENTINEL_VALUE: &str = "lock_taken";

    /// Create a new store-based lock implemented as a value in the crypto-store.
    pub fn new(store: Arc<DynCryptoStore>, lock_key: String) -> Self {
        Self { store, lock_key, backoff: Self::INITIAL_BACKOFF_MS }
    }

    /// Attempt to take the lock, with exponential backoff if the lock has already been taken
    /// before.
    pub async fn lock(&mut self) -> Result<(), CryptoStoreError> {
        loop {
            let inserted = self
                .store
                .insert_custom_value_if_missing(
                    &self.lock_key,
                    Self::SENTINEL_VALUE.as_bytes().to_vec(),
                )
                .await?;
            if inserted {
                self.backoff = Self::INITIAL_BACKOFF_MS;
                return Ok(());
            }

            // Exponential backoff! Multiply by 2 the time we've waited before, cap it to 1 second.
            let wait = self.backoff;

            if wait == Self::MAX_BACKOFF_MS {
                // We've reached the maximum backoff, abandon.
                return Err(CryptoStoreError::LockTimeout);
            }

            self.backoff *= 2;
            self.backoff = self.backoff.min(Self::MAX_BACKOFF_MS);

            sleep(Duration::from_millis(wait.into())).await;
        }
    }

    /// Release the lock taken previously with [`lock()`].
    ///
    /// Will return an error if the lock wasn't taken.
    pub async fn unlock(&mut self) -> Result<(), CryptoStoreError> {
        let read = self
            .store
            .get_custom_value(&self.lock_key)
            .await?
            .ok_or(CryptoStoreError::MissingLockValue)?;

        if read != Self::SENTINEL_VALUE.as_bytes() {
            return Err(CryptoStoreError::IncorrectLockValue);
        }

        let removed = self.store.remove_custom_value(&self.lock_key).await?;
        if removed {
            Ok(())
        } else {
            Err(CryptoStoreError::MissingLockValue)
        }
    }
}

/// RAII struct that implements the semantics of taking/release a `CryptoStoreLock` automatically.
#[derive(Debug)]
pub struct CryptoStoreLockGuard {
    lock: CryptoStoreLock,
}

impl CryptoStoreLockGuard {
    /// Creates a new `CryptoStoreLockGuard` with the given key, in the given store.
    ///
    /// The drop implementation assumes the code is running in a `tokio` environment, so make sure
    /// we're inside a tokio runtime when dropping this data structure.
    ///
    /// See also [`CryptoStoreLock`] to learn more about the lock's properties.
    pub async fn new(
        store: Arc<DynCryptoStore>,
        lock_key: String,
    ) -> Result<Self, CryptoStoreError> {
        let mut lock = CryptoStoreLock::new(store, lock_key);
        lock.lock().await?;
        Ok(Self { lock })
    }
}

impl Drop for CryptoStoreLockGuard {
    fn drop(&mut self) {
        // No async drop 😥
        // 1. Clone the lock as a sacrifice to borrowck (otherwise the `&mut self` reference would
        //    need to be static),
        // 2. We'll need to block_on; spawn a blocking task to do just that.
        let mut lock = self.lock.clone();

        spawn_blocking(move || {
            if let Err(err) = Handle::current().block_on(lock.unlock()) {
                tracing::error!("error when releasing a lock: {err:#}");
                panic!("{err:#}");
            }
        });
    }
}
