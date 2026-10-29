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

//! Removal of expired events from the store, as defined by [MSC1763]
//! (configurable message retention periods).
//!
//! An event is expired if it's a non-state event whose `origin_server_ts` is
//! older than a given timestamp. The most recent event of a room never
//! expires.
//!
//! Threads get a special treatment:
//!
//! - if all the events of a thread are expired, the thread is removed entirely:
//!   its linked chunk, its [`ThreadInfo`], and its thread root event if it's
//!   expired too,
//! - otherwise, if the thread root event is expired, it is replaced by a
//!   placeholder (see [`expired_thread_root_placeholder`]) so that the thread
//!   can still be displayed.
//!
//! [MSC1763]: https://github.com/matrix-org/matrix-spec-proposals/pull/1763
//! [`ThreadInfo`]: matrix_sdk_base::event_cache::thread::ThreadInfo

use std::collections::{BTreeMap, BTreeSet};

use matrix_sdk_base::{
    event_cache::{
        Event, Gap,
        retention::{
            expired_thread_root_placeholder, is_expired, is_expired_thread_root_placeholder,
        },
        store::EventCacheStoreLockGuard,
    },
    linked_chunk::{ChunkContent, LinkedChunkId, Position, Update},
    serde_helpers::extract_thread_root,
};
use ruma::{MilliSecondsSinceUnixEpoch, OwnedEventId, RoomId};
use tracing::{debug, instrument};

use super::{EventCacheError, caches::event_linked_chunk::sort_positions_descending};

/// The outcome of [`remove_expired_events_from_store`].
#[derive(Debug, Default)]
pub(super) struct RemovedExpiredEvents {
    /// The removed events.
    pub removed_events: BTreeSet<OwnedEventId>,

    /// The thread root events replaced by a placeholder.
    pub replaced_thread_roots: BTreeSet<OwnedEventId>,

    /// The threads that still exist, but in which some events have been
    /// removed.
    pub updated_threads: BTreeSet<OwnedEventId>,

    /// The threads that have been removed entirely.
    pub removed_threads: BTreeSet<OwnedEventId>,
}

impl RemovedExpiredEvents {
    /// Whether nothing has been changed.
    pub fn is_empty(&self) -> bool {
        self.removed_events.is_empty()
            && self.replaced_thread_roots.is_empty()
            && self.removed_threads.is_empty()
    }
}

/// Remove all the expired events of a room from the store, i.e. non-state
/// events whose `origin_server_ts` is strictly older than `before`.
///
/// This only touches the store: the in-memory linked chunks must be reloaded
/// afterwards.
#[instrument(skip(store))]
pub(super) async fn remove_expired_events_from_store(
    store: &EventCacheStoreLockGuard,
    room_id: &RoomId,
    before: MilliSecondsSinceUnixEpoch,
) -> Result<RemovedExpiredEvents, EventCacheError> {
    let mut outcome = RemovedExpiredEvents::default();

    let events = store.get_room_events(room_id, None, None).await?;

    // The most recent event of the room never expires.
    let latest_event_id = find_latest_event_id(store, room_id).await?;

    let mut expired_events = BTreeSet::new();
    let mut thread_replies = BTreeMap::<OwnedEventId, Vec<OwnedEventId>>::new();
    let mut events_by_id = BTreeMap::new();

    for event in events {
        let Some(event_id) = event.event_id().map(ToOwned::to_owned) else {
            continue;
        };

        if let Some(thread_root) = extract_thread_root(event.raw()) {
            thread_replies.entry(thread_root).or_default().push(event_id.clone());
        }

        if latest_event_id.as_ref() != Some(&event_id) && is_expired(&event, before) {
            expired_events.insert(event_id.clone());
        }

        events_by_id.insert(event_id, event);
    }

    // Placeholders replacing the expired thread roots.
    let mut placeholders = Vec::new();

    for (thread_root, replies) in &thread_replies {
        let root = events_by_id.get(thread_root);
        let root_is_expired = expired_events.contains(thread_root)
            || root.is_some_and(is_expired_thread_root_placeholder);

        if replies.iter().all(|reply| expired_events.contains(reply)) {
            // All the events of the thread are expired: remove the thread
            // entirely.
            store
                .handle_linked_chunk_updates(
                    LinkedChunkId::Thread(room_id, thread_root),
                    vec![Update::Clear],
                )
                .await?;
            store.remove_thread_info(room_id, thread_root).await?;

            if root_is_expired {
                expired_events.insert(thread_root.clone());
            }

            outcome.removed_threads.insert(thread_root.clone());
        } else {
            if expired_events.remove(thread_root)
                && let Some(placeholder) = root.and_then(expired_thread_root_placeholder)
            {
                placeholders.push(placeholder);
                outcome.replaced_thread_roots.insert(thread_root.clone());
            }
        }
    }

    if expired_events.is_empty() && placeholders.is_empty() {
        return Ok(outcome);
    }

    // Remove the expired events and the thread roots replaced by a placeholder
    // from the room and the pinned events linked chunks…
    let removed_from_room = expired_events
        .iter()
        .cloned()
        .chain(placeholders.iter().filter_map(|placeholder| placeholder.event_id().map(Into::into)))
        .collect::<Vec<_>>();

    for linked_chunk_id in [LinkedChunkId::Room(room_id), LinkedChunkId::PinnedEvents(room_id)] {
        remove_events_from_linked_chunk(store, linked_chunk_id, removed_from_room.clone()).await?;
    }

    // … and the expired events from the remaining thread linked chunks.
    let removed_from_threads = expired_events.iter().cloned().collect::<Vec<_>>();

    for thread_root in thread_replies.keys() {
        if outcome.removed_threads.contains(thread_root) {
            continue;
        }

        if remove_events_from_linked_chunk(
            store,
            LinkedChunkId::Thread(room_id, thread_root),
            removed_from_threads.clone(),
        )
        .await?
        {
            outcome.updated_threads.insert(thread_root.clone());
        }
    }

    // Replace the expired thread roots by their placeholders. It updates them
    // in all the linked chunks they are still part of, or saves them
    // out-of-band otherwise.
    for placeholder in placeholders {
        store.save_event(room_id, placeholder).await?;
    }

    // Finally, remove the content of the expired events, now that they aren't
    // part of any linked chunk.
    store.remove_unlinked_events(room_id, removed_from_threads).await?;

    debug!(
        removed_events = expired_events.len(),
        replaced_thread_roots = outcome.replaced_thread_roots.len(),
        removed_threads = outcome.removed_threads.len(),
        "removed expired events",
    );

    outcome.removed_events = expired_events;

    Ok(outcome)
}

/// Remove the given events from a linked chunk in the store, if they are part
/// of it.
///
/// Returns whether at least one event has been removed.
async fn remove_events_from_linked_chunk(
    store: &EventCacheStoreLockGuard,
    linked_chunk_id: LinkedChunkId<'_>,
    event_ids: Vec<OwnedEventId>,
) -> Result<bool, EventCacheError> {
    let mut positions = store
        .filter_duplicated_events(linked_chunk_id, event_ids)
        .await?
        .into_iter()
        .map(|(_event_id, position)| position)
        .collect::<Vec<Position>>();

    if positions.is_empty() {
        return Ok(false);
    }

    // Positions must be sorted in descending order, so that removing an event
    // doesn't shift the positions of the next events to remove.
    sort_positions_descending(&mut positions);

    let updates = positions
        .into_iter()
        .map(|at| Update::RemoveItem { at })
        .collect::<Vec<Update<Event, Gap>>>();

    store.handle_linked_chunk_updates(linked_chunk_id, updates).await?;

    Ok(true)
}

/// Find the ID of the most recent event of the room linked chunk in the store.
async fn find_latest_event_id(
    store: &EventCacheStoreLockGuard,
    room_id: &RoomId,
) -> Result<Option<OwnedEventId>, EventCacheError> {
    let linked_chunk_id = LinkedChunkId::Room(room_id);

    let (mut chunk, _) = store.load_last_chunk(linked_chunk_id).await?;

    while let Some(current_chunk) = chunk {
        if let ChunkContent::Items(events) = &current_chunk.content
            && let Some(event_id) = events.iter().rev().find_map(|event| event.event_id())
        {
            return Ok(Some(event_id.to_owned()));
        }

        chunk = store.load_previous_chunk(linked_chunk_id, current_chunk.identifier).await?;
    }

    Ok(None)
}

#[cfg(all(test, not(target_family = "wasm")))]
mod tests {
    use std::sync::Arc;

    use assert_matches::assert_matches;
    use matrix_sdk_base::{
        RoomState, ThreadingSupport,
        cross_process_lock::CrossProcessLockConfig,
        event_cache::{
            Event,
            retention::is_expired_thread_root_placeholder,
            store::{EventCacheStore as _, MemoryStore},
        },
        linked_chunk::{ChunkContent, ChunkIdentifier, LinkedChunkId, Position, Update},
        store::StoreConfig,
    };
    use matrix_sdk_test::{User, async_test, event_factory::EventFactory};
    use ruma::{EventId, MilliSecondsSinceUnixEpoch, RoomId, event_id, room_id, uint};

    use crate::{Client, test_utils::client::MockClientBuilder};

    /// Push `events` in a new linked chunk in the store.
    async fn push_events(
        store: &MemoryStore,
        linked_chunk_id: LinkedChunkId<'_>,
        events: Vec<Event>,
    ) {
        store
            .handle_linked_chunk_updates(
                linked_chunk_id,
                vec![
                    Update::NewItemsChunk {
                        previous: None,
                        new: ChunkIdentifier::new(0),
                        next: None,
                    },
                    Update::PushItems {
                        at: Position::new(ChunkIdentifier::new(0), 0),
                        items: events,
                    },
                ],
            )
            .await
            .unwrap();
    }

    /// Return the IDs of the events of a linked chunk in the store.
    async fn stored_event_ids(
        store: &MemoryStore,
        linked_chunk_id: LinkedChunkId<'_>,
    ) -> Vec<String> {
        store
            .load_all_chunks(linked_chunk_id)
            .await
            .unwrap()
            .into_iter()
            .flat_map(|chunk| match chunk.content {
                ChunkContent::Items(events) => events,
                ChunkContent::Gap(_) => Vec::new(),
            })
            .map(|event| event.event_id().unwrap().to_string())
            .collect()
    }

    async fn client_with_store(store: &Arc<MemoryStore>, room_id: &RoomId) -> Client {
        let client = MockClientBuilder::new(None)
            .on_builder(|builder| {
                builder
                    .store_config(
                        StoreConfig::new(CrossProcessLockConfig::multi_process("hodor"))
                            .event_cache_store(store.clone()),
                    )
                    .with_threading_support(ThreadingSupport::Enabled { with_subscriptions: true })
            })
            .build()
            .await;

        client.base_client().get_or_create_room(room_id, RoomState::Joined);

        client
    }

    fn ids(event_ids: &[&EventId]) -> Vec<String> {
        event_ids.iter().map(ToString::to_string).collect()
    }

    #[async_test]
    async fn test_remove_expired_events() {
        let room_id = room_id!("!r0");
        let f = EventFactory::new().room(room_id).sender(User::Alice);

        let store = Arc::new(MemoryStore::new());

        push_events(
            &store,
            LinkedChunkId::Room(room_id),
            vec![
                f.text_msg("old").event_id(event_id!("$old")).server_ts(10).into(),
                f.room_name("name").event_id(event_id!("$old_state")).server_ts(10).into(),
                f.text_msg("recent").event_id(event_id!("$recent")).server_ts(200).into(),
                f.text_msg("old latest").event_id(event_id!("$old_latest")).server_ts(20).into(),
            ],
        )
        .await;
        store
            .save_event(
                room_id,
                f.reaction(event_id!("$recent"), "👍")
                    .event_id(event_id!("$old_out_of_band"))
                    .server_ts(10)
                    .into(),
            )
            .await
            .unwrap();

        let client = client_with_store(&store, room_id).await;
        let event_cache = client.event_cache().unwrap();

        let (room_event_cache, _drop_handles) = event_cache.room(room_id).await.unwrap();
        let (events, _subscriber) = room_event_cache.subscribe().await.unwrap();
        assert_eq!(events.len(), 4);

        let mut generic_updates = event_cache.subscribe_to_room_generic_updates();

        event_cache
            .remove_expired_events(None, MilliSecondsSinceUnixEpoch(uint!(100)))
            .await
            .unwrap();

        // The expired event is removed, but not the state event, the recent
        // event, and the most recent event of the room even if it's expired.
        let expected =
            ids(&[event_id!("$old_state"), event_id!("$recent"), event_id!("$old_latest")]);

        let in_memory = room_event_cache
            .events()
            .await
            .unwrap()
            .into_iter()
            .map(|event| event.event_id().unwrap().to_string())
            .collect::<Vec<_>>();
        assert_eq!(in_memory, expected);
        assert_eq!(stored_event_ids(&store, LinkedChunkId::Room(room_id)).await, expected);

        // The expired events are removed from the store, even out-of-band ones.
        assert!(store.find_event(room_id, event_id!("$old")).await.unwrap().is_none());
        assert!(store.find_event(room_id, event_id!("$old_out_of_band")).await.unwrap().is_none());
        assert!(room_event_cache.find_event(event_id!("$old")).await.unwrap().is_none());

        // Observers are notified that the room has changed.
        assert_matches!(generic_updates.recv().await, Ok(update) => {
            assert_eq!(update.room_id, room_id);
        });
    }

    #[async_test]
    async fn test_remove_expired_events_in_threads() {
        let room_id = room_id!("!r0");
        let f = EventFactory::new().room(room_id).sender(User::Alice);

        let root_a = event_id!("$root_a");
        let root_b = event_id!("$root_b");
        let root_c = event_id!("$root_c");

        let store = Arc::new(MemoryStore::new());

        push_events(
            &store,
            LinkedChunkId::Room(room_id),
            vec![
                f.text_msg("root a").event_id(root_a).server_ts(10).into(),
                f.text_msg("root b").event_id(root_b).server_ts(10).into(),
                f.text_msg("root c").event_id(root_c).server_ts(200).into(),
                f.text_msg("latest").event_id(event_id!("$latest")).server_ts(300).into(),
            ],
        )
        .await;

        // Thread A: an expired reply, and a recent one.
        push_events(
            &store,
            LinkedChunkId::Thread(room_id, root_a),
            vec![
                f.text_msg("a1")
                    .event_id(event_id!("$a1"))
                    .in_thread(root_a, root_a)
                    .server_ts(10)
                    .into(),
                f.text_msg("a2")
                    .event_id(event_id!("$a2"))
                    .in_thread(root_a, event_id!("$a1"))
                    .server_ts(200)
                    .into(),
            ],
        )
        .await;

        // Thread B: an expired reply only, and an expired root.
        push_events(
            &store,
            LinkedChunkId::Thread(room_id, root_b),
            vec![
                f.text_msg("b1")
                    .event_id(event_id!("$b1"))
                    .in_thread(root_b, root_b)
                    .server_ts(10)
                    .into(),
            ],
        )
        .await;

        // Thread C: an expired reply only, but a recent root.
        push_events(
            &store,
            LinkedChunkId::Thread(room_id, root_c),
            vec![
                f.text_msg("c1")
                    .event_id(event_id!("$c1"))
                    .in_thread(root_c, root_c)
                    .server_ts(10)
                    .into(),
            ],
        )
        .await;

        for thread_root in [root_a, root_b, root_c] {
            store.load_thread_info(room_id, thread_root, true).await.unwrap();
        }

        let client = client_with_store(&store, room_id).await;
        let event_cache = client.event_cache().unwrap();

        // Load thread B in memory, to check it's forgotten.
        let (_thread_b, _drop_handles) = event_cache.thread(room_id, root_b).await.unwrap();

        event_cache
            .remove_expired_events(Some(room_id), MilliSecondsSinceUnixEpoch(uint!(100)))
            .await
            .unwrap();

        // The expired thread roots are removed from the room.
        assert_eq!(
            stored_event_ids(&store, LinkedChunkId::Room(room_id)).await,
            ids(&[root_c, event_id!("$latest")])
        );

        // Thread A still exists, with its expired root replaced by a
        // placeholder.
        assert_eq!(
            stored_event_ids(&store, LinkedChunkId::Thread(room_id, root_a)).await,
            ids(&[event_id!("$a2")])
        );
        assert!(store.find_event(room_id, event_id!("$a1")).await.unwrap().is_none());

        let placeholder = store.find_event(room_id, root_a).await.unwrap().unwrap();
        assert!(is_expired_thread_root_placeholder(&placeholder));

        let thread_info = event_cache.thread_info(room_id, root_a).await.unwrap().unwrap();
        assert_eq!(thread_info.number_of_replies, 1);
        assert_eq!(thread_info.latest_event.as_deref(), Some(event_id!("$a2")));

        // Thread B is removed entirely, with its root.
        assert!(stored_event_ids(&store, LinkedChunkId::Thread(room_id, root_b)).await.is_empty());
        assert!(event_cache.thread_info(room_id, root_b).await.unwrap().is_none());
        assert!(store.find_event(room_id, root_b).await.unwrap().is_none());
        assert!(store.find_event(room_id, event_id!("$b1")).await.unwrap().is_none());

        // Thread C is removed entirely, but its root is kept as is.
        assert!(stored_event_ids(&store, LinkedChunkId::Thread(room_id, root_c)).await.is_empty());
        assert!(event_cache.thread_info(room_id, root_c).await.unwrap().is_none());
        let root = store.find_event(room_id, root_c).await.unwrap().unwrap();
        assert!(!is_expired_thread_root_placeholder(&root));
    }

    #[async_test]
    async fn test_remove_expired_events_updates_unread_counts() {
        let room_id = room_id!("!r0");
        let f = EventFactory::new().room(room_id).sender(User::Bob);

        let store = Arc::new(MemoryStore::new());

        push_events(
            &store,
            LinkedChunkId::Room(room_id),
            vec![
                f.text_msg("old 1").event_id(event_id!("$old1")).server_ts(10).into(),
                f.text_msg("old 2").event_id(event_id!("$old2")).server_ts(10).into(),
                f.text_msg("recent").event_id(event_id!("$recent")).server_ts(200).into(),
            ],
        )
        .await;

        let client = client_with_store(&store, room_id).await;
        let event_cache = client.event_cache().unwrap();

        let (room_event_cache, _drop_handles) = event_cache.room(room_id).await.unwrap();
        room_event_cache.state().write().await.unwrap().update_read_receipts(None).await.unwrap();

        let room = client.get_room(room_id).unwrap();
        assert_eq!(room.read_receipts().num_unread, 3);

        event_cache
            .remove_expired_events(Some(room_id), MilliSecondsSinceUnixEpoch(uint!(100)))
            .await
            .unwrap();

        assert_eq!(room.read_receipts().num_unread, 1);
    }
}
