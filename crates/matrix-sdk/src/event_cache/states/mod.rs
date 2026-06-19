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
    collections::HashMap,
    fmt,
    ops::{Deref, DerefMut},
    sync::Arc,
};

use matrix_sdk_base::event_cache::store::{
    EventCacheStoreLock, EventCacheStoreLockGuard, EventCacheStoreLockState,
};
use ruma::{OwnedEventId, OwnedRoomId};
use tokio::sync::{Mutex, RwLock, RwLockMappedWriteGuard, RwLockReadGuard, RwLockWriteGuard};

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

/// The type containing all the states, for real.
pub struct State {
    store: EventCacheStoreLock,
    by_room: HashMap<OwnedRoomId, StateForRoom>,
}

#[derive(Default)]
struct StateForRoom {
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
}

impl StateLock {
    /// Construct a new [`EventCacheStateLock`].
    pub fn new(store: EventCacheStoreLock) -> Self {
        Self {
            inner: Arc::new(StateLockInner {
                locked_state: RwLock::new(State { store, by_room: HashMap::new() }),
                state_lock_upgrade_mutex: Mutex::new(()),
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
    pub(super) async fn read<'state>(&'state self) -> Result<StateLockReadGuard<'state, State>> {
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
        let _state_lock_upgrade_guard = self.inner.state_lock_upgrade_mutex.lock().await;

        // Obtain a read lock.
        let state_guard = self.inner.locked_state.read().await;

        Ok(match state_guard.store.lock().await? {
            EventCacheStoreLockState::Clean(store_guard) => {
                StateLockReadGuard { state: state_guard, store: store_guard }
            }
            EventCacheStoreLockState::Dirty(store_guard) => {
                // Drop the read lock, and take a write lock to modify the state.
                // This is safe because only one reader at a time (see
                // `Self::state_lock_upgrade_mutex`) is allowed.
                drop(state_guard);

                let mut guard = ReloadableStateLockWriteGuard {
                    state: self.inner.locked_state.write().await,
                    store: store_guard,
                };

                // Reload the state.
                guard.reload(ReloadPreprocessing::None).await?;

                // All good now, mark the cross-process lock as non-dirty.
                EventCacheStoreLockGuard::clear_dirty(&guard.store);

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
    async fn write<'state>(&'state self) -> Result<ReloadableStateLockWriteGuard<'state>> {
        let state_guard = self.inner.locked_state.write().await;

        Ok(match state_guard.store.lock().await? {
            EventCacheStoreLockState::Clean(store_guard) => {
                ReloadableStateLockWriteGuard { state: state_guard, store: store_guard }
            }
            EventCacheStoreLockState::Dirty(store_guard) => {
                let mut guard =
                    ReloadableStateLockWriteGuard { state: state_guard, store: store_guard };

                // Reload the state.
                guard.reload(ReloadPreprocessing::None).await?;

                // All good now, mark the cross-process lock as non-dirty.
                EventCacheStoreLockGuard::clear_dirty(&guard.store);

                guard
            }
        })
    }

    /// Clear and reload all states: in-memory and in-store.
    ///
    /// The `caches_for_all_rooms_exclusive_lock_guard` argument ensures an
    /// exclusive lock over all the caches has been acquired. This is required
    /// to ensure safety for this method.
    pub(super) async fn clear_and_reload(
        &self,
        caches_for_all_rooms_exclusive_lock_guard: RwLockWriteGuard<'_, CachesByRoom>,
    ) -> Result<()> {
        let state_guard = self.inner.locked_state.write().await;

        let mut guard = match state_guard.store.lock().await? {
            EventCacheStoreLockState::Clean(store_guard)
            | EventCacheStoreLockState::Dirty(store_guard) => {
                ReloadableStateLockWriteGuard { state: state_guard, store: store_guard }
            }
        };

        // Clear all the events.
        guard.store.clear_all_events().await?;

        // At this point, all the in-memory `LinkedChunk`s are desynchronised
        // from the storage. Resynchronise them manually by reloading them.
        guard.reload(ReloadPreprocessing::ForgetAll).await?;

        if EventCacheStoreLockGuard::is_dirty(&guard.store) {
            // All good because the state has been reloaded, mark the
            // cross-process lock as non-dirty.
            EventCacheStoreLockGuard::clear_dirty(&guard.store);
        }

        drop(caches_for_all_rooms_exclusive_lock_guard);

        Ok(())
    }

    /// Insert a new cache state at location `cache_state_selector` if none
    /// exists.
    ///
    /// This method calls [`Self::write`] to acquire an exclusive access to the
    /// [`State`] in order to insert the cache.
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
            .then(|| CacheStateLock { cache_state_selector, state_lock: self.clone() })
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
    pub state: RwLockReadGuard<'state, S>,

    /// The cross-process lock guard over the store.
    pub store: EventCacheStoreLockGuard,
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
            state: RwLockReadGuard::try_map(self.state, |state| cache_state_selector.select(state))
                .map_err(|_| EventCacheError::from(cache_state_selector))?,
            store: self.store,
        })
    }
}

impl<'state, S> Deref for StateLockReadGuard<'state, S> {
    type Target = S;

    fn deref(&self) -> &Self::Target {
        &self.state
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
    state: RwLockWriteGuard<'state, State>,
    store: EventCacheStoreLockGuard,
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
            state: StateLockWriteGuardKind::MappedGuard(
                RwLockWriteGuard::try_map(self.state, |state| {
                    cache_state_selector.select_mut(state)
                })
                .map_err(|_| EventCacheError::from(cache_state_selector))?,
            ),
            store: self.store,
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
        StateLockReadGuard { state: self.state.downgrade(), store: self.store }
    }

    async fn reload(&mut self, preprocessing: ReloadPreprocessing) -> Result<()> {
        // Iterate over all states and reload them.
        for (room_id, StateForRoom { room, threads, pinned_events, event_focused }) in
            self.state.by_room.iter_mut()
        {
            // Room.
            if let Some(room_state) = room {
                let mut room_state = StateLockWriteGuard {
                    state: StateLockWriteGuardKind::Reference(room_state),
                    store: self.store.clone(),
                };

                let updates_as_vector_diffs = room_state.reload(preprocessing).await?;
                room_state.update_sender.send(
                    room::RoomEventCacheUpdate::UpdateTimelineEvents(TimelineVectorDiffs {
                        diffs: updates_as_vector_diffs,
                        origin: EventsOrigin::Cache,
                    }),
                    Some(room::RoomEventCacheGenericUpdate { room_id: room_id.clone() }),
                );
            }

            // Threads.
            for thread_state in threads.values_mut() {
                let mut thread_state = StateLockWriteGuard {
                    state: StateLockWriteGuardKind::Reference(thread_state),
                    store: self.store.clone(),
                };

                let updates_as_vector_diffs = thread_state.reload(preprocessing).await?;
                thread_state.update_sender.send(
                    TimelineVectorDiffs {
                        diffs: updates_as_vector_diffs,
                        origin: EventsOrigin::Cache,
                    },
                    Some(room::RoomEventCacheGenericUpdate { room_id: room_id.clone() }),
                );
            }

            // Pinned events.
            if let Some(pinned_events_state) = pinned_events {
                let mut pinned_events_state = StateLockWriteGuard {
                    state: StateLockWriteGuardKind::Reference(pinned_events_state),
                    store: self.store.clone(),
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
    MappedGuard(RwLockMappedWriteGuard<'state, S>),
}

impl<'state, S> Deref for StateLockWriteGuardKind<'state, S> {
    type Target = S;

    fn deref(&self) -> &Self::Target {
        match self {
            Self::Reference(state) => state,
            Self::MappedGuard(state) => state.deref(),
        }
    }
}

impl<'state, S> DerefMut for StateLockWriteGuardKind<'state, S> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        match self {
            Self::Reference(state) => state,
            Self::MappedGuard(state) => state.deref_mut(),
        }
    }
}

/// A wrapper around [`State`] with a [`CacheStateSelector`], facilitating the
/// embedding of these API in a single type.
pub struct CacheStateLock<Selector>
where
    Selector: selectors::CacheState,
{
    cache_state_selector: Selector,
    state_lock: StateLock,
}

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
        self.state_lock.write().await?.reload(ReloadPreprocessing::None).await
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
