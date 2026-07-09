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

//! Threads-related data structures.

pub mod pagination;
mod state;
mod updates;

use std::{fmt, sync::Arc};

use matrix_sdk_base::{
    event_cache::Event,
    sync::{JoinedRoomUpdate, LeftRoomUpdate, Timeline},
};
use ruma::{
    EventId, OwnedEventId, OwnedRoomId, OwnedUserId, RoomId, events::relation::RelationType,
    room_version_rules::RoomVersionRules,
};
use tokio::sync::{Notify, broadcast::Sender, mpsc};
use tracing::{instrument, trace};

use self::pagination::ThreadPagination;
pub(in super::super) use self::state::ThreadEventCacheState;
pub(super) use self::updates::ThreadEventCacheUpdateSender;
#[cfg(feature = "e2e-encryption")]
use super::super::redecryptor::ResolvedUtd;
use super::{
    super::{
        Result,
        states::{CacheStateLock, StateLock, selectors::ThreadStateSelector},
    },
    EventsOrigin, TimelineVectorDiffs,
    room::{RoomEventCacheGenericUpdate, RoomEventCacheLinkedChunkUpdate},
    subscriber::{AutoShrinkMessage, Subscriber},
};
use crate::room::WeakRoom;

/// All the information related to a single thread.
///
/// Cloning is shallow, and thus is cheap to do.
#[derive(Clone)]
pub struct ThreadEventCache {
    inner: Arc<ThreadEventCacheInner>,
}

/// The (non-cloneable) details of the `ThreadEventCache`.
struct ThreadEventCacheInner {
    /// The room ID.
    room_id: OwnedRoomId,

    /// The thread root ID.
    thread_id: OwnedEventId,

    /// The room where this thread belongs to.
    weak_room: WeakRoom,

    /// State for this thread's event cache.
    state: CacheStateLock<ThreadStateSelector>,

    /// A notifier that we received a new pagination token.
    pagination_batch_token_notifier: Notify,

    /// Sender to the auto-shrink channel.
    ///
    /// See doc comment around [`EventCache::auto_shrink_linked_chunk_task`] for
    /// more details.
    auto_shrink_sender: mpsc::Sender<AutoShrinkMessage>,

    /// Update sender for this thread.
    update_sender: ThreadEventCacheUpdateSender,
}

impl fmt::Debug for ThreadEventCache {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ThreadEventCache").finish_non_exhaustive()
    }
}

impl ThreadEventCache {
    /// Create a new empty thread event cache.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn new(
        room_id: OwnedRoomId,
        thread_id: OwnedEventId,
        own_user_id: OwnedUserId,
        room_version_rules: RoomVersionRules,
        weak_room: WeakRoom,
        state: &StateLock,
        auto_shrink_sender: mpsc::Sender<AutoShrinkMessage>,
        generic_update_sender: Sender<RoomEventCacheGenericUpdate>,
        linked_chunk_update_sender: Sender<RoomEventCacheLinkedChunkUpdate>,
    ) -> Result<Self> {
        let update_sender = ThreadEventCacheUpdateSender::new(generic_update_sender.clone());

        let cache_state = state
            .try_insert_once_with(
                ThreadStateSelector::new(room_id.clone(), thread_id.clone()),
                |store_guard| {
                    ThreadEventCacheState::new(
                        room_id.clone(),
                        thread_id.clone(),
                        own_user_id,
                        room_version_rules,
                        store_guard,
                        update_sender.clone(),
                        linked_chunk_update_sender,
                    )
                },
            )
            .await?;

        let timeline_is_not_empty =
            cache_state.read().await?.thread_linked_chunk().revents().next().is_some();

        let cache = Self {
            inner: Arc::new(ThreadEventCacheInner {
                room_id: room_id.clone(),
                thread_id,
                weak_room,
                state: cache_state,
                pagination_batch_token_notifier: Notify::new(),
                auto_shrink_sender,
                update_sender,
            }),
        };

        // If at least one event has been loaded, it means there is a timeline. Let's
        // emit a generic update.
        if timeline_is_not_empty {
            let _ = generic_update_sender
                .send(RoomEventCacheGenericUpdate { room_id: room_id.to_owned() });
        }

        Ok(cache)
    }

    /// Get the room ID for this room.
    pub fn room_id(&self) -> &RoomId {
        &self.inner.room_id
    }

    /// Get the thread ID for this thread.
    pub fn thread_id(&self) -> &EventId {
        &self.inner.thread_id
    }

    /// Subscribe to this thread updates, after getting the initial list of
    /// events.
    ///
    /// Creating, and especially dropping, a [`Subscriber`] isn't free, as it
    /// triggers side-effects.
    pub async fn subscribe(&self) -> Result<(Vec<Event>, Subscriber<TimelineVectorDiffs>)> {
        let state = self.inner.state.read().await?;
        let events =
            state.thread_linked_chunk().events().map(|(_position, item)| item.clone()).collect();

        let subscribers_handle = state.subscribers_handle();

        let subscriber = Subscriber::new(
            self.inner.update_sender.new_thread_receiver(),
            AutoShrinkMessage::Thread {
                room_id: self.inner.room_id.clone(),
                thread_id: self.inner.thread_id.clone(),
            },
            self.inner.auto_shrink_sender.clone(),
            subscribers_handle,
        );

        trace!("added a thread event cache subscriber; new count: {}", subscribers_handle.count());

        Ok((events, subscriber))
    }

    /// Return a [`ThreadPagination`] useful for running back-pagination queries
    /// in this thread.
    pub fn pagination(&self) -> ThreadPagination {
        ThreadPagination::new(self.inner.clone())
    }

    /// Return a reference to the state.
    pub(in super::super) fn state(&self) -> &CacheStateLock<ThreadStateSelector> {
        &self.inner.state
    }

    /// Handle a [`JoinedRoomUpdate`].
    #[instrument(skip_all, fields(room_id = %self.inner.room_id, thread_root = %self.inner.thread_id))]
    pub(super) async fn handle_joined_room_update(&self, updates: JoinedRoomUpdate) -> Result<()> {
        self.handle_timeline(updates.timeline).await?;

        Ok(())
    }

    /// Handle a [`LeftRoomUpdate`].
    #[instrument(skip_all, fields(room_id = %self.inner.room_id, thread_root = %self.inner.thread_id))]
    pub(super) async fn handle_left_room_update(&self, updates: LeftRoomUpdate) -> Result<()> {
        self.handle_timeline(updates.timeline).await?;

        Ok(())
    }

    /// Handle a [`Timeline`], i.e. new events received by a sync for this
    /// thread.
    async fn handle_timeline(&self, timeline: Timeline) -> Result<()> {
        if timeline.events.is_empty() && timeline.prev_batch.is_none() {
            return Ok(());
        }

        trace!("adding new events");

        let mut state = self.inner.state.write().await?;

        let (stored_prev_batch_token, timeline_event_diffs) = state.handle_sync(timeline).await?;

        // Now that all events have been added, we can trigger the
        // `pagination_token_notifier`.
        if stored_prev_batch_token {
            self.inner.pagination_batch_token_notifier.notify_one();
        }

        if !timeline_event_diffs.is_empty() {
            state.update_sender.send(
                TimelineVectorDiffs { diffs: timeline_event_diffs, origin: EventsOrigin::Sync },
                // This function is part of the `RoomEventCache` flow. The generic update is
                // handled by it.
                None,
            );
        }

        Ok(())
    }

    /// Find a single event in this thread.
    ///
    /// It starts by looking into loaded events in `EventLinkedChunk` before
    /// looking inside the storage.
    pub(super) async fn find_event(
        &self,
        event_id: &EventId,
    ) -> Result<Option<(super::EventLocation, Event)>> {
        self.inner.state.read().await?.find_event(event_id).await
    }

    /// Try to find an event by ID in this thread, along with its related
    /// events.
    ///
    /// You can filter which types of related events to retrieve using
    /// `filter`. `None` will retrieve related events of any type.
    ///
    /// The related events are sorted like this:
    ///
    /// - events saved out-of-band (with `RoomEventCache::save_events`) will be
    ///   located at the beginning of the array.
    /// - events present in the linked chunk (be it in memory or in the storage)
    ///   will be sorted according to their ordering in the linked chunk.
    pub async fn find_event_with_relations(
        &self,
        event_id: &EventId,
        filter: Option<Vec<RelationType>>,
    ) -> Result<Option<(Event, Vec<Event>)>> {
        // Search in all loaded or stored events.
        Ok(self
            .inner
            .state
            .read()
            .await?
            .find_event_with_relations(event_id, filter)
            .await
            .ok()
            .flatten())
    }

    /// Try to locate the events in the linked chunk corresponding to the given
    /// list of decrypted events, and replace them, while alerting observers
    /// about the update.
    ///
    /// Return `true` if at least one event has been updated.
    #[cfg(feature = "e2e-encryption")]
    pub(in super::super) async fn replace_utds(&self, events: &[ResolvedUtd]) -> Result<bool> {
        let mut state = self.inner.state.write().await?;
        let timeline_event_diffs = state.replace_utds(events).await?;

        Ok(
            if let Some(timeline_event_diffs) = timeline_event_diffs
                && !timeline_event_diffs.is_empty()
            {
                state.update_sender.send(
                    TimelineVectorDiffs {
                        diffs: timeline_event_diffs,
                        origin: EventsOrigin::Cache,
                    },
                    Some(RoomEventCacheGenericUpdate { room_id: self.inner.room_id.clone() }),
                );

                true
            } else {
                false
            },
        )
    }
}

#[cfg(all(test, not(target_family = "wasm")))] // This uses the cross-process lock, so needs time support.
mod timed_tests {
    use std::sync::Arc;

    use assert_matches::assert_matches;
    use assert_matches2::assert_let;
    use eyeball_im::VectorDiff;
    use futures_util::FutureExt as _;
    use matrix_sdk_base::{
        RoomState, ThreadingSupport,
        cross_process_lock::CrossProcessLockConfig,
        event_cache::{
            Gap,
            store::{EventCacheStore as _, MemoryStore},
        },
        linked_chunk::{
            ChunkContent, ChunkIdentifier, LinkedChunkId, Position, Update,
            lazy_loader::from_all_chunks,
        },
        store::StoreConfig,
        sync::{JoinedRoomUpdate, Timeline},
    };
    use matrix_sdk_test::{ALICE, async_test, event_factory::EventFactory};
    use ruma::{
        event_id,
        events::{AnySyncMessageLikeEvent, AnySyncTimelineEvent},
        room_id, user_id,
    };
    use tokio::task::yield_now;

    use super::super::{super::RoomEventCacheGenericUpdate, TimelineVectorDiffs};
    use crate::{assert_let_timeout, test_utils::client::MockClientBuilder};

    #[async_test]
    async fn test_write_to_storage() {
        let room_id = room_id!("!r0");
        let thread_root = event_id!("$t0_ev0");
        let thread_event_id_0 = event_id!("$t0_ev1");

        let f = EventFactory::new().room(room_id).sender(user_id!("@mnt_io:matrix.org"));

        let event_cache_store = Arc::new(MemoryStore::new());

        let client = MockClientBuilder::new(None)
            .on_builder(|builder| {
                builder
                    .store_config(
                        StoreConfig::new(CrossProcessLockConfig::multi_process("hodor"))
                            .event_cache_store(event_cache_store.clone()),
                    )
                    .with_threading_support(ThreadingSupport::Enabled { with_subscriptions: true })
            })
            .build()
            .await;

        let event_cache = client.event_cache();
        event_cache.subscribe().unwrap();

        client.base_client().get_or_create_room(room_id, RoomState::Joined);

        let (thread_event_cache, _drop_handles) =
            event_cache.thread(room_id, thread_root).await.unwrap();
        let (thread_events, mut thread_stream) = thread_event_cache.subscribe().await.unwrap();

        assert!(thread_events.is_empty());

        // Propagate an update for a message and a prev-batch token.
        let timeline = Timeline {
            limited: true,
            prev_batch: Some("raclette".to_owned()),
            events: vec![
                f.text_msg("salut")
                    .event_id(thread_event_id_0)
                    .in_thread(thread_root, thread_root)
                    .into_event(),
            ],
        };

        thread_event_cache
            .handle_joined_room_update(JoinedRoomUpdate { timeline, ..Default::default() })
            .await
            .unwrap();

        assert_matches!(
            thread_stream.recv().await,
            Ok(TimelineVectorDiffs { diffs, .. }) => {
                assert_eq!(diffs.len(), 2);
                assert_matches!(&diffs[0], VectorDiff::Clear);
                assert_matches!(&diffs[1], VectorDiff::Append { values: events } => {
                    assert_eq!(events.len(), 1);
                    assert_eq!(events[0].event_id(), Some(thread_event_id_0));
                });
            }
        );
        assert!(thread_stream.is_empty());

        // Check the storage.
        let linked_chunk = from_all_chunks::<3, _, _>(
            event_cache_store
                .load_all_chunks(LinkedChunkId::Thread(room_id, thread_root))
                .await
                .unwrap(),
        )
        .unwrap()
        .unwrap();

        assert_eq!(linked_chunk.chunks().count(), 2);

        let mut chunks = linked_chunk.chunks();

        // We start with the gap.
        assert_matches!(chunks.next().unwrap().content(), ChunkContent::Gap(gap) => {
            assert_eq!(gap.token, "raclette");
        });

        // Then we have the stored event.
        assert_matches!(chunks.next().unwrap().content(), ChunkContent::Items(events) => {
            assert_eq!(events.len(), 1);
            assert_eq!(events[0].event_id(), Some(thread_event_id_0));
        });

        // That's all, folks!
        assert!(chunks.next().is_none());
    }

    #[async_test]
    async fn test_write_to_storage_strips_bundled_relations() {
        let sender = user_id!("@mnt_io:matrix.org");
        let room_id = room_id!("!r0");
        let thread_root = event_id!("$t0_ev0");
        let thread_event_id_0 = event_id!("$t0_ev1");

        let f = EventFactory::new().room(room_id).sender(sender);

        let event_cache_store = Arc::new(MemoryStore::new());

        let client = MockClientBuilder::new(None)
            .on_builder(|builder| {
                builder
                    .store_config(
                        StoreConfig::new(CrossProcessLockConfig::multi_process("hodor"))
                            .event_cache_store(event_cache_store.clone()),
                    )
                    .with_threading_support(ThreadingSupport::Enabled { with_subscriptions: true })
            })
            .build()
            .await;

        let event_cache = client.event_cache();
        event_cache.subscribe().unwrap();

        client.base_client().get_or_create_room(room_id, RoomState::Joined);

        let (thread_event_cache, _drop_handles) =
            event_cache.thread(room_id, thread_root).await.unwrap();

        // Propagate an update for a message with bundled relations.
        let timeline = Timeline {
            limited: false,
            prev_batch: None,
            events: vec![
                f.text_msg("s 'up")
                    .event_id(thread_event_id_0)
                    .with_bundled_edit(f.text_msg("Hello, Kind Sir").sender(sender))
                    .in_thread(thread_root, thread_root)
                    .into_event(),
            ],
        };

        thread_event_cache
            .handle_joined_room_update(JoinedRoomUpdate { timeline, ..Default::default() })
            .await
            .unwrap();

        // The in-memory linked chunk keeps the bundled relation.
        {
            let (events, _) = thread_event_cache.subscribe().await.unwrap();

            assert_eq!(events.len(), 1);

            let event = events[0].raw().deserialize().unwrap();
            assert_eq!(event.event_id(), thread_event_id_0);
            assert_let!(
                AnySyncTimelineEvent::MessageLike(AnySyncMessageLikeEvent::RoomMessage(msg)) =
                    event
            );
            assert!(msg.as_original().unwrap().unsigned.relations.replace.is_some());
        }

        // The one in storage does not.
        let linked_chunk = from_all_chunks::<3, _, _>(
            event_cache_store
                .load_all_chunks(LinkedChunkId::Thread(room_id, thread_root))
                .await
                .unwrap(),
        )
        .unwrap()
        .unwrap();

        assert_eq!(linked_chunk.chunks().count(), 1);

        let mut chunks = linked_chunk.chunks();
        assert_matches!(chunks.next().unwrap().content(), ChunkContent::Items(events) => {
            assert_eq!(events.len(), 1);

            let event = events[0].raw().deserialize().unwrap();
            assert_eq!(event.event_id(), thread_event_id_0);

            assert_let!(AnySyncTimelineEvent::MessageLike(AnySyncMessageLikeEvent::RoomMessage(msg)) = event);
            assert!(msg.as_original().unwrap().unsigned.relations.replace.is_none());
        });

        // That's all, folks!
        assert!(chunks.next().is_none());
    }

    #[async_test]
    async fn test_clear() {
        let room_id = room_id!("!r0");
        let f = EventFactory::new().room(room_id).sender(user_id!("@mnt_io:matrix.org"));

        let event_cache_store = Arc::new(MemoryStore::new());

        let thread_root = event_id!("$t0_ev0");
        let thread_event_id_0 = event_id!("$t0_ev1");
        let thread_event_id_1 = event_id!("$t0_ev2");

        let thread_event_0 = f
            .text_msg("foo")
            .event_id(thread_event_id_0)
            .in_thread(thread_root, thread_root)
            .into_event();
        let thread_event_1 = f
            .text_msg("bar")
            .event_id(thread_event_id_1)
            .in_thread(thread_root, thread_event_id_0)
            .into_event();

        // Prefill the store with some data.
        event_cache_store
            .handle_linked_chunk_updates(
                LinkedChunkId::Thread(room_id, thread_root),
                vec![
                    // An empty items chunk.
                    Update::NewItemsChunk {
                        previous: None,
                        new: ChunkIdentifier::new(0),
                        next: None,
                    },
                    // A gap chunk.
                    Update::NewGapChunk {
                        previous: Some(ChunkIdentifier::new(0)),
                        // Chunk IDs aren't supposed to be ordered, so use a random value here.
                        new: ChunkIdentifier::new(42),
                        next: None,
                        gap: Gap { token: "comté".to_owned() },
                    },
                    // Another items chunk, non-empty this time.
                    Update::NewItemsChunk {
                        previous: Some(ChunkIdentifier::new(42)),
                        new: ChunkIdentifier::new(1),
                        next: None,
                    },
                    Update::PushItems {
                        at: Position::new(ChunkIdentifier::new(1), 0),
                        items: vec![thread_event_0.clone()],
                    },
                    // And another items chunk, non-empty again.
                    Update::NewItemsChunk {
                        previous: Some(ChunkIdentifier::new(1)),
                        new: ChunkIdentifier::new(2),
                        next: None,
                    },
                    Update::PushItems {
                        at: Position::new(ChunkIdentifier::new(2), 0),
                        items: vec![thread_event_1.clone()],
                    },
                ],
            )
            .await
            .unwrap();

        let client = MockClientBuilder::new(None)
            .on_builder(|builder| {
                builder
                    .store_config(
                        StoreConfig::new(CrossProcessLockConfig::multi_process("hodor"))
                            .event_cache_store(event_cache_store.clone()),
                    )
                    .with_threading_support(ThreadingSupport::Enabled { with_subscriptions: true })
            })
            .build()
            .await;

        let event_cache = client.event_cache();
        event_cache.subscribe().unwrap();

        client.base_client().get_or_create_room(room_id, RoomState::Joined);

        let (thread_event_cache, _drop_handles) =
            event_cache.thread(room_id, thread_root).await.unwrap();
        let (thread_events, mut thread_stream) = thread_event_cache.subscribe().await.unwrap();

        let mut generic_stream = event_cache.subscribe_to_room_generic_updates();

        // The thread knows about all cached events.
        {
            assert!(thread_event_cache.find_event(thread_event_id_0).await.unwrap().is_some());
            assert!(thread_event_cache.find_event(thread_event_id_1).await.unwrap().is_some());
        }

        // But only part of events are loaded from the store.
        {
            // The thread must contain only one event because only one chunk has been
            // loaded.
            assert_eq!(thread_events.len(), 1);
            assert_eq!(thread_events[0].event_id().unwrap(), thread_event_id_1);

            assert!(thread_stream.is_empty());
        }

        // Let's load more chunks to load all events.
        {
            thread_event_cache.pagination().run_backwards_once(20).await.unwrap();

            assert_matches!(
                thread_stream.recv().await,
                Ok(TimelineVectorDiffs { diffs, .. }) => {
                    assert_eq!(diffs.len(), 1);
                    assert_matches!(&diffs[0], VectorDiff::Insert { index: 0, value: event } => {
                        // Here you are `thread_event_0`!
                        assert_eq!(event.event_id(), Some(thread_event_id_0));
                    });
                }
            );
            assert!(thread_stream.is_empty());

            assert_matches!(
                generic_stream.recv().await,
                Ok(RoomEventCacheGenericUpdate { room_id: expected_room_id }) => {
                    assert_eq!(room_id, expected_room_id);
                }
            );
            assert!(generic_stream.is_empty());
        }

        // After clearing,…
        event_cache.clear_all_rooms().await.unwrap();

        //… we get an update that the content has been cleared.
        assert_matches!(
            thread_stream.recv().await,
            Ok(TimelineVectorDiffs { diffs, .. }) => {
                assert_eq!(diffs.len(), 2);
                assert_matches!(&diffs[0], VectorDiff::Clear);
                assert_matches!(&diffs[1], VectorDiff::Append { values } => {
                    assert!(values.is_empty());
                });
            }
        );

        // … same with a generic update.
        // (update for the clearing of the room)
        assert_matches!(
            generic_stream.recv().await,
            Ok(RoomEventCacheGenericUpdate { room_id: received_room_id }) => {
                assert_eq!(received_room_id, room_id);
            }
        );
        // (update for the clearing of the thread)
        assert_matches!(
            generic_stream.recv().await,
            Ok(RoomEventCacheGenericUpdate { room_id: received_room_id }) => {
                assert_eq!(received_room_id, room_id);
            }
        );
        assert!(generic_stream.is_empty());

        // Events individually are forgotten by the event cache, after clearing the
        // threads.
        assert!(thread_event_cache.find_event(thread_event_id_0).await.unwrap().is_none());
        assert!(thread_event_cache.find_event(thread_event_id_1).await.unwrap().is_none());

        // And their presence in a linked chunk is forgotten.
        let (thread_events, _) = thread_event_cache.subscribe().await.unwrap();
        assert!(thread_events.is_empty());

        // The event cache store is totally empty.
        let linked_chunk = from_all_chunks::<3, _, _>(
            event_cache_store
                .load_all_chunks(LinkedChunkId::Thread(room_id, thread_root))
                .await
                .unwrap(),
        )
        .unwrap()
        .unwrap();

        // Note: while the event cache store could return `None` here, clearing it will
        // reset it to its initial form, maintaining the invariant that it
        // contains a single items chunk that's empty.
        assert_eq!(linked_chunk.num_items(), 0);
    }

    #[async_test]
    async fn test_load_from_storage() {
        let room_id = room_id!("!r0");
        let f = EventFactory::new().room(room_id).sender(user_id!("@mnt_io:matrix.org"));

        let event_cache_store = Arc::new(MemoryStore::new());

        let thread_root = event_id!("$t0");
        let thread_event_id_0 = event_id!("$t0_ev0");
        let thread_event_id_1 = event_id!("$t0_ev1");

        let thread_event_0 = f
            .text_msg("hello world")
            .event_id(thread_event_id_0)
            .in_thread(thread_root, thread_root)
            .into_event();
        let thread_event_1 = f
            .text_msg("how's it going")
            .event_id(thread_event_id_1)
            .in_thread(thread_root, thread_event_id_1)
            .into_event();

        // Prefill the store with some data. The room usually has all events duplicated
        // from the threads. It's important to make the test pass when checking the
        // generic update.
        let updates = vec![
            // An empty items chunk.
            Update::NewItemsChunk { previous: None, new: ChunkIdentifier::new(0), next: None },
            // A gap chunk.
            Update::NewGapChunk {
                previous: Some(ChunkIdentifier::new(0)),
                // Chunk IDs aren't supposed to be ordered, so use a random value here.
                new: ChunkIdentifier::new(42),
                next: None,
                gap: Gap { token: "gruyère".to_owned() },
            },
            // Another items chunk, non-empty this time.
            Update::NewItemsChunk {
                previous: Some(ChunkIdentifier::new(42)),
                new: ChunkIdentifier::new(1),
                next: None,
            },
            Update::PushItems {
                at: Position::new(ChunkIdentifier::new(1), 0),
                items: vec![thread_event_0.clone()],
            },
            // And another items chunk, non-empty again.
            Update::NewItemsChunk {
                previous: Some(ChunkIdentifier::new(1)),
                new: ChunkIdentifier::new(2),
                next: None,
            },
            Update::PushItems {
                at: Position::new(ChunkIdentifier::new(2), 0),
                items: vec![thread_event_1.clone()],
            },
        ];
        event_cache_store
            .handle_linked_chunk_updates(LinkedChunkId::Room(room_id), updates.clone())
            .await
            .unwrap();
        event_cache_store
            .handle_linked_chunk_updates(LinkedChunkId::Thread(room_id, thread_root), updates)
            .await
            .unwrap();

        let client = MockClientBuilder::new(None)
            .on_builder(|builder| {
                builder
                    .store_config(
                        StoreConfig::new(CrossProcessLockConfig::multi_process("hodor"))
                            .event_cache_store(event_cache_store.clone()),
                    )
                    .with_threading_support(ThreadingSupport::Enabled { with_subscriptions: true })
            })
            .build()
            .await;

        let event_cache = client.event_cache();
        event_cache.subscribe().unwrap();

        client.base_client().get_or_create_room(room_id, RoomState::Joined);

        // Let's check whether the generic updates are received for the initialisation.
        let mut generic_stream = event_cache.subscribe_to_room_generic_updates();
        let (thread_event_cache, _drop_handles) =
            event_cache.thread(room_id, thread_root).await.unwrap();
        let (thread_events, mut thread_stream) = thread_event_cache.subscribe().await.unwrap();

        // The room **and** the thread have been loaded. Two generic updates must have
        // been triggered.
        for _ in 0..2 {
            assert_matches!(
                generic_stream.recv().await,
                Ok(RoomEventCacheGenericUpdate { room_id: expected_room_id }) => {
                    assert_eq!(room_id, expected_room_id);
                }
            );
        }
        assert!(generic_stream.is_empty());

        // The initial events contain one event because only the last chunk is loaded by
        // default.
        assert_eq!(thread_events.len(), 1);
        assert_eq!(thread_events[0].event_id().unwrap(), thread_event_id_1);
        assert!(thread_stream.is_empty());

        // The thread knows all events in the storage though, even if they aren't
        // loaded.
        assert!(thread_event_cache.find_event(thread_event_id_0).await.unwrap().is_some());
        assert!(thread_event_cache.find_event(thread_event_id_1).await.unwrap().is_some());

        // Let's paginate to load more events.
        thread_event_cache.pagination().run_backwards_once(20).await.unwrap();

        assert_matches!(
            thread_stream.recv().await,
            Ok(TimelineVectorDiffs { diffs, .. }) => {
                assert_eq!(diffs.len(), 1);
                assert_matches!(&diffs[0], VectorDiff::Insert { index: 0, value: event } => {
                    assert_eq!(event.event_id(), Some(thread_event_id_0));
                });
            }
        );
        assert!(thread_stream.is_empty());

        // A generic update is triggered too.
        assert_matches!(
            generic_stream.recv().await,
            Ok(RoomEventCacheGenericUpdate { room_id: expected_room_id }) => {
                assert_eq!(expected_room_id, room_id);
            }
        );
        assert!(generic_stream.is_empty());

        // A new update with one of these events leads to deduplication.
        let timeline = Timeline { limited: false, prev_batch: None, events: vec![thread_event_1] };

        thread_event_cache
            .handle_joined_room_update(JoinedRoomUpdate { timeline, ..Default::default() })
            .await
            .unwrap();

        // Just checking the generic update is correct. There is a duplicate event, so
        // no generic changes whatsoever!
        assert!(generic_stream.recv().now_or_never().is_none());

        // The stream doesn't report these changes *yet*. Use the events vector given
        // when subscribing, to check that the events correspond to their new
        // positions. The duplicated item is removed (so it's not the first
        // element anymore), and it's added to the back of the list.
        let (thread_events, _) = thread_event_cache.subscribe().await.unwrap();
        assert_eq!(thread_events.len(), 2);
        assert_eq!(thread_events[0].event_id(), Some(thread_event_id_0));
        assert_eq!(thread_events[1].event_id(), Some(thread_event_id_1));
    }

    #[async_test]
    async fn test_load_from_storage_resilient_to_failure() {
        let room_id = room_id!("!r0");
        let f = EventFactory::new().room(room_id).sender(user_id!("@mnt_io:matrix.org"));

        let event_cache_store = Arc::new(MemoryStore::new());

        let thread_root = event_id!("$t0");
        let thread_event_id_0 = event_id!("$t0_ev0");

        let thread_event_0 = f
            .text_msg("hello world")
            .event_id(thread_event_id_0)
            .in_thread(thread_root, thread_root)
            .into_event();

        // Prefill the store with invalid data: two chunks that form a cycle.
        event_cache_store
            .handle_linked_chunk_updates(
                LinkedChunkId::Thread(room_id, thread_root),
                vec![
                    Update::NewItemsChunk {
                        previous: None,
                        new: ChunkIdentifier::new(0),
                        next: None,
                    },
                    Update::PushItems {
                        at: Position::new(ChunkIdentifier::new(0), 0),
                        items: vec![thread_event_0],
                    },
                    Update::NewItemsChunk {
                        previous: Some(ChunkIdentifier::new(0)),
                        new: ChunkIdentifier::new(1),
                        next: Some(ChunkIdentifier::new(0)),
                    },
                ],
            )
            .await
            .unwrap();

        let client = MockClientBuilder::new(None)
            .on_builder(|builder| {
                builder
                    .store_config(
                        StoreConfig::new(CrossProcessLockConfig::multi_process("holder"))
                            .event_cache_store(event_cache_store.clone()),
                    )
                    .with_threading_support(ThreadingSupport::Enabled { with_subscriptions: true })
            })
            .build()
            .await;

        let event_cache = client.event_cache();
        event_cache.subscribe().unwrap();

        client.base_client().get_or_create_room(room_id, RoomState::Joined);

        let (thread_event_cache, _drop_handles) =
            event_cache.thread(room_id, thread_root).await.unwrap();
        let (thread_events, _) = thread_event_cache.subscribe().await.unwrap();

        // Because the persisted content was invalid, the thread store is reset:
        // there are no events in the cache.
        assert!(thread_events.is_empty());

        // Storage doesn't contain anything. It would also be valid that it contains a
        // single initial empty items chunk.
        let raw_chunks = event_cache_store
            .load_all_chunks(LinkedChunkId::Thread(room_id, thread_root))
            .await
            .unwrap();
        assert!(raw_chunks.is_empty());
    }

    #[async_test]
    async fn test_reload_when_dirty() {
        let user_id = user_id!("@mnt_io:matrix.org");
        let room_id = room_id!("!raclette:patate.ch");

        // The storage shared by the two clients.
        let event_cache_store = MemoryStore::new();

        // Client for the process 0.
        let client_p0 = MockClientBuilder::new(None)
            .on_builder(|builder| {
                builder
                    .store_config(
                        StoreConfig::new(CrossProcessLockConfig::multi_process("process #0"))
                            .event_cache_store(event_cache_store.clone()),
                    )
                    .with_threading_support(ThreadingSupport::Enabled { with_subscriptions: true })
            })
            .build()
            .await;

        // Client for the process 1.
        let client_p1 = MockClientBuilder::new(None)
            .on_builder(|builder| {
                builder
                    .store_config(
                        StoreConfig::new(CrossProcessLockConfig::multi_process("process #1"))
                            .event_cache_store(event_cache_store),
                    )
                    .with_threading_support(ThreadingSupport::Enabled { with_subscriptions: true })
            })
            .build()
            .await;

        let event_factory = EventFactory::new().room(room_id).sender(user_id);

        let thread_root = event_id!("$t0");
        let thread_event_id_0 = event_id!("$t0_ev0");
        let thread_event_id_1 = event_id!("$t0_ev1");

        let thread_event_0 = event_factory
            .text_msg("comté")
            .event_id(thread_event_id_0)
            .in_thread(thread_root, thread_root)
            .into_event();
        let thread_event_1 = event_factory
            .text_msg("morbier")
            .event_id(thread_event_id_1)
            .in_thread(thread_root, thread_event_id_0)
            .into_event();

        // Add events to the storage (shared by the two clients!).
        client_p0
            .event_cache_store()
            .lock()
            .await
            .expect("[p0] Could not acquire the event cache lock")
            .as_clean()
            .expect("[p0] Could not acquire a clean event cache lock")
            .handle_linked_chunk_updates(
                LinkedChunkId::Thread(room_id, thread_root),
                vec![
                    Update::NewItemsChunk {
                        previous: None,
                        new: ChunkIdentifier::new(0),
                        next: None,
                    },
                    Update::PushItems {
                        at: Position::new(ChunkIdentifier::new(0), 0),
                        items: vec![thread_event_0],
                    },
                    Update::NewItemsChunk {
                        previous: Some(ChunkIdentifier::new(0)),
                        new: ChunkIdentifier::new(1),
                        next: None,
                    },
                    Update::PushItems {
                        at: Position::new(ChunkIdentifier::new(1), 0),
                        items: vec![thread_event_1],
                    },
                ],
            )
            .await
            .unwrap();

        // Subscribe the event caches, and create the room.
        let (thread_event_cache_p0, thread_event_cache_p1) = {
            let event_cache_p0 = client_p0.event_cache();
            event_cache_p0.subscribe().unwrap();

            let event_cache_p1 = client_p1.event_cache();
            event_cache_p1.subscribe().unwrap();

            client_p0.base_client().get_or_create_room(room_id, RoomState::Joined);
            client_p1.base_client().get_or_create_room(room_id, RoomState::Joined);

            let (thread_event_cache_p0, _drop_handles) =
                event_cache_p0.thread(room_id, thread_root).await.unwrap();
            let (thread_event_cache_p1, _drop_handles) =
                event_cache_p1.thread(room_id, thread_root).await.unwrap();

            (thread_event_cache_p0, thread_event_cache_p1)
        };

        // Okay. We are ready for the test!
        //
        // First off, let's check `thread_event_cache_p0` has access to the first event
        // loaded in-memory, then do a pagination, and see more events.
        let mut updates_stream_p0 = {
            let thread_event_cache = &thread_event_cache_p0;

            let (initial_updates, mut updates_stream) =
                thread_event_cache_p0.subscribe().await.unwrap();

            // Initial updates contain `thread_event_id_1` only.
            assert_eq!(initial_updates.len(), 1);
            assert_eq!(initial_updates[0].event_id(), Some(thread_event_id_1));
            assert!(updates_stream.is_empty());

            // Load one more event with a backpagination.
            thread_event_cache.pagination().run_backwards_once(1).await.unwrap();

            // A new update for `ev_id_0` must be present.
            assert_matches!(
                updates_stream.recv().await.unwrap(),
                TimelineVectorDiffs { diffs, .. } => {
                    assert_eq!(diffs.len(), 1, "{diffs:#?}");
                    assert_matches!(
                        &diffs[0],
                        VectorDiff::Insert { index: 0, value: event } => {
                            assert_eq!(event.event_id(), Some(thread_event_id_0));
                        }
                    );
                }
            );

            updates_stream
        };

        // Second, let's check `thread_event_cache_p1` has the same accesses.
        let mut updates_stream_p1 = {
            let thread_event_cache = &thread_event_cache_p1;
            let (initial_updates, mut updates_stream) =
                thread_event_cache_p1.subscribe().await.unwrap();

            // Initial updates contain `thread_event_id_1` only.
            assert_eq!(initial_updates.len(), 1);
            assert_eq!(initial_updates[0].event_id(), Some(thread_event_id_1));
            assert!(updates_stream.is_empty());

            // Load one more event with a backpagination.
            thread_event_cache.pagination().run_backwards_once(1).await.unwrap();

            // A new update for `thread_event_id_0` must be present.
            assert_matches!(
                updates_stream.recv().await.unwrap(),
                TimelineVectorDiffs { diffs, .. } => {
                    assert_eq!(diffs.len(), 1, "{diffs:#?}");
                    assert_matches!(
                        &diffs[0],
                        VectorDiff::Insert { index: 0, value: event } => {
                            assert_eq!(event.event_id(), Some(thread_event_id_0));
                        }
                    );
                }
            );

            updates_stream
        };

        // Do this a couple times, for the fun.
        for _ in 0..3 {
            // Third, because `thread_event_cache_p1` has locked the store, the lock
            // is dirty for `thread_event_cache_p0`, so it will shrink to its last
            // chunk for the thread!
            {
                let thread_event_cache = &thread_event_cache_p0;
                let updates_stream = &mut updates_stream_p0;

                // `thread_event_id_1` must be loaded in memory, just like before.
                // However, `thread_event_id_0` must NOT be loaded in memory. It WAS loaded, but
                // the state has been reloaded to its last chunk.
                let (initial_updates, _) = thread_event_cache.subscribe().await.unwrap();

                assert_eq!(initial_updates.len(), 1);
                assert_eq!(initial_updates[0].event_id(), Some(thread_event_id_1));

                // The reload can be observed via the updates too.
                assert_matches!(
                    updates_stream.recv().await.unwrap(),
                    TimelineVectorDiffs { diffs, .. } => {
                        assert_eq!(diffs.len(), 2, "{diffs:#?}");
                        assert_matches!(&diffs[0], VectorDiff::Clear);
                        assert_matches!(
                            &diffs[1],
                            VectorDiff::Append { values: events } => {
                                assert_eq!(events.len(), 1);
                                assert_eq!(events[0].event_id(), Some(thread_event_id_1));
                            }
                        );
                    }
                );

                // Load one more event with a backpagination.
                thread_event_cache.pagination().run_backwards_once(1).await.unwrap();

                // `thread_event_id_0` must now be loaded in memory.
                // The pagination can be observed via the updates.
                assert_matches!(
                    updates_stream.recv().await.unwrap(),
                    TimelineVectorDiffs { diffs, .. } => {
                        assert_eq!(diffs.len(), 1, "{diffs:#?}");
                        assert_matches!(
                            &diffs[0],
                            VectorDiff::Insert { index: 0, value: event } => {
                                assert_eq!(event.event_id(), Some(thread_event_id_0));
                            }
                        );
                    }
                );
            }

            // Fourth, because `thread_event_cache_p0` has locked the store again, the lock
            // is dirty for `thread_event_cache_p1` too!, so it will shrink to its last
            // chunk for the thread!
            {
                let thread_event_cache = &thread_event_cache_p1;
                let updates_stream = &mut updates_stream_p1;

                // `thread_event_id_1` must be loaded in memory, just like before.
                // However, `thread_event_id_0` must NOT be loaded in memory. It WAS loaded, but
                // the state has shrunk to its last chunk.
                let (initial_updates, _) = thread_event_cache.subscribe().await.unwrap();

                assert_eq!(initial_updates.len(), 1);
                assert_eq!(initial_updates[0].event_id(), Some(thread_event_id_1));

                // The reload can be observed via the updates too.
                assert_matches!(
                    updates_stream.recv().await.unwrap(),
                    TimelineVectorDiffs { diffs, .. } => {
                        assert_eq!(diffs.len(), 2, "{diffs:#?}");
                        assert_matches!(&diffs[0], VectorDiff::Clear);
                        assert_matches!(
                            &diffs[1],
                            VectorDiff::Append { values: events } => {
                                assert_eq!(events.len(), 1);
                                assert_eq!(events[0].event_id(), Some(thread_event_id_1));
                            }
                        );
                    }
                );

                // Load one more event with a backpagination.
                thread_event_cache.pagination().run_backwards_once(1).await.unwrap();

                // `thread_event_id_0` must now be loaded in memory.
                // The pagination can be observed via the updates.
                assert_matches!(
                    updates_stream.recv().await.unwrap(),
                    TimelineVectorDiffs { diffs, .. } => {
                        assert_eq!(diffs.len(), 1, "{diffs:#?}");
                        assert_matches!(
                            &diffs[0],
                            VectorDiff::Insert { index: 0, value: event } => {
                                assert_eq!(event.event_id(), Some(thread_event_id_0));
                            }
                        );
                    }
                );
            }
        }
    }

    #[async_test]
    async fn test_auto_shrink_after_all_subscribers_are_gone() {
        let room_id = room_id!("!r0");
        let thread_id = event_id!("$t0");

        let client = MockClientBuilder::new(None).build().await;

        let f = EventFactory::new().room(room_id).sender(*ALICE);

        let event_id_0 = event_id!("$ev0");
        let event_id_1 = event_id!("$ev1");

        let thread_root =
            f.text_msg("gr00t").event_id(thread_id).in_thread(thread_id, thread_id).into_event();
        let event_0 =
            f.text_msg("hello").event_id(event_id_0).in_thread(thread_id, event_id_0).into_event();
        let event_1 =
            f.text_msg("world").event_id(event_id_1).in_thread(thread_id, event_id_1).into_event();

        // Fill the event cache store with an initial linked chunk with 2 events chunks.
        {
            client
                .event_cache_store()
                .lock()
                .await
                .expect("Could not acquire the event cache lock")
                .as_clean()
                .expect("Could not acquire a clean event cache lock")
                .handle_linked_chunk_updates(
                    LinkedChunkId::Thread(room_id, thread_id),
                    vec![
                        Update::NewItemsChunk {
                            previous: None,
                            new: ChunkIdentifier::new(0),
                            next: None,
                        },
                        Update::PushItems {
                            at: Position::new(ChunkIdentifier::new(0), 0),
                            items: vec![thread_root, event_0],
                        },
                        Update::NewItemsChunk {
                            previous: Some(ChunkIdentifier::new(0)),
                            new: ChunkIdentifier::new(1),
                            next: None,
                        },
                        Update::PushItems {
                            at: Position::new(ChunkIdentifier::new(1), 0),
                            items: vec![event_1],
                        },
                    ],
                )
                .await
                .unwrap();
        }

        let event_cache = client.event_cache();
        event_cache.subscribe().unwrap();

        client.base_client().get_or_create_room(room_id, RoomState::Joined);

        let (thread_event_cache, _drop_handles) =
            event_cache.thread(room_id, thread_id).await.unwrap();

        // Sanity check: lazily loaded, so only includes one item at start.
        let (events1, mut stream1) = thread_event_cache.subscribe().await.unwrap();
        assert_eq!(events1.len(), 1);
        assert_eq!(events1[0].event_id(), Some(event_id_1));
        assert!(stream1.is_empty());

        let mut generic_stream = event_cache.subscribe_to_room_generic_updates();

        // Force loading the full linked chunk by back-paginating.
        let outcome = thread_event_cache.pagination().run_backwards_once(20).await.unwrap();
        assert_eq!(outcome.events.len(), 2);
        assert_eq!(outcome.events[0].event_id(), Some(event_id_0));
        assert_eq!(outcome.events[1].event_id(), Some(thread_id));
        assert!(outcome.reached_start);

        // We also get an update about the loading from the store. Ignore it, for this
        // test's sake.
        assert_let_timeout!(Ok(TimelineVectorDiffs { diffs, .. }) = stream1.recv());
        assert_eq!(diffs.len(), 2);
        assert_matches!(&diffs[0], VectorDiff::Insert { index: 0, value } => {
            assert_eq!(value.event_id(), Some(thread_id));
        });
        assert_matches!(&diffs[1], VectorDiff::Insert { index: 1, value } => {
            assert_eq!(value.event_id(), Some(event_id_0));
        });

        assert!(stream1.is_empty());

        assert_let_timeout!(
            Ok(RoomEventCacheGenericUpdate { room_id: expected_room_id }) = generic_stream.recv()
        );
        assert_eq!(expected_room_id, room_id);
        assert!(generic_stream.is_empty());

        // Have another subscriber.
        // Since it's not the first one, and the previous one loaded some more events,
        // the second subscribers sees them all.
        let (events2, stream2) = thread_event_cache.subscribe().await.unwrap();
        assert_eq!(events2.len(), 3);
        assert_eq!(events2[0].event_id(), Some(thread_id));
        assert_eq!(events2[1].event_id(), Some(event_id_0));
        assert_eq!(events2[2].event_id(), Some(event_id_1));
        assert!(stream2.is_empty());

        // Grab a receiver for testing no diffs is sent.
        let subscriber = {
            let state = thread_event_cache.inner.state.read().await.unwrap();
            state.update_sender.new_thread_receiver()
        };

        // Drop the first stream, and wait a bit.
        drop(stream1);
        yield_now().await;

        // The second stream remains undisturbed.
        assert!(stream2.is_empty());

        // Now drop the second stream, and wait a bit.
        drop(stream2);
        yield_now().await;

        // The linked chunk must have auto-shrunk by now.

        {
            // Check the inner state: there's no more shared auto-shrinker.
            let state = thread_event_cache.inner.state.read().await.unwrap();
            assert_eq!(state.subscribers_handle().count(), 0);

            // No diff is sent when the linked chunk has auto-shrunk.
            assert!(subscriber.is_empty());
            assert!(generic_stream.is_empty());
        }

        // Getting the events will only give us the latest chunk.
        let events3 = thread_event_cache
            .inner
            .state
            .read()
            .await
            .unwrap()
            .thread_linked_chunk()
            .events()
            .map(|(_position, item)| item.clone())
            .collect::<Vec<_>>();
        assert_eq!(events3.len(), 1);
        assert_eq!(events3[0].event_id(), Some(event_id_1));
    }
}
