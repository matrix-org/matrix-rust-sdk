// Copyright 2024 The Matrix.org Foundation C.I.C.
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

//! Trait and macro of integration tests for `EventCacheStore` implementations.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

use assert_matches::assert_matches;
use assert_matches2::assert_let;
use matrix_sdk_common::{
    deserialized_responses::{
        AlgorithmInfo, DecryptedRoomEvent, EncryptionInfo, TimelineEvent, TimelineEventKind,
        UnableToDecryptInfo, UnableToDecryptReason, VerificationState,
    },
    linked_chunk::{
        ChunkContent, ChunkIdentifier as CId, LinkedChunkId, Position, Update, lazy_loader,
    },
};
use matrix_sdk_test::{ALICE, DEFAULT_TEST_ROOM_ID, event_factory::EventFactory};
use ruma::{
    EventId, RoomId, event_id,
    events::{
        AnyMessageLikeEvent, AnyTimelineEvent, relation::RelationType,
        room::message::RoomMessageEventContentWithoutRelation,
    },
    push::Action,
    room_id,
};

use super::DynEventCacheStore;
use crate::event_cache::{Gap, store::DEFAULT_CHUNK_CAPACITY};

/// Create a test event with all data filled, for testing that linked chunk
/// correctly stores event data.
///
/// Keep in sync with [`check_test_event`].
pub fn make_test_event(room_id: &RoomId, content: &str) -> TimelineEvent {
    make_test_event_with_event_id(room_id, content, None)
}

/// Create a `m.room.encrypted` test event with all data filled, for testing
/// that linked chunk correctly stores event data for encrypted events.
pub fn make_encrypted_test_event(room_id: &RoomId, session_id: &str) -> TimelineEvent {
    let device_id = "DEVICEID";
    let builder = EventFactory::new()
        .encrypted("", "curve_key", device_id, session_id)
        .room(room_id)
        .sender(*ALICE);

    let event = builder.into_raw();
    let utd_info = UnableToDecryptInfo {
        session_id: Some(session_id.to_owned()),
        reason: UnableToDecryptReason::MissingMegolmSession { withheld_code: None },
    };

    TimelineEvent::from_utd(event, utd_info)
}

/// Same as [`make_test_event`], with an extra event id.
pub fn make_test_event_with_event_id(
    room_id: &RoomId,
    content: &str,
    event_id: Option<&EventId>,
) -> TimelineEvent {
    let encryption_info = Arc::new(EncryptionInfo {
        sender: (*ALICE).into(),
        sender_device: None,
        forwarder: None,
        algorithm_info: AlgorithmInfo::MegolmV1AesSha2 {
            curve25519_key: "1337".to_owned(),
            sender_claimed_keys: Default::default(),
            session_id: Some("mysessionid9".to_owned()),
        },
        verification_state: VerificationState::Verified,
    });

    let mut builder = EventFactory::new().text_msg(content).room(room_id).sender(*ALICE);
    if let Some(event_id) = event_id {
        builder = builder.event_id(event_id);
    }
    let event = builder.into_raw();

    TimelineEvent::from_decrypted(
        DecryptedRoomEvent { event, encryption_info, unsigned_encryption_info: None },
        Some(vec![Action::Notify]),
    )
}

/// Check that an event created with [`make_test_event`] contains the expected
/// data.
///
/// Keep in sync with [`make_test_event`].
#[track_caller]
pub fn check_test_event(event: &TimelineEvent, text: &str) {
    // Check push actions.
    let actions = event.push_actions().unwrap();
    assert_eq!(actions.len(), 1);
    assert_matches!(&actions[0], Action::Notify);

    // Check content.
    assert_matches!(&event.kind, TimelineEventKind::Decrypted(d) => {
        // Check encryption fields.
        assert_eq!(d.encryption_info.sender, *ALICE);
        assert_matches!(&d.encryption_info.algorithm_info, AlgorithmInfo::MegolmV1AesSha2 { curve25519_key, .. } => {
            assert_eq!(curve25519_key, "1337");
        });

        // Check event.
        let deserialized = d.event.deserialize().unwrap();
        assert_matches!(deserialized, AnyTimelineEvent::MessageLike(AnyMessageLikeEvent::RoomMessage(msg)) => {
            assert_eq!(msg.as_original().unwrap().content.body(), text);
        });
    });
}

/// `EventCacheStore` integration tests.
///
/// This trait is not meant to be used directly, but will be used with the
/// `event_cache_store_integration_tests!` macro.
#[allow(async_fn_in_trait)]
pub trait EventCacheStoreIntegrationTests {
    /// Test handling updates to a linked chunk and reloading these updates from
    /// the store.
    async fn test_handle_updates_and_rebuild_linked_chunk(&self);

    /// Test loading a linked chunk incrementally (chunk by chunk) from the
    /// store.
    async fn test_linked_chunk_incremental_loading(&self);

    /// Test that rebuilding a linked chunk from an empty store doesn't return
    /// anything.
    async fn test_rebuild_empty_linked_chunk(&self);

    /// Test that loading a linked chunk's metadata works as intended.
    async fn test_load_all_chunks_metadata(&self);

    /// Test that clear all the rooms' linked chunks works.
    async fn test_clear_all_linked_chunks(&self);

    /// Test that removing a room from storage empties all associated data.
    async fn test_remove_room(&self);

    /// Test that filtering duplicated events works as expected.
    async fn test_filter_duplicated_events(&self);

    /// Test that an event can be found or not.
    async fn test_find_event(&self);

    /// Test that finding event relations works as expected.
    async fn test_find_event_relations(&self);

    /// Test that getting all events in a room works as expected.
    async fn test_get_room_events(&self);

    /// Test that getting events in a room of a certain type works as expected.
    async fn test_get_room_events_filtered(&self);

    /// Test that saving an event works as expected.
    async fn test_save_event(&self);

    /// Test multiple things related to distinguishing a thread linked chunk
    /// from a room linked chunk.
    async fn test_thread_vs_room_linked_chunk(&self);
}

impl EventCacheStoreIntegrationTests for DynEventCacheStore {
    async fn test_handle_updates_and_rebuild_linked_chunk(&self) {
        let room_id = room_id!("!r0:matrix.org");
        let linked_chunk_id = LinkedChunkId::Room(room_id);

        self.handle_linked_chunk_updates(
            linked_chunk_id,
            vec![
                // new chunk
                Update::NewItemsChunk { previous: None, new: CId::new(0), next: None },
                // new items on 0
                Update::PushItems {
                    at: Position::new(CId::new(0), 0),
                    items: vec![
                        make_test_event(room_id, "hello"),
                        make_test_event(room_id, "world"),
                    ],
                },
                // a gap chunk
                Update::NewGapChunk {
                    previous: Some(CId::new(0)),
                    new: CId::new(1),
                    next: None,
                    gap: Gap { prev_token: "parmesan".to_owned() },
                },
                // another items chunk
                Update::NewItemsChunk { previous: Some(CId::new(1)), new: CId::new(2), next: None },
                // new items on 2
                Update::PushItems {
                    at: Position::new(CId::new(2), 0),
                    items: vec![make_test_event(room_id, "sup")],
                },
            ],
        )
        .await
        .unwrap();

        // The linked chunk is correctly reloaded.
        let lc = lazy_loader::from_all_chunks::<3, _, _>(
            self.load_all_chunks(linked_chunk_id).await.unwrap(),
        )
        .unwrap()
        .unwrap();

        let mut chunks = lc.chunks();

        {
            let first = chunks.next().unwrap();
            // Note: we can't assert the previous/next chunks, as these fields and their
            // getters are private.
            assert_eq!(first.identifier(), CId::new(0));

            assert_matches!(first.content(), ChunkContent::Items(events) => {
                assert_eq!(events.len(), 2);
                check_test_event(&events[0], "hello");
                check_test_event(&events[1], "world");
            });
        }

        {
            let second = chunks.next().unwrap();
            assert_eq!(second.identifier(), CId::new(1));

            assert_matches!(second.content(), ChunkContent::Gap(gap) => {
                assert_eq!(gap.prev_token, "parmesan");
            });
        }

        {
            let third = chunks.next().unwrap();
            assert_eq!(third.identifier(), CId::new(2));

            assert_matches!(third.content(), ChunkContent::Items(events) => {
                assert_eq!(events.len(), 1);
                check_test_event(&events[0], "sup");
            });
        }

        assert!(chunks.next().is_none());
    }

    async fn test_load_all_chunks_metadata(&self) {
        let room_id = room_id!("!r0:matrix.org");
        let linked_chunk_id = LinkedChunkId::Room(room_id);

        self.handle_linked_chunk_updates(
            linked_chunk_id,
            vec![
                // new chunk
                Update::NewItemsChunk { previous: None, new: CId::new(0), next: None },
                // new items on 0
                Update::PushItems {
                    at: Position::new(CId::new(0), 0),
                    items: vec![
                        make_test_event(room_id, "hello"),
                        make_test_event(room_id, "world"),
                    ],
                },
                // a gap chunk
                Update::NewGapChunk {
                    previous: Some(CId::new(0)),
                    new: CId::new(1),
                    next: None,
                    gap: Gap { prev_token: "parmesan".to_owned() },
                },
                // another items chunk
                Update::NewItemsChunk { previous: Some(CId::new(1)), new: CId::new(2), next: None },
                // new items on 2
                Update::PushItems {
                    at: Position::new(CId::new(2), 0),
                    items: vec![make_test_event(room_id, "sup")],
                },
                // and an empty items chunk to finish
                Update::NewItemsChunk { previous: Some(CId::new(2)), new: CId::new(3), next: None },
            ],
        )
        .await
        .unwrap();

        let metas = self.load_all_chunks_metadata(linked_chunk_id).await.unwrap();
        assert_eq!(metas.len(), 4);

        // The first chunk has two items.
        assert_eq!(metas[0].identifier, CId::new(0));
        assert_eq!(metas[0].previous, None);
        assert_eq!(metas[0].next, Some(CId::new(1)));
        assert_eq!(metas[0].num_items, 2);

        // The second chunk is a gap, so it has 0 items.
        assert_eq!(metas[1].identifier, CId::new(1));
        assert_eq!(metas[1].previous, Some(CId::new(0)));
        assert_eq!(metas[1].next, Some(CId::new(2)));
        assert_eq!(metas[1].num_items, 0);

        // The third event chunk has one item.
        assert_eq!(metas[2].identifier, CId::new(2));
        assert_eq!(metas[2].previous, Some(CId::new(1)));
        assert_eq!(metas[2].next, Some(CId::new(3)));
        assert_eq!(metas[2].num_items, 1);

        // The final event chunk is empty.
        assert_eq!(metas[3].identifier, CId::new(3));
        assert_eq!(metas[3].previous, Some(CId::new(2)));
        assert_eq!(metas[3].next, None);
        assert_eq!(metas[3].num_items, 0);
    }

    async fn test_linked_chunk_incremental_loading(&self) {
        let room_id = room_id!("!r0:matrix.org");
        let linked_chunk_id = LinkedChunkId::Room(room_id);
        let event = |msg: &str| make_test_event(room_id, msg);

        // Load the last chunk, but none exists yet.
        {
            let (last_chunk, chunk_identifier_generator) =
                self.load_last_chunk(linked_chunk_id).await.unwrap();

            assert!(last_chunk.is_none());
            assert_eq!(chunk_identifier_generator.current(), 0);
        }

        self.handle_linked_chunk_updates(
            linked_chunk_id,
            vec![
                // new chunk for items
                Update::NewItemsChunk { previous: None, new: CId::new(0), next: None },
                // new items on 0
                Update::PushItems {
                    at: Position::new(CId::new(0), 0),
                    items: vec![event("a"), event("b")],
                },
                // new chunk for a gap
                Update::NewGapChunk {
                    previous: Some(CId::new(0)),
                    new: CId::new(1),
                    next: None,
                    gap: Gap { prev_token: "morbier".to_owned() },
                },
                // new chunk for items
                Update::NewItemsChunk { previous: Some(CId::new(1)), new: CId::new(2), next: None },
                // new items on 2
                Update::PushItems {
                    at: Position::new(CId::new(2), 0),
                    items: vec![event("c"), event("d"), event("e")],
                },
            ],
        )
        .await
        .unwrap();

        // Load the last chunk.
        let mut linked_chunk = {
            let (last_chunk, chunk_identifier_generator) =
                self.load_last_chunk(linked_chunk_id).await.unwrap();

            assert_eq!(chunk_identifier_generator.current(), 2);

            let linked_chunk = lazy_loader::from_last_chunk::<DEFAULT_CHUNK_CAPACITY, _, _>(
                last_chunk,
                chunk_identifier_generator,
            )
            .unwrap() // unwrap the `Result`
            .unwrap(); // unwrap the `Option`

            let mut rchunks = linked_chunk.rchunks();

            // A unique chunk.
            assert_matches!(rchunks.next(), Some(chunk) => {
                assert_eq!(chunk.identifier(), 2);
                assert_eq!(chunk.lazy_previous(), Some(CId::new(1)));

                assert_matches!(chunk.content(), ChunkContent::Items(events) => {
                    assert_eq!(events.len(), 3);
                    check_test_event(&events[0], "c");
                    check_test_event(&events[1], "d");
                    check_test_event(&events[2], "e");
                });
            });

            assert!(rchunks.next().is_none());

            linked_chunk
        };

        // Load the previous chunk: this is a gap.
        {
            let first_chunk = linked_chunk.chunks().next().unwrap().identifier();
            let previous_chunk =
                self.load_previous_chunk(linked_chunk_id, first_chunk).await.unwrap().unwrap();

            lazy_loader::insert_new_first_chunk(&mut linked_chunk, previous_chunk).unwrap();

            let mut rchunks = linked_chunk.rchunks();

            // The last chunk.
            assert_matches!(rchunks.next(), Some(chunk) => {
                assert_eq!(chunk.identifier(), 2);
                assert!(chunk.lazy_previous().is_none());

                // Already asserted, but let's be sure nothing breaks.
                assert_matches!(chunk.content(), ChunkContent::Items(events) => {
                    assert_eq!(events.len(), 3);
                    check_test_event(&events[0], "c");
                    check_test_event(&events[1], "d");
                    check_test_event(&events[2], "e");
                });
            });

            // The new chunk.
            assert_matches!(rchunks.next(), Some(chunk) => {
                assert_eq!(chunk.identifier(), 1);
                assert_eq!(chunk.lazy_previous(), Some(CId::new(0)));

                assert_matches!(chunk.content(), ChunkContent::Gap(gap) => {
                    assert_eq!(gap.prev_token, "morbier");
                });
            });

            assert!(rchunks.next().is_none());
        }

        // Load the previous chunk: these are items.
        {
            let first_chunk = linked_chunk.chunks().next().unwrap().identifier();
            let previous_chunk =
                self.load_previous_chunk(linked_chunk_id, first_chunk).await.unwrap().unwrap();

            lazy_loader::insert_new_first_chunk(&mut linked_chunk, previous_chunk).unwrap();

            let mut rchunks = linked_chunk.rchunks();

            // The last chunk.
            assert_matches!(rchunks.next(), Some(chunk) => {
                assert_eq!(chunk.identifier(), 2);
                assert!(chunk.lazy_previous().is_none());

                // Already asserted, but let's be sure nothing breaks.
                assert_matches!(chunk.content(), ChunkContent::Items(events) => {
                    assert_eq!(events.len(), 3);
                    check_test_event(&events[0], "c");
                    check_test_event(&events[1], "d");
                    check_test_event(&events[2], "e");
                });
            });

            // Its previous chunk.
            assert_matches!(rchunks.next(), Some(chunk) => {
                assert_eq!(chunk.identifier(), 1);
                assert!(chunk.lazy_previous().is_none());

                // Already asserted, but let's be sure nothing breaks.
                assert_matches!(chunk.content(), ChunkContent::Gap(gap) => {
                    assert_eq!(gap.prev_token, "morbier");
                });
            });

            // The new chunk.
            assert_matches!(rchunks.next(), Some(chunk) => {
                assert_eq!(chunk.identifier(), 0);
                assert!(chunk.lazy_previous().is_none());

                assert_matches!(chunk.content(), ChunkContent::Items(events) => {
                    assert_eq!(events.len(), 2);
                    check_test_event(&events[0], "a");
                    check_test_event(&events[1], "b");
                });
            });

            assert!(rchunks.next().is_none());
        }

        // Load the previous chunk: there is none.
        {
            let first_chunk = linked_chunk.chunks().next().unwrap().identifier();
            let previous_chunk =
                self.load_previous_chunk(linked_chunk_id, first_chunk).await.unwrap();

            assert!(previous_chunk.is_none());
        }

        // One last check: a round of assert by using the forwards chunk iterator
        // instead of the backwards chunk iterator.
        {
            let mut chunks = linked_chunk.chunks();

            // The first chunk.
            assert_matches!(chunks.next(), Some(chunk) => {
                assert_eq!(chunk.identifier(), 0);
                assert!(chunk.lazy_previous().is_none());

                assert_matches!(chunk.content(), ChunkContent::Items(events) => {
                    assert_eq!(events.len(), 2);
                    check_test_event(&events[0], "a");
                    check_test_event(&events[1], "b");
                });
            });

            // The second chunk.
            assert_matches!(chunks.next(), Some(chunk) => {
                assert_eq!(chunk.identifier(), 1);
                assert!(chunk.lazy_previous().is_none());

                assert_matches!(chunk.content(), ChunkContent::Gap(gap) => {
                    assert_eq!(gap.prev_token, "morbier");
                });
            });

            // The third and last chunk.
            assert_matches!(chunks.next(), Some(chunk) => {
                assert_eq!(chunk.identifier(), 2);
                assert!(chunk.lazy_previous().is_none());

                assert_matches!(chunk.content(), ChunkContent::Items(events) => {
                    assert_eq!(events.len(), 3);
                    check_test_event(&events[0], "c");
                    check_test_event(&events[1], "d");
                    check_test_event(&events[2], "e");
                });
            });

            assert!(chunks.next().is_none());
        }
    }

    async fn test_rebuild_empty_linked_chunk(&self) {
        // When I rebuild a linked chunk from an empty store, it's empty.
        let linked_chunk = lazy_loader::from_all_chunks::<3, _, _>(
            self.load_all_chunks(LinkedChunkId::Room(&DEFAULT_TEST_ROOM_ID)).await.unwrap(),
        )
        .unwrap();
        assert!(linked_chunk.is_none());
    }

    async fn test_clear_all_linked_chunks(&self) {
        let r0 = room_id!("!r0:matrix.org");
        let linked_chunk_id0 = LinkedChunkId::Room(r0);
        let r1 = room_id!("!r1:matrix.org");
        let linked_chunk_id1 = LinkedChunkId::Room(r1);

        // Add updates for the first room.
        self.handle_linked_chunk_updates(
            linked_chunk_id0,
            vec![
                // new chunk
                Update::NewItemsChunk { previous: None, new: CId::new(0), next: None },
                // new items on 0
                Update::PushItems {
                    at: Position::new(CId::new(0), 0),
                    items: vec![make_test_event(r0, "hello"), make_test_event(r0, "world")],
                },
            ],
        )
        .await
        .unwrap();

        // Add updates for the second room.
        self.handle_linked_chunk_updates(
            linked_chunk_id1,
            vec![
                // Empty items chunk.
                Update::NewItemsChunk { previous: None, new: CId::new(0), next: None },
                // a gap chunk
                Update::NewGapChunk {
                    previous: Some(CId::new(0)),
                    new: CId::new(1),
                    next: None,
                    gap: Gap { prev_token: "bleu d'auvergne".to_owned() },
                },
                // another items chunk
                Update::NewItemsChunk { previous: Some(CId::new(1)), new: CId::new(2), next: None },
                // new items on 0
                Update::PushItems {
                    at: Position::new(CId::new(2), 0),
                    items: vec![make_test_event(r0, "yummy")],
                },
            ],
        )
        .await
        .unwrap();

        // Sanity check: both linked chunks can be reloaded.
        assert!(
            lazy_loader::from_all_chunks::<3, _, _>(
                self.load_all_chunks(linked_chunk_id0).await.unwrap()
            )
            .unwrap()
            .is_some()
        );
        assert!(
            lazy_loader::from_all_chunks::<3, _, _>(
                self.load_all_chunks(linked_chunk_id1).await.unwrap()
            )
            .unwrap()
            .is_some()
        );

        // Clear the chunks.
        self.clear_all_linked_chunks().await.unwrap();

        // Both rooms now have no linked chunk.
        assert!(
            lazy_loader::from_all_chunks::<3, _, _>(
                self.load_all_chunks(linked_chunk_id0).await.unwrap()
            )
            .unwrap()
            .is_none()
        );
        assert!(
            lazy_loader::from_all_chunks::<3, _, _>(
                self.load_all_chunks(linked_chunk_id1).await.unwrap()
            )
            .unwrap()
            .is_none()
        );
    }

    async fn test_remove_room(&self) {
        let r0 = room_id!("!r0:matrix.org");
        let linked_chunk_id0 = LinkedChunkId::Room(r0);
        let r1 = room_id!("!r1:matrix.org");
        let linked_chunk_id1 = LinkedChunkId::Room(r1);

        // Add updates to the first room.
        self.handle_linked_chunk_updates(
            linked_chunk_id0,
            vec![
                // new chunk
                Update::NewItemsChunk { previous: None, new: CId::new(0), next: None },
                // new items on 0
                Update::PushItems {
                    at: Position::new(CId::new(0), 0),
                    items: vec![make_test_event(r0, "hello"), make_test_event(r0, "world")],
                },
            ],
        )
        .await
        .unwrap();

        // Add updates to the second room.
        self.handle_linked_chunk_updates(
            linked_chunk_id1,
            vec![
                // new chunk
                Update::NewItemsChunk { previous: None, new: CId::new(0), next: None },
                // new items on 0
                Update::PushItems {
                    at: Position::new(CId::new(0), 0),
                    items: vec![make_test_event(r0, "yummy")],
                },
            ],
        )
        .await
        .unwrap();

        // Try to remove content from r0.
        self.remove_room(r0).await.unwrap();

        // Check that r0 doesn't have a linked chunk anymore.
        let r0_linked_chunk = self.load_all_chunks(linked_chunk_id0).await.unwrap();
        assert!(r0_linked_chunk.is_empty());

        // Check that r1 is unaffected.
        let r1_linked_chunk = self.load_all_chunks(linked_chunk_id1).await.unwrap();
        assert!(!r1_linked_chunk.is_empty());
    }

    async fn test_filter_duplicated_events(&self) {
        let room_id = room_id!("!r0:matrix.org");
        let linked_chunk_id = LinkedChunkId::Room(room_id);
        let another_room_id = room_id!("!r1:matrix.org");
        let another_linked_chunk_id = LinkedChunkId::Room(another_room_id);
        let event = |msg: &str| make_test_event(room_id, msg);

        let event_comte = event("comté");
        let event_brigand = event("brigand du jorat");
        let event_raclette = event("raclette");
        let event_morbier = event("morbier");
        let event_gruyere = event("gruyère");
        let event_tome = event("tome");
        let event_mont_dor = event("mont d'or");

        self.handle_linked_chunk_updates(
            linked_chunk_id,
            vec![
                Update::NewItemsChunk { previous: None, new: CId::new(0), next: None },
                Update::PushItems {
                    at: Position::new(CId::new(0), 0),
                    items: vec![event_comte.clone(), event_brigand.clone()],
                },
                Update::NewGapChunk {
                    previous: Some(CId::new(0)),
                    new: CId::new(1),
                    next: None,
                    gap: Gap { prev_token: "brillat-savarin".to_owned() },
                },
                Update::NewItemsChunk { previous: Some(CId::new(1)), new: CId::new(2), next: None },
                Update::PushItems {
                    at: Position::new(CId::new(2), 0),
                    items: vec![event_morbier.clone(), event_mont_dor.clone()],
                },
            ],
        )
        .await
        .unwrap();

        // Add other events in another room, to ensure filtering take the `room_id` into
        // account.
        self.handle_linked_chunk_updates(
            another_linked_chunk_id,
            vec![
                Update::NewItemsChunk { previous: None, new: CId::new(0), next: None },
                Update::PushItems {
                    at: Position::new(CId::new(0), 0),
                    items: vec![event_tome.clone()],
                },
            ],
        )
        .await
        .unwrap();

        let duplicated_events = BTreeMap::from_iter(
            self.filter_duplicated_events(
                linked_chunk_id,
                vec![
                    event_comte.event_id().unwrap().to_owned(),
                    event_raclette.event_id().unwrap().to_owned(),
                    event_morbier.event_id().unwrap().to_owned(),
                    event_gruyere.event_id().unwrap().to_owned(),
                    event_tome.event_id().unwrap().to_owned(),
                    event_mont_dor.event_id().unwrap().to_owned(),
                ],
            )
            .await
            .unwrap(),
        );

        assert_eq!(duplicated_events.len(), 3);

        assert_eq!(
            *duplicated_events.get(&event_comte.event_id().unwrap()).unwrap(),
            Position::new(CId::new(0), 0)
        );
        assert_eq!(
            *duplicated_events.get(&event_morbier.event_id().unwrap()).unwrap(),
            Position::new(CId::new(2), 0)
        );
        assert_eq!(
            *duplicated_events.get(&event_mont_dor.event_id().unwrap()).unwrap(),
            Position::new(CId::new(2), 1)
        );
    }

    async fn test_find_event(&self) {
        let room_id = room_id!("!r0:matrix.org");
        let another_room_id = room_id!("!r1:matrix.org");
        let another_linked_chunk_id = LinkedChunkId::Room(another_room_id);
        let event = |msg: &str| make_test_event(room_id, msg);

        let event_comte = event("comté");
        let event_gruyere = event("gruyère");

        // Add one event in one room.
        self.handle_linked_chunk_updates(
            LinkedChunkId::Room(room_id),
            vec![
                Update::NewItemsChunk { previous: None, new: CId::new(0), next: None },
                Update::PushItems {
                    at: Position::new(CId::new(0), 0),
                    items: vec![event_comte.clone()],
                },
            ],
        )
        .await
        .unwrap();

        // Add another event in another room.
        self.handle_linked_chunk_updates(
            another_linked_chunk_id,
            vec![
                Update::NewItemsChunk { previous: None, new: CId::new(0), next: None },
                Update::PushItems {
                    at: Position::new(CId::new(0), 0),
                    items: vec![event_gruyere.clone()],
                },
            ],
        )
        .await
        .unwrap();

        // Now let's find the event.
        let event = self
            .find_event(room_id, event_comte.event_id().unwrap().as_ref())
            .await
            .expect("failed to query for finding an event")
            .expect("failed to find an event");

        assert_eq!(event.event_id(), event_comte.event_id());

        // Now let's try to find an event that exists, but not in the expected room.
        assert!(
            self.find_event(room_id, event_gruyere.event_id().unwrap().as_ref())
                .await
                .expect("failed to query for finding an event")
                .is_none()
        );

        // Clearing the rooms also clears the event's storage.
        self.clear_all_linked_chunks().await.expect("failed to clear all rooms chunks");
        assert!(
            self.find_event(room_id, event_comte.event_id().unwrap().as_ref())
                .await
                .expect("failed to query for finding an event")
                .is_none()
        );
    }

    async fn test_find_event_relations(&self) {
        let room_id = room_id!("!r0:matrix.org");
        let another_room_id = room_id!("!r1:matrix.org");

        let f = EventFactory::new().room(room_id).sender(*ALICE);

        // Create event and related events for the first room.
        let eid1 = event_id!("$event1:matrix.org");
        let e1 = f.text_msg("comter").event_id(eid1).into_event();

        let edit_eid1 = event_id!("$edit_event1:matrix.org");
        let edit_e1 = f
            .text_msg("* comté")
            .event_id(edit_eid1)
            .edit(eid1, RoomMessageEventContentWithoutRelation::text_plain("comté"))
            .into_event();

        let reaction_eid1 = event_id!("$reaction_event1:matrix.org");
        let reaction_e1 = f.reaction(eid1, "👍").event_id(reaction_eid1).into_event();

        let eid2 = event_id!("$event2:matrix.org");
        let e2 = f.text_msg("galette saucisse").event_id(eid2).into_event();

        // Create events for the second room.
        let f = f.room(another_room_id);

        let eid3 = event_id!("$event3:matrix.org");
        let e3 = f.text_msg("gruyère").event_id(eid3).into_event();

        let reaction_eid3 = event_id!("$reaction_event3:matrix.org");
        let reaction_e3 = f.reaction(eid3, "👍").event_id(reaction_eid3).into_event();

        // Save All The Things!
        self.save_event(room_id, e1).await.unwrap();
        self.save_event(room_id, edit_e1).await.unwrap();
        self.save_event(room_id, reaction_e1.clone()).await.unwrap();
        self.save_event(room_id, e2).await.unwrap();
        self.save_event(another_room_id, e3).await.unwrap();
        self.save_event(another_room_id, reaction_e3).await.unwrap();

        // Finding relations without a filter returns all of them.
        let relations = self.find_event_relations(room_id, eid1, None).await.unwrap();
        assert_eq!(relations.len(), 2);
        // The position is `None` for items outside the linked chunk.
        assert!(
            relations
                .iter()
                .any(|(ev, pos)| ev.event_id().as_deref() == Some(edit_eid1) && pos.is_none())
        );
        assert!(
            relations
                .iter()
                .any(|(ev, pos)| ev.event_id().as_deref() == Some(reaction_eid1) && pos.is_none())
        );

        // Finding relations with a filter only returns a subset.
        let relations = self
            .find_event_relations(room_id, eid1, Some(&[RelationType::Replacement]))
            .await
            .unwrap();
        assert_eq!(relations.len(), 1);
        assert_eq!(relations[0].0.event_id().as_deref(), Some(edit_eid1));

        let relations = self
            .find_event_relations(
                room_id,
                eid1,
                Some(&[RelationType::Replacement, RelationType::Annotation]),
            )
            .await
            .unwrap();
        assert_eq!(relations.len(), 2);
        assert!(relations.iter().any(|r| r.0.event_id().as_deref() == Some(edit_eid1)));
        assert!(relations.iter().any(|r| r.0.event_id().as_deref() == Some(reaction_eid1)));

        // We can't find relations using the wrong room.
        let relations = self
            .find_event_relations(another_room_id, eid1, Some(&[RelationType::Replacement]))
            .await
            .unwrap();
        assert!(relations.is_empty());

        // But if an event exists in the linked chunk, we may have its position when
        // it's found as a relationship.

        // Add reaction_e1 to the room's linked chunk.
        self.handle_linked_chunk_updates(
            LinkedChunkId::Room(room_id),
            vec![
                Update::NewItemsChunk { previous: None, new: CId::new(0), next: None },
                Update::PushItems { at: Position::new(CId::new(0), 0), items: vec![reaction_e1] },
            ],
        )
        .await
        .unwrap();

        // When looking for aggregations to e1, we should have the position for
        // reaction_e1.
        let relations = self.find_event_relations(room_id, eid1, None).await.unwrap();

        // The position is set for `reaction_eid1` now.
        assert!(relations.iter().any(|(ev, pos)| {
            ev.event_id().as_deref() == Some(reaction_eid1)
                && *pos == Some(Position::new(CId::new(0), 0))
        }));

        // But it's still not set for the other related events.
        assert!(
            relations
                .iter()
                .any(|(ev, pos)| ev.event_id().as_deref() == Some(edit_eid1) && pos.is_none())
        );
    }

    async fn test_get_room_events(&self) {
        let room_id = room_id!("!r0:matrix.org");
        let another_room_id = room_id!("!r1:matrix.org");
        let linked_chunk_id = LinkedChunkId::Room(room_id);
        let another_linked_chunk_id = LinkedChunkId::Room(another_room_id);
        let event = |msg: &str| make_test_event(room_id, msg);

        let event_comte = event("comté");
        let event_gruyere = event("gruyère");
        let event_stilton = event("stilton");

        // Add one event in one room.
        self.handle_linked_chunk_updates(
            linked_chunk_id,
            vec![
                Update::NewItemsChunk { previous: None, new: CId::new(0), next: None },
                Update::PushItems {
                    at: Position::new(CId::new(0), 0),
                    items: vec![event_comte.clone(), event_gruyere.clone()],
                },
            ],
        )
        .await
        .unwrap();

        // Add an event in a different room.
        self.handle_linked_chunk_updates(
            another_linked_chunk_id,
            vec![
                Update::NewItemsChunk { previous: None, new: CId::new(0), next: None },
                Update::PushItems {
                    at: Position::new(CId::new(0), 0),
                    items: vec![event_stilton.clone()],
                },
            ],
        )
        .await
        .unwrap();

        // Now let's find the events.
        let events = self
            .get_room_events(room_id, None, None)
            .await
            .expect("failed to query for room events");

        assert_eq!(events.len(), 2);

        let got_ids: Vec<_> = events.into_iter().map(|ev| ev.event_id()).collect();
        let expected_ids = vec![event_comte.event_id(), event_gruyere.event_id()];

        for expected in expected_ids {
            assert!(
                got_ids.contains(&expected),
                "Expected event {expected:?} not in got events: {got_ids:?}."
            );
        }
    }

    async fn test_get_room_events_filtered(&self) {
        macro_rules! assert_expected_events {
            ($events:expr, [$($item:expr),* $(,)?]) => {{
                let got_ids: BTreeSet<_> = $events.into_iter().map(|ev| ev.event_id().unwrap()).collect();
                let expected_ids = BTreeSet::from([$($item.event_id().unwrap()),*]);

                assert_eq!(got_ids, expected_ids);
            }};
        }

        let room_id = room_id!("!r0:matrix.org");
        let linked_chunk_id = LinkedChunkId::Room(room_id);
        let another_room_id = room_id!("!r1:matrix.org");
        let another_linked_chunk_id = LinkedChunkId::Room(another_room_id);

        let event = |session_id: &str| make_encrypted_test_event(room_id, session_id);

        let first_event = event("session_1");
        let second_event = event("session_2");
        let third_event = event("session_3");
        let fourth_event = make_test_event(room_id, "It's a secret to everybody");

        // Add one event in one room.
        self.handle_linked_chunk_updates(
            linked_chunk_id,
            vec![
                Update::NewItemsChunk { previous: None, new: CId::new(0), next: None },
                Update::PushItems {
                    at: Position::new(CId::new(0), 0),
                    items: vec![first_event.clone(), second_event.clone(), fourth_event.clone()],
                },
            ],
        )
        .await
        .unwrap();

        // Add an event in a different room.
        self.handle_linked_chunk_updates(
            another_linked_chunk_id,
            vec![
                Update::NewItemsChunk { previous: None, new: CId::new(0), next: None },
                Update::PushItems {
                    at: Position::new(CId::new(0), 0),
                    items: vec![third_event.clone()],
                },
            ],
        )
        .await
        .unwrap();

        // Now let's find all the encrypted events of the first room.
        let events = self
            .get_room_events(room_id, Some("m.room.encrypted"), None)
            .await
            .expect("failed to query for room events");

        assert_eq!(events.len(), 2);
        assert_expected_events!(events, [first_event, second_event]);

        // Now let's find all the encrypted events which were encrypted using the first
        // session ID.
        let events = self
            .get_room_events(room_id, Some("m.room.encrypted"), Some("session_1"))
            .await
            .expect("failed to query for room events");

        assert_eq!(events.len(), 1);
        assert_expected_events!(events, [first_event]);
    }

    async fn test_save_event(&self) {
        let room_id = room_id!("!r0:matrix.org");
        let another_room_id = room_id!("!r1:matrix.org");

        let event = |msg: &str| make_test_event(room_id, msg);
        let event_comte = event("comté");
        let event_gruyere = event("gruyère");

        // Add one event in one room.
        self.save_event(room_id, event_comte.clone()).await.unwrap();

        // Add another event in another room.
        self.save_event(another_room_id, event_gruyere.clone()).await.unwrap();

        // Events can be found, when searched in their own rooms.
        let event = self
            .find_event(room_id, event_comte.event_id().unwrap().as_ref())
            .await
            .expect("failed to query for finding an event")
            .expect("failed to find an event");
        assert_eq!(event.event_id(), event_comte.event_id());

        let event = self
            .find_event(another_room_id, event_gruyere.event_id().unwrap().as_ref())
            .await
            .expect("failed to query for finding an event")
            .expect("failed to find an event");
        assert_eq!(event.event_id(), event_gruyere.event_id());

        // But they won't be returned when searching in the wrong room.
        assert!(
            self.find_event(another_room_id, event_comte.event_id().unwrap().as_ref())
                .await
                .expect("failed to query for finding an event")
                .is_none()
        );
        assert!(
            self.find_event(room_id, event_gruyere.event_id().unwrap().as_ref())
                .await
                .expect("failed to query for finding an event")
                .is_none()
        );
    }

    async fn test_thread_vs_room_linked_chunk(&self) {
        let room_id = room_id!("!r0:matrix.org");

        let event = |msg: &str| make_test_event(room_id, msg);

        let thread1_ev = event("comté");
        let thread2_ev = event("gruyère");
        let thread2_ev2 = event("beaufort");
        let room_ev = event("brillat savarin triple crème");

        let thread_root1 = event("thread1");
        let thread_root2 = event("thread2");

        // Add one event in a thread linked chunk.
        self.handle_linked_chunk_updates(
            LinkedChunkId::Thread(room_id, thread_root1.event_id().unwrap().as_ref()),
            vec![
                Update::NewItemsChunk { previous: None, new: CId::new(0), next: None },
                Update::PushItems {
                    at: Position::new(CId::new(0), 0),
                    items: vec![thread1_ev.clone()],
                },
            ],
        )
        .await
        .unwrap();

        // Add one event in another thread linked chunk (same room).
        self.handle_linked_chunk_updates(
            LinkedChunkId::Thread(room_id, thread_root2.event_id().unwrap().as_ref()),
            vec![
                Update::NewItemsChunk { previous: None, new: CId::new(0), next: None },
                Update::PushItems {
                    at: Position::new(CId::new(0), 0),
                    items: vec![thread2_ev.clone(), thread2_ev2.clone()],
                },
            ],
        )
        .await
        .unwrap();

        // Add another event to the room linked chunk.
        self.handle_linked_chunk_updates(
            LinkedChunkId::Room(room_id),
            vec![
                Update::NewItemsChunk { previous: None, new: CId::new(0), next: None },
                Update::PushItems {
                    at: Position::new(CId::new(0), 0),
                    items: vec![room_ev.clone()],
                },
            ],
        )
        .await
        .unwrap();

        // All the events can be found with `find_event()` for the room.
        self.find_event(room_id, thread2_ev.event_id().unwrap().as_ref())
            .await
            .expect("failed to query for finding an event")
            .expect("failed to find thread1_ev");

        self.find_event(room_id, thread2_ev.event_id().unwrap().as_ref())
            .await
            .expect("failed to query for finding an event")
            .expect("failed to find thread2_ev");

        self.find_event(room_id, thread2_ev2.event_id().unwrap().as_ref())
            .await
            .expect("failed to query for finding an event")
            .expect("failed to find thread2_ev2");

        self.find_event(room_id, room_ev.event_id().unwrap().as_ref())
            .await
            .expect("failed to query for finding an event")
            .expect("failed to find room_ev");

        // Finding duplicates operates based on the linked chunk id.
        let dups = self
            .filter_duplicated_events(
                LinkedChunkId::Thread(room_id, thread_root1.event_id().unwrap().as_ref()),
                vec![
                    thread1_ev.event_id().unwrap().to_owned(),
                    room_ev.event_id().unwrap().to_owned(),
                ],
            )
            .await
            .unwrap();
        assert_eq!(dups.len(), 1);
        assert_eq!(dups[0].0, thread1_ev.event_id().unwrap());

        // Loading all chunks operates based on the linked chunk id.
        let all_chunks = self
            .load_all_chunks(LinkedChunkId::Thread(
                room_id,
                thread_root2.event_id().unwrap().as_ref(),
            ))
            .await
            .unwrap();
        assert_eq!(all_chunks.len(), 1);
        assert_eq!(all_chunks[0].identifier, CId::new(0));
        assert_let!(ChunkContent::Items(observed_items) = all_chunks[0].content.clone());
        assert_eq!(observed_items.len(), 2);
        assert_eq!(observed_items[0].event_id(), thread2_ev.event_id());
        assert_eq!(observed_items[1].event_id(), thread2_ev2.event_id());

        // Loading the metadata of all chunks operates based on the linked chunk
        // id.
        let metas = self
            .load_all_chunks_metadata(LinkedChunkId::Thread(
                room_id,
                thread_root2.event_id().unwrap().as_ref(),
            ))
            .await
            .unwrap();
        assert_eq!(metas.len(), 1);
        assert_eq!(metas[0].identifier, CId::new(0));
        assert_eq!(metas[0].num_items, 2);

        // Loading the last chunk operates based on the linked chunk id.
        let (last_chunk, _chunk_identifier_generator) = self
            .load_last_chunk(LinkedChunkId::Thread(
                room_id,
                thread_root1.event_id().unwrap().as_ref(),
            ))
            .await
            .unwrap();
        let last_chunk = last_chunk.unwrap();
        assert_eq!(last_chunk.identifier, CId::new(0));
        assert_let!(ChunkContent::Items(observed_items) = last_chunk.content);
        assert_eq!(observed_items.len(), 1);
        assert_eq!(observed_items[0].event_id(), thread1_ev.event_id());
    }
}

/// Macro building to allow your `EventCacheStore` implementation to run the
/// entire tests suite locally.
///
/// You need to provide a `async fn get_event_cache_store() ->
/// EventCacheStoreResult<impl EventCacheStore>` providing a fresh event cache
/// store on the same level you invoke the macro.
///
/// ## Usage Example:
/// ```no_run
/// # use matrix_sdk_base::event_cache::store::{
/// #    EventCacheStore,
/// #    MemoryStore as MyStore,
/// #    Result as EventCacheStoreResult,
/// # };
///
/// #[cfg(test)]
/// mod tests {
///     use super::{EventCacheStore, EventCacheStoreResult, MyStore};
///
///     async fn get_event_cache_store()
///     -> EventCacheStoreResult<impl EventCacheStore> {
///         Ok(MyStore::new())
///     }
///
///     event_cache_store_integration_tests!();
/// }
/// ```
#[allow(unused_macros, unused_extern_crates)]
#[macro_export]
macro_rules! event_cache_store_integration_tests {
    () => {
        mod event_cache_store_integration_tests {
            use matrix_sdk_test::async_test;
            use $crate::event_cache::store::{
                EventCacheStoreIntegrationTests, IntoEventCacheStore,
            };

            use super::get_event_cache_store;

            #[async_test]
            async fn test_handle_updates_and_rebuild_linked_chunk() {
                let event_cache_store =
                    get_event_cache_store().await.unwrap().into_event_cache_store();
                event_cache_store.test_handle_updates_and_rebuild_linked_chunk().await;
            }

            #[async_test]
            async fn test_linked_chunk_incremental_loading() {
                let event_cache_store =
                    get_event_cache_store().await.unwrap().into_event_cache_store();
                event_cache_store.test_linked_chunk_incremental_loading().await;
            }

            #[async_test]
            async fn test_rebuild_empty_linked_chunk() {
                let event_cache_store =
                    get_event_cache_store().await.unwrap().into_event_cache_store();
                event_cache_store.test_rebuild_empty_linked_chunk().await;
            }

            #[async_test]
            async fn test_load_all_chunks_metadata() {
                let event_cache_store =
                    get_event_cache_store().await.unwrap().into_event_cache_store();
                event_cache_store.test_load_all_chunks_metadata().await;
            }

            #[async_test]
            async fn test_clear_all_linked_chunks() {
                let event_cache_store =
                    get_event_cache_store().await.unwrap().into_event_cache_store();
                event_cache_store.test_clear_all_linked_chunks().await;
            }

            #[async_test]
            async fn test_remove_room() {
                let event_cache_store =
                    get_event_cache_store().await.unwrap().into_event_cache_store();
                event_cache_store.test_remove_room().await;
            }

            #[async_test]
            async fn test_filter_duplicated_events() {
                let event_cache_store =
                    get_event_cache_store().await.unwrap().into_event_cache_store();
                event_cache_store.test_filter_duplicated_events().await;
            }

            #[async_test]
            async fn test_find_event() {
                let event_cache_store =
                    get_event_cache_store().await.unwrap().into_event_cache_store();
                event_cache_store.test_find_event().await;
            }

            #[async_test]
            async fn test_find_event_relations() {
                let event_cache_store =
                    get_event_cache_store().await.unwrap().into_event_cache_store();
                event_cache_store.test_find_event_relations().await;
            }

            #[async_test]
            async fn test_get_room_events() {
                let event_cache_store =
                    get_event_cache_store().await.unwrap().into_event_cache_store();
                event_cache_store.test_get_room_events().await;
            }

            #[async_test]
            async fn test_get_room_events_filtered() {
                let event_cache_store =
                    get_event_cache_store().await.unwrap().into_event_cache_store();
                event_cache_store.test_get_room_events_filtered().await;
            }

            #[async_test]
            async fn test_save_event() {
                let event_cache_store =
                    get_event_cache_store().await.unwrap().into_event_cache_store();
                event_cache_store.test_save_event().await;
            }

            #[async_test]
            async fn test_thread_vs_room_linked_chunk() {
                let event_cache_store =
                    get_event_cache_store().await.unwrap().into_event_cache_store();
                event_cache_store.test_thread_vs_room_linked_chunk().await;
            }
        }
    };
}

/// Macro generating tests for the event cache store, related to time (mostly
/// for the cross-process lock).
#[allow(unused_macros)]
#[macro_export]
macro_rules! event_cache_store_integration_tests_time {
    () => {
        mod event_cache_store_integration_tests_time {
            use std::time::Duration;

            #[cfg(all(target_family = "wasm", target_os = "unknown"))]
            use gloo_timers::future::sleep;
            use matrix_sdk_test::async_test;
            #[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
            use tokio::time::sleep;
            use $crate::event_cache::store::IntoEventCacheStore;

            use super::get_event_cache_store;

            #[async_test]
            async fn test_lease_locks() {
                let store = get_event_cache_store().await.unwrap().into_event_cache_store();

                let acquired0 = store.try_take_leased_lock(0, "key", "alice").await.unwrap();
                assert_eq!(acquired0, Some(1)); // first lock generation

                // Should extend the lease automatically (same holder).
                let acquired2 = store.try_take_leased_lock(300, "key", "alice").await.unwrap();
                assert_eq!(acquired2, Some(1)); // same lock generation

                // Should extend the lease automatically (same holder + time is ok).
                let acquired3 = store.try_take_leased_lock(300, "key", "alice").await.unwrap();
                assert_eq!(acquired3, Some(1)); // same lock generation

                // Another attempt at taking the lock should fail, because it's taken.
                let acquired4 = store.try_take_leased_lock(300, "key", "bob").await.unwrap();
                assert!(acquired4.is_none()); // not acquired

                // Even if we insist.
                let acquired5 = store.try_take_leased_lock(300, "key", "bob").await.unwrap();
                assert!(acquired5.is_none()); // not acquired

                // That's a nice test we got here, go take a little nap.
                sleep(Duration::from_millis(50)).await;

                // Still too early.
                let acquired55 = store.try_take_leased_lock(300, "key", "bob").await.unwrap();
                assert!(acquired55.is_none()); // not acquired

                // Ok you can take another nap then.
                sleep(Duration::from_millis(250)).await;

                // At some point, we do get the lock.
                let acquired6 = store.try_take_leased_lock(0, "key", "bob").await.unwrap();
                assert_eq!(acquired6, Some(2)); // new lock generation!

                sleep(Duration::from_millis(1)).await;

                // The other gets it almost immediately too.
                let acquired7 = store.try_take_leased_lock(0, "key", "alice").await.unwrap();
                assert_eq!(acquired7, Some(3)); // new lock generation!

                sleep(Duration::from_millis(1)).await;

                // But when we take a longer lease…
                let acquired8 = store.try_take_leased_lock(300, "key", "bob").await.unwrap();
                assert_eq!(acquired8, Some(4)); // new lock generation!

                // It blocks the other user.
                let acquired9 = store.try_take_leased_lock(300, "key", "alice").await.unwrap();
                assert!(acquired9.is_none()); // not acquired

                // We can hold onto our lease.
                let acquired10 = store.try_take_leased_lock(300, "key", "bob").await.unwrap();
                assert_eq!(acquired10, Some(4)); // same lock generation
            }
        }
    };
}
