// Copyright 2025 The Matrix.org Foundation C.I.C.
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

use std::{collections::HashMap, ops::ControlFlow, sync::Arc};

use matrix_sdk_base::RoomInfoNotableUpdateReasons;
use ruma::{EventId, OwnedEventId, UserId, events::room::power_levels::RoomPowerLevels};
use tokio::sync::{OnceCell, OwnedRwLockReadGuard, OwnedRwLockWriteGuard, RwLock};
use tracing::{debug, error, instrument, warn};

use super::{
    LatestEvent, filter_timeline_event,
    latest_event::{IsLatestEventValueNone, NeedMoreEvents, With},
};
use crate::{
    Room,
    event_cache::{
        BackPaginationOutcome, EventCache, EventCacheError, RoomEventCache,
        back_pagination_queue::{self, BackPaginationRequest},
    },
    room::WeakRoom,
    send_queue::RoomSendQueueUpdate,
};

/// Type holding the [`LatestEvent`] for a room and for all its threads.
#[derive(Debug)]
pub(super) struct RoomLatestEvents {
    /// The state of this type.
    state: Arc<RwLock<RoomLatestEventsState>>,
}

impl RoomLatestEvents {
    /// Create a new [`RoomLatestEvents`].
    pub fn new(
        weak_room: WeakRoom,
        event_cache: &EventCache,
    ) -> With<Self, IsLatestEventValueNone> {
        let latest_event_with = Self::create_latest_event(&weak_room, None);

        With::map(latest_event_with, |for_the_room| Self {
            state: Arc::new(RwLock::new(RoomLatestEventsState {
                for_the_room,
                per_thread: HashMap::new(),
                weak_room,
                event_cache: event_cache.clone(),
                room_event_cache: OnceCell::new(),
            })),
        })
    }

    fn create_latest_event(
        weak_room: &WeakRoom,
        thread_id: Option<&EventId>,
    ) -> With<LatestEvent, IsLatestEventValueNone> {
        LatestEvent::new(weak_room, thread_id)
    }

    /// Lock this type with shared read access, and return an owned lock guard.
    pub async fn read(&self) -> RoomLatestEventsReadGuard {
        RoomLatestEventsReadGuard { inner: self.state.clone().read_owned().await }
    }

    /// Lock this type with exclusive write access, and return an owned lock
    /// guard.
    pub async fn write(&self) -> RoomLatestEventsWriteGuard {
        RoomLatestEventsWriteGuard { inner: self.state.clone().write_owned().await }
    }
}

/// The state of [`RoomLatestEvents`].
#[derive(Debug)]
struct RoomLatestEventsState {
    /// The latest event of the room.
    for_the_room: LatestEvent,

    /// The latest events for each thread.
    per_thread: HashMap<OwnedEventId, LatestEvent>,

    /// The event cache.
    event_cache: EventCache,

    /// The room event cache (lazily-loaded).
    room_event_cache: OnceCell<RoomEventCache>,

    /// The (weak) room.
    ///
    /// It used to to get the power-levels of the user for this room when
    /// computing the latest events.
    weak_room: WeakRoom,
}

/// The owned lock guard returned by [`RoomLatestEvents::read`].
pub(super) struct RoomLatestEventsReadGuard {
    inner: OwnedRwLockReadGuard<RoomLatestEventsState>,
}

impl RoomLatestEventsReadGuard {
    /// Get the [`LatestEvent`] for the room.
    pub fn for_room(&self) -> &LatestEvent {
        &self.inner.for_the_room
    }

    /// Get the [`LatestEvent`] for the thread if it exists.
    pub fn for_thread(&self, thread_id: &EventId) -> Option<&LatestEvent> {
        self.inner.per_thread.get(thread_id)
    }

    #[cfg(test)]
    pub fn per_thread(&self) -> &HashMap<OwnedEventId, LatestEvent> {
        &self.inner.per_thread
    }
}

/// The owned lock guard returned by [`RoomLatestEvents::write`].
pub(super) struct RoomLatestEventsWriteGuard {
    inner: OwnedRwLockWriteGuard<RoomLatestEventsState>,
}

impl RoomLatestEventsWriteGuard {
    /// Check whether this [`RoomLatestEvents`] has a latest event for a
    /// particular thread.
    pub fn has_thread(&self, thread_id: &EventId) -> bool {
        self.inner.per_thread.contains_key(thread_id)
    }

    /// Create the [`LatestEvent`] for thread `thread_id` and insert it in this
    /// [`RoomLatestEvents`].
    pub fn create_and_insert_latest_event_for_thread(&mut self, thread_id: &EventId) {
        let latest_event_with =
            RoomLatestEvents::create_latest_event(&self.inner.weak_room, Some(thread_id));

        self.inner.per_thread.insert(thread_id.to_owned(), With::inner(latest_event_with));
    }

    /// Forget the thread `thread_id`.
    pub fn forget_thread(&mut self, thread_id: &EventId) {
        self.inner.per_thread.remove(thread_id);
    }

    /// Update the latest events for the room and its threads, based on the
    /// event cache data.
    pub async fn update_with_event_cache(&mut self) {
        // Get the power levels of the user for the current room if the `WeakRoom` is
        // still valid.
        //
        // Get it once for all the updates of all the latest events for this room (be
        // the room and its threads).
        let Some(room) = self.inner.weak_room.get() else {
            // No room? Let's stop the update.
            error!(room = ?self.inner.weak_room, "Room is unknown");

            return;
        };
        let own_user_id = room.own_user_id();
        let power_levels = room.power_levels().await.ok();

        let inner = &mut *self.inner;
        let for_the_room = &mut inner.for_the_room;
        let per_thread = &mut inner.per_thread;

        // Lazy-load the `RoomEventCache`.
        let room_event_cache = match inner
            .room_event_cache
            .get_or_try_init(|| async {
                // It's fine to drop the `EventCacheDropHandles` here as the caller
                // (`LatestEventState`) owns a clone of the `EventCache`.
                let (room_event_cache, _drop_handles) =
                    inner.event_cache.room(room.room_id()).await?;

                Ok::<RoomEventCache, EventCacheError>(room_event_cache)
            })
            .await
        {
            Ok(room_event_cache) => room_event_cache,
            Err(err) => {
                error!(room_id = ?room.room_id(), ?err, "Failed to fetch the `RoomEventCache`");
                return;
            }
        };

        // The room is left without a latest event so back-paginate its history
        // until a suitable event surfaces, then recompute.
        if matches!(
            for_the_room
                .update_with_event_cache(room_event_cache, own_user_id, power_levels.as_ref())
                .await,
            NeedMoreEvents::Yes
        ) && room.client().event_cache().back_pagination_queue().is_some()
        {
            Self::back_paginate_for_candidate(&room, own_user_id, power_levels.as_ref()).await;

            for_the_room
                .update_with_event_cache(room_event_cache, own_user_id, power_levels.as_ref())
                .await;
        }

        for latest_event in per_thread.values_mut() {
            latest_event
                .update_with_event_cache(room_event_cache, own_user_id, power_levels.as_ref())
                .await;
        }
    }

    /// Update the latest events for the room and its threads, based on the
    /// send queue update.
    pub async fn update_with_send_queue(&mut self, send_queue_update: &RoomSendQueueUpdate) {
        // Get the power levels of the user for the current room if the `WeakRoom` is
        // still valid.
        //
        // Get it once for all the updates of all the latest events for this room (be
        // the room and its threads).
        let Some(room) = self.inner.weak_room.get() else {
            // No room? Let's stop the update.
            return;
        };
        let own_user_id = room.own_user_id();
        let power_levels = room.power_levels().await.ok();

        let inner = &mut *self.inner;
        let for_the_room = &mut inner.for_the_room;
        let per_thread = &mut inner.per_thread;

        // Lazy-load the `RoomEventCache`.
        let room_event_cache = match inner
            .room_event_cache
            .get_or_try_init(|| async {
                // It's fine to drop the `EventCacheDropHandles` here as the caller
                // (`LatestEventState`) owns a clone of the `EventCache`.
                let (room_event_cache, _drop_handles) =
                    inner.event_cache.room(room.room_id()).await?;

                Ok::<RoomEventCache, EventCacheError>(room_event_cache)
            })
            .await
        {
            Ok(room_event_cache) => room_event_cache,
            Err(err) => {
                error!(room_id = ?room.room_id(), ?err, "Failed to fetch the `RoomEventCache`");
                return;
            }
        };

        for_the_room
            .update_with_send_queue(
                send_queue_update,
                room_event_cache,
                own_user_id,
                power_levels.as_ref(),
            )
            .await;

        for latest_event in per_thread.values_mut() {
            latest_event
                .update_with_send_queue(
                    send_queue_update,
                    room_event_cache,
                    own_user_id,
                    power_levels.as_ref(),
                )
                .await;
        }
    }

    /// Update the latest events for the room and its threads, based on the room
    /// info.
    pub async fn update_with_room_info(&mut self, reasons: RoomInfoNotableUpdateReasons) {
        // Get the state of the current room if the `WeakRoom` is still valid.
        let Some(room) = self.inner.weak_room.get() else {
            // No room? Let's stop the update.
            return;
        };

        self.inner.for_the_room.update_with_room_info(room, reasons).await;
    }

    /// Back-paginate the room until a suitable latest-event candidate is loaded
    /// into memory or the start of the timeline is reached.
    ///
    /// Enqueues a high-priority request on the shared [`BackPaginationQueue`],
    /// with a stop predicate that fires as soon as a freshly loaded batch
    /// contains a suitable latest-event candidate, and awaits it. No-ops if
    /// automatic backpagination is disabled.
    ///
    /// [`BackPaginationQueue`]: crate::event_cache::BackPaginationQueue
    #[instrument(skip_all, fields(room_id = %room.room_id()))]
    async fn back_paginate_for_candidate(
        room: &Room,
        own_user_id: &UserId,
        power_levels: Option<&RoomPowerLevels>,
    ) {
        let Some(queue) = room.client().event_cache().back_pagination_queue() else {
            return;
        };

        let own_user_id = own_user_id.to_owned();
        let power_levels = power_levels.cloned();

        // This filters each batch to spot a candidate and `Builder::new_remote`
        // filters the same events again when it computes the value afterwards. That
        // second pass can't be skipped though as an event's edits are newer than it
        // and a stop condition only ever sees the batch it just loaded.
        let stop = move |outcome: &BackPaginationOutcome| {
            let found = outcome.events.iter().any(|event| {
                filter_timeline_event(event, None, &own_user_id, power_levels.as_ref()).is_break()
            });

            if found { ControlFlow::Break(()) } else { ControlFlow::Continue(()) }
        };

        debug!("started backfill request for latest events");

        let handle = match queue.enqueue(BackPaginationRequest {
            room_id: room.room_id().to_owned(),
            priority: back_pagination_queue::Priority::High,
            stop: Box::new(stop),
            batch_size: back_pagination_queue::BATCH_SIZE,
            max_batches: None,
        }) {
            Ok(handle) => handle,
            Err(err) => {
                warn!("couldn't enqueue a latest-event backfill request: {err}");
                return;
            }
        };

        handle.join().await;

        debug!("finished backfill request for latest events");
    }
}

#[cfg(all(test, not(target_family = "wasm")))]
mod tests {
    use assert_matches::assert_matches;
    use matrix_sdk_base::{
        RoomState,
        event_cache::Gap,
        linked_chunk::{ChunkIdentifier, LinkedChunkId, Update},
    };
    use matrix_sdk_test::{async_test, event_factory::EventFactory};
    use ruma::{event_id, room_id, user_id};

    use super::RoomLatestEvents;
    use crate::{
        client::WeakClient,
        latest_events::LatestEventValue,
        room::WeakRoom,
        test_utils::mocks::{MatrixMockServer, RoomMessagesResponseTemplate},
    };

    /// When no latest-event candidate is present in memory, but one exists
    /// behind a gap, `update_with_event_cache` back-paginates to surface it.
    #[async_test]
    async fn test_update_with_event_cache_backfills_for_a_candidate() {
        let room_id = room_id!("!r0");
        let sender = user_id!("@bob:example.org");

        let server = MatrixMockServer::new().await;
        let client = server
            .client_builder()
            .on_builder(|builder| builder.with_enable_automatic_back_pagination(true))
            .build()
            .await;

        client.base_client().get_or_create_room(room_id, RoomState::Joined);

        // A linked chunk with a single gap: no events in memory (so no candidate),
        // but a token to paginate from. Set up directly so no sync (and thus no
        // competing read-receipt pagination) races the backfill.
        client
            .event_cache_store()
            .lock()
            .await
            .unwrap()
            .as_clean()
            .unwrap()
            .handle_linked_chunk_updates(
                LinkedChunkId::Room(room_id),
                vec![Update::NewGapChunk {
                    previous: None,
                    new: ChunkIdentifier::new(0),
                    next: None,
                    gap: Gap { token: "prev_batch".to_owned() },
                }],
            )
            .await
            .unwrap();

        let event_cache = client.event_cache();
        event_cache.subscribe().unwrap();

        // A displayable message lives behind the gap.
        let f = EventFactory::new().room(room_id).sender(sender);
        server
            .mock_room_messages()
            .match_from("prev_batch")
            .ok(RoomMessagesResponseTemplate::default()
                .events(vec![f.text_msg("hello").event_id(event_id!("$1"))]))
            .mock_once()
            .mount()
            .await;

        let weak_room = WeakRoom::new(WeakClient::from_client(&client), room_id.to_owned());
        let room_latest_events = RoomLatestEvents::new(weak_room, event_cache);

        // No candidate in memory yet.
        assert_matches!(
            room_latest_events.read().await.for_room().get().await,
            LatestEventValue::None
        );

        room_latest_events.write().await.update_with_event_cache().await;

        // The backfill surfaced the message; the latest event resolved to it.
        assert_matches!(
            room_latest_events.read().await.for_room().get().await,
            LatestEventValue::Remote(_)
        );
    }
}
