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

use std::{collections::HashMap, ops::Deref, sync::Arc};

use eyeball::SharedObservable;
use eyeball_im::VectorDiff;
use matrix_sdk_base::{
    ThreadingSupport,
    event_cache::{Event, store::EventCacheStoreLock},
    linked_chunk::Position,
    sync::{JoinedRoomUpdate, LeftRoomUpdate},
};
use ruma::{OwnedEventId, OwnedRoomId, RoomId};
use tokio::sync::{OwnedRwLockReadGuard, OwnedRwLockWriteGuard, RwLock, broadcast::Sender, mpsc};

use super::{EventCacheError, EventsOrigin, Result, automatic_pagination::AutomaticPagination};
use crate::{client::WeakClient, room::WeakRoom};

mod aggregator;
pub mod event_focused;
pub mod event_linked_chunk;
pub(super) mod lock;
pub mod pagination;
pub mod pinned_events;
mod read_receipts;
pub mod room;
pub mod thread;

/// A type to hold all the caches for a given room.
#[derive(Debug)]
pub(super) struct Caches {
    pub room: room::RoomEventCache,
    pub threads: Arc<RwLock<HashMap<OwnedEventId, thread::ThreadEventCache>>>,
    internals: CachesInternals,
}

#[derive(Debug)]
struct CachesInternals {
    store: EventCacheStoreLock,
    linked_chunk_update_sender: Sender<room::RoomEventCacheLinkedChunkUpdate>,
}

impl Caches {
    /// Create a new [`Caches`].
    pub async fn new(
        weak_client: &WeakClient,
        room_id: &RoomId,
        generic_update_sender: Sender<room::RoomEventCacheGenericUpdate>,
        linked_chunk_update_sender: Sender<room::RoomEventCacheLinkedChunkUpdate>,
        auto_shrink_sender: mpsc::Sender<OwnedRoomId>,
        store: EventCacheStoreLock,
        automatic_pagination: Option<AutomaticPagination>,
    ) -> Result<Self> {
        let Some(client) = weak_client.get() else {
            return Err(EventCacheError::ClientDropped);
        };

        let weak_room = WeakRoom::new(weak_client.clone(), room_id.to_owned());

        let room = client
            .get_room(room_id)
            .ok_or_else(|| EventCacheError::RoomNotFound { room_id: room_id.to_owned() })?;
        let room_version_rules = room.clone_info().room_version_rules_or_default();

        let pagination_status = SharedObservable::new(pagination::SharedPaginationStatus::Idle {
            hit_timeline_start: false,
        });

        let enabled_thread_support =
            matches!(client.base_client().threading_support, ThreadingSupport::Enabled { .. });

        let update_sender = room::RoomEventCacheUpdateSender::new(generic_update_sender.clone());

        let own_user_id =
            client.user_id().expect("the user must be logged in, at this point").to_owned();

        let room_state = room::LockedRoomEventCacheState::new(
            own_user_id.clone(),
            room_id.to_owned(),
            weak_room.clone(),
            room_version_rules,
            enabled_thread_support,
            update_sender.clone(),
            linked_chunk_update_sender.clone(),
            store.clone(),
            pagination_status.clone(),
            automatic_pagination,
        )
        .await?;

        let timeline_is_not_empty =
            room_state.read().await?.room_linked_chunk().revents().next().is_some();

        let room_event_cache = room::RoomEventCache::new(
            room_id.to_owned(),
            weak_room,
            own_user_id,
            room_state,
            pagination_status,
            auto_shrink_sender,
            update_sender,
        );

        // If at least one event has been loaded, it means there is a timeline. Let's
        // emit a generic update.
        if timeline_is_not_empty {
            let _ = generic_update_sender
                .send(room::RoomEventCacheGenericUpdate { room_id: room_id.to_owned() });
        }

        Ok(Self {
            room: room_event_cache,
            threads: Arc::new(RwLock::new(HashMap::new())),
            internals: CachesInternals { store, linked_chunk_update_sender },
        })
    }

    /// Get the [`RoomEventCache`].
    ///
    /// [`RoomEventCache`]: room::RoomEventCache
    pub async fn room(&self) -> &room::RoomEventCache {
        &self.room
    }

    /// Get or create a [`ThreadEventCache`].
    ///
    /// Note: it is impossible to know if `thread_id` represents a valid thread
    /// identifier. It means it's possible to create a [`ThreadEventCache`] for
    /// an event that is not a thread root.
    ///
    /// [`ThreadEventCache`]: thread::ThreadEventCache
    pub async fn thread(
        &self,
        thread_id: OwnedEventId,
    ) -> Result<
        OwnedRwLockReadGuard<
            HashMap<OwnedEventId, thread::ThreadEventCache>,
            thread::ThreadEventCache,
        >,
    > {
        Ok(
            match OwnedRwLockWriteGuard::try_downgrade_map(
                self.threads.clone().write_owned().await,
                |threads| threads.get(&thread_id),
            ) {
                // Thread exists.
                Ok(locked_cache) => locked_cache,
                // Thread does not exist, let's create it.
                Err(mut threads) => {
                    let room = &self.room;
                    let cache = thread::ThreadEventCache::new(
                        room.room_id().to_owned(),
                        thread_id.clone(),
                        room.own_user_id().to_owned(),
                        room.weak_room().to_owned(),
                        self.internals.store.clone(),
                        room.update_sender().generic_update_sender().clone(),
                        self.internals.linked_chunk_update_sender.clone(),
                    )
                    .await?;

                    threads.insert(thread_id.clone(), cache);

                    OwnedRwLockWriteGuard::downgrade_map(threads, |threads| {
                        threads.get(&thread_id).unwrap()
                    })
                }
            },
        )
    }

    /// Update all the event caches with a [`JoinedRoomUpdate`].
    pub(super) async fn handle_joined_room_update(&self, updates: JoinedRoomUpdate) -> Result<()> {
        let Self { room, threads, internals: _ } = &self;

        // Room.
        {
            let mut updates = updates.clone();
            updates.timeline = aggregator::aggregate_timeline_for_room(updates.timeline);

            room.handle_joined_room_update(updates).await?;
        }

        // Threads.
        {
            let mut updates = updates.clone();
            updates.account_data.clear();
            updates.ambiguity_changes.clear();

            let timeline_for_threads = aggregator::aggregate_timeline_for_threads(
                &updates.timeline,
                threads.read().await.deref(),
                room.state().read().await?,
            )
            .await?;

            for (thread_id, timeline) in timeline_for_threads {
                let mut updates = updates.clone();
                updates.timeline = timeline;

                let thread = self.thread(thread_id).await?;
                thread.handle_joined_room_update(updates).await?;

                if let Some(thread_summary) =
                    thread.state().read().await?.compute_thread_summary().await?
                {
                    room.update_thread_summary(thread.thread_id(), thread_summary).await?;
                }
            }
        }

        Ok(())
    }

    /// Update all the event caches with a [`LeftRoomUpdate`].
    pub(super) async fn handle_left_room_update(&self, updates: LeftRoomUpdate) -> Result<()> {
        let Self { room, threads, internals: _ } = &self;

        // Room.
        {
            let mut updates = updates.clone();
            updates.timeline = aggregator::aggregate_timeline_for_room(updates.timeline);

            room.handle_left_room_update(updates).await?;
        }

        // Threads.
        {
            let mut updates = updates.clone();
            updates.account_data.clear();
            updates.ambiguity_changes.clear();

            let timeline_for_threads = aggregator::aggregate_timeline_for_threads(
                &updates.timeline,
                threads.read().await.deref(),
                room.state().read().await?,
            )
            .await?;

            for (thread_id, timeline) in timeline_for_threads {
                let mut updates = updates.clone();
                updates.timeline = timeline;

                let thread = self.thread(thread_id).await?;
                thread.handle_left_room_update(updates).await?;

                if let Some(thread_summary) =
                    thread.state().read().await?.compute_thread_summary().await?
                {
                    room.update_thread_summary(thread.thread_id(), thread_summary).await?;
                }
            }
        }

        Ok(())
    }

    /// Try to acquire exclusive locks over all the event caches managed by
    /// this [`Caches`], in order to reset all the in-memory data.
    ///
    /// Note that this method takes `&mut self`, ensuring only one reset can
    /// happen at a time.
    ///
    /// If the returned value is dropped, no data will be reset.
    pub async fn prepare_to_reset(&mut self) -> Result<ResetCaches<'_>> {
        ResetCaches::new(self).await
    }

    /// Get all events from all the event caches manged by this [`Cacches`].
    ///
    /// Events can be duplicated if present in different event caches.
    #[cfg(feature = "e2e-encryption")]
    pub async fn all_events(&self) -> Result<impl Iterator<Item = Event>> {
        let events_from_room = self.room.events().await?;

        Ok(events_from_room.into_iter())
    }
}

/// Type holding exclusive locks over all event caches managed by a
/// [`Caches`].
///
/// To reset all the event caches, call [`ResetCaches::reset_all`]. If this type
/// is dropped, no reset happens and the exclusive lock is released.
pub(super) struct ResetCaches<'c> {
    room_lock: (room::RoomEventCacheStateLockWriteGuard<'c>, room::RoomEventCacheUpdateSender),
    threads_lock: OwnedRwLockWriteGuard<HashMap<OwnedEventId, thread::ThreadEventCache>>,
    thread_locks: Vec<(
        thread::OwnedThreadEventCacheStateLockWriteGuard,
        thread::ThreadEventCacheUpdateSender,
    )>,
}

impl<'c> ResetCaches<'c> {
    /// Create a new [`ResetCaches`].
    ///
    /// It can fail if acquiring an exclusive lock fails.
    async fn new(Caches { room, threads, internals: _ }: &'c mut Caches) -> Result<Self> {
        // Acquire an exclusive access to the state of the room.
        let room_lock = (room.state().write().await?, room.update_sender().clone());

        // Acquire an exclusive access to the threads.
        // Then, for each thread, acquire an exclusive access to its state.
        let threads_lock = threads.clone().write_owned().await;
        let mut thread_locks = Vec::new();

        for thread in threads_lock.values() {
            thread_locks
                .push((thread.state().write_owned().await?, thread.update_sender().clone()));
        }

        Ok(Self { room_lock, threads_lock, thread_locks })
    }

    /// Reset all the event caches, and broadcast the [`TimelineVectorDiffs`].
    ///
    /// Note that this method consumes `self`, ensuring the acquired exclusive
    /// locks over the event caches are released.
    ///
    /// It can fail if resetting an event cache fails.
    pub async fn reset_all(self) -> Result<()> {
        let Self { room_lock, threads_lock, thread_locks } = self;

        {
            let (mut room_state, room_update_sender) = room_lock;

            let updates_as_vector_diffs = room_state.reset().await?;
            room_update_sender.send(
                room::RoomEventCacheUpdate::UpdateTimelineEvents(TimelineVectorDiffs {
                    diffs: updates_as_vector_diffs,
                    origin: EventsOrigin::Cache,
                }),
                Some(room::RoomEventCacheGenericUpdate { room_id: room_state.room_id.clone() }),
            );
        }

        {
            for thread_lock in thread_locks {
                let (mut thread_state, thread_update_sender) = thread_lock;

                let updates_as_vector_diffs = thread_state.reset().await?;
                thread_update_sender.send(
                    TimelineVectorDiffs {
                        diffs: updates_as_vector_diffs,
                        origin: EventsOrigin::Cache,
                    },
                    // This function is part of the `RoomEventCache` flow. The generic update is
                    // handled by it.
                    None,
                );
            }

            // Now we can release the exclusive acces over the threads.
            drop(threads_lock);
        }

        Ok(())
    }
}

/// A diff update for an event cache timeline represented as a vector.
#[derive(Clone, Debug)]
pub struct TimelineVectorDiffs {
    /// New vector diff for the thread timeline.
    pub diffs: Vec<VectorDiff<Event>>,
    /// The origin that triggered this update.
    pub origin: EventsOrigin,
}

/// An enum representing where an event has been found.
pub(super) enum EventLocation {
    /// Event lives in memory (and likely in the store!).
    Memory(Position),

    /// Event lives in the store only, it has not been loaded in memory yet.
    Store,
}
