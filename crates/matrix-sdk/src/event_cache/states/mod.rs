// Copyright 2026 The Matrix.org Foundation C.I.C.
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

//! This module handles the state of the [`EventCache`].

use std::{
    collections::{HashMap, HashSet},
    fmt,
    future::Future,
    ops::{Deref, DerefMut},
    sync::{
        Arc, Mutex as StdMutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use matrix_sdk_base::{
    event_cache::store::{EventCacheStoreLock, EventCacheStoreLockGuard, EventCacheStoreLockState},
    timer,
    tracing_timer::TracingTimer,
};
use matrix_sdk_common::timeout::timeout;
use ruma::{OwnedEventId, OwnedRoomId, RoomId, time::Instant};
use tokio::sync::{Mutex, RwLock, RwLockMappedWriteGuard, RwLockReadGuard, RwLockWriteGuard};
use tracing::{error, info, instrument, trace, warn};

use super::{
    CachesByRoom, EventCacheError, EventsOrigin, Result,
    caches::{
        TimelineVectorDiffs,
        event_focused::{EventFocusedCacheKey, EventFocusedCacheState},
        pinned_events::PinnedEventsCacheState,
        room::{self, RoomEventCacheState},
        thread::ThreadEventCacheState,
    },
};

pub(in super::super) mod selectors;

/// Determine which rooms a dirty-lock recovery must reload.
///
/// `None` means the set couldn't be determined (never held before, no journal,
/// a store-wide operation was journaled, or the journal read failed) and
/// everything must reload.
async fn touched_rooms_for_recovery(
    store_guard: &EventCacheStoreLockGuard,
) -> Option<HashSet<OwnedRoomId>> {
    let generation = EventCacheStoreLockGuard::dirty_since_generation(store_guard)?;

    match store_guard.load_rooms_touched_since(generation).await {
        Ok(Some(room_ids)) => {
            info!(
                num_rooms = room_ids.len(),
                since_generation = generation,
                "Scoping the dirty-lock recovery to the journaled rooms"
            );

            Some(room_ids.into_iter().collect())
        }
        Ok(None) => None,
        Err(error) => {
            warn!(?error, "Failed to read the touched-rooms journal, reloading everything");

            None
        }
    }
}

/// The type containing all the states, for real.
pub struct State {
    store: EventCacheStoreLock,
    by_room: HashMap<OwnedRoomId, StateForRoom>,
}

#[derive(Default)]
pub(super) struct StateForRoom {
    room: Option<RoomEventCacheState>,
    threads: HashMap<OwnedEventId, ThreadEventCacheState>,
    pinned_events: Option<PinnedEventsCacheState>,
    event_focused: HashMap<EventFocusedCacheKey, EventFocusedCacheState>,
}

/// State for the entire Event Cache.
///
/// This aims at containing all the inner mutable states that ought to be
/// updated, behind a per-process lock and a cross-process lock.
///
/// This type can be cloned at low-cost. It will do a shallow clone.
#[derive(Clone)]
pub struct StateLock {
    inner: Arc<StateLockInner>,
}

struct StateLockInner {
    /// The per-process lock around the real state.
    locked_state: RwLock<State>,

    /// A lock taken to avoid multiple attempts to upgrade from a read lock
    /// to a write lock.
    ///
    /// Please see inline comment of [`Self::read`] to understand why it
    /// exists.
    state_lock_upgrade_mutex: Mutex<()>,

    /// Bookkeeping of the live guards over this lock, used to attribute
    /// stalled acquisitions to their holders (see
    /// [`HolderRegistry::wait_attributed`]).
    holders: Arc<HolderRegistry>,
}

/// Bookkeeping of the live guards over the [`StateLock`].
///
/// Every guard carries a [`HolderTicket`] registering who acquired it (the
/// tracing span current at acquisition time) and when; a stalled acquisition
/// logs the registry's snapshot, naming the holders of a deadlock or convoy.
/// Purely diagnostic: it never changes locking behaviour.
#[derive(Default)]
struct HolderRegistry {
    next_id: AtomicU64,
    holders: StdMutex<HashMap<u64, HolderInfo>>,
}

struct HolderInfo {
    kind: &'static str,
    owner: &'static str,
    since: Instant,
}

impl HolderRegistry {
    fn register(self: &Arc<Self>, kind: &'static str) -> HolderTicket {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let owner =
            tracing::Span::current().metadata().map(|metadata| metadata.name()).unwrap_or("?");

        self.holders.lock().unwrap().insert(id, HolderInfo { kind, owner, since: Instant::now() });

        HolderTicket { registry: self.clone(), id }
    }

    fn snapshot(&self) -> String {
        let holders = self.holders.lock().unwrap();

        if holders.is_empty() {
            return "none".to_owned();
        }

        let mut holders = holders
            .values()
            .map(|info| {
                format!("{} held by `{}` for {:?}", info.kind, info.owner, info.since.elapsed())
            })
            .collect::<Vec<_>>();
        holders.sort();
        holders.join("; ")
    }

    /// Await a lock acquisition, logging an error every 10 seconds it stalls,
    /// along with a snapshot of the live guard holders.
    ///
    /// Deliberately never gives up: a stalled acquisition is a bug to surface
    /// loudly, not to paper over with a timeout. The pending acquisition is
    /// polled by reference, so its position in the lock's fair queue is
    /// preserved across the periodic reports.
    async fn wait_attributed<F>(&self, what: &'static str, future: F) -> F::Output
    where
        F: Future,
    {
        let mut future = std::pin::pin!(future);
        let started = Instant::now();

        loop {
            match timeout(&mut future, Duration::from_secs(10)).await {
                Ok(output) => {
                    let waited = started.elapsed();
                    if waited > Duration::from_secs(10) {
                        info!(what, ?waited, "Stalled event cache state lock acquisition resolved");
                    }
                    return output;
                }
                Err(_elapsed) => {
                    error!(
                        what,
                        waited_secs = started.elapsed().as_secs(),
                        holders = %self.snapshot(),
                        "Event cache state lock acquisition is stalled"
                    );
                }
            }
        }
    }
}

/// RAII registration of a live guard in a [`HolderRegistry`].
struct HolderTicket {
    registry: Arc<HolderRegistry>,
    id: u64,
}

impl Drop for HolderTicket {
    fn drop(&mut self) {
        self.registry.holders.lock().unwrap().remove(&self.id);
    }
}

impl StateLock {
    /// Construct a new [`EventCacheStateLock`].
    pub fn new(store: EventCacheStoreLock) -> Self {
        Self {
            inner: Arc::new(StateLockInner {
                locked_state: RwLock::new(State { store, by_room: HashMap::new() }),
                state_lock_upgrade_mutex: Mutex::new(()),
                holders: Arc::new(HolderRegistry::default()),
            }),
        }
    }

    /// Lock this [`StateLock`] with per-thread shared access.
    ///
    /// This method locks the per-thread lock over the state, and then locks
    /// the cross-process lock over the store. It returns an RAII guard
    /// which will drop the read access to the state and to the store when
    /// dropped.
    ///
    /// If the cross-process lock over the store is dirty (see
    /// [`EventCacheStoreLockState`]), the state is reloaded.
    ///
    /// Note: deliberately not `#[instrument]`ed, so that the caller's span
    /// names the acquirer in the holder registry and in stall reports.
    pub(super) async fn read<'state>(&'state self) -> Result<StateLockReadGuard<'state, State>> {
        trace!("Acquiring the lock");
        let tracing_timer = timer!("`read` lock");
        let holders = &self.inner.holders;

        // Only one call at a time to `read` is allowed.
        //
        // Why? Because in case the cross-process lock over the store is dirty, we need
        // to upgrade the read lock over the state to a write lock.
        //
        // ## Upgradable read lock
        //
        // One may argue that this upgrades can be done with an _upgradable read lock_
        // [^1] [^2]. We don't want to use this solution: an upgradable read lock is
        // basically a mutex because we are losing the shared access property, i.e.
        // having multiple read locks at the same time. This is an important property to
        // hold for performance concerns.
        //
        // ## Downgradable write lock
        //
        // One may also argue we could first obtain a write lock over the state from the
        // beginning, thus removing the need to upgrade the read lock to a write lock.
        // The write lock is then downgraded to a read lock once the dirty is cleaned
        // up. It can potentially create a deadlock in the following situation:
        //
        // - `read` is called once, it takes a write lock, then downgrades it to a read
        //   lock: the guard is kept alive somewhere,
        // - `read` is called again, and waits to obtain the write lock, which is
        //   impossible as long as the guard from the previous call is not dropped.
        //
        // ## “Atomic” read and write
        //
        // One may finally argue to first obtain a read lock over the state, then drop
        // it if the cross-process lock over the store is dirty, and immediately obtain
        // a write lock (which can later be downgraded to a read lock). The problem is
        // that this write lock is async: anything can happen between the drop and the
        // new lock acquisition, and it's not possible to pause the runtime in the
        // meantime.
        //
        // ## Semaphore with 1 permit, aka a Mutex
        //
        // The chosen idea is to allow only one execution at a time of this method: it
        // becomes a critical section. That way we are free to “upgrade” the read lock
        // by dropping it and obtaining a new write lock. All callers to this method are
        // waiting, so nothing can happen in the meantime.
        //
        // Note that it doesn't conflict with the `write` method because this latter
        // immediately obtains a write lock, which avoids any conflict with this method.
        //
        // [^1]: https://docs.rs/lock_api/0.4.14/lock_api/struct.RwLock.html#method.upgradable_read
        // [^2]: https://docs.rs/async-lock/3.4.1/async_lock/struct.RwLock.html#method.upgradable_read
        let _state_lock_upgrade_guard = holders
            .wait_attributed("upgrade mutex (read)", self.inner.state_lock_upgrade_mutex.lock())
            .await;
        let _upgrade_mutex_holder = holders.register("upgrade mutex (read)");

        // Obtain a read lock.
        let state_guard =
            holders.wait_attributed("state (read)", self.inner.locked_state.read()).await;
        let holder = holders.register("state (read)");

        Ok(match holders.wait_attributed("store (read)", state_guard.store.lock()).await? {
            EventCacheStoreLockState::Clean(store_guard) => {
                trace!("Lock acquired (from clean)");

                StateLockReadGuard {
                    state: StateLockReadGuardKind::Owned(state_guard),
                    store: store_guard,
                    tracing_timer: Some(tracing_timer),
                    _holder: Some(holder),
                }
            }
            EventCacheStoreLockState::Dirty(store_guard) => {
                // Drop the read lock, and take a write lock to modify the state.
                // This is safe because only one reader at a time (see
                // `Self::state_lock_upgrade_mutex`) is allowed.
                drop(state_guard);
                drop(holder);

                let mut guard = ReloadableStateLockWriteGuard {
                    state: holders
                        .wait_attributed(
                            "state (read, dirty upgrade)",
                            self.inner.locked_state.write(),
                        )
                        .await,
                    store: store_guard,
                    tracing_timer,
                    _holder: holders.register("state (read, dirty upgrade)"),
                };

                // Reload the state, scoped to the journaled rooms when possible.
                let touched_rooms = touched_rooms_for_recovery(&guard.store).await;
                guard.reload(ReloadPreprocessing::None, touched_rooms.as_ref()).await?;

                // All good now, mark the cross-process lock as non-dirty.
                EventCacheStoreLockGuard::clear_dirty(&guard.store);

                trace!("Lock acquired (from dirty)");

                // Downgrade the write guard to a read guard, and map it into a cache state.
                guard.downgrade()
            }
        })
    }

    /// Lock this [`StateLock`] with exclusive per-thread write access.
    ///
    /// This method locks the per-thread lock over the state, and then locks
    /// the cross-process lock over the store. It returns an RAII guard
    /// which will drop the write access to the state and to the store when
    /// dropped.
    ///
    /// If the cross-process lock over the store is dirty (see
    /// [`EventCacheStoreLockState`]), the state is reloaded automatically.
    ///
    /// Note: deliberately not `#[instrument]`ed, so that the caller's span
    /// names the acquirer in the holder registry and in stall reports.
    async fn write<'state>(&'state self) -> Result<ReloadableStateLockWriteGuard<'state>> {
        trace!("Acquiring lock");
        let tracing_timer = timer!("`write` lock");
        let holders = &self.inner.holders;

        let state_guard =
            holders.wait_attributed("state (write)", self.inner.locked_state.write()).await;
        let holder = holders.register("state (write)");

        Ok(match holders.wait_attributed("store (write)", state_guard.store.lock()).await? {
            EventCacheStoreLockState::Clean(store_guard) => {
                trace!("Lock acquired (from clean)");

                ReloadableStateLockWriteGuard {
                    state: state_guard,
                    store: store_guard,
                    tracing_timer,
                    _holder: holder,
                }
            }
            EventCacheStoreLockState::Dirty(store_guard) => {
                let mut guard = ReloadableStateLockWriteGuard {
                    state: state_guard,
                    store: store_guard,
                    tracing_timer,
                    _holder: holder,
                };

                // Reload the state, scoped to the journaled rooms when possible.
                let touched_rooms = touched_rooms_for_recovery(&guard.store).await;
                guard.reload(ReloadPreprocessing::None, touched_rooms.as_ref()).await?;

                // All good now, mark the cross-process lock as non-dirty.
                EventCacheStoreLockGuard::clear_dirty(&guard.store);

                trace!("Lock acquired (from dirty)");

                guard
            }
        })
    }

    /// Clear and reload all states —in-memory and in-store— for all rooms if
    /// `room` is `None`, otherwise for a single room.
    ///
    /// The `caches_for_all_rooms_exclusive_lock_guard` argument ensures an
    /// exclusive lock over all the caches has been acquired. This is required
    /// to ensure safety for this method.
    #[instrument(skip_all)]
    pub(super) async fn clear_and_reload(
        &self,
        _caches_for_all_rooms_exclusive_lock_guard: &RwLockWriteGuard<'_, CachesByRoom>,
        room_id: Option<&RoomId>,
    ) -> Result<()> {
        let tracing_timer = timer!("`clear_and_reload` lock");
        let holders = &self.inner.holders;

        let state_guard = holders
            .wait_attributed("state (clear_and_reload)", self.inner.locked_state.write())
            .await;
        let holder = holders.register("state (clear_and_reload)");

        let mut guard = match holders
            .wait_attributed("store (clear_and_reload)", state_guard.store.lock())
            .await?
        {
            EventCacheStoreLockState::Clean(store_guard)
            | EventCacheStoreLockState::Dirty(store_guard) => ReloadableStateLockWriteGuard {
                state: state_guard,
                store: store_guard,
                tracing_timer,
                _holder: holder,
            },
        };

        // Clear all the events.
        guard.store.clear_all_events(room_id).await?;

        // At this point, all the in-memory `LinkedChunk`s are desynchronised
        // from the storage. Resynchronise them manually by reloading them.
        guard.reload(ReloadPreprocessing::ForgetAll, None).await?;

        if EventCacheStoreLockGuard::is_dirty(&guard.store) {
            // All good because the state has been reloaded, mark the
            // cross-process lock as non-dirty.
            EventCacheStoreLockGuard::clear_dirty(&guard.store);
        }

        Ok(())
    }

    /// Insert a new cache state at location `cache_state_selector` if none
    /// exists.
    ///
    /// This method calls [`Self::write`] to acquire an exclusive access to the
    /// [`State`] in order to insert the cache.
    #[instrument(skip_all)]
    pub(super) async fn try_insert_once_with<Selector, Constructor>(
        &self,
        cache_state_selector: Selector,
        cache_constructor: Constructor,
    ) -> Result<CacheStateLock<Selector>>
    where
        Selector: selectors::CacheState,
        Constructor: AsyncFnOnce(EventCacheStoreLockGuard) -> Result<Selector::Item>,
    {
        let mut state = self.write().await?;
        let cache_state = cache_constructor(state.store).await?;

        cache_state_selector
            .insert_once(&mut state.state, cache_state)
            .then(|| CacheStateLock::new(cache_state_selector, self.clone()))
            .ok_or_else(|| EventCacheError::CacheStateAlreadyExists)
    }
}

impl fmt::Debug for StateLock {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("StateLock").finish_non_exhaustive()
    }
}

/// The read lock guard returned by [`StateLock::read`].
pub struct StateLockReadGuard<'state, S> {
    /// The per-thread read lock guard over the state `S`.
    pub state: StateLockReadGuardKind<'state, S>,

    /// The cross-process lock guard over the store.
    pub store: EventCacheStoreLockGuard,

    /// The [`timer!`] value, used to compute the time the lock is live.
    tracing_timer: Option<TracingTimer>,

    /// Registration of this guard in the holder registry, for stalled
    /// acquisition reports. `None` for guards mapped by reference from
    /// another guard (which holds the registration).
    _holder: Option<HolderTicket>,
}

impl<'state> StateLockReadGuard<'state, State> {
    /// Try to map this read lock guard over a [`State`] to over a
    /// [`selectors::CacheState::Item`].
    ///
    /// In other words, it returns a subset of the state, selected by
    /// `cache_state_selector`.
    fn try_map_into_cache_state<'selector, Selector>(
        self,
        cache_state_selector: &'selector Selector,
    ) -> Result<StateLockReadGuard<'state, Selector::Item>>
    where
        Selector: selectors::CacheState,
        EventCacheError: From<&'selector Selector>,
    {
        Ok(StateLockReadGuard {
            state: match self.state {
                StateLockReadGuardKind::Reference(state) => StateLockReadGuardKind::Reference(
                    cache_state_selector
                        .select(state)
                        .ok_or_else(|| EventCacheError::from(cache_state_selector))?,
                ),

                StateLockReadGuardKind::Owned(state) => StateLockReadGuardKind::Owned(
                    RwLockReadGuard::try_map(state, |state| cache_state_selector.select(state))
                        .map_err(|_| EventCacheError::from(cache_state_selector))?,
                ),
            },
            store: self.store,
            tracing_timer: self.tracing_timer,
            _holder: self._holder,
        })
    }
}

impl<'state> StateLockReadGuard<'state, StateForRoom> {
    /// Project the current read lock guard onto the room cache state.
    pub(super) fn room(&'state self) -> Option<StateLockReadGuard<'state, RoomEventCacheState>> {
        self.state.room.as_ref().map(|room| StateLockReadGuard {
            state: StateLockReadGuardKind::Reference(room),
            store: self.store.clone(),
            tracing_timer: None,
            _holder: None,
        })
    }

    /// Project the current read lock guard onto all thread cache states.
    pub(super) fn threads(
        &'state self,
    ) -> StateLockReadGuard<'state, HashMap<OwnedEventId, ThreadEventCacheState>> {
        StateLockReadGuard {
            state: StateLockReadGuardKind::Reference(&self.state.threads),
            store: self.store.clone(),
            tracing_timer: None,
            _holder: None,
        }
    }
}

impl<'state> StateLockReadGuard<'state, HashMap<OwnedEventId, ThreadEventCacheState>> {
    /// Project the current read lock guard onto all thread cache states via an
    /// iterator.
    pub(super) fn values(
        &'state self,
    ) -> impl Iterator<Item = StateLockReadGuard<'state, ThreadEventCacheState>> {
        self.state.values().map(|item| StateLockReadGuard {
            state: StateLockReadGuardKind::Reference(item),
            store: self.store.clone(),
            tracing_timer: None,
            _holder: None,
        })
    }
}

impl<'state, S> Deref for StateLockReadGuard<'state, S> {
    type Target = S;

    fn deref(&self) -> &Self::Target {
        &self.state
    }
}

/// The kind of guard [`StateLockReadGuard`] owns.
pub enum StateLockReadGuardKind<'state, S> {
    /// A read lock over the state is acquired, and this is a reference to a
    /// cache (sub-)state.
    ///
    /// This is useful if one needs to run operations over multiple cache
    /// (sub-)states without mapping the read lock guard over the state
    /// (because it would consume it).
    Reference(&'state S),

    /// The read lock over the state `S` is acquired, and this is a mapped
    /// guard to a cache (sub-)state.
    Owned(RwLockReadGuard<'state, S>),
}

impl<'state, S> Deref for StateLockReadGuardKind<'state, S> {
    type Target = S;

    fn deref(&self) -> &Self::Target {
        match self {
            Self::Reference(state) => state,
            Self::Owned(state) => state.deref(),
        }
    }
}

/// Private type to hold a “reloadable” write lock guard around the state and
/// the store.
///
/// This type aims at being transient: either it maps to a
/// [`StateLockReadGuard`] with [`Self::downgrade`], or it maps to a
/// [`StateLockWriteGuard`] with [`Self::try_map_into_cache_state`]. Its main
/// goal remains to provide the [`Self::reload`] method to reload all the state
/// of the Event Cache.
struct ReloadableStateLockWriteGuard<'state> {
    /// The per-thread read lock guard over the state `S`.
    state: RwLockWriteGuard<'state, State>,

    /// The cross-process lock guard over the store.
    store: EventCacheStoreLockGuard,

    /// The [`timer!`] value, used to compute the time the lock is live.
    tracing_timer: TracingTimer,

    /// Registration of this guard in the holder registry, for stalled
    /// acquisition reports.
    _holder: HolderTicket,
}

impl<'state> ReloadableStateLockWriteGuard<'state> {
    /// Try to map this write lock guard over a [`State`] to over a
    /// [`selectors::CacheState::Item`].
    ///
    /// In other words, it returns a subset of the state, selected by
    /// `cache_state_selector`.
    fn try_map_into_cache_state<'selector, Selector>(
        self,
        cache_state_selector: &'selector Selector,
    ) -> Result<StateLockWriteGuard<'state, Selector::Item>>
    where
        Selector: selectors::CacheState,
        EventCacheError: From<&'selector Selector>,
    {
        Ok(StateLockWriteGuard {
            state: StateLockWriteGuardKind::Owned(
                RwLockWriteGuard::try_map(self.state, |state| {
                    cache_state_selector.select_mut(state)
                })
                .map_err(|_| EventCacheError::from(cache_state_selector))?,
            ),
            store: self.store,
            _tracing_timer: Some(self.tracing_timer),
            _holder: Some(self._holder),
        })
    }

    /// Synchronously downgrades a write lock into a read lock.
    ///
    /// The per-thread/state lock is downgraded atomically, without allowing
    /// any writers to take exclusive access of the lock in the meantime.
    ///
    /// It returns an RAII guard which will drop the read access to the
    /// state and to the store when dropped.
    fn downgrade(self) -> StateLockReadGuard<'state, State> {
        StateLockReadGuard {
            state: StateLockReadGuardKind::Owned(self.state.downgrade()),
            store: self.store,
            tracing_timer: Some(self.tracing_timer),
            _holder: Some(self._holder),
        }
    }

    /// Reload the state from the store.
    ///
    /// When `touched_rooms` is `Some`, only the states of those rooms are
    /// reloaded: the store content of every other room was last written by
    /// this process, so its in-memory state is already in sync. `None` means
    /// the set of modified rooms is unknown and everything reloads.
    async fn reload(
        &mut self,
        preprocessing: ReloadPreprocessing,
        touched_rooms: Option<&HashSet<OwnedRoomId>>,
    ) -> Result<()> {
        match touched_rooms {
            Some(touched_rooms) => trace!(
                num_touched_rooms = touched_rooms.len(),
                num_loaded_rooms = self.state.by_room.len(),
                "Reloading the state (scoped to the touched rooms)"
            ),
            None => trace!("Reloading the state (all rooms)"),
        }

        // Iterate over all states and reload them.
        for (room_id, StateForRoom { room, threads, pinned_events, event_focused }) in
            self.state.by_room.iter_mut()
        {
            if let Some(touched_rooms) = touched_rooms
                && !touched_rooms.contains(room_id)
            {
                continue;
            }

            // Room.
            if let Some(room_state) = room {
                let mut room_state = StateLockWriteGuard {
                    state: StateLockWriteGuardKind::Reference(room_state),
                    store: self.store.clone(),
                    _tracing_timer: None,
                    _holder: None,
                };

                let updates_as_vector_diffs = room_state.reload(preprocessing).await?;
                room_state.update_sender.send(
                    room::RoomEventCacheUpdate::UpdateTimelineEvents(TimelineVectorDiffs {
                        diffs: updates_as_vector_diffs,
                        origin: EventsOrigin::Cache,
                    }),
                    Some(room::RoomEventCacheGenericUpdate {
                        room_id: room_id.clone(),
                        origin: EventsOrigin::Cache,
                    }),
                );
            }

            // Threads.
            for thread_state in threads.values_mut() {
                let mut thread_state = StateLockWriteGuard {
                    state: StateLockWriteGuardKind::Reference(thread_state),
                    store: self.store.clone(),
                    _tracing_timer: None,
                    _holder: None,
                };

                let updates_as_vector_diffs = thread_state.reload(preprocessing).await?;
                thread_state.send_timeline_updates(
                    updates_as_vector_diffs,
                    EventsOrigin::Cache,
                    Some(room::RoomEventCacheGenericUpdate {
                        room_id: room_id.clone(),
                        origin: EventsOrigin::Cache,
                    }),
                );
            }

            // Pinned events.
            if let Some(pinned_events_state) = pinned_events {
                let mut pinned_events_state = StateLockWriteGuard {
                    state: StateLockWriteGuardKind::Reference(pinned_events_state),
                    store: self.store.clone(),
                    _tracing_timer: None,
                    _holder: None,
                };

                let updates_as_vector_diffs = pinned_events_state.reload(preprocessing).await?;
                pinned_events_state.update_sender.send(TimelineVectorDiffs {
                    diffs: updates_as_vector_diffs,
                    origin: EventsOrigin::Cache,
                });
            }

            // Event-focused.
            for event_focused_state in event_focused.values_mut() {
                let mut event_focused_state = StateLockWriteGuard {
                    state: StateLockWriteGuardKind::Reference(event_focused_state),
                    store: self.store.clone(),
                    _tracing_timer: None,
                    _holder: None,
                };

                let updates_as_vector_diffs = event_focused_state.reload(preprocessing).await?;
                let _ = event_focused_state.update_sender.send(TimelineVectorDiffs {
                    diffs: updates_as_vector_diffs,
                    origin: EventsOrigin::Cache,
                });
            }
        }

        Ok(())
    }
}

/// The write lock guard returned by [`StateLock::write`].
pub struct StateLockWriteGuard<'state, S> {
    /// The per-thread write lock guard over the state `S`.
    pub state: StateLockWriteGuardKind<'state, S>,

    /// The cross-process lock guard over the store.
    pub store: EventCacheStoreLockGuard,

    /// The [`timer!`] value, used to compute the time the lock is live.
    _tracing_timer: Option<TracingTimer>,

    /// Registration of this guard in the holder registry, for stalled
    /// acquisition reports.
    _holder: Option<HolderTicket>,
}

impl<'state, S> Deref for StateLockWriteGuard<'state, S> {
    type Target = S;

    fn deref(&self) -> &Self::Target {
        &self.state
    }
}

impl<'state, S> DerefMut for StateLockWriteGuard<'state, S> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.state
    }
}

/// The kind of guard [`StateLockWriteGuard`] owns.
pub enum StateLockWriteGuardKind<'state, S> {
    /// A write lock over the state is acquired, and this is a reference to a
    /// cache (sub-)state.
    ///
    /// This is useful if one needs to run operations over multiple cache
    /// (sub-)states without mapping the write lock guard over the state
    /// (because it would consume it).
    Reference(&'state mut S),

    /// The write lock over the state `S` is acquired, and this is a mapped
    /// guard to a cache (sub-)state.
    Owned(RwLockMappedWriteGuard<'state, S>),
}

impl<'state, S> Deref for StateLockWriteGuardKind<'state, S> {
    type Target = S;

    fn deref(&self) -> &Self::Target {
        match self {
            Self::Reference(state) => state,
            Self::Owned(state) => state.deref(),
        }
    }
}

impl<'state, S> DerefMut for StateLockWriteGuardKind<'state, S> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        match self {
            Self::Reference(state) => state,
            Self::Owned(state) => state.deref_mut(),
        }
    }
}

/// A wrapper around [`State`] with a [`CacheStateSelector`], facilitating the
/// embedding of these API in a single type.
pub struct CacheStateLock<Selector> {
    cache_state_selector: Selector,
    state_lock: StateLock,
}

impl<Selector> CacheStateLock<Selector>
where
    Selector: selectors::CacheState,
{
    pub(super) fn new(cache_state_selector: Selector, state_lock: StateLock) -> Self {
        Self { cache_state_selector, state_lock }
    }
}

// Fallible methods.
impl<Selector> CacheStateLock<Selector>
where
    Selector: selectors::CacheState,
    EventCacheError: for<'a> From<&'a Selector>,
{
    /// Lock this [`CacheStateLock`] by locking the full [`State`] with
    /// per-thread shared access.
    ///
    /// This method locks the per-thread lock over the state, and then locks
    /// the cross-process lock over the store. It returns an RAII guard
    /// which will drop the read access to the state and to the store when
    /// dropped.
    ///
    /// If the cross-process lock over the store is dirty (see
    /// [`EventCacheStoreLockState`]), the state is reloaded.
    pub async fn read(&self) -> Result<StateLockReadGuard<'_, Selector::Item>> {
        self.state_lock.read().await?.try_map_into_cache_state(&self.cache_state_selector)
    }

    /// Lock this [`CacheStateLock`] by locking the full [`State`] with
    /// exclusive per-thread write access.
    ///
    /// This method locks the per-thread lock over the state, and then locks
    /// the cross-process lock over the store. It returns an RAII guard
    /// which will drop the write access to the state and to the store when
    /// dropped.
    ///
    /// If the cross-process lock over the store is dirty (see
    /// [`EventCacheStoreLockState`]), the state is reloaded.
    pub async fn write(&self) -> Result<StateLockWriteGuard<'_, Selector::Item>> {
        self.state_lock.write().await?.try_map_into_cache_state(&self.cache_state_selector)
    }

    /// Shortcut to reload (with no preprocessing) the state cache just for
    /// test.
    #[cfg(test)]
    pub async fn reload_no_preprocessing(&self) -> Result<()> {
        self.state_lock.write().await?.reload(ReloadPreprocessing::None, None).await
    }
}

/// Kind of pre-processing to do when reloading a cache.
#[derive(Clone, Copy)]
pub enum ReloadPreprocessing {
    /// Erase all events before reloading.
    ForgetAll,

    /// Do nothing before reloading.
    None,
}
