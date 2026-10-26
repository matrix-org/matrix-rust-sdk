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
        // Get the power levels of the user for the current room if the
        // `WeakRoom` is still valid.
        //
        // Get it once for all the updates of all the latest events for this
        // room (be the room and its threads).
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
                // It's fine to drop the `EventCacheDropHandles` here as the
                // caller (`LatestEventState`) owns a clone of the `EventCache`.
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

        // A gap was reached without finding a latest event, so back-paginate
        // the room's history in the background until a suitable event
        // surfaces.
        if matches!(
            for_the_room
                .update_with_event_cache(room_event_cache, own_user_id, power_levels.as_ref())
                .await,
            NeedMoreEvents::Yes
        ) {
            Self::back_paginate_for_candidate(
                &room,
                &inner.event_cache,
                own_user_id,
                power_levels.as_ref(),
            );
        }

        for latest_event in per_thread.values_mut() {
            latest_event
                .update_with_event_cache(room_event_cache, own_user_id, power_levels.as_ref())
                .await;
        }
    }

    /// Update the latest events for the room and its threads, based on the send
    /// queue update.
    pub async fn update_with_send_queue(&mut self, send_queue_update: &RoomSendQueueUpdate) {
        // Get the power levels of the user for the current room if the
        // `WeakRoom` is still valid.
        //
        // Get it once for all the updates of all the latest events for this
        // room (be the room and its threads).
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
                // It's fine to drop the `EventCacheDropHandles` here as the
                // caller (`LatestEventState`) owns a clone of the `EventCache`.
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
    /// contains a suitable latest-event candidate. No-ops if automatic
    /// backpagination is disabled.
    ///
    /// Fire-and-forget as the events it loads emit an event cache update which
    /// will trigger a recomputation.
    ///
    /// [`BackPaginationQueue`]: crate::event_cache::BackPaginationQueue
    #[instrument(skip_all, fields(room_id = %room.room_id()))]
    fn back_paginate_for_candidate(
        room: &Room,
        event_cache: &EventCache,
        own_user_id: &UserId,
        power_levels: Option<&RoomPowerLevels>,
    ) {
        let Some(queue) = event_cache.back_pagination_queue() else {
            return;
        };

        let own_user_id = own_user_id.to_owned();
        let power_levels = power_levels.cloned();

        // This filters each batch to spot a candidate and `Builder::new_remote`
        // filters the same events again when it computes the value afterwards.
        // That second pass can't be skipped though as an event's edits are
        // newer than it and a stop condition only ever sees the batch it just
        // loaded.
        let stop = move |outcome: &BackPaginationOutcome| {
            let found = outcome.events.iter().any(|event| {
                filter_timeline_event(event, None, &own_user_id, power_levels.as_ref()).is_break()
            });

            if found { ControlFlow::Break(()) } else { ControlFlow::Continue(()) }
        };

        debug!("started backfill request for latest events");

        match queue.enqueue(BackPaginationRequest {
            room_id: room.room_id().to_owned(),
            priority: back_pagination_queue::Priority::High,
            stop: Box::new(stop),
            batch_size: back_pagination_queue::BATCH_SIZE,
            max_batches: None,
        }) {
            // Nobody awaits the result, so detach the handle to let the request
            // run to completion instead of cancelling it on drop.
            Ok(handle) => handle.detach(),
            Err(err) => warn!("couldn't enqueue a latest-event backfill request: {err}"),
        }
    }
}

#[cfg(all(test, not(target_family = "wasm")))]
mod tests {
    use std::time::Duration;

    use assert_matches::assert_matches;
    use matrix_sdk_base::{
        RoomState,
        cross_process_lock::CrossProcessLockConfig,
        event_cache::{
            Gap,
            store::{EventCacheStore, MemoryStore},
        },
        linked_chunk::{ChunkIdentifier, LinkedChunkId, Position, Update},
        store::StoreConfig,
    };
    use matrix_sdk_test::{JoinedRoomBuilder, async_test, event_factory::EventFactory};
    use ruma::{
        EventId, RoomId, event_id, events::room::message::RoomMessageEventContent, room_id, user_id,
    };
    use tokio::time::timeout;

    use super::RoomLatestEvents;
    use crate::{
        Client, assert_let_timeout,
        event_cache::{RFindMapOutcome, RoomEventCache},
        latest_events::LatestEventValue,
        room::WeakRoom,
        test_utils::mocks::{MatrixMockServer, RoomMessagesResponseTemplate},
    };

    /// A client with automatic back-pagination, on top of `event_cache_store`.
    async fn client_with_store(
        server: &MatrixMockServer,
        event_cache_store: MemoryStore,
    ) -> Client {
        server
            .client_builder()
            .on_builder(|builder| {
                builder.with_enable_automatic_back_pagination(true).store_config(
                    StoreConfig::new(CrossProcessLockConfig::MultiProcess {
                        holder_name: "foo".to_owned(),
                    })
                    .event_cache_store(event_cache_store),
                )
            })
            .build()
            .await
    }

    /// A client with automatic back-pagination whose `room_id` has a single
    /// gap: no events in memory (so no candidate), but a token to paginate
    /// from.
    async fn client_with_a_gap(server: &MatrixMockServer, room_id: &RoomId) -> Client {
        let event_cache_store = MemoryStore::new();

        // A linked chunk with a single gap: no events in memory (so no
        // candidate), but a token to paginate from. Set up directly so no sync
        // (and thus no competing read-receipt pagination) races the backfill.
        event_cache_store
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

        client_with_store(server, event_cache_store).await
    }

    /// A client with automatic back-pagination whose `room_id` is stored as
    /// two chunks: `older` then `last`. Only `last` gets loaded in memory.
    async fn client_with_two_chunks(
        server: &MatrixMockServer,
        room_id: &RoomId,
        older: Vec<matrix_sdk_base::event_cache::Event>,
        last: Vec<matrix_sdk_base::event_cache::Event>,
    ) -> Client {
        let event_cache_store = MemoryStore::new();

        event_cache_store
            .handle_linked_chunk_updates(
                LinkedChunkId::Room(room_id),
                vec![
                    Update::NewItemsChunk {
                        previous: None,
                        new: ChunkIdentifier::new(0),
                        next: None,
                    },
                    Update::PushItems {
                        at: Position::new(ChunkIdentifier::new(0), 0),
                        items: older,
                    },
                    Update::NewItemsChunk {
                        previous: Some(ChunkIdentifier::new(0)),
                        new: ChunkIdentifier::new(1),
                        next: None,
                    },
                    Update::PushItems {
                        at: Position::new(ChunkIdentifier::new(1), 0),
                        items: last,
                    },
                ],
            )
            .await
            .unwrap();

        let client = client_with_store(server, event_cache_store).await;
        client.base_client().get_or_create_room(room_id, RoomState::Joined);

        client
    }

    /// Wait until the room has shrunk enough for `event_id` to leave memory.
    async fn wait_until_unloaded(room_event_cache: &RoomEventCache, event_id: &EventId) {
        timeout(Duration::from_secs(1), async {
            while room_event_cache
                .events()
                .await
                .unwrap()
                .iter()
                .any(|event| event.event_id() == Some(event_id))
            {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the room should have shrunk");
    }

    /// When no latest-event candidate is present in memory, but one exists
    /// behind a gap, `update_with_event_cache` back-paginates to surface it.
    #[async_test]
    async fn test_update_with_event_cache_backfills_for_a_candidate() {
        let room_id = room_id!("!r0");
        let sender = user_id!("@bob:example.org");

        let server = MatrixMockServer::new().await;
        let client = client_with_a_gap(&server, room_id).await;

        client.base_client().get_or_create_room(room_id, RoomState::Joined);

        let event_cache = client.event_cache().unwrap();

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

        let weak_room = WeakRoom::new(client.downgrade(), room_id.to_owned());
        let room_latest_events = RoomLatestEvents::new(weak_room, event_cache);

        // No candidate in memory yet.
        assert_matches!(
            room_latest_events.read().await.for_room().get().await,
            LatestEventValue::None
        );

        let mut updates = event_cache.subscribe_to_room_generic_updates();

        room_latest_events.write().await.update_with_event_cache().await;

        // The backfill runs in the background; wait for the events it loads,
        // then recompute as the update it emits would.
        assert_let_timeout!(Ok(_) = updates.recv());

        room_latest_events.write().await.update_with_event_cache().await;

        // The backfill surfaced the message; the latest event resolved to it.
        assert_matches!(
            room_latest_events.read().await.for_room().get().await,
            LatestEventValue::Remote(_)
        );
    }

    /// The stored chunks read past the in-memory ones are capped.
    #[async_test]
    async fn test_rfind_map_event_until_gap_by_reads_at_most_max_stored_chunks() {
        let room_id = room_id!("!r0");
        let f = EventFactory::new().room(room_id).sender(user_id!("@bob:example.org"));

        // Chunks: [$0 (candidate)] [$1] [$2], only the last one is in memory.
        let event_cache_store = MemoryStore::new();
        let mut updates = Vec::new();
        for (i, event_id) in
            [event_id!("$0"), event_id!("$1"), event_id!("$2")].into_iter().enumerate()
        {
            let chunk = ChunkIdentifier::new(i as u64);
            updates.push(Update::NewItemsChunk {
                previous: i.checked_sub(1).map(|p| ChunkIdentifier::new(p as u64)),
                new: chunk,
                next: None,
            });
            updates.push(Update::PushItems {
                at: Position::new(chunk, 0),
                items: vec![f.text_msg("hello").event_id(event_id).into_event()],
            });
        }
        event_cache_store
            .handle_linked_chunk_updates(LinkedChunkId::Room(room_id), updates)
            .await
            .unwrap();

        let server = MatrixMockServer::new().await;
        let client = client_with_store(&server, event_cache_store).await;
        client.base_client().get_or_create_room(room_id, RoomState::Joined);
        let (room_event_cache, _drop_handles) =
            client.event_cache().unwrap().room(room_id).await.unwrap();

        let is_candidate = |event: &matrix_sdk_base::event_cache::Event| {
            (event.event_id() == Some(event_id!("$0"))).then_some(())
        };

        for (max_stored_chunks, expected_found) in [(0, false), (1, false), (2, true)] {
            let outcome = room_event_cache
                .rfind_map_event_until_gap_by(max_stored_chunks, is_candidate)
                .await
                .unwrap();

            if expected_found {
                assert_matches!(outcome, RFindMapOutcome::Found(()));
            } else {
                assert_matches!(outcome, RFindMapOutcome::ReachedLimit);
            }
        }
    }

    /// A candidate in a stored chunk that's not in memory, as after a shrink,
    /// is used as is: no backfill, which would otherwise be unloaded by the
    /// next shrink and requested again, endlessly.
    #[async_test]
    async fn test_update_with_event_cache_uses_a_stored_candidate_without_backfilling() {
        let room_id = room_id!("!r0");
        let f = EventFactory::new().room(room_id).sender(user_id!("@bob:example.org"));

        // A candidate in the older chunk, and only a reaction in the last one.
        let server = MatrixMockServer::new().await;
        let client = client_with_two_chunks(
            &server,
            room_id,
            vec![f.text_msg("hello").event_id(event_id!("$1")).into_event()],
            vec![f.reaction(event_id!("$1"), "+1").event_id(event_id!("$2")).into_event()],
        )
        .await;

        let event_cache = client.event_cache().unwrap();
        let (_room_event_cache, _drop_handles) = event_cache.room(room_id).await.unwrap();

        let weak_room = WeakRoom::new(client.downgrade(), room_id.to_owned());
        let room_latest_events = RoomLatestEvents::new(weak_room, event_cache);

        let mut updates = event_cache.subscribe_to_room_generic_updates();
        room_latest_events.write().await.update_with_event_cache().await;

        assert!(
            timeout(Duration::from_millis(100), updates.recv()).await.is_err(),
            "the latest event was backfilled despite a stored candidate"
        );
        assert_eq!(
            room_latest_events.read().await.for_room().get().await.event_id().as_deref(),
            Some(event_id!("$1"))
        );
    }

    /// A newer event brought by a pagination into an older chunk becomes the
    /// latest event, even if the room shrank before it was computed.
    #[async_test]
    async fn test_update_with_event_cache_finds_a_paginated_event_after_a_shrink() {
        let room_id = room_id!("!r0");
        let f = EventFactory::new().room(room_id).sender(user_id!("@bob:example.org"));

        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;

        server
            .sync_room(
                &client,
                JoinedRoomBuilder::new(room_id)
                    .add_timeline_event(f.text_msg("hello").event_id(event_id!("$1"))),
            )
            .await;

        let event_cache = client.event_cache().unwrap();
        let (room_event_cache, _drop_handles) = event_cache.room(room_id).await.unwrap();
        let (_, subscriber) = room_event_cache.subscribe().await.unwrap();

        let weak_room = WeakRoom::new(client.downgrade(), room_id.to_owned());
        let room_latest_events = RoomLatestEvents::new(weak_room, event_cache);
        room_latest_events.write().await.update_with_event_cache().await;

        // A limited sync: a gap, then only a reaction. `$X`, newer than `$1`,
        // is in the gap.
        server
            .mock_room_messages()
            .match_from("gap")
            .ok(RoomMessagesResponseTemplate::default()
                .events(vec![f.text_msg("sent while away").event_id(event_id!("$X"))]))
            .mount()
            .await;
        server
            .sync_room(
                &client,
                JoinedRoomBuilder::new(room_id)
                    .set_timeline_limited()
                    .set_timeline_prev_batch("gap")
                    .add_timeline_event(
                        f.reaction(event_id!("$1"), "+1").event_id(event_id!("$r")),
                    ),
            )
            .await;

        // A pagination fills the gap, then the room shrinks before the latest
        // event gets computed.
        room_event_cache.pagination().run_backwards_once(30).await.unwrap();
        drop(subscriber);
        wait_until_unloaded(&room_event_cache, event_id!("$X")).await;

        room_latest_events.write().await.update_with_event_cache().await;

        assert_eq!(
            room_latest_events.read().await.for_room().get().await.event_id().as_deref(),
            Some(event_id!("$X")),
            "the paginated event didn't become the latest event"
        );
    }

    /// An edit of an event that's not in memory anymore still edits the latest
    /// event, without a backfill.
    #[async_test]
    async fn test_update_with_event_cache_applies_an_edit_of_an_unloaded_event() {
        let room_id = room_id!("!r0");
        let f = EventFactory::new().room(room_id).sender(user_id!("@bob:example.org"));

        // The original in the older chunk, and only its edit in the last one.
        let server = MatrixMockServer::new().await;
        let client = client_with_two_chunks(
            &server,
            room_id,
            vec![f.text_msg("hello").event_id(event_id!("$1")).into_event()],
            vec![
                f.text_msg("* hello edited")
                    .edit(
                        event_id!("$1"),
                        RoomMessageEventContent::text_plain("hello edited").into(),
                    )
                    .event_id(event_id!("$edit"))
                    .into_event(),
            ],
        )
        .await;

        let weak_room = WeakRoom::new(client.downgrade(), room_id.to_owned());
        let room_latest_events = RoomLatestEvents::new(weak_room, client.event_cache().unwrap());
        room_latest_events.write().await.update_with_event_cache().await;

        assert_eq!(
            room_latest_events.read().await.for_room().get().await.event_id().as_deref(),
            Some(event_id!("$edit")),
            "the edit didn't become the latest event"
        );
    }

    /// A left room isn't back-paginated for a latest-event candidate.
    #[async_test]
    async fn test_update_with_event_cache_skips_backfill_for_left_room() {
        let room_id = room_id!("!r0");

        let server = MatrixMockServer::new().await;
        let client = client_with_a_gap(&server, room_id).await;

        client.base_client().get_or_create_room(room_id, RoomState::Left);

        server
            .mock_room_messages()
            .ok(RoomMessagesResponseTemplate::default())
            .never()
            .mount()
            .await;

        let weak_room = WeakRoom::new(client.downgrade(), room_id.to_owned());
        let room_latest_events = RoomLatestEvents::new(weak_room, client.event_cache().unwrap());

        let mut updates = client.event_cache().unwrap().subscribe_to_room_generic_updates();

        room_latest_events.write().await.update_with_event_cache().await;

        // Nothing is enqueued, so no events get loaded.
        assert!(timeout(Duration::from_millis(100), updates.recv()).await.is_err());
    }
}
