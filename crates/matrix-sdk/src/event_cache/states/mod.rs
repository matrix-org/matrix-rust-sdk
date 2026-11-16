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
    collections::{BTreeSet, HashMap},
    fmt,
    ops::{Deref, DerefMut},
    sync::Arc,
};

use itertools::Either;
use matrix_sdk_base::{
    deserialized_responses::ThreadSummary,
    event_cache::store::{EventCacheStoreLock, EventCacheStoreLockGuard, EventCacheStoreLockState},
    linked_chunk::{LinkedChunkId, OwnedLinkedChunkId, Update},
    serde_helpers::extract_thread_root,
    timer,
    tracing_timer::TracingTimer,
};
use ruma::{EventId, OwnedEventId, OwnedRoomId, RoomId, events::relation::RelationType};
use tokio::sync::{Mutex, RwLock, RwLockMappedWriteGuard, RwLockReadGuard, RwLockWriteGuard};
use tracing::{instrument, trace};

use super::{
    CachesByRoom, EventCacheError, EventsOrigin, Result,
    caches::{
        TimelineVectorDiffs,
        event_focused::{EventFocusedCacheKey, EventFocusedCacheState},
        event_linked_chunk::{EventPositions, sort_positions_descending},
        pinned_events::PinnedEventsCacheState,
        room::{self, RoomEventCacheState},
        thread::{self, ThreadEventCacheState},
    },
};

pub(in super::super) mod selectors;

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

    /// A lock taken to avoid multiple attempts to upgrade from a read lock to a
    /// write lock.
    ///
    /// Please see inline comment of [`Self::read`] to understand why it exists.
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
    /// This method locks the per-thread lock over the state, and then locks the
    /// cross-process lock over the store. It returns an RAII guard which will
    /// drop the read access to the state and to the store when dropped.
    ///
    /// If the cross-process lock over the store is dirty (see
    /// [`EventCacheStoreLockState`]), the state is reloaded.
    #[instrument(skip_all)]
    pub(super) async fn read<'state>(&'state self) -> Result<StateLockReadGuard<'state, State>> {
        trace!("Acquiring the lock");
        let tracing_timer = timer!("`read` lock");

        // Only one call at a time to `read` is allowed.
        //
        // Why? Because in case the cross-process lock over the store is dirty,
        // we need to upgrade the read lock over the state to a write lock.
        //
        // ## Upgradable read lock
        //
        // One may argue that this upgrades can be done with an
        // _upgradable read lock_ [^1] [^2]. We don't want to use this solution:
        // an upgradable read lock is basically a mutex because we are losing
        // the shared access property, i.e. having multiple read locks at the
        // same time. This is an important property to hold for performance
        // concerns.
        //
        // ## Downgradable write lock
        //
        // One may also argue we could first obtain a write lock over the state
        // from the beginning, thus removing the need to upgrade the read lock
        // to a write lock. The write lock is then downgraded to a read lock
        // once the dirty is cleaned up. It can potentially create a deadlock in
        // the following situation:
        //
        // - `read` is called once, it takes a write lock, then downgrades it to
        //   a read lock: the guard is kept alive somewhere,
        // - `read` is called again, and waits to obtain the write lock, which
        //   is impossible as long as the guard from the previous call is not
        //   dropped.
        //
        // ## “Atomic” read and write
        //
        // One may finally argue to first obtain a read lock over the state,
        // then drop it if the cross-process lock over the store is dirty, and
        // immediately obtain a write lock (which can later be downgraded to a
        // read lock). The problem is that this write lock is async: anything
        // can happen between the drop and the new lock acquisition, and it's
        // not possible to pause the runtime in the meantime.
        //
        // ## Semaphore with 1 permit, aka a mutex
        //
        // The chosen idea is to allow only one execution at a time of this
        // method: it becomes a critical section. That way we are free to
        // “upgrade” the read lock by dropping it and obtaining a new write
        // lock. All callers to this method are waiting, so nothing can happen
        // in the meantime.
        //
        // Note that it doesn't conflict with the `write` method because this
        // latter immediately obtains a write lock, which avoids any conflict
        // with this method.
        //
        // [^1]: https://docs.rs/lock_api/0.4.14/lock_api/struct.RwLock.html#method.upgradable_read
        // [^2]: https://docs.rs/async-lock/3.4.1/async_lock/struct.RwLock.html#method.upgradable_read
        let _state_lock_upgrade_guard = self.inner.state_lock_upgrade_mutex.lock().await;

        // Obtain a read lock.
        let state_guard = self.inner.locked_state.read().await;

        Ok(match state_guard.store.lock().await? {
            EventCacheStoreLockState::Clean(store_guard) => {
                trace!("Lock acquired (from clean)");

                StateLockReadGuard {
                    state: StateLockReadGuardKind::Owned(state_guard),
                    store: store_guard,
                    tracing_timer: Some(tracing_timer),
                }
            }
            EventCacheStoreLockState::Dirty(store_guard) => {
                // Drop the read lock, and take a write lock to modify the
                // state. This is safe because only one reader at a time (see
                // `Self::state_lock_upgrade_mutex`) is allowed.
                drop(state_guard);

                let mut guard = ReloadableStateLockWriteGuard {
                    state: self.inner.locked_state.write().await,
                    store: store_guard,
                    tracing_timer,
                };

                // Reload the state.
                guard.reload(ReloadPreprocessing::None, None).await?;

                // All good now, mark the cross-process lock as non-dirty.
                EventCacheStoreLockGuard::clear_dirty(&guard.store);

                trace!("Lock acquired (from dirty)");

                // Downgrade the write guard to a read guard, and map it into a
                // cache state.
                guard.downgrade()
            }
        })
    }

    /// Lock this [`StateLock`] with exclusive per-thread write access.
    ///
    /// This method locks the per-thread lock over the state, and then locks the
    /// cross-process lock over the store. It returns an RAII guard which will
    /// drop the write access to the state and to the store when dropped.
    ///
    /// If the cross-process lock over the store is dirty (see
    /// [`EventCacheStoreLockState`]), the state is reloaded automatically.
    #[instrument(skip_all)]
    async fn write<'state>(&'state self) -> Result<ReloadableStateLockWriteGuard<'state>> {
        trace!("Acquiring lock");
        let tracing_timer = timer!("`write` lock");

        let state_guard = self.inner.locked_state.write().await;

        Ok(match state_guard.store.lock().await? {
            EventCacheStoreLockState::Clean(store_guard) => {
                trace!("Lock acquired (from clean)");

                ReloadableStateLockWriteGuard {
                    state: state_guard,
                    store: store_guard,
                    tracing_timer,
                }
            }
            EventCacheStoreLockState::Dirty(store_guard) => {
                let mut guard = ReloadableStateLockWriteGuard {
                    state: state_guard,
                    store: store_guard,
                    tracing_timer,
                };

                // Reload the state.
                guard.reload(ReloadPreprocessing::None, None).await?;

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

        let state_guard = self.inner.locked_state.write().await;

        let mut guard = match state_guard.store.lock().await? {
            EventCacheStoreLockState::Clean(store_guard)
            | EventCacheStoreLockState::Dirty(store_guard) => ReloadableStateLockWriteGuard {
                state: state_guard,
                store: store_guard,
                tracing_timer,
            },
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

    /// Remove the events with the given IDs of a room from all the caches, in
    /// memory and in the store.
    ///
    /// The events are removed from the room, thread and pinned events linked
    /// chunks, whether they are loaded in memory or not, and from the
    /// event-focused caches. Then, they are deleted from the store, including
    /// those saved out-of-band.
    ///
    /// The subscribers of the loaded caches are notified, and the summaries of
    /// the threads the events were part of are updated.
    #[instrument(skip(self, event_ids))]
    pub(super) async fn remove_events(
        &self,
        room_id: &RoomId,
        event_ids: Vec<OwnedEventId>,
    ) -> Result<()> {
        if event_ids.is_empty() {
            return Ok(());
        }

        let mut guard = self.write().await?;
        let store = guard.store.clone();
        let generic_update = || room::RoomEventCacheGenericUpdate { room_id: room_id.to_owned() };

        // Find the threads the events are part of, while they are still in the
        // store.
        let threads_of_events = store
            .find_events(room_id, event_ids.clone())
            .await?
            .iter()
            .filter_map(|event| extract_thread_root(event.raw()))
            .collect::<BTreeSet<_>>();

        // Find the positions of the events in all the linked chunks they may be
        // part of: the room, the pinned events, the threads of the events, and
        // the threads of the events which are thread roots, since a thread root
        // is part of its own thread.
        let linked_chunk_ids = [
            OwnedLinkedChunkId::Room(room_id.to_owned()),
            OwnedLinkedChunkId::PinnedEvents(room_id.to_owned()),
        ]
        .into_iter()
        .chain(
            threads_of_events
                .iter()
                .chain(&event_ids)
                .collect::<BTreeSet<_>>()
                .into_iter()
                .map(|thread_id| OwnedLinkedChunkId::Thread(room_id.to_owned(), thread_id.clone())),
        )
        .collect();

        let mut positions_by_linked_chunk = HashMap::<_, EventPositions>::new();

        for (linked_chunk_id, event_id, position) in
            store.find_event_positions(linked_chunk_ids, event_ids.clone()).await?
        {
            positions_by_linked_chunk
                .entry(linked_chunk_id)
                .or_default()
                .push((event_id, position));
        }

        let mut no_state_for_room = StateForRoom::default();
        let StateForRoom { room, threads, pinned_events, event_focused } =
            guard.state.by_room.get_mut(room_id).unwrap_or(&mut no_state_for_room);

        // The threads which some events have been removed from, or which some
        // events were part of: their summary must be updated.
        let mut affected_thread_ids = threads_of_events;

        for (linked_chunk_id, positions) in positions_by_linked_chunk {
            match linked_chunk_id {
                OwnedLinkedChunkId::Room(_) => {
                    if let Some(room_state) = room {
                        let mut room_state = StateLockWriteGuard {
                            state: StateLockWriteGuardKind::Reference(room_state),
                            store: store.clone(),
                            _tracing_timer: None,
                        };

                        let diffs = room_state.remove_events_at(positions).await?;

                        if !diffs.is_empty() {
                            room_state.update_sender.send(
                                room::RoomEventCacheUpdate::UpdateTimelineEvents(
                                    TimelineVectorDiffs { diffs, origin: EventsOrigin::Cache },
                                ),
                                Some(generic_update()),
                            );
                        }
                    } else {
                        remove_events_from_linked_chunk_in_store(
                            &store,
                            linked_chunk_id.as_ref(),
                            positions,
                        )
                        .await?;
                    }
                }

                OwnedLinkedChunkId::Thread(_, ref thread_id) => {
                    affected_thread_ids.insert(thread_id.clone());

                    if let Some(thread_state) = threads.get_mut(thread_id) {
                        let mut thread_state = StateLockWriteGuard {
                            state: StateLockWriteGuardKind::Reference(thread_state),
                            store: store.clone(),
                            _tracing_timer: None,
                        };

                        let diffs = thread_state.remove_events_at(positions).await?;

                        if !diffs.is_empty() {
                            thread_state.update_sender.send(
                                thread::ThreadEventCacheUpdate::UpdateTimelineEvents(
                                    TimelineVectorDiffs { diffs, origin: EventsOrigin::Cache },
                                ),
                                Some(generic_update()),
                            );
                        }
                    } else {
                        remove_events_from_linked_chunk_in_store(
                            &store,
                            linked_chunk_id.as_ref(),
                            positions,
                        )
                        .await?;
                    }
                }

                OwnedLinkedChunkId::PinnedEvents(_) => {
                    if let Some(pinned_events_state) = pinned_events {
                        let mut pinned_events_state = StateLockWriteGuard {
                            state: StateLockWriteGuardKind::Reference(pinned_events_state),
                            store: store.clone(),
                            _tracing_timer: None,
                        };

                        let diffs = pinned_events_state.remove_events_at(positions).await?;

                        if !diffs.is_empty() {
                            pinned_events_state
                                .update_sender
                                .send(TimelineVectorDiffs { diffs, origin: EventsOrigin::Cache });
                        }
                    } else {
                        remove_events_from_linked_chunk_in_store(
                            &store,
                            linked_chunk_id.as_ref(),
                            positions,
                        )
                        .await?;
                    }
                }

                // Event-focused caches aren't persisted, they are handled below.
                OwnedLinkedChunkId::EventFocused(..) => {}
            }
        }

        // Event-focused, which only live in memory.
        for event_focused_state in event_focused.values_mut() {
            let diffs = event_focused_state.remove_events_by_id(&event_ids);

            if !diffs.is_empty() {
                let _ = event_focused_state
                    .update_sender
                    .send(TimelineVectorDiffs { diffs, origin: EventsOrigin::Cache });
            }
        }

        // The events aren't part of any linked chunk anymore: delete them.
        store.delete_out_of_band_events(room_id, event_ids.clone()).await?;

        // Update the summaries of the threads, now that the events are gone.
        let removed_event_ids = event_ids.into_iter().collect::<BTreeSet<_>>();

        for thread_id in &affected_thread_ids {
            let thread_summary = if let Some(thread_state) = threads.get_mut(thread_id) {
                let mut thread_state = StateLockWriteGuard {
                    state: StateLockWriteGuardKind::Reference(thread_state),
                    store: store.clone(),
                    _tracing_timer: None,
                };

                let thread_summary = thread_state.update_thread_summary().await?;
                thread_state.update_sender.send(
                    thread::ThreadEventCacheUpdate::UpdateSummary(thread_summary.clone()),
                    None,
                );

                thread_summary
            } else if let Some(thread_summary) =
                update_thread_info_in_store(&store, room_id, thread_id, &removed_event_ids).await?
            {
                thread_summary
            } else {
                continue;
            };

            if let Some(room_state) = room {
                room_state.update_sender.send(
                    room::RoomEventCacheUpdate::UpdateThreadSummary {
                        thread_root: thread_id.clone(),
                        thread_summary,
                    },
                    Some(generic_update()),
                );
            }
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

/// Remove the events at the given positions from a linked chunk which isn't
/// loaded in memory, i.e. only from the store.
async fn remove_events_from_linked_chunk_in_store(
    store: &EventCacheStoreLockGuard,
    linked_chunk_id: LinkedChunkId<'_>,
    positions: EventPositions,
) -> Result<()> {
    let mut positions =
        positions.into_iter().map(|(_event_id, position)| position).collect::<Vec<_>>();

    if positions.is_empty() {
        return Ok(());
    }

    // Remove the events from the last one, so the positions of the next ones
    // to remove don't change.
    sort_positions_descending(&mut positions);

    store
        .handle_linked_chunk_updates(
            linked_chunk_id,
            positions.into_iter().map(|at| Update::RemoveItem { at }).collect(),
        )
        .await?;

    Ok(())
}

/// Update the [`ThreadInfo`] in the store of a thread which isn't loaded in
/// memory, after some events have been removed, and return the new
/// [`ThreadSummary`] if it has changed.
///
/// Returns `None` if the thread doesn't exist, or if its summary is unchanged.
async fn update_thread_info_in_store(
    store: &EventCacheStoreLockGuard,
    room_id: &RoomId,
    thread_id: &EventId,
    removed_event_ids: &BTreeSet<OwnedEventId>,
) -> Result<Option<ThreadSummary>> {
    let Some(mut thread_info) = store.load_thread_info(room_id, thread_id, false).await? else {
        return Ok(None);
    };

    let replies =
        store.find_event_relations(room_id, thread_id, Some(&[RelationType::Thread])).await?;

    let number_of_replies = u32::try_from(replies.len()).unwrap_or(u32::MAX);
    let latest_event = if thread_info
        .latest_event
        .as_ref()
        .is_some_and(|latest_event| removed_event_ids.contains(latest_event))
    {
        // The latest event has been removed, use the most recent remaining
        // reply instead.
        replies
            .iter()
            .map(|(event, _position)| event)
            .max_by_key(|event| event.timestamp())
            .and_then(|event| event.event_id().map(ToOwned::to_owned))
    } else {
        thread_info.latest_event.clone()
    };

    if number_of_replies == thread_info.number_of_replies
        && latest_event == thread_info.latest_event
    {
        return Ok(None);
    }

    thread_info.number_of_replies = number_of_replies;
    thread_info.latest_event = latest_event;
    store.update_thread_info(room_id, thread_id, &thread_info).await?;

    Ok(Some(ThreadSummary::new(thread_info.latest_event, thread_info.number_of_replies)))
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

    /// The read lock over the state `S` is acquired, and this is a mapped guard
    /// to a cache (sub-)state.
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
        })
    }

    /// Synchronously downgrades a write lock into a read lock.
    ///
    /// The per-thread/state lock is downgraded atomically, without allowing any
    /// writers to take exclusive access of the lock in the meantime.
    ///
    /// It returns an RAII guard which will drop the read access to the state
    /// and to the store when dropped.
    fn downgrade(self) -> StateLockReadGuard<'state, State> {
        StateLockReadGuard {
            state: StateLockReadGuardKind::Owned(self.state.downgrade()),
            store: self.store,
            tracing_timer: Some(self.tracing_timer),
        }
    }

    async fn reload(
        &mut self,
        preprocessing: ReloadPreprocessing,
        room_id: Option<&RoomId>,
    ) -> Result<()> {
        trace!("Reloading the state");

        let rooms = if let Some(room_id) = room_id {
            Either::Left(
                self.state.by_room.get_mut(room_id).into_iter().map(move |state| (room_id, state)),
            )
        } else {
            Either::Right(
                self.state.by_room.iter_mut().map(|(event_id, state)| (event_id.as_ref(), state)),
            )
        };

        for (room_id, StateForRoom { room, threads, pinned_events, event_focused }) in rooms {
            // Room.
            if let Some(room_state) = room {
                let mut room_state = StateLockWriteGuard {
                    state: StateLockWriteGuardKind::Reference(room_state),
                    store: self.store.clone(),
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
    /// This method locks the per-thread lock over the state, and then locks the
    /// cross-process lock over the store. It returns an RAII guard which will
    /// drop the read access to the state and to the store when dropped.
    ///
    /// If the cross-process lock over the store is dirty (see
    /// [`EventCacheStoreLockState`]), the state is reloaded.
    pub async fn read(&self) -> Result<StateLockReadGuard<'_, Selector::Item>> {
        self.state_lock.read().await?.try_map_into_cache_state(&self.cache_state_selector)
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
