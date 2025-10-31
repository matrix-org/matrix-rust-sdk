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

//! A cross-process lock implementation.
//!
//! This is a per-process lock that may be used only for very specific use
//! cases, where multiple processes might concurrently write to the same
//! database at the same time; this would invalidate store caches, so
//! that should be done mindfully. Such a lock can be obtained multiple times by
//! the same process, and it remains active as long as there's at least one user
//! in a given process.
//!
//! The lock is implemented using time-based leases. The lock maintains the lock
//! identifier (key), who's the current holder (value), and an expiration
//! timestamp on the side; see also `CryptoStore::try_take_leased_lock` for more
//! details.
//!
//! The lock is initially obtainedd for a certain period of time (namely, the
//! duration of a lease, aka `LEASE_DURATION_MS`), and then a “heartbeat” task
//! renews the lease to extend its duration, every so often (namely, every
//! `EXTEND_LEASE_EVERY_MS`). Since the Tokio scheduler might be busy, the
//! extension request should happen way more frequently than the duration of a
//! lease, in case a deadline is missed. The current values have been chosen to
//! reflect that, with a ratio of 1:10 as of 2023-06-23.
//!
//! Releasing the lock happens naturally, by not renewing a lease. It happens
//! automatically after the duration of the last lease, at most.

use std::{
    error::Error,
    future::Future,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
    },
    time::Duration,
};

use tokio::sync::Mutex;
use tracing::{debug, error, instrument, trace, warn};

use crate::{
    SendOutsideWasm,
    executor::{JoinHandle, spawn},
    sleep::sleep,
};

/// A lock generation is an integer incremented each time the lock is taken by
/// a different holder.
///
/// This is used to know if a lock has been dirtied.
pub type CrossProcessLockGeneration = u64;

/// Trait used to try to take a lock. Foundation of [`CrossProcessLock`].
pub trait TryLock {
    #[cfg(not(target_family = "wasm"))]
    type LockError: Error + Send + Sync;

    #[cfg(target_family = "wasm")]
    type LockError: Error;

    /// Try to take a leased lock.
    ///
    /// This attempts to take a lock for the given lease duration.
    ///
    /// - If we already had the lease, this will extend the lease.
    /// - If we didn't, but the previous lease has expired, we will obtain the
    ///   lock.
    /// - If there was no previous lease, we will obtain the lock.
    /// - Otherwise, we don't get the lock.
    ///
    /// Returns `Some(_)` to indicate the lock succeeded, `None` otherwise. The
    /// cross-process lock generation must be compared to the generation before
    /// the call to see if the lock has been dirtied: a different generation
    /// means the lock has been dirtied, i.e. taken by a different holder in
    /// the meantime.
    fn try_lock(
        &self,
        lease_duration_ms: u32,
        key: &str,
        holder: &str,
    ) -> impl Future<Output = Result<Option<CrossProcessLockGeneration>, Self::LockError>>
    + SendOutsideWasm;
}

/// Small state machine to handle wait times.
#[derive(Clone, Debug)]
enum WaitingTime {
    /// Some time to wait, in milliseconds.
    Some(u32),
    /// Stop waiting when seeing this value.
    Stop,
}

/// A guard of a cross-process lock.
///
/// The lock will be automatically released a short period of time after all the
/// guards have dropped.
#[derive(Debug)]
pub struct CrossProcessLockGuard {
    num_holders: Arc<AtomicU32>,
}

impl CrossProcessLockGuard {
    fn new(num_holders: Arc<AtomicU32>) -> Self {
        Self { num_holders }
    }
}

impl Drop for CrossProcessLockGuard {
    fn drop(&mut self) {
        self.num_holders.fetch_sub(1, Ordering::SeqCst);
    }
}

/// A cross-process lock implementation.
///
/// See the doc-comment of this module for more information.
#[derive(Clone, Debug)]
pub struct CrossProcessLock<L>
where
    L: TryLock + Clone + SendOutsideWasm + 'static,
{
    /// The locker implementation.
    ///
    /// `L` is responsible for trying to take the lock, while
    /// [`CrossProcessLock`] is responsible to make it cross-process, with the
    /// retry mechanism, plus guard and so on.
    locker: L,

    /// Number of holders of the lock in this process.
    ///
    /// If greater than 0, this means we've already obtained this lock, in this
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

    /// This lock generation.
    generation: Arc<AtomicU64>,

    /// Whether the lock has been dirtied.
    ///
    /// See [`CrossProcessLockResult::Dirty`] to learn more about the semantics
    /// of _dirty_.
    is_dirty: Arc<AtomicBool>,
}

/// Amount of time a lease of the lock should last, in milliseconds.
pub const LEASE_DURATION_MS: u32 = 500;

/// Period of time between two attempts to extend the lease. We'll
/// re-request a lease for an entire duration of `LEASE_DURATION_MS`
/// milliseconds, every `EXTEND_LEASE_EVERY_MS`, so this has to
/// be an amount safely low compared to `LEASE_DURATION_MS`, to make sure
/// that we can miss a deadline without compromising the lock.
pub const EXTEND_LEASE_EVERY_MS: u64 = 50;

/// Initial backoff, in milliseconds. This is the time we wait the first
/// time, if taking the lock initially failed.
const INITIAL_BACKOFF_MS: u32 = 10;

/// Maximal backoff, in milliseconds. This is the maximum amount of time
/// we'll wait for the lock, *between two attempts*.
pub const MAX_BACKOFF_MS: u32 = 1000;

/// Sentinel value representing the absence of a lock generation value.
///
/// When the lock is created, it has no generation. Once locked, it receives its
/// first generation from [`TryLock::try_lock`]. Subsequent lockings may
/// generate new lock generation. The generation is incremented by 1 every time.
///
/// The first generation is defined by [`FIRST_CROSS_PROCESS_LOCK_GENERATION`].
pub const NO_CROSS_PROCESS_LOCK_GENERATION: CrossProcessLockGeneration = 0;

/// Describe the first lock generation value (see
/// [`CrossProcessLockGeneration`]).
pub const FIRST_CROSS_PROCESS_LOCK_GENERATION: CrossProcessLockGeneration = 1;

impl<L> CrossProcessLock<L>
where
    L: TryLock + Clone + SendOutsideWasm + 'static,
{
    /// Create a new cross-process lock.
    ///
    /// # Parameters
    ///
    /// - `lock_key`: key in the key-value store to store the lock's state.
    /// - `lock_holder`: identify the lock's holder with this given value.
    pub fn new(locker: L, lock_key: String, lock_holder: String) -> Self {
        Self {
            locker,
            lock_key,
            lock_holder,
            backoff: Arc::new(Mutex::new(WaitingTime::Some(INITIAL_BACKOFF_MS))),
            num_holders: Arc::new(0.into()),
            locking_attempt: Arc::new(Mutex::new(())),
            renew_task: Default::default(),
            generation: Arc::new(AtomicU64::new(NO_CROSS_PROCESS_LOCK_GENERATION)),
            is_dirty: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Determine whether the cross-process lock is dirty.
    ///
    /// See [`CrossProcessLockResult::Dirty`] to learn more about the semantics
    /// of _dirty_.
    pub fn is_dirty(&self) -> bool {
        self.is_dirty.load(Ordering::SeqCst)
    }

    /// Clear the dirty state from this cross-process lock.
    ///
    /// If the cross-process lock is dirtied, it will remain dirtied until
    /// this method is called. This allows recovering from a dirty state and
    /// marking that it has recovered.
    pub fn clear_dirty(&self) {
        self.is_dirty.store(false, Ordering::SeqCst);
        self.generation.store(NO_CROSS_PROCESS_LOCK_GENERATION, Ordering::SeqCst);
    }

    /// Try to lock once, returns whether the lock was obtained or not.
    ///
    /// The lock can be obtained but it can be dirty. In all cases, the renew
    /// task will run in the background.
    #[instrument(skip(self), fields(?self.lock_key, ?self.lock_holder))]
    pub async fn try_lock_once(&self) -> Result<CrossProcessLockResult, CrossProcessLockError> {
        // Hold onto the locking attempt mutex for the entire lifetime of this
        // function, to avoid multiple reentrant calls.
        let mut _attempt = self.locking_attempt.lock().await;

        // If another thread obtained the lock, make sure to only superficially increase
        // the number of holders, and carry on.
        if self.num_holders.load(Ordering::SeqCst) > 0 {
            // Note: between the above load and the fetch_add below, another thread may
            // decrement `num_holders`. That's fine because that means the lock
            // was taken by at least one thread, and after this call it will be
            // taken by at least one thread.
            trace!("We already had the lock, incrementing holder count");

            self.num_holders.fetch_add(1, Ordering::SeqCst);

            return Ok(CrossProcessLockResult::Clean(CrossProcessLockGuard::new(
                self.num_holders.clone(),
            )));
        }

        if let Some(new_generation) = self
            .locker
            .try_lock(LEASE_DURATION_MS, &self.lock_key, &self.lock_holder)
            .await
            .map_err(|err| CrossProcessLockError::TryLockError(Box::new(err)))?
        {
            match self.generation.swap(new_generation, Ordering::SeqCst) {
                // If there was no lock generation, it means this is the first time the lock is
                // obtained. It cannot be dirty.
                NO_CROSS_PROCESS_LOCK_GENERATION => {
                    trace!(?new_generation, "Setting the lock generation for the first time");
                }

                // This was NOT the same generation, the lock has been dirtied!
                previous_generation if previous_generation != new_generation => {
                    warn!(
                        ?previous_generation,
                        ?new_generation,
                        "The lock has been obtained, but it's been dirtied!"
                    );
                    self.is_dirty.store(true, Ordering::SeqCst);
                }

                // This was the same generation, no problem.
                _ => {
                    trace!("Same lock generation; no problem");
                }
            }

            trace!("Lock obtained!");
        } else {
            trace!("Couldn't obtain the lock immediately.");
            return Ok(CrossProcessLockResult::Unobtained);
        }

        trace!("Obtained the lock, spawning the lease extension task.");

        // This is the first time we've obtaind the lock. We're going to spawn the task
        // that will renew the lease.

        // Clone data to be owned by the task.
        let this = (*self).clone();

        let mut renew_task = self.renew_task.lock().await;

        // Cancel the previous task, if any. That's safe to do, because:
        // - either the task was done,
        // - or it was still running, but taking a lock in the db has to be an atomic
        //   operation running in a transaction.

        if let Some(_prev) = renew_task.take() {
            #[cfg(not(target_family = "wasm"))]
            if !_prev.is_finished() {
                trace!("aborting the previous renew task");
                _prev.abort();
            }
        }

        // Restart a new one.
        *renew_task = Some(spawn(async move {
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
                    if this.num_holders.load(Ordering::SeqCst) == 0 {
                        trace!("exiting the lease extension loop");

                        // Cancel the lease with another 0ms lease.
                        // If we don't get the lock, that's (weird but) fine.
                        let fut = this.locker.try_lock(0, &this.lock_key, &this.lock_holder);
                        let _ = fut.await;

                        // Exit the loop.
                        break;
                    }
                }

                sleep(Duration::from_millis(EXTEND_LEASE_EVERY_MS)).await;

                match this
                    .locker
                    .try_lock(LEASE_DURATION_MS, &this.lock_key, &this.lock_holder)
                    .await
                {
                    Ok(Some(_generation)) => {
                        // It's impossible that the generation can be different
                        // from the previous generation.
                        //
                        // As long as the task runs, the lock is renewed, so the
                        // generation remains the same. If the lock is not
                        // taken, it's because the lease has expired, which is
                        // represented by the `Ok(None)` value, and the task
                        // must stop.
                    }

                    Ok(None) => {
                        error!("Failed to renew the lock lease: the lock could not be obtained");

                        // Exit the loop.
                        break;
                    }

                    Err(err) => {
                        error!("Error when extending the lock lease: {err:#}");

                        // Exit the loop.
                        break;
                    }
                }
            }
        }));

        self.num_holders.fetch_add(1, Ordering::SeqCst);

        let guard = CrossProcessLockGuard::new(self.num_holders.clone());

        Ok(if self.is_dirty() {
            CrossProcessLockResult::Dirty(guard)
        } else {
            CrossProcessLockResult::Clean(guard)
        })
    }

    /// Attempt to take the lock, with exponential backoff if the lock has
    /// already been taken before.
    ///
    /// The `max_backoff` parameter is the maximum time (in milliseconds) that
    /// should be waited for, between two attempts. When that time is
    /// reached a second time, the lock will stop attempting to get the lock
    /// and will return a timeout error upon locking. If not provided,
    /// will wait for [`MAX_BACKOFF_MS`].
    #[instrument(skip(self), fields(?self.lock_key, ?self.lock_holder))]
    pub async fn spin_lock(
        &self,
        max_backoff: Option<u32>,
    ) -> Result<CrossProcessLockGuard, CrossProcessLockError> {
        let max_backoff = max_backoff.unwrap_or(MAX_BACKOFF_MS);

        // Note: reads/writes to the backoff are racy across threads in theory, but the
        // lock in `try_lock_once` should sequentialize it all.

        loop {
            if let Some(guard) = self.try_lock_once().await?.ok() {
                // Reset backoff before returning, for the next attempt to lock.
                *self.backoff.lock().await = WaitingTime::Some(INITIAL_BACKOFF_MS);
                return Ok(guard);
            }

            // Exponential backoff! Multiply by 2 the time we've waited before, cap it to
            // max_backoff.
            let mut backoff = self.backoff.lock().await;

            let wait = match &mut *backoff {
                WaitingTime::Some(val) => {
                    let wait = *val;
                    *val = val.saturating_mul(2);
                    if *val >= max_backoff {
                        *backoff = WaitingTime::Stop;
                    }
                    wait
                }
                WaitingTime::Stop => {
                    // We've reached the maximum backoff, abandon.
                    return Err(CrossProcessLockError::LockTimeout);
                }
            };

            debug!("Waiting {wait} before re-attempting to take the lock");
            sleep(Duration::from_millis(wait.into())).await;
        }
    }

    /// Returns the value in the database that represents the holder's
    /// identifier.
    pub fn lock_holder(&self) -> &str {
        &self.lock_holder
    }
}

/// Represent the result of a locking attempt, either by
/// [`CrossProcessLock::try_lock_once`] or [`CrossProcessLock::spin_lock`].
#[derive(Debug)]
pub enum CrossProcessLockResult {
    /// The lock has been obtained successfully, all good.
    Clean(CrossProcessLockGuard),

    /// The lock has been obtained successfully, but the lock is dirty!
    ///
    /// This holder has obtained this cross-process lock once, then another
    /// holder has obtained this cross-process lock _before_ this holder
    /// obtained it again. The lock is marked as dirty. It means the value
    /// protected by the cross-process lock may need to be reloaded if
    /// synchronisation is important.
    Dirty(CrossProcessLockGuard),

    /// The lock has not been obtained.
    Unobtained,
}

impl CrossProcessLockResult {
    /// Convert from [`CrossProcessLockResult`] to
    /// [`Option<T>`] where `T` is [`CrossProcessLockGuard`].
    pub fn ok(self) -> Option<CrossProcessLockGuard> {
        match self {
            Self::Clean(guard) | Self::Dirty(guard) => Some(guard),
            Self::Unobtained => None,
        }
    }

    /// Return `true` if the lock has been obtained, `false` otherwise.
    pub fn is_ok(&self) -> bool {
        matches!(self, Self::Clean(_) | Self::Dirty(_))
    }
}

/// Error related to the locking API of the store.
#[derive(Debug, thiserror::Error)]
pub enum CrossProcessLockError {
    /// Spent too long waiting for a database lock.
    #[error("a lock timed out")]
    LockTimeout,

    #[error(transparent)]
    #[cfg(not(target_family = "wasm"))]
    TryLockError(#[from] Box<dyn Error + Send + Sync>),

    #[error(transparent)]
    #[cfg(target_family = "wasm")]
    TryLockError(Box<dyn Error>),
}

#[cfg(test)]
#[cfg(not(target_family = "wasm"))] // These tests require tokio::time, which is not implemented on wasm.
mod tests {
    use std::{
        collections::HashMap,
        ops::Not,
        sync::{Arc, RwLock, atomic},
    };

    use assert_matches::assert_matches;
    use matrix_sdk_test_macros::async_test;
    use tokio::{
        spawn,
        time::{Duration, sleep},
    };

    use super::{
        CrossProcessLock, CrossProcessLockError, CrossProcessLockGeneration,
        CrossProcessLockResult, EXTEND_LEASE_EVERY_MS, TryLock,
        memory_store_helper::{Lease, try_take_leased_lock},
    };

    #[derive(Clone, Default)]
    struct TestStore {
        leases: Arc<RwLock<HashMap<String, Lease>>>,
    }

    impl TestStore {
        fn try_take_leased_lock(
            &self,
            lease_duration_ms: u32,
            key: &str,
            holder: &str,
        ) -> Option<CrossProcessLockGeneration> {
            try_take_leased_lock(&mut self.leases.write().unwrap(), lease_duration_ms, key, holder)
        }
    }

    #[derive(Debug, thiserror::Error)]
    enum DummyError {}

    impl TryLock for TestStore {
        type LockError = DummyError;

        /// Try to take a lock using the given store.
        async fn try_lock(
            &self,
            lease_duration_ms: u32,
            key: &str,
            holder: &str,
        ) -> Result<Option<CrossProcessLockGeneration>, Self::LockError> {
            Ok(self.try_take_leased_lock(lease_duration_ms, key, holder))
        }
    }

    async fn release_lock(result: CrossProcessLockResult) {
        drop(result);
        sleep(Duration::from_millis(EXTEND_LEASE_EVERY_MS)).await;
    }

    type TestResult = Result<(), CrossProcessLockError>;

    #[async_test]
    async fn test_simple_lock_unlock() -> TestResult {
        let store = TestStore::default();
        let lock = CrossProcessLock::new(store, "key".to_owned(), "first".to_owned());

        // The lock plain works when used with a single holder.
        let result = lock.try_lock_once().await?;
        assert!(result.is_ok());
        assert_eq!(lock.num_holders.load(atomic::Ordering::SeqCst), 1);

        // Releasing works.
        release_lock(result).await;
        assert_eq!(lock.num_holders.load(atomic::Ordering::SeqCst), 0);

        // Spin locking on the same lock always works, assuming no concurrent access.
        let guard = lock.spin_lock(None).await.unwrap();

        // Releasing still works.
        release_lock(CrossProcessLockResult::Clean(guard)).await;
        assert_eq!(lock.num_holders.load(atomic::Ordering::SeqCst), 0);

        Ok(())
    }

    #[async_test]
    async fn test_self_recovery() -> TestResult {
        let store = TestStore::default();
        let lock = CrossProcessLock::new(store.clone(), "key".to_owned(), "first".to_owned());

        // When a lock is obtained...
        let result = lock.try_lock_once().await?;
        assert!(result.is_ok());
        assert_eq!(lock.num_holders.load(atomic::Ordering::SeqCst), 1);

        // But then forgotten... (note: no need to release the guard)
        drop(lock);

        // And when rematerializing the lock with the same key/value...
        let lock = CrossProcessLock::new(store.clone(), "key".to_owned(), "first".to_owned());

        // We still got it.
        let result = lock.try_lock_once().await?;
        assert!(result.is_ok());
        assert_eq!(lock.num_holders.load(atomic::Ordering::SeqCst), 1);

        Ok(())
    }

    #[async_test]
    async fn test_multiple_holders_same_process() -> TestResult {
        let store = TestStore::default();
        let lock = CrossProcessLock::new(store, "key".to_owned(), "first".to_owned());

        // Taking the lock twice...
        let result1 = lock.try_lock_once().await?;
        assert!(result1.is_ok());

        let result2 = lock.try_lock_once().await?;
        assert!(result2.is_ok());

        assert_eq!(lock.num_holders.load(atomic::Ordering::SeqCst), 2);

        // ...means we can release it twice.
        release_lock(result1).await;
        assert_eq!(lock.num_holders.load(atomic::Ordering::SeqCst), 1);

        release_lock(result2).await;
        assert_eq!(lock.num_holders.load(atomic::Ordering::SeqCst), 0);

        Ok(())
    }

    #[async_test]
    async fn test_multiple_processes() -> TestResult {
        let store = TestStore::default();
        let lock1 = CrossProcessLock::new(store.clone(), "key".to_owned(), "first".to_owned());
        let lock2 = CrossProcessLock::new(store, "key".to_owned(), "second".to_owned());

        // When the first process takes the lock...
        let result1 = lock1.try_lock_once().await?;
        assert!(result1.is_ok());

        // The second can't take it immediately.
        let result2 = lock2.try_lock_once().await?;
        assert!(result2.is_ok().not());

        let lock2_clone = lock2.clone();
        let handle = spawn(async move { lock2_clone.spin_lock(Some(1000)).await });

        sleep(Duration::from_millis(100)).await;

        drop(result1);

        // lock2 in the background manages to get the lock at some point.
        let _result2 = handle
            .await
            .expect("join handle is properly awaited")
            .expect("lock was obtained after spin-locking");

        // Now if lock1 tries to get the lock with a small timeout, it will fail.
        assert_matches!(lock1.spin_lock(Some(200)).await, Err(CrossProcessLockError::LockTimeout));

        Ok(())
    }
}

/// Some code that is shared by almost all `MemoryStore` implementations out
/// there.
pub mod memory_store_helper {
    use std::collections::{HashMap, hash_map::Entry};

    use ruma::time::{Duration, Instant};

    use super::{CrossProcessLockGeneration, FIRST_CROSS_PROCESS_LOCK_GENERATION};

    #[derive(Debug)]
    pub struct Lease {
        holder: String,
        expiration: Instant,
        generation: CrossProcessLockGeneration,
    }

    pub fn try_take_leased_lock(
        leases: &mut HashMap<String, Lease>,
        lease_duration_ms: u32,
        key: &str,
        holder: &str,
    ) -> Option<CrossProcessLockGeneration> {
        let now = Instant::now();
        let expiration = now + Duration::from_millis(lease_duration_ms.into());

        match leases.entry(key.to_owned()) {
            // There is an existing holder.
            Entry::Occupied(mut entry) => {
                let Lease {
                    holder: current_holder,
                    expiration: current_expiration,
                    generation: current_generation,
                } = entry.get_mut();

                if current_holder == holder {
                    // We had the lease before, extend it.
                    *current_expiration = expiration;

                    Some(*current_generation)
                } else {
                    // We didn't have it.
                    if *current_expiration < now {
                        // Steal it!
                        *current_holder = holder.to_owned();
                        *current_expiration = expiration;
                        *current_generation += 1;

                        Some(*current_generation)
                    } else {
                        // We tried our best.
                        None
                    }
                }
            }

            // There is no holder, easy.
            Entry::Vacant(entry) => {
                entry.insert(Lease {
                    holder: holder.to_owned(),
                    expiration: Instant::now() + Duration::from_millis(lease_duration_ms.into()),
                    generation: FIRST_CROSS_PROCESS_LOCK_GENERATION,
                });

                Some(FIRST_CROSS_PROCESS_LOCK_GENERATION)
            }
        }
    }
}
