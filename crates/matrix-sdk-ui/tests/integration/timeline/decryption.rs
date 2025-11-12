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

use std::io::Write;

use assert_matches::assert_matches;
use eyeball_im::VectorDiff;
use matrix_sdk::{
    assert_next_matches_with_timeout,
    linked_chunk::{ChunkIdentifier, LinkedChunkId, Position, Update},
    test_utils::mocks::MatrixMockServer,
};
use matrix_sdk_test::{BOB, async_test, event_factory::EventFactory};
use matrix_sdk_ui::timeline::RoomExt;
use ruma::{
    event_id,
    events::room::encrypted::{
        EncryptedEventScheme, MegolmV1AesSha2ContentInit, RoomEncryptedEventContent,
    },
    room_id,
};
use stream_assert::assert_pending;
use tempfile::NamedTempFile;

/// The event cache can store Unable To Decrypt event
/// ([`TimelineEventKind::UnableToDecrypt`]). If such event is part of the
/// initial items of the `Timeline`, they are automatically decrypted if
/// possible.
///
/// [`TimelineEventKind::UnableToDecrypt`]: matrix_sdk::deserialized_responses::TimelineEventKind::UnableToDecrypt
#[async_test]
async fn test_an_utd_from_the_event_cache_as_an_initial_item_is_decrypted() {
    const SESSION_ID: &str = "gM8i47Xhu0q52xLfgUXzanCMpLinoyVyH7R58cBuVBU";
    const SESSION_KEY: &[u8] = b"\
        -----BEGIN MEGOLM SESSION DATA-----\n\
        ASKcWoiAVUM97482UAi83Avce62hSLce7i5JhsqoF6xeAAAACqt2Cg3nyJPRWTTMXxXH7TXnkfdlmBXbQtq5\
        bpHo3LRijcq2Gc6TXilESCmJN14pIsfKRJrWjZ0squ/XsoTFytuVLWwkNaW3QF6obeg2IoVtJXLMPdw3b2vO\
        vgwGY3OMP0XafH13j1vcb6YLzvgLkZQLnYvd47hv3yK/9GmKS9tokuaQ7dCVYckYcIOS09EDTs70YdxUd5WG\
        rQynATCLFP1p/NAGv70r9MK7Cy/mNpjD0r4qC7UEDIoi1kOWzHgnLo19wtvwsb8Fg8ATxcs3Wmtj8hIUYpDx\
        ia4sM10zbytUuaPUAfCDf42IyxdmOnGe1CueXhgI71y+RW0s0argNqUt7jB70JT0o9CyX6UBGRaqLk2MPY9T\
        hUu5J8X3UgIa6rcbWigzohzWm9rdbEHFrSWqjpfQYMaAKQQgETrjSy4XTrp2RhC2oNqG/hylI4ab+F4X6fpH\
        DYP1NqNMP5g36xNu7LhDnrUB5qsPjYOmWORxGLfudpF3oLYCSlr3DgHqEIB6HjQblLZ3KQuPBse3zxyROTnS\
        AhdPH4a/z1wioFtKNVph3hecsiKEdqnz4Y2coSIdhz58mJ9JWNQoFAENE5CSsoEZAGvafYZVpW4C75YY2zq1\
        wIeiFi1dT43/jLAUGkslsi1VvnyfUu8qO404RxYO3XHoGLMFoFLOO+lZ+VGci2Vz10AhxJhEBHxRKxw4k2uB\
        HztoSJUr/2Y\n\
        -----END MEGOLM SESSION DATA-----";

    let room_id = room_id!("!DovneieKSTkdHKpIXy:morpheus.localhost");
    let event_factory = EventFactory::new().room(room_id).sender(&BOB);

    let mock_server = MatrixMockServer::new().await;
    let client = mock_server.client_builder().build().await;

    // Set up the event cache store.
    {
        let event_cache_store = client.event_cache_store().lock().await.unwrap();

        // The event cache contains one chunk as such:
        //
        // 1. a chunk of 1 item
        //
        // The item is an encrypted event! It has been stored before having a chance to
        // be decrypted. Damn. We want to see if decryption will trigger automatically.
        event_cache_store
            .as_clean()
            .unwrap()
            .handle_linked_chunk_updates(
                LinkedChunkId::Room(room_id),
                vec![
                    // chunk #1
                    Update::NewItemsChunk {
                        previous: None,
                        new: ChunkIdentifier::new(0),
                        next: None,
                    },
                    // … and its item
                    Update::PushItems {
                        at: Position::new(ChunkIdentifier::new(0), 0),
                        items: vec![event_factory
                            .event(RoomEncryptedEventContent::new(
                                EncryptedEventScheme::MegolmV1AesSha2(
                                    MegolmV1AesSha2ContentInit {
                                        ciphertext: "\
                                            AwgAEtABPRMavuZMDJrPo6pGQP4qVmpcuapuXtzKXJyi3YpEsjSWdzuRKIgJzD4P\
                                            cSqJM1A8kzxecTQNJsC5q22+KSFEPxPnI4ltpm7GFowSoPSW9+bFdnlfUzEP1jPq\
                                            YevHAsMJp2fRKkzQQbPordrUk1gNqEpGl4BYFeRqKl9GPdKFwy45huvQCLNNueql\
                                            CFZVoYMuhxrfyMiJJAVNTofkr2um2mKjDTlajHtr39pTG8k0eOjSXkLOSdZvNOMz\
                                            hGhSaFNeERSA2G2YbeknOvU7MvjiO0AKuxaAe1CaVhAI14FCgzrJ8g0y5nly+n7x\
                                            QzL2G2Dn8EoXM5Iqj8W99iokQoVsSrUEnaQ1WnSIfewvDDt4LCaD/w7PGETMCQ"
                                            .to_owned(),
                                        sender_key: "DeHIg4gwhClxzFYcmNntPNF9YtsdZbmMy8+3kzCMXHA"
                                            .to_owned(),
                                        device_id: "NLAZCWIOCO".into(),
                                        session_id: SESSION_ID.into(),
                                    }
                                    .into(),
                                ),
                                None,
                            ))
                            .event_id(event_id!("$ev0"))
                            .into_utd_sync_timeline_event(),
                          ],
                    },
                ],
            )
            .await
            .expect("Failed to setup the event cache");
    }

    // Import the key to decrypt the cached event.
    {
        let mut tempfile =
            NamedTempFile::new().expect("Failed to create a temporary file for the keys");
        tempfile.write_all(SESSION_KEY).expect("Failed to write the keys in the temporary file");
        let tempfile_path = tempfile.into_temp_path();

        let room_key_import_result = client
            .encryption()
            .import_room_keys(tempfile_path.to_path_buf(), "1234")
            .await
            .expect("Failed to import the keys");
        assert_eq!(room_key_import_result.imported_count, 1);
    }

    // Set up the event cache.
    let event_cache = client.event_cache();
    event_cache.subscribe().unwrap();

    let room = mock_server.sync_joined_room(&client, room_id).await;
    let timeline = room.timeline().await.unwrap();
    let (initial_updates, mut updates_stream) = timeline.subscribe().await;

    assert_eq!(initial_updates.len(), 2);

    // First item is the date divider.
    assert!(&initial_updates[0].is_date_divider());

    // Second item is `$ev0` but as an UTD!
    assert_matches!(&initial_updates[1].as_event(), Some(event) => {
        assert_eq!(event.event_id().unwrap().as_str(), "$ev0");
        assert!(event.content().is_unable_to_decrypt());
    });

    // But wait… there is more!
    assert_next_matches_with_timeout!(updates_stream, 250, updates => {
        // Ho ho, one update?
        assert_eq!(updates.len(), 1, "Expecting 1 update from the `Timeline`");

        // Yes, the event is decrypted!
        assert_matches!(&updates[0], VectorDiff::Set { index: 1, value: event } => {
            assert_matches!(event.as_event(), Some(event) => {
                assert_eq!(event.event_id().unwrap().as_str(), "$ev0");
                assert_matches!(event.content().as_message(), Some(message) => {
                    assert_eq!(message.body(), "It's a secret to everybody");
                });
            });
        });
    });

    // That's all folks!
    assert_pending!(updates_stream);
}

/// The event cache can store Unable To Decrypt event
/// ([`TimelineEventKind::UnableToDecrypt`]). If such event is part of the
/// paginated items of the `Timeline`, they are automatically decrypted if
/// possible.
///
/// [`TimelineEventKind::UnableToDecrypt`]: matrix_sdk::deserialized_responses::TimelineEventKind::UnableToDecrypt
#[async_test]
async fn test_an_utd_from_the_event_cache_as_a_paginated_item_is_decrypted() {
    const SESSION_ID: &str = "gM8i47Xhu0q52xLfgUXzanCMpLinoyVyH7R58cBuVBU";
    const SESSION_KEY: &[u8] = b"\
        -----BEGIN MEGOLM SESSION DATA-----\n\
        ASKcWoiAVUM97482UAi83Avce62hSLce7i5JhsqoF6xeAAAACqt2Cg3nyJPRWTTMXxXH7TXnkfdlmBXbQtq5\
        bpHo3LRijcq2Gc6TXilESCmJN14pIsfKRJrWjZ0squ/XsoTFytuVLWwkNaW3QF6obeg2IoVtJXLMPdw3b2vO\
        vgwGY3OMP0XafH13j1vcb6YLzvgLkZQLnYvd47hv3yK/9GmKS9tokuaQ7dCVYckYcIOS09EDTs70YdxUd5WG\
        rQynATCLFP1p/NAGv70r9MK7Cy/mNpjD0r4qC7UEDIoi1kOWzHgnLo19wtvwsb8Fg8ATxcs3Wmtj8hIUYpDx\
        ia4sM10zbytUuaPUAfCDf42IyxdmOnGe1CueXhgI71y+RW0s0argNqUt7jB70JT0o9CyX6UBGRaqLk2MPY9T\
        hUu5J8X3UgIa6rcbWigzohzWm9rdbEHFrSWqjpfQYMaAKQQgETrjSy4XTrp2RhC2oNqG/hylI4ab+F4X6fpH\
        DYP1NqNMP5g36xNu7LhDnrUB5qsPjYOmWORxGLfudpF3oLYCSlr3DgHqEIB6HjQblLZ3KQuPBse3zxyROTnS\
        AhdPH4a/z1wioFtKNVph3hecsiKEdqnz4Y2coSIdhz58mJ9JWNQoFAENE5CSsoEZAGvafYZVpW4C75YY2zq1\
        wIeiFi1dT43/jLAUGkslsi1VvnyfUu8qO404RxYO3XHoGLMFoFLOO+lZ+VGci2Vz10AhxJhEBHxRKxw4k2uB\
        HztoSJUr/2Y\n\
        -----END MEGOLM SESSION DATA-----";

    let room_id = room_id!("!DovneieKSTkdHKpIXy:morpheus.localhost");
    let event_factory = EventFactory::new().room(room_id).sender(&BOB);

    let mock_server = MatrixMockServer::new().await;
    let client = mock_server.client_builder().build().await;

    // Set up the event cache store.
    {
        let event_cache_store = client.event_cache_store().lock().await.unwrap();

        // The event cache contains one chunk as such:
        //
        // 1. a chunk of 1 item
        // 2. a chunk of 1 item
        //
        // The older item is an encrypted event! It has been stored before having a
        // chance to be decrypted. Damn. We want to see if decryption will trigger
        // automatically.
        event_cache_store
            .as_clean()
            .unwrap()
            .handle_linked_chunk_updates(
                LinkedChunkId::Room(room_id),
                vec![
                    // chunk #1
                    Update::NewItemsChunk {
                        previous: None,
                        new: ChunkIdentifier::new(0),
                        next: None,
                    },
                    // … and its item
                    Update::PushItems {
                        at: Position::new(ChunkIdentifier::new(0), 0),
                        items: vec![event_factory
                            .event(RoomEncryptedEventContent::new(
                                EncryptedEventScheme::MegolmV1AesSha2(
                                    MegolmV1AesSha2ContentInit {
                                        ciphertext: "\
                                            AwgAEtABPRMavuZMDJrPo6pGQP4qVmpcuapuXtzKXJyi3YpEsjSWdzuRKIgJzD4P\
                                            cSqJM1A8kzxecTQNJsC5q22+KSFEPxPnI4ltpm7GFowSoPSW9+bFdnlfUzEP1jPq\
                                            YevHAsMJp2fRKkzQQbPordrUk1gNqEpGl4BYFeRqKl9GPdKFwy45huvQCLNNueql\
                                            CFZVoYMuhxrfyMiJJAVNTofkr2um2mKjDTlajHtr39pTG8k0eOjSXkLOSdZvNOMz\
                                            hGhSaFNeERSA2G2YbeknOvU7MvjiO0AKuxaAe1CaVhAI14FCgzrJ8g0y5nly+n7x\
                                            QzL2G2Dn8EoXM5Iqj8W99iokQoVsSrUEnaQ1WnSIfewvDDt4LCaD/w7PGETMCQ"
                                            .to_owned(),
                                        sender_key: "DeHIg4gwhClxzFYcmNntPNF9YtsdZbmMy8+3kzCMXHA"
                                            .to_owned(),
                                        device_id: "NLAZCWIOCO".into(),
                                        session_id: SESSION_ID.into(),
                                    }
                                    .into(),
                                ),
                                None,
                            ))
                            .event_id(event_id!("$ev0"))
                            .into_utd_sync_timeline_event(),
                          ],
                    },
                    // chunk #2
                    Update::NewItemsChunk {
                        previous: Some(ChunkIdentifier::new(0)),
                        new: ChunkIdentifier::new(1),
                        next: None,
                    },
                    // … and its item
                    Update::PushItems {
                        at: Position::new(ChunkIdentifier::new(1), 0),
                        items: vec![event_factory
                            .text_msg("hello")
                            .event_id(event_id!("$ev1"))
                            .into_event()
                        ]
                    }
                ],
            )
            .await
            .expect("Failed to setup the event cache");
    }

    // Import the key to decrypt the cached event.
    {
        let mut tempfile =
            NamedTempFile::new().expect("Failed to create a temporary file for the keys");
        tempfile.write_all(SESSION_KEY).expect("Failed to write the keys in the temporary file");
        let tempfile_path = tempfile.into_temp_path();

        let room_key_import_result = client
            .encryption()
            .import_room_keys(tempfile_path.to_path_buf(), "1234")
            .await
            .expect("Failed to import the keys");
        assert_eq!(room_key_import_result.imported_count, 1);
    }

    // Set up the event cache.
    let event_cache = client.event_cache();
    event_cache.subscribe().unwrap();

    let room = mock_server.sync_joined_room(&client, room_id).await;
    let timeline = room.timeline().await.unwrap();
    let (initial_updates, mut updates_stream) = timeline.subscribe().await;

    assert_eq!(initial_updates.len(), 2);

    // First item is the date divider.
    assert!(&initial_updates[0].is_date_divider());

    // Second item is `$ev1`.
    assert_matches!(&initial_updates[1].as_event(), Some(event) => {
        assert_eq!(event.event_id().unwrap().as_str(), "$ev1");
        assert_matches!(event.content().as_message(), Some(message) => {
            assert_eq!(message.body(), "hello");
        });
    });

    // The stream is pending, because, well, everything is alright so far!
    assert_pending!(updates_stream);

    // Now we can paginate to load the UTD!
    let reached_start = timeline.paginate_backwards(1).await.unwrap();

    // We have reached the start of the timeline. Not really part of this test, but
    // let's test everything :-).
    assert!(reached_start);

    assert_next_matches_with_timeout!(updates_stream, 250, updates => {
        assert_eq!(updates.len(), 1, "We get the start of timeline item");

        assert_matches!(&updates[0], VectorDiff::PushFront { value } => {
            assert!(value.is_timeline_start());
        });
    });

    // Now, let's look at the updates. We must observe an update reflecting the UTD
    // has entered the `Timeline`.
    assert_next_matches_with_timeout!(updates_stream, 250, updates => {
        assert_eq!(updates.len(), 2, "Expecting 2 updates from the `Timeline`");

        // UTD! UTD!
        assert_matches!(&updates[0], VectorDiff::Insert { index: 2, value: event } => {
            assert_matches!(event.as_event(), Some(event) => {
                assert_eq!(event.event_id().unwrap().as_str(), "$ev0");
                assert!(event.content().is_unable_to_decrypt());
            });
        });

        // UTD is decrypted now!
        assert_matches!(&updates[1], VectorDiff::Set { index: 2, value: event } => {
            assert_matches!(event.as_event(), Some(event) => {
                assert_eq!(event.event_id().unwrap().as_str(), "$ev0");
                assert_matches!(event.content().as_message(), Some(message) => {
                    assert_eq!(message.body(), "It's a secret to everybody");
                });
            });
        });
    });

    // That's all folks!
    assert_pending!(updates_stream);
}
