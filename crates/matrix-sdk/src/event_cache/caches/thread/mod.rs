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

use matrix_sdk_base::event_cache::{Event, store::EventCacheStoreLock};
use ruma::{EventId, OwnedEventId, OwnedRoomId, OwnedUserId};
use tokio::sync::{
    Notify,
    broadcast::{Receiver, Sender},
};
use tracing::{error, trace};

pub(super) use self::state::LockedThreadEventCacheState;
use self::{pagination::ThreadPagination, updates::ThreadEventCacheUpdateSender};
use super::{
    super::Result,
    EventsOrigin, TimelineVectorDiffs,
    room::{RoomEventCacheGenericUpdate, RoomEventCacheLinkedChunkUpdate},
};
use crate::room::WeakRoom;

/// All the information related to a single thread.
pub(super) struct ThreadEventCache {
    inner: Arc<ThreadEventCacheInner>,
}

/// The (non-cloneable) details of the `RoomEventCache`.
struct ThreadEventCacheInner {
    /// The room ID.
    room_id: OwnedRoomId,

    /// The thread root ID.
    thread_id: OwnedEventId,

    /// The room where this thread belongs to.
    weak_room: WeakRoom,

    /// State for this thread's event cache.
    state: LockedThreadEventCacheState,

    /// A notifier that we received a new pagination token.
    pagination_batch_token_notifier: Notify,

    /// Update sender for this room.
    update_sender: ThreadEventCacheUpdateSender,
}

impl fmt::Debug for ThreadEventCache {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ThreadEventCache").finish_non_exhaustive()
    }
}

impl ThreadEventCache {
    /// Create a new empty thread event cache.
    pub async fn new(
        room_id: OwnedRoomId,
        thread_id: OwnedEventId,
        own_user_id: OwnedUserId,
        weak_room: WeakRoom,
        store: EventCacheStoreLock,
        generic_update_sender: Sender<RoomEventCacheGenericUpdate>,
        linked_chunk_update_sender: Sender<RoomEventCacheLinkedChunkUpdate>,
    ) -> Result<Self> {
        let update_sender = ThreadEventCacheUpdateSender::new(generic_update_sender);

        Ok(Self {
            inner: Arc::new(ThreadEventCacheInner {
                room_id: room_id.clone(),
                thread_id: thread_id.clone(),
                weak_room,
                state: LockedThreadEventCacheState::new(
                    room_id,
                    thread_id,
                    own_user_id,
                    store,
                    update_sender.clone(),
                    linked_chunk_update_sender,
                )
                .await?,
                pagination_batch_token_notifier: Notify::new(),
                update_sender,
            }),
        })
    }

    /// Subscribe to live events from this thread.
    pub async fn subscribe(&self) -> Result<(Vec<Event>, Receiver<TimelineVectorDiffs>)> {
        let state = self.inner.state.read().await?;

        let events =
            state.thread_linked_chunk().events().map(|(_position, item)| item.clone()).collect();

        let recv = self.inner.update_sender.new_thread_receiver();

        Ok((events, recv))
    }

    /// Return a [`ThreadPagination`] useful for running back-pagination queries
    /// in this thread.
    pub fn pagination(&self) -> ThreadPagination {
        ThreadPagination::new(self.inner.clone())
    }

    /// Clear a thread.
    pub async fn clear(&mut self) -> Result<()> {
        let updates_as_vector_diffs = self.inner.state.write().await?.reset().await?;

        if !updates_as_vector_diffs.is_empty() {
            self.inner.update_sender.send(
                TimelineVectorDiffs { diffs: updates_as_vector_diffs, origin: EventsOrigin::Cache },
                // This function is part of the `RoomEventCache` flow. The generic update is
                // handled by it.
                None,
            );
        }

        Ok(())
    }

    /// Push some live events to this thread, and propagate the updates to
    /// the listeners.
    pub async fn add_live_events(
        &mut self,
        events: Vec<Event>,
        prev_batch_token: &Option<String>,
    ) -> Result<()> {
        if events.is_empty() {
            return Ok(());
        }

        trace!("adding new events");

        let (stored_prev_batch_token, timeline_event_diffs) =
            self.inner.state.write().await?.handle_sync(events, prev_batch_token).await?;

        // Now that all events have been added, we can trigger the
        // `pagination_token_notifier`.
        if stored_prev_batch_token {
            self.inner.pagination_batch_token_notifier.notify_one();
        }

        if !timeline_event_diffs.is_empty() {
            self.inner.update_sender.send(
                TimelineVectorDiffs { diffs: timeline_event_diffs, origin: EventsOrigin::Sync },
                // This function is part of the `RoomEventCache` flow. The generic update is
                // handled by it.
                None,
            );
        }

        Ok(())
    }

    /// Replaces a single event, be it saved in memory or in the store.
    ///
    /// If it was saved in memory, this will emit a notification to
    /// observers that a single item has been replaced. Otherwise,
    /// such a notification is not emitted, because observers are
    /// unlikely to observe the store updates directly.
    pub(super) async fn replace_event_if_present(
        &mut self,
        event_id: &EventId,
        new_event: Event,
    ) -> Result<()> {
        let mut state = self.inner.state.write().await?;

        if let Err(err) = state.replace_event_if_present(event_id, new_event).await {
            error!(%err, "failed to replace an event");
            return Err(err);
        }

        let timeline_event_diffs = state.thread_linked_chunk_mut().updates_as_vector_diffs();

        if !timeline_event_diffs.is_empty() {
            self.inner.update_sender.send(
                TimelineVectorDiffs { diffs: timeline_event_diffs, origin: EventsOrigin::Sync },
                // This function is part of the `RoomEventCache` flow. The generic update is
                // handled by it.
                None,
            );
        }

        Ok(())
    }

    /// Returns the latest event ID in this thread, if any.
    pub async fn latest_event_id(&self) -> Result<Option<OwnedEventId>> {
        Ok(self
            .inner
            .state
            .read()
            .await?
            .thread_linked_chunk()
            .revents()
            .next()
            .and_then(|(_position, event)| event.event_id()))
    }
}

#[cfg(all(test, not(target_family = "wasm")))] // This uses the cross-process lock, so needs time support.
mod timed_tests {
    use std::sync::Arc;

    use assert_matches::assert_matches;
    use assert_matches2::assert_let;
    use eyeball_im::VectorDiff;
    use matrix_sdk_base::{
        RoomState, ThreadingSupport,
        cross_process_lock::CrossProcessLockConfig,
        event_cache::store::{EventCacheStore as _, MemoryStore},
        linked_chunk::{ChunkContent, LinkedChunkId, lazy_loader::from_all_chunks},
        store::StoreConfig,
        sync::{JoinedRoomUpdate, Timeline},
    };
    use matrix_sdk_test::{async_test, event_factory::EventFactory};
    use ruma::{
        event_id,
        events::{AnySyncMessageLikeEvent, AnySyncTimelineEvent},
        room_id, user_id,
    };

    use super::super::{
        super::RoomEventCacheGenericUpdate, TimelineVectorDiffs, room::RoomEventCacheUpdate,
    };
    use crate::test_utils::client::MockClientBuilder;

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
        let room = client.get_room(room_id).unwrap();

        let mut generic_stream = event_cache.subscribe_to_room_generic_updates();
        let (room_event_cache, _drop_handles) = room.event_cache().await.unwrap();
        let (room_events, mut room_stream) = room_event_cache.subscribe().await.unwrap();
        let (thread_events, mut thread_stream) =
            room_event_cache.subscribe_to_thread(thread_root.to_owned()).await.unwrap();

        assert!(room_events.is_empty());
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

        room_event_cache
            .handle_joined_room_update(JoinedRoomUpdate { timeline, ..Default::default() })
            .await
            .unwrap();

        // Checking the update are corrects.
        assert_matches!(
            generic_stream.recv().await,
            Ok(RoomEventCacheGenericUpdate { room_id: expected_room_id }) => {
                assert_eq!(expected_room_id, room_id);
            }
        );
        assert!(generic_stream.is_empty());

        assert_matches!(
            room_stream.recv().await,
            Ok(RoomEventCacheUpdate::UpdateTimelineEvents(TimelineVectorDiffs { diffs, .. })) => {
                assert_eq!(diffs.len(), 2);
                assert_matches!(&diffs[0], VectorDiff::Clear);
                assert_matches!(&diffs[1], VectorDiff::Append { values: events } => {
                    assert_eq!(events.len(), 1);
                    assert_eq!(events[0].event_id().as_deref(), Some(thread_event_id_0));
                });
            }
        );
        assert!(room_stream.is_empty());

        assert_matches!(
            thread_stream.recv().await,
            Ok(TimelineVectorDiffs { diffs, .. }) => {
                assert_eq!(diffs.len(), 1);
                assert_matches!(&diffs[0], VectorDiff::Append { values: events } => {
                    assert_eq!(events.len(), 1);
                    assert_eq!(events[0].event_id().as_deref(), Some(thread_event_id_0));
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
            assert_eq!(events[0].event_id().as_deref(), Some(thread_event_id_0));
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
        let room = client.get_room(room_id).unwrap();

        let mut generic_stream = event_cache.subscribe_to_room_generic_updates();
        let (room_event_cache, _drop_handles) = room.event_cache().await.unwrap();

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

        room_event_cache
            .handle_joined_room_update(JoinedRoomUpdate { timeline, ..Default::default() })
            .await
            .unwrap();

        // Just checking the generic update is correct.
        assert_matches!(
            generic_stream.recv().await,
            Ok(RoomEventCacheGenericUpdate { room_id: expected_room_id }) => {
                assert_eq!(expected_room_id, room_id);
            }
        );
        assert!(generic_stream.is_empty());

        // The in-memory linked chunk keeps the bundled relation.
        {
            let events = room_event_cache.events().await.unwrap();

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
            event_cache_store.load_all_chunks(LinkedChunkId::Room(room_id)).await.unwrap(),
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
}
