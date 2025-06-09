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

use std::sync::Arc;

use assert_matches::assert_matches;
use matrix_sdk_common::{
    deserialized_responses::{
        AlgorithmInfo, DecryptedRoomEvent, EncryptionInfo, TimelineEvent, TimelineEventKind,
        VerificationState,
    },
    linked_chunk::{
        lazy_loader, ChunkContent, ChunkIdentifier as CId, LinkedChunkId, Position, Update,
    },
};
use matrix_sdk_test::{event_factory::EventFactory, ALICE, DEFAULT_TEST_ROOM_ID};
use ruma::{
    api::client::media::get_content_thumbnail::v3::Method,
    event_id,
    events::{
        relation::RelationType,
        room::{message::RoomMessageEventContentWithoutRelation, MediaSource},
    },
    mxc_uri,
    push::Action,
    room_id, uint, EventId, RoomId,
};

use super::{media::IgnoreMediaRetentionPolicy, DynEventCacheStore};
use crate::{
    event_cache::{store::DEFAULT_CHUNK_CAPACITY, Gap},
    media::{MediaFormat, MediaRequestParameters, MediaThumbnailSettings},
};

/// Create a test event with all data filled, for testing that linked chunk
/// correctly stores event data.
///
/// Keep in sync with [`check_test_event`].
pub fn make_test_event(room_id: &RoomId, content: &str) -> TimelineEvent {
    make_test_event_with_event_id(room_id, content, None)
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
    let event = builder.into_raw_timeline().cast();

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
        assert_matches!(deserialized, ruma::events::AnyMessageLikeEvent::RoomMessage(msg) => {
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
    /// Test media content storage.
    async fn test_media_content(&self);

    /// Test replacing a MXID.
    async fn test_replace_media_key(&self);

    /// Test handling updates to a linked chunk and reloading these updates from
    /// the store.
    async fn test_handle_updates_and_rebuild_linked_chunk(&self);

    /// Test loading a linked chunk incrementally (chunk by chunk) from the
    /// store.
    async fn test_linked_chunk_incremental_loading(&self);

    /// Test that rebuilding a linked chunk from an empty store doesn't return
    /// anything.
    async fn test_rebuild_empty_linked_chunk(&self);

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

    /// Test that saving an event works as expected.
    async fn test_save_event(&self);
}

impl EventCacheStoreIntegrationTests for DynEventCacheStore {
    async fn test_media_content(&self) {
        let uri = mxc_uri!("mxc://localhost/media");
        let request_file = MediaRequestParameters {
            source: MediaSource::Plain(uri.to_owned()),
            format: MediaFormat::File,
        };
        let request_thumbnail = MediaRequestParameters {
            source: MediaSource::Plain(uri.to_owned()),
            format: MediaFormat::Thumbnail(MediaThumbnailSettings::with_method(
                Method::Crop,
                uint!(100),
                uint!(100),
            )),
        };

        let other_uri = mxc_uri!("mxc://localhost/media-other");
        let request_other_file = MediaRequestParameters {
            source: MediaSource::Plain(other_uri.to_owned()),
            format: MediaFormat::File,
        };

        let content: Vec<u8> = "hello".into();
        let thumbnail_content: Vec<u8> = "world".into();
        let other_content: Vec<u8> = "foo".into();

        // Media isn't present in the cache.
        assert!(
            self.get_media_content(&request_file).await.unwrap().is_none(),
            "unexpected media found"
        );
        assert!(
            self.get_media_content(&request_thumbnail).await.unwrap().is_none(),
            "media not found"
        );

        // Let's add the media.
        self.add_media_content(&request_file, content.clone(), IgnoreMediaRetentionPolicy::No)
            .await
            .expect("adding media failed");

        // Media is present in the cache.
        assert_eq!(
            self.get_media_content(&request_file).await.unwrap().as_ref(),
            Some(&content),
            "media not found though added"
        );
        assert_eq!(
            self.get_media_content_for_uri(uri).await.unwrap().as_ref(),
            Some(&content),
            "media not found by URI though added"
        );

        // Let's remove the media.
        self.remove_media_content(&request_file).await.expect("removing media failed");

        // Media isn't present in the cache.
        assert!(
            self.get_media_content(&request_file).await.unwrap().is_none(),
            "media still there after removing"
        );
        assert!(
            self.get_media_content_for_uri(uri).await.unwrap().is_none(),
            "media still found by URI after removing"
        );

        // Let's add the media again.
        self.add_media_content(&request_file, content.clone(), IgnoreMediaRetentionPolicy::No)
            .await
            .expect("adding media again failed");

        assert_eq!(
            self.get_media_content(&request_file).await.unwrap().as_ref(),
            Some(&content),
            "media not found after adding again"
        );

        // Let's add the thumbnail media.
        self.add_media_content(
            &request_thumbnail,
            thumbnail_content.clone(),
            IgnoreMediaRetentionPolicy::No,
        )
        .await
        .expect("adding thumbnail failed");

        // Media's thumbnail is present.
        assert_eq!(
            self.get_media_content(&request_thumbnail).await.unwrap().as_ref(),
            Some(&thumbnail_content),
            "thumbnail not found"
        );

        // We get a file with the URI, we don't know which one.
        assert!(
            self.get_media_content_for_uri(uri).await.unwrap().is_some(),
            "media not found by URI though two where added"
        );

        // Let's add another media with a different URI.
        self.add_media_content(
            &request_other_file,
            other_content.clone(),
            IgnoreMediaRetentionPolicy::No,
        )
        .await
        .expect("adding other media failed");

        // Other file is present.
        assert_eq!(
            self.get_media_content(&request_other_file).await.unwrap().as_ref(),
            Some(&other_content),
            "other file not found"
        );
        assert_eq!(
            self.get_media_content_for_uri(other_uri).await.unwrap().as_ref(),
            Some(&other_content),
            "other file not found by URI"
        );

        // Let's remove media based on URI.
        self.remove_media_content_for_uri(uri).await.expect("removing all media for uri failed");

        assert!(
            self.get_media_content(&request_file).await.unwrap().is_none(),
            "media wasn't removed"
        );
        assert!(
            self.get_media_content(&request_thumbnail).await.unwrap().is_none(),
            "thumbnail wasn't removed"
        );
        assert!(
            self.get_media_content(&request_other_file).await.unwrap().is_some(),
            "other media was removed"
        );
        assert!(
            self.get_media_content_for_uri(uri).await.unwrap().is_none(),
            "media found by URI wasn't removed"
        );
        assert!(
            self.get_media_content_for_uri(other_uri).await.unwrap().is_some(),
            "other media found by URI was removed"
        );
    }

    async fn test_replace_media_key(&self) {
        let uri = mxc_uri!("mxc://sendqueue.local/tr4n-s4ct-10n1-d");
        let req = MediaRequestParameters {
            source: MediaSource::Plain(uri.to_owned()),
            format: MediaFormat::File,
        };

        let content = "hello".as_bytes().to_owned();

        // Media isn't present in the cache.
        assert!(self.get_media_content(&req).await.unwrap().is_none(), "unexpected media found");

        // Add the media.
        self.add_media_content(&req, content.clone(), IgnoreMediaRetentionPolicy::No)
            .await
            .expect("adding media failed");

        // Sanity-check: media is found after adding it.
        assert_eq!(self.get_media_content(&req).await.unwrap().unwrap(), b"hello");

        // Replacing a media request works.
        let new_uri = mxc_uri!("mxc://matrix.org/tr4n-s4ct-10n1-d");
        let new_req = MediaRequestParameters {
            source: MediaSource::Plain(new_uri.to_owned()),
            format: MediaFormat::File,
        };
        self.replace_media_key(&req, &new_req)
            .await
            .expect("replacing the media request key failed");

        // Finding with the previous request doesn't work anymore.
        assert!(
            self.get_media_content(&req).await.unwrap().is_none(),
            "unexpected media found with the old key"
        );

        // Finding with the new request does work.
        assert_eq!(self.get_media_content(&new_req).await.unwrap().unwrap(), b"hello");
    }

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
        assert!(lazy_loader::from_all_chunks::<3, _, _>(
            self.load_all_chunks(linked_chunk_id0).await.unwrap()
        )
        .unwrap()
        .is_some());
        assert!(lazy_loader::from_all_chunks::<3, _, _>(
            self.load_all_chunks(linked_chunk_id1).await.unwrap()
        )
        .unwrap()
        .is_some());

        // Clear the chunks.
        self.clear_all_linked_chunks().await.unwrap();

        // Both rooms now have no linked chunk.
        assert!(lazy_loader::from_all_chunks::<3, _, _>(
            self.load_all_chunks(linked_chunk_id0).await.unwrap()
        )
        .unwrap()
        .is_none());
        assert!(lazy_loader::from_all_chunks::<3, _, _>(
            self.load_all_chunks(linked_chunk_id1).await.unwrap()
        )
        .unwrap()
        .is_none());
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

        let duplicated_events = self
            .filter_duplicated_events(
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
            .unwrap();

        assert_eq!(duplicated_events.len(), 3);
        assert_eq!(
            duplicated_events[0],
            (event_comte.event_id().unwrap(), Position::new(CId::new(0), 0))
        );
        assert_eq!(
            duplicated_events[1],
            (event_morbier.event_id().unwrap(), Position::new(CId::new(2), 0))
        );
        assert_eq!(
            duplicated_events[2],
            (event_mont_dor.event_id().unwrap(), Position::new(CId::new(2), 1))
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
        assert!(self
            .find_event(room_id, event_gruyere.event_id().unwrap().as_ref())
            .await
            .expect("failed to query for finding an event")
            .is_none());

        // Clearing the rooms also clears the event's storage.
        self.clear_all_linked_chunks().await.expect("failed to clear all rooms chunks");
        assert!(self
            .find_event(room_id, event_comte.event_id().unwrap().as_ref())
            .await
            .expect("failed to query for finding an event")
            .is_none());
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
        self.save_event(room_id, reaction_e1).await.unwrap();
        self.save_event(room_id, e2).await.unwrap();
        self.save_event(another_room_id, e3).await.unwrap();
        self.save_event(another_room_id, reaction_e3).await.unwrap();

        // Finding relations without a filter returns all of them.
        let relations = self.find_event_relations(room_id, eid1, None).await.unwrap();
        assert_eq!(relations.len(), 2);
        assert!(relations.iter().any(|r| r.event_id().as_deref() == Some(edit_eid1)));
        assert!(relations.iter().any(|r| r.event_id().as_deref() == Some(reaction_eid1)));

        // Finding relations with a filter only returns a subset.
        let relations = self
            .find_event_relations(room_id, eid1, Some(&[RelationType::Replacement]))
            .await
            .unwrap();
        assert_eq!(relations.len(), 1);
        assert_eq!(relations[0].event_id().as_deref(), Some(edit_eid1));

        let relations = self
            .find_event_relations(
                room_id,
                eid1,
                Some(&[RelationType::Replacement, RelationType::Annotation]),
            )
            .await
            .unwrap();
        assert_eq!(relations.len(), 2);
        assert!(relations.iter().any(|r| r.event_id().as_deref() == Some(edit_eid1)));
        assert!(relations.iter().any(|r| r.event_id().as_deref() == Some(reaction_eid1)));

        // We can't find relations using the wrong room.
        let relations = self
            .find_event_relations(another_room_id, eid1, Some(&[RelationType::Replacement]))
            .await
            .unwrap();
        assert!(relations.is_empty());
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
        assert!(self
            .find_event(another_room_id, event_comte.event_id().unwrap().as_ref())
            .await
            .expect("failed to query for finding an event")
            .is_none());
        assert!(self
            .find_event(room_id, event_gruyere.event_id().unwrap().as_ref())
            .await
            .expect("failed to query for finding an event")
            .is_none());
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
///     async fn get_event_cache_store(
///     ) -> EventCacheStoreResult<impl EventCacheStore> {
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
            async fn test_media_content() {
                let event_cache_store =
                    get_event_cache_store().await.unwrap().into_event_cache_store();
                event_cache_store.test_media_content().await;
            }

            #[async_test]
            async fn test_replace_media_key() {
                let event_cache_store =
                    get_event_cache_store().await.unwrap().into_event_cache_store();
                event_cache_store.test_replace_media_key().await;
            }

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
            async fn test_save_event() {
                let event_cache_store =
                    get_event_cache_store().await.unwrap().into_event_cache_store();
                event_cache_store.test_save_event().await;
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
        #[cfg(not(target_family = "wasm"))]
        mod event_cache_store_integration_tests_time {
            use std::time::Duration;

            use matrix_sdk_test::async_test;
            use $crate::event_cache::store::IntoEventCacheStore;

            use super::get_event_cache_store;

            #[async_test]
            async fn test_lease_locks() {
                let store = get_event_cache_store().await.unwrap().into_event_cache_store();

                let acquired0 = store.try_take_leased_lock(0, "key", "alice").await.unwrap();
                assert!(acquired0);

                // Should extend the lease automatically (same holder).
                let acquired2 = store.try_take_leased_lock(300, "key", "alice").await.unwrap();
                assert!(acquired2);

                // Should extend the lease automatically (same holder + time is ok).
                let acquired3 = store.try_take_leased_lock(300, "key", "alice").await.unwrap();
                assert!(acquired3);

                // Another attempt at taking the lock should fail, because it's taken.
                let acquired4 = store.try_take_leased_lock(300, "key", "bob").await.unwrap();
                assert!(!acquired4);

                // Even if we insist.
                let acquired5 = store.try_take_leased_lock(300, "key", "bob").await.unwrap();
                assert!(!acquired5);

                // That's a nice test we got here, go take a little nap.
                tokio::time::sleep(Duration::from_millis(50)).await;

                // Still too early.
                let acquired55 = store.try_take_leased_lock(300, "key", "bob").await.unwrap();
                assert!(!acquired55);

                // Ok you can take another nap then.
                tokio::time::sleep(Duration::from_millis(250)).await;

                // At some point, we do get the lock.
                let acquired6 = store.try_take_leased_lock(0, "key", "bob").await.unwrap();
                assert!(acquired6);

                tokio::time::sleep(Duration::from_millis(1)).await;

                // The other gets it almost immediately too.
                let acquired7 = store.try_take_leased_lock(0, "key", "alice").await.unwrap();
                assert!(acquired7);

                tokio::time::sleep(Duration::from_millis(1)).await;

                // But when we take a longer lease...
                let acquired8 = store.try_take_leased_lock(300, "key", "bob").await.unwrap();
                assert!(acquired8);

                // It blocks the other user.
                let acquired9 = store.try_take_leased_lock(300, "key", "alice").await.unwrap();
                assert!(!acquired9);

                // We can hold onto our lease.
                let acquired10 = store.try_take_leased_lock(300, "key", "bob").await.unwrap();
                assert!(acquired10);
            }
        }
    };
}
