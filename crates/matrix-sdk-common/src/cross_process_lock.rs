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
//! The lock is initially obtained for a certain period of time (namely, the
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
        Arc, Weak,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};

use tokio::sync::Mutex;
use tracing::{debug, error, instrument, trace, warn};

use crate::{
    SendOutsideWasm, SyncOutsideWasm,
    executor::{AbortOnDrop, JoinHandleExt, spawn},
    sleep::sleep,
};

/// A lock generation is an integer incremented each time the lock is taken by
/// a different holder.
///
/// This is used to know if a lock has been dirtied.
pub type CrossProcessLockGeneration = u64;

/// A trait that represents any function which can be used to
/// acquire the underlying lock of a [`CrossProcessLock`].
///
/// For example, this can be useful when writing a function which
/// is parameterized to acquire the underlying lock through either
/// [`CrossProcessLock::spin_lock`] or [`CrossProcessLock::try_lock_once`].
pub trait AcquireCrossProcessLockFn<L>
where
    Self: AsyncFn(&CrossProcessLock<L>) -> AcquireCrossProcessLockResult<L::LockError>,
    L: TryLock + Clone + SendOutsideWasm + 'static,
{
}

impl<L, T> AcquireCrossProcessLockFn<L> for T
where
    T: AsyncFn(&CrossProcessLock<L>) -> AcquireCrossProcessLockResult<L::LockError>,
    L: TryLock + Clone + SendOutsideWasm + 'static,
{
}

/// A convenience type for the [`Result`] returned from calling
/// or [`CrossProcessLock::try_lock_once`] or [`CrossProcessLock::spin_lock`].
pub type AcquireCrossProcessLockResult<E> =
    Result<Result<CrossProcessLockState, CrossProcessLockUnobtained>, E>;

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
#[derive(Clone, Debug)]
#[must_use = "If unused, the `CrossProcessLock` will unlock at the end of the lease"]
pub struct CrossProcessLockGuard {
    /// A clone of [`CrossProcessLock::inner`].
    ///
    /// The number of guards/holders is based on the `Weak::weak_count`.
    ///
    /// - Every time [`CrossProcessLockGuard`] is cloned, `Weak` is cloned, and
    ///   thus the number of holders of `CrossProcessLockInner` increases.
    /// - Every time [`CrossProcessLockGuard`] is dropped, `Weak` is dropped,
    ///   and thus the number of holders of `CrossProcessLockInner` decreases.
    inner: Weak<CrossProcessLockInner>,
}

impl CrossProcessLockGuard {
    fn new(inner: &Arc<CrossProcessLockInner>) -> Self {
        // Downgrading the strong pointer to a weak pointer to represent a new lock
        // holder.
        Self { inner: Arc::downgrade(inner) }
    }

    /// Determine whether the cross-process lock associated to this guard is
    /// dirty.
    ///
    /// See [`CrossProcessLockState::Dirty`] to learn more about the semantics
    /// of _dirty_.
    pub fn is_dirty(&self) -> bool {
        self.inner
            .upgrade()
            .map(|inner| inner.is_dirty())
            // If it's not possible to upgrade the weak pointer, it means the lock _and_ the
            // `renew_task` have been dropped. In this case, whether the lock is dirty or
            // not doesn't make any difference.
            .unwrap_or(false)
    }

    /// Clear the dirty state from the cross-process lock associated to this
    /// guard.
    ///
    /// If the cross-process lock is dirtied, it will remain dirtied until
    /// this method is called. This allows recovering from a dirty state and
    /// marking that it has recovered.
    pub fn clear_dirty(&self) {
        // If it's not possible to upgrade the weak pointer, it means the lock _and_ the
        // `renew_task` have been dropped. Marking the lock as non-dirty makes no
        // particular sense, so we do nothing.
        if let Some(inner) = self.inner.upgrade() {
            inner.clear_dirty();
        }
    }

    #[cfg(test)]
    fn count_holders(inner: &Weak<CrossProcessLockInner>) -> usize {
        Weak::weak_count(inner)
    }
}

/// A cross-process lock implementation.
///
/// See the doc-comment of this module for more information.
#[derive(Clone, Debug)]
pub struct CrossProcessLock<L> {
    /// The locker implementation.
    ///
    /// `L` is responsible for trying to take the lock, while
    /// [`CrossProcessLock`] is responsible to make it cross-process, with the
    /// retry mechanism, plus guard and so on.
    locker: Arc<L>,

    /// The inner data of the lock, shared with all the lock holders.
    ///
    /// The number of lock holders must be computed with
    /// [`CrossProcessLock::count_holders`].
    ///
    /// If the number of lock holders is greater than 0, this means we've
    /// already obtained this lock, in this process, and the store lock
    /// mustn't be touched.
    ///
    /// When the number of holders is decreased to 0, then the lock must be
    /// released in the store.
    //
    // Notes about the `Arc`/`Weak` usage:
    //
    // - We want to track the number of holders, i.e. the number of guards. To achieve that, we
    //   could use a thread-safe counter, or hijack `Arc` and `Weak` which provide two thread-safe
    //   counters: strong count and weak count.
    // - `CrossProcessLock` holds an `Arc` (this field).
    // - `renew_task` holds an `Arc` (a clone of this field).
    // - `CrossProcessLockGuard` holds a `Weak` (it could use an `Arc`, but a `Weak` is fine in
    //   this context and offers a unique counter for guards!).
    // - Counting holders = counting the number of `Weak` pointers.
    // - It is safe to upgrade the `Weak` pointer to an `Arc` (to get information about dirtiness)
    //   in a guard because the `renew_task` holds a clone of the `Arc` and will not exit until all
    //   guards have been dropped.
    // - It is always possible to create a `Weak` pointer (i) either from `CrossProcessLock` by
    //   using `Arc::downgrade`, (ii) or from `CrossProcessLockGuard` by cloning it.
    inner: Arc<CrossProcessLockInner>,

    /// The key used in the key/value mapping for the lock entry.
    lock_key: String,

    /// A mutex to control an attempt to take the lock, to prevent someone using
    /// it in a re-entrant way.
    locking_attempt: Arc<Mutex<()>>,

    /// Backoff time, in milliseconds.
    backoff: Arc<Mutex<WaitingTime>>,

    /// The cross-process lock configuration.
    config: CrossProcessLockConfig,
}

/// Inner data for [`CrossProcessLock`] and [`CrossProcessLockGuard`].
#[derive(Debug)]
struct CrossProcessLockInner {
    /// Current renew task spawned by [`CrossProcessLock::try_lock_once`].
    ///
    /// It is not used directly by [`CrossProcessLockGuard`]. It is stored here
    /// to ensure the task will drop once the lock and all the guards drop.
    renew_task: Mutex<Option<AbortOnDrop<()>>>,

    /// This lock generation.
    generation: AtomicU64,

    /// Whether the lock has been dirtied.
    ///
    /// See [`CrossProcessLockState::Dirty`] to learn more about the semantics
    /// of _dirty_.
    is_dirty: AtomicBool,
}

impl CrossProcessLockInner {
    /// Determine whether the cross-process lock is dirty.
    ///
    /// See [`CrossProcessLockState::Dirty`] to learn more about the semantics
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
    }
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
    L: TryLock + Clone + SendOutsideWasm + SyncOutsideWasm + 'static,
{
    /// Create a new cross-process lock.
    ///
    /// # Parameters
    ///
    /// - `lock_key`: key in the key-value store to store the lock's state.
    /// - `config`: the cross-process lock configuration to use, if it's
    ///   [`CrossProcessLockConfig::SingleProcess`], no actual lock will be
    ///   taken.
    pub fn new(locker: L, lock_key: String, config: CrossProcessLockConfig) -> Self {
        Self {
            locker: Arc::new(locker),
            lock_key,
            locking_attempt: Arc::new(Mutex::new(())),
            inner: Arc::new(CrossProcessLockInner {
                renew_task: Default::default(),
                generation: AtomicU64::new(NO_CROSS_PROCESS_LOCK_GENERATION),

                is_dirty: AtomicBool::new(false),
            }),
            backoff: Arc::new(Mutex::new(WaitingTime::Some(INITIAL_BACKOFF_MS))),
            config,
        }
    }

    /// Count the number of holders.
    ///
    /// # Safety
    ///
    /// This method by itself is safe, but using it correctly requires extra
    /// care. Another thread can change the weak count at any time, including
    /// potentially between calling this method and acting on the result.
    fn count_holders(inner: &Arc<CrossProcessLockInner>) -> usize {
        Arc::weak_count(inner)
    }

    /// Determine whether the cross-process lock is dirty.
    ///
    /// See [`CrossProcessLockState::Dirty`] to learn more about the semantics
    /// of _dirty_.
    pub fn is_dirty(&self) -> bool {
        self.inner.is_dirty()
    }

    /// Clear the dirty state from this cross-process lock.
    ///
    /// If the cross-process lock is dirtied, it will remain dirtied until
    /// this method is called. This allows recovering from a dirty state and
    /// marking that it has recovered.
    pub fn clear_dirty(&self) {
        self.inner.clear_dirty();
    }

    /// Try to lock once, returns whether the lock was obtained or not.
    ///
    /// The lock can be obtained but it can be dirty. In all cases, the renew
    /// task will run in the background.
    #[instrument(skip(self), fields(?self.lock_key, ?self.config, ?self.inner.generation))]
    pub async fn try_lock_once(&self) -> AcquireCrossProcessLockResult<L::LockError> {
        // If it's not `MultiProcess`, this behaves as a no-op
        let CrossProcessLockConfig::MultiProcess { holder_name } = &self.config else {
            let guard = CrossProcessLockGuard::new(&self.inner);
            return Ok(Ok(CrossProcessLockState::Clean(guard)));
        };

        // Hold onto the locking attempt mutex for the entire lifetime of this
        // function, to avoid multiple reentrant calls.
        let mut _attempt = self.locking_attempt.lock().await;

        // If there is at least one other holder, it means the lock has already been
        // acquired, and we can safely generate a new guard.
        if Self::count_holders(&self.inner) > 0 {
            // Note: between the above “count” and the `CrossProcessLockGuard::new` below,
            // another thread may decrement the number of holders. That's fine because that
            // means the lock was taken by at least one thread, and after this
            // call it will be taken by at least one thread.
            //
            // Because `locking_attempt` is acquired, the task cannot drop the lock while
            // the “count” might change.
            trace!("We already had the lock, incrementing holder count");

            return Ok(Ok(CrossProcessLockState::Clean(CrossProcessLockGuard::new(&self.inner))));
        }

        if let Some(new_generation) =
            self.locker.try_lock(LEASE_DURATION_MS, &self.lock_key, holder_name).await?
        {
            match self.inner.generation.swap(new_generation, Ordering::SeqCst) {
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
                    self.inner.is_dirty.store(true, Ordering::SeqCst);
                }

                // This was the same generation, no problem.
                _ => {
                    trace!("Same lock generation; no problem");
                }
            }

            trace!("Lock obtained!");
        } else {
            trace!("Couldn't obtain the lock immediately.");
            return Ok(Err(CrossProcessLockUnobtained::Busy));
        }

        trace!("Obtained the lock, spawning the lease extension task.");

        // No lock was acquired before (either because it's the first time the lock is
        // acquired, or because all previous guards have been dropped). We're going to
        // spawn the task that will renew the lease.

        let mut renew_task = self.inner.renew_task.lock().await;

        // Cancel the previous task, if any. That's safe to do, because:
        // - either the task was done,
        // - or it was still running, but taking a lock in the database has to be an
        //   atomic operation running in a transaction.
        drop(renew_task.take());

        // Restart a new one.
        *renew_task = Some(
            spawn({
                let locker = self.locker.clone();
                let lock_key = self.lock_key.clone();
                let locking_attempt = self.locking_attempt.clone();
                let config = self.config.clone();

                // By cloning `CrossProcessLockInner`, we ensure the task acts as a lock holder.
                let inner = self.inner.clone();

                async move {
                    let CrossProcessLockConfig::MultiProcess { holder_name } = config else {
                        return;
                    };

                    loop {
                        {
                            // First, check if there are still users of this lock.
                            //
                            // This is not racy, because:
                            // - the `locking_attempt` mutex makes sure we don't have unexpected
                            //   interactions with the non-atomic sequence above in `try_lock_once`,
                            // - other holders will only decrease over time.

                            let _guard = locking_attempt.lock().await;

                            // There are no more holders. We can quit.
                            if Self::count_holders(&inner) == 0 {
                                trace!("exiting the lease extension loop");

                                // Cancel the lease with another 0ms lease.
                                // If we don't get the lock, that's (weird but) fine.
                                let fut = locker.try_lock(0, &lock_key, &holder_name);
                                let _ = fut.await;

                                // Exit the loop.
                                break;
                            }
                        }

                        sleep(Duration::from_millis(EXTEND_LEASE_EVERY_MS)).await;

                        match locker.try_lock(LEASE_DURATION_MS, &lock_key, &holder_name).await {
                            Ok(Some(_generation)) => {
                                // It's impossible that the generation can be
                                // different from the previous generation.
                                //
                                // As long as the task runs, the lock is
                                // renewed, so the generation remains the same.
                                // If the lock is not taken, it's because the
                                // lease has expired, which is represented by
                                // the `Ok(None)` value, and the task must stop.
                            }

                            Ok(None) => {
                                error!(
                                    "Failed to renew the lock lease: the lock could not be obtained"
                                );

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
                }
            })
            .abort_on_drop(),
        );

        let guard = CrossProcessLockGuard::new(&self.inner);

        Ok(Ok(if self.is_dirty() {
            CrossProcessLockState::Dirty(guard)
        } else {
            CrossProcessLockState::Clean(guard)
        }))
    }

    /// Attempt to take the lock, with exponential backoff if the lock has
    /// already been taken before.
    ///
    /// The `max_backoff` parameter is the maximum time (in milliseconds) that
    /// should be waited for, between two attempts. When that time is
    /// reached a second time, the lock will stop attempting to get the lock
    /// and will return a timeout error upon locking. If not provided,
    /// will wait for [`MAX_BACKOFF_MS`].
    #[instrument(skip(self), fields(?self.lock_key, ?self.config))]
    pub async fn spin_lock(
        &self,
        max_backoff: Option<u32>,
    ) -> AcquireCrossProcessLockResult<L::LockError> {
        // If there is no holder, this behaves as a no-op
        let max_backoff = max_backoff.unwrap_or(MAX_BACKOFF_MS);

        // Note: reads/writes to the backoff are racy across threads in theory, but the
        // lock in `try_lock_once` should sequentialize it all.

        loop {
            // If the cross-process lock config is not `MultiProcess`, this behaves as a
            // no-op and we just return
            let lock_result = self.try_lock_once().await?;

            if lock_result.is_ok() {
                if matches!(self.config, CrossProcessLockConfig::MultiProcess { .. }) {
                    // Reset backoff before returning, for the next attempt to lock.
                    *self.backoff.lock().await = WaitingTime::Some(INITIAL_BACKOFF_MS);
                }

                return Ok(lock_result);
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
                    return Ok(Err(CrossProcessLockUnobtained::TimedOut));
                }
            };

            debug!("Waiting {wait} before re-attempting to take the lock");
            sleep(Duration::from_millis(wait.into())).await;
        }
    }

    /// Returns the value in the database that represents the holder's
    /// identifier.
    pub fn lock_holder(&self) -> Option<&str> {
        self.config.holder_name()
    }
}

/// Represent a successful result of a locking attempt, either by
/// [`CrossProcessLock::try_lock_once`] or [`CrossProcessLock::spin_lock`].
#[derive(Debug)]
#[must_use = "If unused, the `CrossProcessLock` will unlock at the end of the lease"]
pub enum CrossProcessLockState {
    /// The lock has been obtained successfully, all good.
    Clean(CrossProcessLockGuard),

    /// The lock has been obtained successfully, but the lock is dirty!
    ///
    /// This holder has obtained this cross-process lock once, then another
    /// holder has obtained this cross-process lock _before_ this holder
    /// obtained it again. The lock is marked as dirty. It means the value
    /// protected by the cross-process lock may need to be reloaded if
    /// synchronisation is important.
    ///
    /// Until [`CrossProcessLock::clear_dirty`] is called,
    /// [`CrossProcessLock::is_dirty`], [`CrossProcessLock::try_lock_once`] and
    /// [`CrossProcessLock::spin_lock`] will report the lock as dirty. Put it
    /// differently: dirty once, dirty forever, unless
    /// [`CrossProcessLock::clear_dirty`] is called.
    Dirty(CrossProcessLockGuard),
}

impl CrossProcessLockState {
    /// Map this value into the inner [`CrossProcessLockGuard`].
    pub fn into_guard(self) -> CrossProcessLockGuard {
        match self {
            Self::Clean(guard) | Self::Dirty(guard) => guard,
        }
    }

    /// Map this [`CrossProcessLockState`] into a
    /// [`MappedCrossProcessLockState`].
    ///
    /// This is helpful when one wants to create its own wrapper over
    /// [`CrossProcessLockGuard`].
    pub fn map<F, G>(self, mapper: F) -> MappedCrossProcessLockState<G>
    where
        F: FnOnce(CrossProcessLockGuard) -> G,
    {
        match self {
            Self::Clean(guard) => MappedCrossProcessLockState::Clean(mapper(guard)),
            Self::Dirty(guard) => MappedCrossProcessLockState::Dirty(mapper(guard)),
        }
    }
}

/// A mapped [`CrossProcessLockState`].
///
/// Created by [`CrossProcessLockState::map`].
#[derive(Debug)]
#[must_use = "If unused, the `CrossProcessLock` will unlock at the end of the lease"]
pub enum MappedCrossProcessLockState<G> {
    /// The equivalent of [`CrossProcessLockState::Clean`].
    Clean(G),

    /// The equivalent of [`CrossProcessLockState::Dirty`].
    Dirty(G),
}

impl<G> MappedCrossProcessLockState<G> {
    /// Return `Some(G)` if `Self` is [`Clean`][Self::Clean].
    pub fn as_clean(&self) -> Option<&G> {
        match self {
            Self::Clean(guard) => Some(guard),
            Self::Dirty(_) => None,
        }
    }
}

/// Represent an unsuccessful result of a lock attempt, either by
/// [`CrossProcessLock::try_lock_once`] or [`CrossProcessLock::spin_lock`].
#[derive(Clone, Debug, thiserror::Error)]
pub enum CrossProcessLockUnobtained {
    /// The lock couldn't be obtained immediately because it is busy, i.e. it is
    /// held by another holder.
    #[error(
        "The lock couldn't be obtained immediately because it is busy, i.e. it is held by another holder"
    )]
    Busy,

    /// The lock couldn't be obtained after several attempts: locking has timed
    /// out.
    #[error("The lock couldn't be obtained after several attempts: locking has timed out")]
    TimedOut,
}

/// Union of [`CrossProcessLockUnobtained`] and [`TryLock::LockError`].
#[derive(Clone, Debug, thiserror::Error)]
pub enum CrossProcessLockError {
    #[error(transparent)]
    Unobtained(#[from] CrossProcessLockUnobtained),

    #[error(transparent)]
    #[cfg(not(target_family = "wasm"))]
    TryLock(#[from] Arc<dyn Error + Send + Sync>),

    #[error(transparent)]
    #[cfg(target_family = "wasm")]
    TryLock(#[from] Arc<dyn Error>),
}

/// The cross-process lock config to use for the various stores.
#[derive(Clone, Debug)]
pub enum CrossProcessLockConfig {
    /// The stores will be used in multiple processes, the holder name for the
    /// cross-process lock is the associated `String`.
    MultiProcess {
        /// The name of the holder of the cross-process lock.
        holder_name: String,
    },
    /// The stores will be used in a single process, there is no need for a
    /// cross-process lock.
    SingleProcess,
}

impl CrossProcessLockConfig {
    /// Helper for quickly creating a [`CrossProcessLockConfig::MultiProcess`]
    /// variant.
    pub fn multi_process(holder_name: impl Into<String>) -> Self {
        Self::MultiProcess { holder_name: holder_name.into() }
    }

    /// The holder name for the cross-process lock. This is only relevant for
    /// [`CrossProcessLockConfig::MultiProcess`] variants.
    pub fn holder_name(&self) -> Option<&str> {
        match self {
            Self::MultiProcess { holder_name } => Some(holder_name),
            Self::SingleProcess => None,
        }
    }
}

#[cfg(test)]
#[cfg(not(target_family = "wasm"))] // These tests require tokio::time, which is not implemented on wasm.
mod tests {
    use std::{
        collections::HashMap,
        ops::Not,
        sync::{Arc, RwLock},
        time::Duration,
    };

    use assert_matches::assert_matches;
    use assert_matches2::assert_let;
    use matrix_sdk_test_macros::async_test;
    use tokio::{spawn, task::yield_now, time::sleep};

    use super::{
        CrossProcessLockConfig, CrossProcessLockError, CrossProcessLockGeneration,
        CrossProcessLockGuard, CrossProcessLockState, CrossProcessLockUnobtained, TryLock,
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

    impl From<DummyError> for CrossProcessLockError {
        fn from(value: DummyError) -> Self {
            Self::TryLock(Arc::new(value))
        }
    }

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

    async fn release_lock(lock: CrossProcessLockGuard) {
        drop(lock);
        yield_now().await;
    }

    type TestResult = Result<(), CrossProcessLockError>;
    type CrossProcessLock = super::CrossProcessLock<TestStore>;

    #[async_test]
    async fn test_simple_lock_unlock() -> TestResult {
        let store = TestStore::default();
        let lock = CrossProcessLock::new(
            store,
            "key".to_owned(),
            CrossProcessLockConfig::multi_process("first"),
        );

        // The lock plain works when used with a single holder.
        let guard = lock.try_lock_once().await?.expect("lock must be obtained successfully");
        assert_let!(CrossProcessLockState::Clean(guard) = guard);
        assert!(lock.is_dirty().not());

        assert_eq!(CrossProcessLock::count_holders(&lock.inner), 1);
        assert_eq!(CrossProcessLockGuard::count_holders(&guard.inner), 1);

        let guard_clone = guard.clone();

        assert_eq!(CrossProcessLock::count_holders(&lock.inner), 2);
        assert_eq!(CrossProcessLockGuard::count_holders(&guard.inner), 2);
        assert_eq!(CrossProcessLockGuard::count_holders(&guard_clone.inner), 2);

        // Dropping a guard decreases the number of holders.
        drop(guard_clone);
        assert_eq!(CrossProcessLock::count_holders(&lock.inner), 1);
        assert_eq!(CrossProcessLockGuard::count_holders(&guard.inner), 1);

        // Releasing works.
        release_lock(guard).await;
        assert!(lock.is_dirty().not());

        assert_eq!(CrossProcessLock::count_holders(&lock.inner), 0);

        // Spin locking on the same lock always works, assuming no concurrent access.
        let guard = lock.spin_lock(None).await?.expect("spin lock must be obtained successfully");
        assert_let!(CrossProcessLockState::Clean(guard) = guard);
        assert!(lock.is_dirty().not());

        assert_eq!(CrossProcessLock::count_holders(&lock.inner), 1);

        // Releasing still works.
        release_lock(guard).await;
        assert!(lock.is_dirty().not());

        assert_eq!(CrossProcessLock::count_holders(&lock.inner), 0);

        Ok(())
    }

    #[async_test]
    async fn test_self_recovery() -> TestResult {
        let store = TestStore::default();
        let lock = CrossProcessLock::new(
            store.clone(),
            "key".to_owned(),
            CrossProcessLockConfig::multi_process("first"),
        );

        // When a lock is obtained…
        let guard = lock.try_lock_once().await?.expect("lock must be obtained successfully");
        assert_let!(CrossProcessLockState::Clean(guard) = guard);
        assert!(lock.is_dirty().not());
        assert_eq!(CrossProcessLock::count_holders(&lock.inner), 1);
        assert_eq!(CrossProcessLockGuard::count_holders(&guard.inner), 1);

        // But then forgotten…
        drop(lock);

        // Let's ensure the guard keeps acting as a lock holder even if the lock has
        // dropped.
        assert_eq!(CrossProcessLockGuard::count_holders(&guard.inner), 1);

        // Okay, enough fun, time to drop it.
        release_lock(guard).await;

        // And when rematerializing the lock with the same key/value…
        let lock = CrossProcessLock::new(
            store.clone(),
            "key".to_owned(),
            CrossProcessLockConfig::multi_process("first"),
        );

        // We still got it.
        let guard =
            lock.try_lock_once().await?.expect("lock (again) must be obtained successfully");
        assert_let!(CrossProcessLockState::Clean(guard) = guard);
        assert!(lock.is_dirty().not());
        assert_eq!(CrossProcessLock::count_holders(&lock.inner), 1);
        assert_eq!(CrossProcessLockGuard::count_holders(&guard.inner), 1);

        Ok(())
    }

    #[async_test]
    async fn test_multiple_holders_same_process() -> TestResult {
        let store = TestStore::default();
        let lock = CrossProcessLock::new(
            store,
            "key".to_owned(),
            CrossProcessLockConfig::multi_process("first"),
        );

        // Taking the lock twice…
        let guard1 = lock.try_lock_once().await?.expect("lock must be obtained successfully");
        assert_let!(CrossProcessLockState::Clean(guard1) = guard1);

        let guard2 = lock.try_lock_once().await?.expect("lock must be obtained successfully");
        assert_let!(CrossProcessLockState::Clean(guard2) = guard2);

        assert!(lock.is_dirty().not());
        assert_eq!(CrossProcessLock::count_holders(&lock.inner), 2);
        assert_eq!(CrossProcessLockGuard::count_holders(&guard1.inner), 2);
        assert_eq!(CrossProcessLockGuard::count_holders(&guard2.inner), 2);

        // … means we can release it twice.
        release_lock(guard1).await;
        assert_eq!(CrossProcessLock::count_holders(&lock.inner), 1);
        assert_eq!(CrossProcessLockGuard::count_holders(&guard2.inner), 1);

        release_lock(guard2).await;
        assert_eq!(CrossProcessLock::count_holders(&lock.inner), 0);

        assert!(lock.is_dirty().not());

        Ok(())
    }

    #[async_test]
    async fn test_multiple_processes() -> TestResult {
        let store = TestStore::default();
        let lock1 = CrossProcessLock::new(
            store.clone(),
            "key".to_owned(),
            CrossProcessLockConfig::multi_process("first"),
        );
        let lock2 = CrossProcessLock::new(
            store,
            "key".to_owned(),
            CrossProcessLockConfig::multi_process("second"),
        );

        // `lock1` acquires the lock.
        let guard1 = lock1.try_lock_once().await?.expect("lock must be obtained successfully");
        assert_let!(CrossProcessLockState::Clean(guard1) = guard1);
        assert!(lock1.is_dirty().not());
        assert_eq!(CrossProcessLock::count_holders(&lock1.inner), 1);
        assert_eq!(CrossProcessLockGuard::count_holders(&guard1.inner), 1);

        // `lock2` cannot acquire the lock.
        let err = lock2.try_lock_once().await?.expect_err("lock must NOT be obtained");
        assert_matches!(err, CrossProcessLockUnobtained::Busy);

        // `lock2` is waiting in a task.
        let lock2_clone = lock2.clone();
        let task = spawn(async move { lock2_clone.spin_lock(Some(500)).await });

        yield_now().await;

        release_lock(guard1).await;
        sleep(Duration::from_millis(super::EXTEND_LEASE_EVERY_MS * 2)).await;
        assert_eq!(CrossProcessLock::count_holders(&lock1.inner), 0);

        // Once `lock1` is released, `lock2` managed to obtain it.
        let guard2 = task
            .await
            .expect("join handle is properly awaited")
            .expect("lock is successfully attempted")
            .expect("lock must be obtained successfully");
        assert_let!(CrossProcessLockState::Clean(guard2) = guard2);

        assert_eq!(CrossProcessLock::count_holders(&lock1.inner), 0);
        assert_eq!(CrossProcessLock::count_holders(&lock2.inner), 1);
        assert_eq!(CrossProcessLockGuard::count_holders(&guard2.inner), 1);

        // `lock1` and `lock2` are both clean!
        assert!(lock1.is_dirty().not());
        assert!(lock2.is_dirty().not());

        // Now if `lock1` tries to obtain the lock with a small timeout, it will fail.
        assert_matches!(
            lock1.spin_lock(Some(200)).await,
            Ok(Err(CrossProcessLockUnobtained::TimedOut))
        );

        Ok(())
    }

    #[async_test]
    async fn test_multiple_processes_up_to_dirty() -> TestResult {
        let store = TestStore::default();
        let lock1 = CrossProcessLock::new(
            store.clone(),
            "key".to_owned(),
            CrossProcessLockConfig::multi_process("first"),
        );
        let lock2 = CrossProcessLock::new(
            store,
            "key".to_owned(),
            CrossProcessLockConfig::multi_process("second"),
        );

        // Obtain `lock1` once.
        {
            let guard = lock1.try_lock_once().await?.expect("lock must be obtained successfully");
            assert_matches!(guard, CrossProcessLockState::Clean(_));
            assert!(lock1.is_dirty().not());
            drop(guard);

            yield_now().await;
        }

        // Obtain `lock2` once.
        {
            let guard = lock2.try_lock_once().await?.expect("lock must be obtained successfully");
            assert_matches!(guard, CrossProcessLockState::Clean(_));
            assert!(lock1.is_dirty().not());
            drop(guard);

            yield_now().await;
        }

        for _ in 0..3 {
            // Obtain `lock1` once more. Now it's dirty because `lock2` has acquired the
            // lock meanwhile.
            {
                let guard =
                    lock1.try_lock_once().await?.expect("lock must be obtained successfully");
                assert_matches!(guard, CrossProcessLockState::Dirty(_));
                assert!(lock1.is_dirty());

                drop(guard);
                yield_now().await;
            }

            // Obtain `lock1` once more! It still dirty because it has not been marked as
            // non-dirty.
            {
                let guard =
                    lock1.try_lock_once().await?.expect("lock must be obtained successfully");
                assert_matches!(guard, CrossProcessLockState::Dirty(_));
                assert!(lock1.is_dirty());
                lock1.clear_dirty();

                drop(guard);
                yield_now().await;
            }

            // Obtain `lock1` once more. Now it's clear!
            {
                let guard =
                    lock1.try_lock_once().await?.expect("lock must be obtained successfully");
                assert_matches!(guard, CrossProcessLockState::Clean(_));
                assert!(lock1.is_dirty().not());

                drop(guard);
                yield_now().await;
            }

            // Same dance with `lock2`!
            {
                let guard =
                    lock2.try_lock_once().await?.expect("lock must be obtained successfully");
                assert_matches!(guard, CrossProcessLockState::Dirty(_));
                assert!(lock2.is_dirty());
                lock2.clear_dirty();

                drop(guard);
                yield_now().await;
            }
        }

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
