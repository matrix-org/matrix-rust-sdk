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

use assert_matches::assert_matches;
use async_trait::async_trait;
use matrix_sdk_common::{
    deserialized_responses::{
        AlgorithmInfo, DecryptedRoomEvent, EncryptionInfo, SyncTimelineEvent, TimelineEventKind,
        VerificationState,
    },
    linked_chunk::{ChunkContent, Position, Update},
};
use matrix_sdk_test::{event_factory::EventFactory, ALICE, DEFAULT_TEST_ROOM_ID};
use ruma::{
    api::client::media::get_content_thumbnail::v3::Method, events::room::MediaSource, mxc_uri,
    push::Action, room_id, uint, RoomId,
};

use super::DynEventCacheStore;
use crate::{
    event_cache::Gap,
    media::{MediaFormat, MediaRequestParameters, MediaThumbnailSettings},
};

/// Create a test event with all data filled, for testing that linked chunk
/// correctly stores event data.
///
/// Keep in sync with [`check_test_event`].
pub fn make_test_event(room_id: &RoomId, content: &str) -> SyncTimelineEvent {
    let encryption_info = EncryptionInfo {
        sender: (*ALICE).into(),
        sender_device: None,
        algorithm_info: AlgorithmInfo::MegolmV1AesSha2 {
            curve25519_key: "1337".to_owned(),
            sender_claimed_keys: Default::default(),
        },
        verification_state: VerificationState::Verified,
    };

    let event = EventFactory::new()
        .text_msg(content)
        .room(room_id)
        .sender(*ALICE)
        .into_raw_timeline()
        .cast();

    SyncTimelineEvent {
        kind: TimelineEventKind::Decrypted(DecryptedRoomEvent {
            event,
            encryption_info,
            unsigned_encryption_info: None,
        }),
        push_actions: vec![Action::Notify],
    }
}

/// Check that an event created with [`make_test_event`] contains the expected
/// data.
///
/// Keep in sync with [`make_test_event`].
#[track_caller]
pub fn check_test_event(event: &SyncTimelineEvent, text: &str) {
    // Check push actions.
    let actions = &event.push_actions;
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
/// [`event_cache_store_integration_tests!`] macro.
#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
pub trait EventCacheStoreIntegrationTests {
    /// Test media content storage.
    async fn test_media_content(&self);

    /// Test replacing a MXID.
    async fn test_replace_media_key(&self);

    /// Test handling updates to a linked chunk and reloading these updates from
    /// the store.
    async fn test_handle_updates_and_rebuild_linked_chunk(&self);

    /// Test that rebuilding a linked chunk from an empty store doesn't return
    /// anything.
    async fn test_rebuild_empty_linked_chunk(&self);
}

#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
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
        self.add_media_content(&request_file, content.clone()).await.expect("adding media failed");

        // Media is present in the cache.
        assert_eq!(
            self.get_media_content(&request_file).await.unwrap().as_ref(),
            Some(&content),
            "media not found though added"
        );

        // Let's remove the media.
        self.remove_media_content(&request_file).await.expect("removing media failed");

        // Media isn't present in the cache.
        assert!(
            self.get_media_content(&request_file).await.unwrap().is_none(),
            "media still there after removing"
        );

        // Let's add the media again.
        self.add_media_content(&request_file, content.clone())
            .await
            .expect("adding media again failed");

        assert_eq!(
            self.get_media_content(&request_file).await.unwrap().as_ref(),
            Some(&content),
            "media not found after adding again"
        );

        // Let's add the thumbnail media.
        self.add_media_content(&request_thumbnail, thumbnail_content.clone())
            .await
            .expect("adding thumbnail failed");

        // Media's thumbnail is present.
        assert_eq!(
            self.get_media_content(&request_thumbnail).await.unwrap().as_ref(),
            Some(&thumbnail_content),
            "thumbnail not found"
        );

        // Let's add another media with a different URI.
        self.add_media_content(&request_other_file, other_content.clone())
            .await
            .expect("adding other media failed");

        // Other file is present.
        assert_eq!(
            self.get_media_content(&request_other_file).await.unwrap().as_ref(),
            Some(&other_content),
            "other file not found"
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
        self.add_media_content(&req, content.clone()).await.expect("adding media failed");

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
        use matrix_sdk_common::linked_chunk::ChunkIdentifier as CId;

        let room_id = room_id!("!r0:matrix.org");

        self.handle_linked_chunk_updates(
            room_id,
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
                // new items on 0
                Update::PushItems {
                    at: Position::new(CId::new(2), 0),
                    items: vec![make_test_event(room_id, "sup")],
                },
            ],
        )
        .await
        .unwrap();

        // The linked chunk is correctly reloaded.
        let lc = self.reload_linked_chunk(room_id).await.unwrap().expect("linked chunk not empty");

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

    async fn test_rebuild_empty_linked_chunk(&self) {
        // When I rebuild a linked chunk from an empty store, it's empty.
        assert!(self.reload_linked_chunk(&DEFAULT_TEST_ROOM_ID).await.unwrap().is_none());
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
            async fn test_rebuild_empty_linked_chunk() {
                let event_cache_store =
                    get_event_cache_store().await.unwrap().into_event_cache_store();
                event_cache_store.test_rebuild_empty_linked_chunk().await;
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
        #[cfg(not(target_arch = "wasm32"))]
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
