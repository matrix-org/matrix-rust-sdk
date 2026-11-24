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

use matrix_sdk_base::{
    event_cache::store::{EventCacheStoreLock, EventCacheStoreLockGuard, EventCacheStoreLockState},
    locks::Mutex as SyncMutex,
    timer,
    tracing_timer::TracingTimer,
};
use ruma::{OwnedEventId, OwnedRoomId, RoomId};
use tokio::sync::{
    OwnedRwLockMappedWriteGuard, OwnedRwLockReadGuard, OwnedRwLockWriteGuard, RwLock,
    RwLockReadGuard, RwLockWriteGuard,
};
use tracing::{instrument, trace};

use super::{
    CachesByRoom, EventCacheError, EventsOrigin, Result,
    caches::{
        TimelineVectorDiffs,
        event_focused::{EventFocusedCacheKey, EventFocusedCacheState},
        pinned_events::PinnedEventsCacheState,
        room::{self, RoomEventCacheState},
        thread::{self, ThreadEventCacheState},
    },
};

pub(in super::super) mod selectors;

/// The type containing all the states, for real.
///
/// The states are grouped per room, each behind its own lock: see
/// [`RoomStateLock`].
pub struct State {
    store: EventCacheStoreLock,

    /// The states of each room.
    ///
    /// This map is behind a synchronous mutex so that a room can be added while
    /// holding the lock over [`State`] in shared mode. The mutex is only held
    /// to look up or insert a room, never across an `.await` point.
    by_room: SyncMutex<HashMap<OwnedRoomId, RoomStateLock>>,
}

impl State {
    /// Get the lock of the states of a room, if it exists.
    fn room(&self, room_id: &RoomId) -> Option<RoomStateLock> {
        self.by_room.lock().get(room_id).cloned()
    }

    /// Get the lock of the states of a room, creating empty states for it if
    /// it doesn't exist.
    fn room_or_default(&self, room_id: &RoomId) -> RoomStateLock {
        self.by_room.lock().entry(room_id.to_owned()).or_default().clone()
    }
}

/// The lock around all the states of a single room.
///
/// ## Locking
///
/// Operations on the states of a room acquire, in this order:
///
/// 1. the lock over the full [`State`] in **shared** mode,
/// 2. the lock of the room, in shared or exclusive mode,
/// 3. the cross-process lock over the store.
///
/// Operations on different rooms thus run concurrently. The lock over the
/// full [`State`] is only acquired in exclusive mode to reload or clear the
/// states of all rooms, see [`StateLock::write`]. Because every room guard is
/// paired with a shared guard over [`State`], an exclusive guard over [`State`]
/// guarantees that no room lock is held: a room lock is then never contended.
///
/// No task must acquire a lock it already holds: Tokio's `RwLock` is
/// write-preferring, so a task holding a read guard and acquiring another read
/// guard on the same lock deadlocks as soon as a writer is queued in between.
///
/// It is an `Arc` so that its guard can outlive the borrow of [`State`].
type RoomStateLock = Arc<RwLock<StateForRoom>>;

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
}

/// Get the guard out of a [`EventCacheStoreLockState`].
///
/// Whether the cross-process lock is dirty must be checked with
/// [`EventCacheStoreLockGuard::is_dirty`] and not with the
/// [`EventCacheStoreLockState`] variant: when a guard is created while other
/// guards are alive, it is always [`EventCacheStoreLockState::Clean`], even if
/// the states haven't been reloaded yet since the lock has been dirtied.
fn into_store_guard(state: EventCacheStoreLockState) -> EventCacheStoreLockGuard {
    match state {
        EventCacheStoreLockState::Clean(store_guard)
        | EventCacheStoreLockState::Dirty(store_guard) => store_guard,
    }
}

impl StateLock {
    /// Construct a new [`EventCacheStateLock`].
    pub fn new(store: EventCacheStoreLock) -> Self {
        Self {
            inner: Arc::new(StateLockInner {
                locked_state: RwLock::new(State { store, by_room: Default::default() }),
            }),
        }
    }

    /// Lock this [`StateLock`] with per-thread shared access.
    ///
    /// This method locks the per-thread lock over the state, and then locks the
    /// cross-process lock over the store. It returns an RAII guard which will
    /// drop the read access to the state and to the store when dropped.
    ///
    /// If the cross-process lock over the store is dirty (see
    /// [`EventCacheStoreLockState`]), the state is reloaded first.
    #[instrument(skip_all)]
    pub(super) async fn read<'state>(&'state self) -> Result<AllStatesReadGuard<'state>> {
        trace!("Acquiring the lock");
        let tracing_timer = timer!("`read` lock");

        loop {
            let state_guard = self.inner.locked_state.read().await;
            let store_guard = into_store_guard(state_guard.store.lock().await?);

            if EventCacheStoreLockGuard::is_dirty(&store_guard) {
                // Release everything, reload, and try again. See
                // `Self::reload_if_dirty` to learn why.
                drop(store_guard);
                drop(state_guard);

                self.reload_if_dirty().await?;

                continue;
            }

            trace!("Lock acquired");

            return Ok(AllStatesReadGuard {
                _state: state_guard,
                store: store_guard,
                _tracing_timer: Some(tracing_timer),
            });
        }
    }

    /// Lock the state selected by `cache_state_selector` with per-thread shared
    /// access.
    ///
    /// The lock over the full [`State`] is acquired in shared mode, then the
    /// lock of the room in shared mode, and then the cross-process lock over
    /// the store. See [`RoomStateLock`] to learn more.
    ///
    /// If the cross-process lock over the store is dirty (see
    /// [`EventCacheStoreLockState`]), the state is reloaded first.
    #[instrument(skip_all)]
    async fn read_cache_state<'state, 'selector, Selector>(
        &'state self,
        cache_state_selector: &'selector Selector,
    ) -> Result<StateLockReadGuard<'state, Selector::Item>>
    where
        Selector: selectors::CacheState,
        Selector::Item: 'state,
        EventCacheError: From<&'selector Selector>,
    {
        trace!("Acquiring the lock");
        let tracing_timer = timer!("`read` lock");

        loop {
            let state_guard = self.inner.locked_state.read().await;

            let room_state = state_guard
                .room(cache_state_selector.room_id())
                .ok_or_else(|| EventCacheError::from(cache_state_selector))?;
            let room_guard = room_state.read_owned().await;

            let store_guard = into_store_guard(state_guard.store.lock().await?);

            if EventCacheStoreLockGuard::is_dirty(&store_guard) {
                // Release everything, reload, and try again. See
                // `Self::reload_if_dirty` to learn why.
                drop(store_guard);
                drop(room_guard);
                drop(state_guard);

                self.reload_if_dirty().await?;

                continue;
            }

            let room_guard = OwnedRwLockReadGuard::try_map(room_guard, |room_state| {
                cache_state_selector.select(room_state)
            })
            .map_err(|_| EventCacheError::from(cache_state_selector))?;

            trace!("Lock acquired");

            return Ok(StateLockReadGuard {
                state: StateLockReadGuardKind::Owned(room_guard),
                store: store_guard,
                _all_states: Some(state_guard),
                _tracing_timer: Some(tracing_timer),
            });
        }
    }

    /// Lock the state selected by `cache_state_selector` with per-thread
    /// exclusive access.
    ///
    /// The lock over the full [`State`] is acquired in shared mode, then the
    /// lock of the room in exclusive mode, and then the cross-process lock over
    /// the store. See [`RoomStateLock`] to learn more.
    ///
    /// If the cross-process lock over the store is dirty (see
    /// [`EventCacheStoreLockState`]), the state is reloaded first.
    #[instrument(skip_all)]
    async fn write_cache_state<'state, 'selector, Selector>(
        &'state self,
        cache_state_selector: &'selector Selector,
    ) -> Result<StateLockWriteGuard<'state, Selector::Item>>
    where
        Selector: selectors::CacheState,
        Selector::Item: 'state,
        EventCacheError: From<&'selector Selector>,
    {
        trace!("Acquiring lock");
        let tracing_timer = timer!("`write` lock");

        loop {
            let state_guard = self.inner.locked_state.read().await;

            let room_state = state_guard
                .room(cache_state_selector.room_id())
                .ok_or_else(|| EventCacheError::from(cache_state_selector))?;
            let room_guard = room_state.write_owned().await;

            let store_guard = into_store_guard(state_guard.store.lock().await?);

            if EventCacheStoreLockGuard::is_dirty(&store_guard) {
                // Release everything, reload, and try again. See
                // `Self::reload_if_dirty` to learn why.
                drop(store_guard);
                drop(room_guard);
                drop(state_guard);

                self.reload_if_dirty().await?;

                continue;
            }

            let room_guard = OwnedRwLockWriteGuard::try_map(room_guard, |room_state| {
                cache_state_selector.select_mut(room_state)
            })
            .map_err(|_| EventCacheError::from(cache_state_selector))?;

            trace!("Lock acquired");

            return Ok(StateLockWriteGuard {
                state: StateLockWriteGuardKind::Owned(room_guard),
                store: store_guard,
                _all_states: Some(state_guard),
                _tracing_timer: Some(tracing_timer),
            });
        }
    }

    /// Reload the states of all rooms if the cross-process lock over the store
    /// is dirty.
    ///
    /// When another process has taken the cross-process lock over the store, it
    /// may have modified the store: the cross-process lock is then marked as
    /// dirty, and all the in-memory states must be reloaded before being used.
    ///
    /// A task that finds the cross-process lock dirty while holding the lock
    /// over [`State`] in shared mode can't reload the states: it would need an
    /// exclusive access, and upgrading a shared lock isn't possible. Instead,
    /// it releases all its locks, calls this method, and tries again. This
    /// method acquires the lock over [`State`] in exclusive mode, which
    /// guarantees that no room lock is held, and checks again whether the
    /// cross-process lock is dirty, since another task may have reloaded
    /// the states in the meantime.
    async fn reload_if_dirty(&self) -> Result<()> {
        // `Self::write` reloads the states if the cross-process lock is dirty.
        drop(self.write().await?);

        Ok(())
    }

    /// Lock this [`StateLock`] with per-thread exclusive write access.
    ///
    /// This method locks the per-thread lock over the state, and then locks the
    /// cross-process lock over the store. It returns an RAII guard which will
    /// drop the write access to the state and to the store when dropped.
    ///
    /// Because it waits for all the room locks to be released, and prevents
    /// any room lock from being acquired, it must only be used for operations
    /// on all rooms.
    ///
    /// If the cross-process lock over the store is dirty (see
    /// [`EventCacheStoreLockState`]), the state is reloaded automatically.
    #[instrument(skip_all)]
    async fn write<'state>(&'state self) -> Result<ReloadableStateLockWriteGuard<'state>> {
        trace!("Acquiring the exclusive lock");
        let tracing_timer = timer!("exclusive `write` lock");

        let state_guard = self.inner.locked_state.write().await;
        let store_guard = into_store_guard(state_guard.store.lock().await?);

        let mut guard = ReloadableStateLockWriteGuard {
            state: state_guard,
            store: store_guard,
            _tracing_timer: tracing_timer,
        };

        if EventCacheStoreLockGuard::is_dirty(&guard.store) {
            // Reload the state.
            guard.reload(ReloadPreprocessing::None, None).await?;

            // All good now, mark the cross-process lock as non-dirty.
            EventCacheStoreLockGuard::clear_dirty(&guard.store);

            trace!("Exclusive lock acquired (from dirty)");
        } else {
            trace!("Exclusive lock acquired (from clean)");
        }

        Ok(guard)
    }

    /// Lock this [`StateLock`] with exclusive access, which blocks all the
    /// operations on all the rooms until the returned guard is dropped.
    #[cfg(test)]
    pub(super) async fn write_for_tests(&self) -> Result<impl Sized + '_> {
        self.write().await
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

        let state_guard = self.inner.locked_state.write().await;
        let store_guard = into_store_guard(state_guard.store.lock().await?);

        let mut guard = ReloadableStateLockWriteGuard {
            state: state_guard,
            store: store_guard,
            _tracing_timer: tracing_timer,
        };

        // Clear all the events.
        guard.store.clear_all_events(room_id).await?;

        // At this point, all the affected in-memory `LinkedChunk`s are
        // desynchronised from the storage. Resynchronise them manually
        // by reloading them.
        guard.reload(ReloadPreprocessing::ForgetAll, room_id).await?;

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
    /// The lock over the full [`State`] is acquired in shared mode, and the
    /// lock of the room in exclusive mode: creating a state only blocks the
    /// operations on the same room.
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
        let _tracing_timer = timer!("`try_insert_once_with` lock");

        loop {
            let state_guard = self.inner.locked_state.read().await;

            let room_state = state_guard.room_or_default(cache_state_selector.room_id());
            let mut room_guard = room_state.write_owned().await;

            let store_guard = into_store_guard(state_guard.store.lock().await?);

            if EventCacheStoreLockGuard::is_dirty(&store_guard) {
                // Release everything, reload, and try again. See
                // `Self::reload_if_dirty` to learn why.
                drop(store_guard);
                drop(room_guard);
                drop(state_guard);

                self.reload_if_dirty().await?;

                continue;
            }

            // Don't load the state if it already exists.
            if cache_state_selector.select(&room_guard).is_some() {
                return Err(EventCacheError::CacheStateAlreadyExists);
            }

            let cache_state = cache_constructor(store_guard).await?;

            return cache_state_selector
                .insert_once(&mut room_guard, cache_state)
                .then(|| CacheStateLock::new(cache_state_selector, self.clone()))
                .ok_or_else(|| EventCacheError::CacheStateAlreadyExists);
        }
    }
}

impl fmt::Debug for StateLock {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("StateLock").finish_non_exhaustive()
    }
}

/// The read lock guard over the full [`State`] returned by
/// [`StateLock::read`].
///
/// It gives access to the store, but not to the states.
pub(super) struct AllStatesReadGuard<'state> {
    /// The per-thread read lock guard over the full state.
    _state: RwLockReadGuard<'state, State>,

    /// The cross-process lock guard over the store.
    pub store: EventCacheStoreLockGuard,

    /// The [`timer!`] value, used to compute the time the lock is live.
    _tracing_timer: Option<TracingTimer>,
}

/// The read lock guard returned by [`CacheStateLock::read`].
pub struct StateLockReadGuard<'state, S> {
    /// The per-thread read lock guard over the state `S`.
    pub state: StateLockReadGuardKind<'state, S>,

    /// The cross-process lock guard over the store.
    pub store: EventCacheStoreLockGuard,

    /// The per-thread read lock guard over the full state, if this guard owns
    /// it, i.e. if it's not a projection of another guard.
    ///
    /// Declared after `state`, so that it's released after it.
    _all_states: Option<RwLockReadGuard<'state, State>>,

    /// The [`timer!`] value, used to compute the time the lock is live.
    _tracing_timer: Option<TracingTimer>,
}

impl<'state> StateLockReadGuard<'state, StateForRoom> {
    /// Project the current read lock guard onto the room cache state.
    pub(super) fn room(&'state self) -> Option<StateLockReadGuard<'state, RoomEventCacheState>> {
        self.state.room.as_ref().map(|room| StateLockReadGuard {
            state: StateLockReadGuardKind::Reference(room),
            store: self.store.clone(),
            _all_states: None,
            _tracing_timer: None,
        })
    }

    /// Project the current read lock guard onto all thread cache states.
    pub(super) fn threads(
        &'state self,
    ) -> StateLockReadGuard<'state, HashMap<OwnedEventId, ThreadEventCacheState>> {
        StateLockReadGuard {
            state: StateLockReadGuardKind::Reference(&self.state.threads),
            store: self.store.clone(),
            _all_states: None,
            _tracing_timer: None,
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
            _all_states: None,
            _tracing_timer: None,
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
    /// (sub-)states without mapping the read lock guard over the state (because
    /// it would consume it).
    Reference(&'state S),

    /// The read lock over the state of the room is acquired, and this is a
    /// mapped guard to a cache (sub-)state.
    Owned(OwnedRwLockReadGuard<StateForRoom, S>),
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

/// Private type to hold a “reloadable” exclusive write lock guard around the
/// full state and the store.
///
/// Its main goal is to provide the [`Self::reload`] method to reload all the
/// state of the Event Cache.
struct ReloadableStateLockWriteGuard<'state> {
    /// The per-thread write lock guard over the full state.
    state: RwLockWriteGuard<'state, State>,

    /// The cross-process lock guard over the store.
    store: EventCacheStoreLockGuard,

    /// The [`timer!`] value, used to compute the time the lock is live.
    _tracing_timer: TracingTimer,
}

impl<'state> ReloadableStateLockWriteGuard<'state> {
    async fn reload(
        &mut self,
        preprocessing: ReloadPreprocessing,
        room_id: Option<&RoomId>,
    ) -> Result<()> {
        trace!("Reloading the state");

        // Collect the rooms first, to not hold the synchronous mutex across
        // `.await` points.
        let rooms: Vec<(OwnedRoomId, RoomStateLock)> = {
            let by_room = self.state.by_room.lock();

            if let Some(room_id) = room_id {
                by_room
                    .get(room_id)
                    .map(|room_state| (room_id.to_owned(), room_state.clone()))
                    .into_iter()
                    .collect()
            } else {
                by_room
                    .iter()
                    .map(|(room_id, room_state)| (room_id.clone(), room_state.clone()))
                    .collect()
            }
        };

        for (room_id, room_state) in rooms {
            let room_id: &RoomId = &room_id;

            // The lock over the full `State` is exclusive: nobody can hold the
            // room lock meanwhile, it's not contended.
            let mut room_state = room_state.write().await;
            let StateForRoom { room, threads, pinned_events, event_focused } = &mut *room_state;

            // Room.
            if let Some(room_state) = room {
                let mut room_state = StateLockWriteGuard {
                    state: StateLockWriteGuardKind::Reference(room_state),
                    store: self.store.clone(),
                    _all_states: None,
                    _tracing_timer: None,
                };

                let updates_as_vector_diffs = room_state.reload(preprocessing).await?;
                room_state.update_sender.send(
                    room::RoomEventCacheUpdate::UpdateTimelineEvents(TimelineVectorDiffs {
                        diffs: updates_as_vector_diffs,
                        origin: EventsOrigin::Cache,
                    }),
                    Some(room::RoomEventCacheGenericUpdate { room_id: room_id.to_owned() }),
                );
            }

            // Threads.
            for thread_state in threads.values_mut() {
                let mut thread_state = StateLockWriteGuard {
                    state: StateLockWriteGuardKind::Reference(thread_state),
                    store: self.store.clone(),
                    _all_states: None,
                    _tracing_timer: None,
                };

                let (updates_as_vector_diffs, thread_summary) =
                    thread_state.reload(preprocessing).await?;
                thread_state.update_sender.send(
                    thread::ThreadEventCacheUpdate::UpdateTimelineEvents(TimelineVectorDiffs {
                        diffs: updates_as_vector_diffs,
                        origin: EventsOrigin::Cache,
                    }),
                    Some(room::RoomEventCacheGenericUpdate { room_id: room_id.to_owned() }),
                );
                thread_state
                    .update_sender
                    .send(thread::ThreadEventCacheUpdate::UpdateSummary(thread_summary), None);
            }

            // Pinned events.
            if let Some(pinned_events_state) = pinned_events {
                let mut pinned_events_state = StateLockWriteGuard {
                    state: StateLockWriteGuardKind::Reference(pinned_events_state),
                    store: self.store.clone(),
                    _all_states: None,
                    _tracing_timer: None,
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
                    _all_states: None,
                    _tracing_timer: None,
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

    /// The per-thread read lock guard over the full state, if this guard owns
    /// it, i.e. if it's not a projection of another guard.
    ///
    /// Declared after `state`, so that it's released after it.
    _all_states: Option<RwLockReadGuard<'state, State>>,

    /// The [`timer!`] value, used to compute the time the lock is live.
    _tracing_timer: Option<TracingTimer>,
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

    /// The write lock over the state of the room is acquired, and this is a
    /// mapped guard to a cache (sub-)state.
    Owned(OwnedRwLockMappedWriteGuard<StateForRoom, S>),
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
    /// This method locks the per-thread lock over the state, and then locks the
    /// cross-process lock over the store. It returns an RAII guard which will
    /// drop the read access to the state and to the store when dropped.
    ///
    /// If the cross-process lock over the store is dirty (see
    /// [`EventCacheStoreLockState`]), the state is reloaded.
    pub async fn read(&self) -> Result<StateLockReadGuard<'_, Selector::Item>> {
        self.state_lock.read_cache_state(&self.cache_state_selector).await
    }

    /// Lock this [`CacheStateLock`] by locking the full [`State`] with
    /// exclusive per-thread write access.
    ///
    /// This method locks the per-thread lock over the state, and then locks the
    /// cross-process lock over the store. It returns an RAII guard which will
    /// drop the write access to the state and to the store when dropped.
    ///
    /// If the cross-process lock over the store is dirty (see
    /// [`EventCacheStoreLockState`]), the state is reloaded.
    pub async fn write(&self) -> Result<StateLockWriteGuard<'_, Selector::Item>> {
        self.state_lock.write_cache_state(&self.cache_state_selector).await
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
