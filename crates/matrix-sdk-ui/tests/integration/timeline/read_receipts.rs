// Copyright 2023 The Matrix.org Foundation C.I.C.
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

use std::{ops::Not as _, time::Duration};

use assert_matches::assert_matches;
use assert_matches2::assert_let;
use eyeball_im::VectorDiff;
use futures_util::StreamExt;
use matrix_sdk::{config::SyncSettings, room::Receipts, test_utils::logged_in_client_with_server};
use matrix_sdk_test::{
    async_test, sync_timeline_event, EphemeralTestEvent, JoinedRoomBuilder,
    RoomAccountDataTestEvent, SyncResponseBuilder, ALICE, BOB,
};
use matrix_sdk_ui::timeline::RoomExt;
use ruma::{
    api::client::receipt::create_receipt::v3::ReceiptType,
    event_id,
    events::{
        receipt::ReceiptThread,
        room::message::{MessageType, SyncRoomMessageEvent},
        AnySyncMessageLikeEvent, AnySyncTimelineEvent,
    },
    room_id, user_id, RoomVersionId,
};
use serde_json::json;
use wiremock::{
    matchers::{body_json, header, method, path_regex},
    Mock, ResponseTemplate,
};

use crate::mock_sync;

fn filter_notice(ev: &AnySyncTimelineEvent, _room_version: &RoomVersionId) -> bool {
    match ev {
        AnySyncTimelineEvent::MessageLike(AnySyncMessageLikeEvent::RoomMessage(
            SyncRoomMessageEvent::Original(msg),
        )) => !matches!(msg.content.msgtype, MessageType::Notice(_)),
        _ => true,
    }
}

#[async_test]
async fn test_read_receipts_updates() {
    let room_id = room_id!("!a98sd12bjh:example.org");
    let (client, server) = logged_in_client_with_server().await;
    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

    let own_user_id = client.user_id().unwrap();
    let alice = user_id!("@alice:localhost");
    let bob = user_id!("@bob:localhost");

    let second_event_id = event_id!("$e32037280er453l:localhost");
    let third_event_id = event_id!("$Sg2037280074GZr34:localhost");

    let mut ev_builder = SyncResponseBuilder::new();
    ev_builder.add_joined_room(JoinedRoomBuilder::new(room_id));

    mock_sync(&server, ev_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    let room = client.get_room(room_id).unwrap();
    let timeline = room.timeline().await.unwrap();
    let (items, mut timeline_stream) = timeline.subscribe().await;

    assert!(items.is_empty());

    let own_receipt = timeline.latest_user_read_receipt(own_user_id).await;
    assert_matches!(own_receipt, None);
    let alice_receipt = timeline.latest_user_read_receipt(alice).await;
    assert_matches!(alice_receipt, None);
    let bob_receipt = timeline.latest_user_read_receipt(bob).await;
    assert_matches!(bob_receipt, None);

    ev_builder.add_joined_room(
        JoinedRoomBuilder::new(room_id)
            .add_timeline_event(sync_timeline_event!({
                "content": {
                    "body": "is dancing", "format":
                    "org.matrix.custom.html",
                    "formatted_body": "<strong>is dancing</strong>",
                    "msgtype": "m.text"
                },
                "event_id": "$152037280074GZeOm:localhost",
                "origin_server_ts": 152037280,
                "sender": "@example:localhost",
                "type": "m.room.message",
                "unsigned": {
                    "age": 598971
                }
            }))
            .add_timeline_event(sync_timeline_event!({
                "content": {
                    "body": "I'm dancing too",
                    "msgtype": "m.text"
                },
                "event_id": second_event_id,
                "origin_server_ts": 152039280,
                "sender": alice,
                "type": "m.room.message",
            }))
            .add_timeline_event(sync_timeline_event!({
                "content": {
                    "body": "Viva la macarena!",
                    "msgtype": "m.text"
                },
                "event_id": third_event_id,
                "origin_server_ts": 152045280,
                "sender": alice,
                "type": "m.room.message",
            })),
    );

    mock_sync(&server, ev_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    // We don't list the read receipt of our own user on events.
    assert_let!(Some(VectorDiff::PushBack { value: first_item }) = timeline_stream.next().await);
    let first_event = first_item.as_event().unwrap();
    assert!(first_event.read_receipts().is_empty());

    let (own_receipt_event_id, _) = timeline.latest_user_read_receipt(own_user_id).await.unwrap();
    assert_eq!(own_receipt_event_id, first_event.event_id().unwrap());

    // Implicit read receipt of @alice:localhost.
    assert_let!(Some(VectorDiff::PushBack { value: second_item }) = timeline_stream.next().await);
    let second_event = second_item.as_event().unwrap();
    assert_eq!(second_event.read_receipts().len(), 1);

    // Read receipt of @alice:localhost is moved to third event.
    assert_let!(
        Some(VectorDiff::Set { index: 1, value: second_item }) = timeline_stream.next().await
    );
    let second_event = second_item.as_event().unwrap();
    assert!(second_event.read_receipts().is_empty());

    assert_let!(Some(VectorDiff::PushBack { value: third_item }) = timeline_stream.next().await);
    let third_event = third_item.as_event().unwrap();
    assert_eq!(third_event.read_receipts().len(), 1);

    let (alice_receipt_event_id, _) = timeline.latest_user_read_receipt(alice).await.unwrap();
    assert_eq!(alice_receipt_event_id, third_event_id);

    assert_let!(Some(VectorDiff::PushFront { value: day_divider }) = timeline_stream.next().await);
    assert!(day_divider.is_day_divider());

    // Read receipt on unknown event is ignored.
    ev_builder.add_joined_room(JoinedRoomBuilder::new(room_id).add_ephemeral_event(
        EphemeralTestEvent::Custom(json!({
            "content": {
                "$unknowneventid": {
                    "m.read": {
                        alice: {
                            "ts": 1436453550,
                        },
                    },
                },
            },
            "type": "m.receipt",
        })),
    ));

    mock_sync(&server, ev_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    let (alice_receipt_event_id, _) = timeline.latest_user_read_receipt(alice).await.unwrap();
    assert_eq!(alice_receipt_event_id, third_event.event_id().unwrap());

    // Read receipt on older event is ignored.
    ev_builder.add_joined_room(JoinedRoomBuilder::new(room_id).add_ephemeral_event(
        EphemeralTestEvent::Custom(json!({
            "content": {
                second_event_id: {
                    "m.read": {
                        alice: {
                            "ts": 1436451550,
                        },
                    },
                },
            },
            "type": "m.receipt",
        })),
    ));

    let (alice_receipt_event_id, _) = timeline.latest_user_read_receipt(alice).await.unwrap();
    assert_eq!(alice_receipt_event_id, third_event_id);

    // Read receipt on same event is ignored.
    ev_builder.add_joined_room(JoinedRoomBuilder::new(room_id).add_ephemeral_event(
        EphemeralTestEvent::Custom(json!({
            "content": {
                third_event_id: {
                    "m.read": {
                        alice: {
                            "ts": 1436451550,
                        },
                    },
                },
            },
            "type": "m.receipt",
        })),
    ));

    let (alice_receipt_event_id, _) = timeline.latest_user_read_receipt(alice).await.unwrap();
    assert_eq!(alice_receipt_event_id, third_event_id);

    // New user with explicit threaded and unthreaded read receipts.
    ev_builder.add_joined_room(JoinedRoomBuilder::new(room_id).add_ephemeral_event(
        EphemeralTestEvent::Custom(json!({
            "content": {
                second_event_id: {
                    "m.read": {
                        bob: {
                            "ts": 1436451350,
                        },
                    },
                },
                third_event_id: {
                    "m.read": {
                        bob: {
                            "ts": 1436451550,
                            "thread_id": "main",
                        },
                    },
                },
            },
            "type": "m.receipt",
        })),
    ));

    mock_sync(&server, ev_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    assert_let!(
        Some(VectorDiff::Set { index: 3, value: third_item }) = timeline_stream.next().await
    );
    let third_event = third_item.as_event().unwrap();
    assert_eq!(third_event.read_receipts().len(), 2);

    let (bob_receipt_event_id, _) = timeline.latest_user_read_receipt(bob).await.unwrap();
    assert_eq!(bob_receipt_event_id, third_event_id);

    // Private read receipt is updated.
    ev_builder.add_joined_room(JoinedRoomBuilder::new(room_id).add_ephemeral_event(
        EphemeralTestEvent::Custom(json!({
            "content": {
                second_event_id: {
                    "m.read.private": {
                        own_user_id: {
                            "ts": 1436453550,
                        },
                    },
                },
            },
            "type": "m.receipt",
        })),
    ));

    mock_sync(&server, ev_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    let (own_user_receipt_event_id, _) =
        timeline.latest_user_read_receipt(own_user_id).await.unwrap();
    assert_eq!(own_user_receipt_event_id, second_event_id);
}

#[async_test]
async fn test_read_receipts_updates_on_filtered_events() {
    let room_id = room_id!("!a98sd12bjh:example.org");
    let (client, server) = logged_in_client_with_server().await;
    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

    let own_user_id = client.user_id().unwrap();

    let event_a_id = event_id!("$152037280074GZeOm:localhost");
    let event_b_id = event_id!("$e32037280er453l:localhost");
    let event_c_id = event_id!("$Sg2037280074GZr34:localhost");

    let mut ev_builder = SyncResponseBuilder::new();
    ev_builder.add_joined_room(JoinedRoomBuilder::new(room_id));

    mock_sync(&server, ev_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    let room = client.get_room(room_id).unwrap();
    let timeline = room.timeline_builder().event_filter(filter_notice).build().await.unwrap();
    let (items, mut timeline_stream) = timeline.subscribe().await;

    assert!(items.is_empty());

    let own_receipt = timeline.latest_user_read_receipt(own_user_id).await;
    assert_matches!(own_receipt, None);
    let own_receipt_timeline_event =
        timeline.latest_user_read_receipt_timeline_event_id(own_user_id).await;
    assert_matches!(own_receipt_timeline_event, None);
    let alice_receipt = timeline.latest_user_read_receipt(*ALICE).await;
    assert_matches!(alice_receipt, None);
    let alice_receipt_timeline_event =
        timeline.latest_user_read_receipt_timeline_event_id(*ALICE).await;
    assert_matches!(alice_receipt_timeline_event, None);
    let bob_receipt = timeline.latest_user_read_receipt(*BOB).await;
    assert_matches!(bob_receipt, None);
    let bob_receipt_timeline_event =
        timeline.latest_user_read_receipt_timeline_event_id(*BOB).await;
    assert_matches!(bob_receipt_timeline_event, None);

    ev_builder.add_joined_room(
        JoinedRoomBuilder::new(room_id)
            // Event A
            .add_timeline_event(sync_timeline_event!({
                "content": {
                    "body": "is dancing",
                    "msgtype": "m.text"
                },
                "event_id": event_a_id,
                "origin_server_ts": 152037280,
                "sender": own_user_id,
                "type": "m.room.message",
            }))
            // Event B
            .add_timeline_event(sync_timeline_event!({
                "content": {
                    "body": "I'm dancing too",
                    "msgtype": "m.notice"
                },
                "event_id": event_b_id,
                "origin_server_ts": 152039280,
                "sender": *BOB,
                "type": "m.room.message",
            }))
            // Event C
            .add_timeline_event(sync_timeline_event!({
                "content": {
                    "body": "Viva la macarena!",
                    "msgtype": "m.text"
                },
                "event_id": event_c_id,
                "origin_server_ts": 152045280,
                "sender": *ALICE,
                "type": "m.room.message",
            })),
    );

    mock_sync(&server, ev_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    // We don't list the read receipt of our own user on events.
    assert_let!(Some(VectorDiff::PushBack { value: item_a }) = timeline_stream.next().await);
    let event_a = item_a.as_event().unwrap();
    assert!(event_a.read_receipts().is_empty());

    let (own_receipt_event_id, _) = timeline.latest_user_read_receipt(own_user_id).await.unwrap();
    assert_eq!(own_receipt_event_id, event_a_id);
    let own_receipt_timeline_event =
        timeline.latest_user_read_receipt_timeline_event_id(own_user_id).await.unwrap();
    assert_eq!(own_receipt_timeline_event, event_a_id);

    // Implicit read receipt of @bob:localhost.
    assert_let!(Some(VectorDiff::Set { index: 0, value: item_a }) = timeline_stream.next().await);
    let event_a = item_a.as_event().unwrap();
    assert_eq!(event_a.read_receipts().len(), 1);

    // Real receipt is on event B.
    let (bob_receipt_event_id, _) = timeline.latest_user_read_receipt(*BOB).await.unwrap();
    assert_eq!(bob_receipt_event_id, event_b_id);
    // Visible receipt is on event A.
    let bob_receipt_timeline_event =
        timeline.latest_user_read_receipt_timeline_event_id(*BOB).await.unwrap();
    assert_eq!(bob_receipt_timeline_event, event_a.event_id().unwrap());

    // Implicit read receipt of @alice:localhost.
    assert_let!(Some(VectorDiff::PushBack { value: item_c }) = timeline_stream.next().await);
    let event_c = item_c.as_event().unwrap();
    assert_eq!(event_c.read_receipts().len(), 1);

    let (alice_receipt_event_id, _) = timeline.latest_user_read_receipt(*ALICE).await.unwrap();
    assert_eq!(alice_receipt_event_id, event_c_id);
    let alice_receipt_timeline_event =
        timeline.latest_user_read_receipt_timeline_event_id(*ALICE).await.unwrap();
    assert_eq!(alice_receipt_timeline_event, event_c_id);

    assert_let!(Some(VectorDiff::PushFront { value: day_divider }) = timeline_stream.next().await);
    assert!(day_divider.is_day_divider());

    // Read receipt on filtered event.
    ev_builder.add_joined_room(JoinedRoomBuilder::new(room_id).add_ephemeral_event(
        EphemeralTestEvent::Custom(json!({
            "content": {
                event_b_id: {
                    "m.read": {
                        own_user_id: {
                            "ts": 1436451550,
                        },
                    },
                },
            },
            "type": "m.receipt",
        })),
    ));

    mock_sync(&server, ev_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    // Real receipt changed to event B.
    let (own_receipt_event_id, _) = timeline.latest_user_read_receipt(own_user_id).await.unwrap();
    assert_eq!(own_receipt_event_id, event_b_id);
    // Visible receipt is still on event A.
    let own_receipt_timeline_event =
        timeline.latest_user_read_receipt_timeline_event_id(own_user_id).await.unwrap();
    assert_eq!(own_receipt_timeline_event, event_a.event_id().unwrap());

    // Update with explicit read receipt.
    ev_builder.add_joined_room(JoinedRoomBuilder::new(room_id).add_ephemeral_event(
        EphemeralTestEvent::Custom(json!({
            "content": {
                event_c_id: {
                    "m.read": {
                        *BOB: {
                            "ts": 1436451550,
                        },
                    },
                },
            },
            "type": "m.receipt",
        })),
    ));

    mock_sync(&server, ev_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    assert_let!(Some(VectorDiff::Set { index: 1, value: item_a }) = timeline_stream.next().await);
    let event_a = item_a.as_event().unwrap();
    assert!(event_a.read_receipts().is_empty());

    assert_let!(Some(VectorDiff::Set { index: 2, value: item_c }) = timeline_stream.next().await);
    let event_c = item_c.as_event().unwrap();
    assert_eq!(event_c.read_receipts().len(), 2);

    // Both real and visible receipts are now on event C.
    let (bob_receipt_event_id, _) = timeline.latest_user_read_receipt(*BOB).await.unwrap();
    assert_eq!(bob_receipt_event_id, event_c_id);
    let bob_receipt_timeline_event =
        timeline.latest_user_read_receipt_timeline_event_id(*BOB).await.unwrap();
    assert_eq!(bob_receipt_timeline_event, event_c_id);

    // Private read receipt is updated.
    ev_builder.add_joined_room(JoinedRoomBuilder::new(room_id).add_ephemeral_event(
        EphemeralTestEvent::Custom(json!({
            "content": {
                event_c_id: {
                    "m.read.private": {
                        own_user_id: {
                            "ts": 1436453550,
                        },
                    },
                },
            },
            "type": "m.receipt",
        })),
    ));

    mock_sync(&server, ev_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    // Both real and visible receipts are now on event C.
    let (own_user_receipt_event_id, _) =
        timeline.latest_user_read_receipt(own_user_id).await.unwrap();
    assert_eq!(own_user_receipt_event_id, event_c_id);
    let own_receipt_timeline_event =
        timeline.latest_user_read_receipt_timeline_event_id(own_user_id).await.unwrap();
    assert_eq!(own_receipt_timeline_event, event_c_id);
}

#[async_test]
async fn test_send_single_receipt() {
    let room_id = room_id!("!a98sd12bjh:example.org");
    let (client, server) = logged_in_client_with_server().await;
    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

    let own_user_id = client.user_id().unwrap();

    let mut ev_builder = SyncResponseBuilder::new();
    ev_builder.add_joined_room(JoinedRoomBuilder::new(room_id));

    mock_sync(&server, ev_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    let room = client.get_room(room_id).unwrap();
    let timeline = room.timeline().await.unwrap();

    // Unknown receipts are sent.
    let first_receipts_event_id = event_id!("$first_receipts_event_id");

    Mock::given(method("POST"))
        .and(path_regex(r"^/_matrix/client/r0/rooms/.*/receipt/m\.read/"))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
        .expect(1)
        .named("Public read receipt")
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path_regex(r"^/_matrix/client/r0/rooms/.*/receipt/m\.read\.private/"))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
        .expect(1)
        .named("Private read receipt")
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path_regex(r"^/_matrix/client/r0/rooms/.*/receipt/m\.fully_read/"))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
        .expect(1)
        .named("Fully-read marker")
        .mount(&server)
        .await;

    timeline
        .send_single_receipt(
            ReceiptType::Read,
            ReceiptThread::Unthreaded,
            first_receipts_event_id.to_owned(),
        )
        .await
        .unwrap();
    timeline
        .send_single_receipt(
            ReceiptType::ReadPrivate,
            ReceiptThread::Unthreaded,
            first_receipts_event_id.to_owned(),
        )
        .await
        .unwrap();
    timeline
        .send_single_receipt(
            ReceiptType::FullyRead,
            ReceiptThread::Unthreaded,
            first_receipts_event_id.to_owned(),
        )
        .await
        .unwrap();
    server.reset().await;

    // Unchanged receipts are not sent.
    ev_builder.add_joined_room(
        JoinedRoomBuilder::new(room_id)
            .add_ephemeral_event(EphemeralTestEvent::Custom(json!({
                "content": {
                    first_receipts_event_id: {
                        "m.read.private": {
                            own_user_id: {
                                "ts": 1436453550,
                            },
                        },
                        "m.read": {
                            own_user_id: {
                                "ts": 1436453550,
                            },
                        },
                    },
                },
                "type": "m.receipt",
            })))
            .add_account_data(RoomAccountDataTestEvent::Custom(json!({
                "content": {
                    "event_id": first_receipts_event_id,
                },
                "type": "m.fully_read",
            }))),
    );

    mock_sync(&server, ev_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    timeline
        .send_single_receipt(
            ReceiptType::Read,
            ReceiptThread::Unthreaded,
            first_receipts_event_id.to_owned(),
        )
        .await
        .unwrap();
    timeline
        .send_single_receipt(
            ReceiptType::ReadPrivate,
            ReceiptThread::Unthreaded,
            first_receipts_event_id.to_owned(),
        )
        .await
        .unwrap();
    timeline
        .send_single_receipt(
            ReceiptType::FullyRead,
            ReceiptThread::Unthreaded,
            first_receipts_event_id.to_owned(),
        )
        .await
        .unwrap();
    server.reset().await;

    // Receipts with unknown previous receipts are always sent.
    let second_receipts_event_id = event_id!("$second_receipts_event_id");

    Mock::given(method("POST"))
        .and(path_regex(r"^/_matrix/client/r0/rooms/.*/receipt/m\.read/"))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
        .expect(1)
        .named("Public read receipt")
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path_regex(r"^/_matrix/client/r0/rooms/.*/receipt/m\.read\.private/"))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
        .expect(1)
        .named("Private read receipt")
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path_regex(r"^/_matrix/client/r0/rooms/.*/receipt/m\.fully_read/"))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
        .expect(1)
        .named("Fully-read marker")
        .mount(&server)
        .await;

    timeline
        .send_single_receipt(
            ReceiptType::Read,
            ReceiptThread::Unthreaded,
            second_receipts_event_id.to_owned(),
        )
        .await
        .unwrap();
    timeline
        .send_single_receipt(
            ReceiptType::ReadPrivate,
            ReceiptThread::Unthreaded,
            second_receipts_event_id.to_owned(),
        )
        .await
        .unwrap();
    timeline
        .send_single_receipt(
            ReceiptType::FullyRead,
            ReceiptThread::Unthreaded,
            second_receipts_event_id.to_owned(),
        )
        .await
        .unwrap();
    server.reset().await;

    // Newer receipts in the timeline are sent.
    let third_receipts_event_id = event_id!("$third_receipts_event_id");

    ev_builder.add_joined_room(
        JoinedRoomBuilder::new(room_id)
            .add_timeline_event(sync_timeline_event!({
                "content": {
                    "body": "I'm User A",
                    "msgtype": "m.text",
                },
                "event_id": second_receipts_event_id,
                "origin_server_ts": 152046694,
                "sender": "@user_a:example.org",
                "type": "m.room.message",
            }))
            .add_timeline_event(sync_timeline_event!({
                "content": {
                    "body": "I'm User B",
                    "msgtype": "m.text",
                },
                "event_id": third_receipts_event_id,
                "origin_server_ts": 152049794,
                "sender": "@user_b:example.org",
                "type": "m.room.message",
            }))
            .add_ephemeral_event(EphemeralTestEvent::Custom(json!({
                "content": {
                    second_receipts_event_id: {
                        "m.read.private": {
                            own_user_id: {
                                "ts": 1436453550,
                            },
                        },
                        "m.read": {
                            own_user_id: {
                                "ts": 1436453550,
                            },
                        },
                    },
                },
                "type": "m.receipt",
            })))
            .add_account_data(RoomAccountDataTestEvent::Custom(json!({
                "content": {
                    "event_id": second_receipts_event_id,
                },
                "type": "m.fully_read",
            }))),
    );

    mock_sync(&server, ev_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    Mock::given(method("POST"))
        .and(path_regex(r"^/_matrix/client/r0/rooms/.*/receipt/m\.read/"))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
        .expect(1)
        .named("Public read receipt")
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path_regex(r"^/_matrix/client/r0/rooms/.*/receipt/m\.read\.private/"))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
        .expect(1)
        .named("Private read receipt")
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path_regex(r"^/_matrix/client/r0/rooms/.*/receipt/m\.fully_read/"))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
        .expect(1)
        .named("Fully-read marker")
        .mount(&server)
        .await;

    timeline
        .send_single_receipt(
            ReceiptType::Read,
            ReceiptThread::Unthreaded,
            third_receipts_event_id.to_owned(),
        )
        .await
        .unwrap();
    timeline
        .send_single_receipt(
            ReceiptType::ReadPrivate,
            ReceiptThread::Unthreaded,
            third_receipts_event_id.to_owned(),
        )
        .await
        .unwrap();
    timeline
        .send_single_receipt(
            ReceiptType::FullyRead,
            ReceiptThread::Unthreaded,
            third_receipts_event_id.to_owned(),
        )
        .await
        .unwrap();
    server.reset().await;

    // Older receipts in the timeline are not sent.
    ev_builder.add_joined_room(
        JoinedRoomBuilder::new(room_id)
            .add_ephemeral_event(EphemeralTestEvent::Custom(json!({
                "content": {
                    third_receipts_event_id: {
                        "m.read.private": {
                            own_user_id: {
                                "ts": 1436453550,
                            },
                        },
                        "m.read": {
                            own_user_id: {
                                "ts": 1436453550,
                            },
                        },
                    },
                },
                "type": "m.receipt",
            })))
            .add_account_data(RoomAccountDataTestEvent::Custom(json!({
                "content": {
                    "event_id": third_receipts_event_id,
                },
                "type": "m.fully_read",
            }))),
    );

    mock_sync(&server, ev_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    timeline
        .send_single_receipt(
            ReceiptType::Read,
            ReceiptThread::Unthreaded,
            second_receipts_event_id.to_owned(),
        )
        .await
        .unwrap();
    timeline
        .send_single_receipt(
            ReceiptType::ReadPrivate,
            ReceiptThread::Unthreaded,
            second_receipts_event_id.to_owned(),
        )
        .await
        .unwrap();
    timeline
        .send_single_receipt(
            ReceiptType::FullyRead,
            ReceiptThread::Unthreaded,
            second_receipts_event_id.to_owned(),
        )
        .await
        .unwrap();
}

#[async_test]
async fn test_mark_as_read() {
    let room_id = room_id!("!a98sd12bjh:example.org");
    let (client, server) = logged_in_client_with_server().await;
    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

    let own_user_id = client.user_id().unwrap();

    let mut ev_builder = SyncResponseBuilder::new();
    ev_builder.add_joined_room(JoinedRoomBuilder::new(room_id));

    mock_sync(&server, ev_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    let room = client.get_room(room_id).unwrap();
    let timeline = room.timeline().await.unwrap();

    let original_event_id = event_id!("$original_event_id");
    let reaction_event_id = event_id!("$reaction_event_id");

    // When I receive an event with a reaction on it,
    ev_builder.add_joined_room(
        JoinedRoomBuilder::new(room_id)
            .add_timeline_event(sync_timeline_event!({
                "content": {
                    "body": "I like big Rust and I cannot lie",
                    "msgtype": "m.text",
                },
                "event_id": original_event_id,
                "origin_server_ts": 152046694,
                "sender": "@sir-axalot:example.org",
                "type": "m.room.message",
            }))
            .add_ephemeral_event(EphemeralTestEvent::Custom(json!({
                "content": {
                    original_event_id: {
                        "m.read": {
                            own_user_id: {
                                "ts": 1436453550,
                            },
                        },
                    },
                },
                "type": "m.receipt",
            })))
            .add_timeline_event(sync_timeline_event!({
                "content": {
                    "m.relates_to": {
                        "event_id": original_event_id,
                        "key": "🔥🔥🔥",
                        "rel_type": "m.annotation",
                    },
                },
                "event_id": reaction_event_id,
                "origin_server_ts": 152038300,
                "sender": "@prime-minirusta:example.org",
                "type": "m.reaction",
            })),
    );

    mock_sync(&server, ev_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    // And I try to mark the latest event related to a timeline item as read,
    let latest_event = timeline.latest_event().await.expect("missing timeline event item");
    let latest_event_id =
        latest_event.event_id().expect("missing event id for latest timeline event item");

    let has_sent = timeline
        .send_single_receipt(
            ReceiptType::Read,
            ReceiptThread::Unthreaded,
            latest_event_id.to_owned(),
        )
        .await
        .unwrap();

    // Then no request is actually sent, because the event forming the timeline item
    // (the message) is known as read.
    assert!(has_sent.not());
    server.reset().await;

    Mock::given(method("POST"))
        .and(path_regex(r"^/_matrix/client/r0/rooms/.*/receipt/m\.read/"))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
        .expect(1)
        .named("Public read receipt")
        .mount(&server)
        .await;

    // But when I mark the room as read by sending a read receipt to the latest
    // event,
    let has_sent = timeline.mark_as_read(ReceiptType::Read).await.unwrap();

    // It works.
    assert!(has_sent);

    server.reset().await;
}

#[async_test]
async fn test_send_multiple_receipts() {
    let room_id = room_id!("!a98sd12bjh:example.org");
    let (client, server) = logged_in_client_with_server().await;
    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

    let own_user_id = client.user_id().unwrap();

    let mut ev_builder = SyncResponseBuilder::new();
    ev_builder.add_joined_room(JoinedRoomBuilder::new(room_id));

    mock_sync(&server, ev_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    let room = client.get_room(room_id).unwrap();
    let timeline = room.timeline().await.unwrap();

    // Unknown receipts are sent.
    let first_receipts_event_id = event_id!("$first_receipts_event_id");
    let first_receipts = Receipts::new()
        .fully_read_marker(Some(first_receipts_event_id.to_owned()))
        .public_read_receipt(Some(first_receipts_event_id.to_owned()))
        .private_read_receipt(Some(first_receipts_event_id.to_owned()));

    Mock::given(method("POST"))
        .and(path_regex(r"^/_matrix/client/r0/rooms/.*/read_markers$"))
        .and(header("authorization", "Bearer 1234"))
        .and(body_json(json!({
            "m.fully_read": first_receipts_event_id,
            "m.read": first_receipts_event_id,
            "m.read.private": first_receipts_event_id,
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
        .expect(1)
        .mount(&server)
        .await;

    timeline.send_multiple_receipts(first_receipts.clone()).await.unwrap();
    server.reset().await;

    // Unchanged receipts are not sent.
    ev_builder.add_joined_room(
        JoinedRoomBuilder::new(room_id)
            .add_ephemeral_event(EphemeralTestEvent::Custom(json!({
                "content": {
                    first_receipts_event_id: {
                        "m.read.private": {
                            own_user_id: {
                                "ts": 1436453550,
                            },
                        },
                        "m.read": {
                            own_user_id: {
                                "ts": 1436453550,
                            },
                        },
                    },
                },
                "type": "m.receipt",
            })))
            .add_account_data(RoomAccountDataTestEvent::Custom(json!({
                "content": {
                    "event_id": first_receipts_event_id,
                },
                "type": "m.fully_read",
            }))),
    );

    mock_sync(&server, ev_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    timeline.send_multiple_receipts(first_receipts).await.unwrap();
    server.reset().await;

    // Receipts with unknown previous receipts are always sent.
    let second_receipts_event_id = event_id!("$second_receipts_event_id");
    let second_receipts = Receipts::new()
        .fully_read_marker(Some(second_receipts_event_id.to_owned()))
        .public_read_receipt(Some(second_receipts_event_id.to_owned()))
        .private_read_receipt(Some(second_receipts_event_id.to_owned()));

    Mock::given(method("POST"))
        .and(path_regex(r"^/_matrix/client/r0/rooms/.*/read_markers$"))
        .and(header("authorization", "Bearer 1234"))
        .and(body_json(json!({
            "m.fully_read": second_receipts_event_id,
            "m.read": second_receipts_event_id,
            "m.read.private": second_receipts_event_id,
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
        .expect(1)
        .mount(&server)
        .await;

    timeline.send_multiple_receipts(second_receipts.clone()).await.unwrap();
    server.reset().await;

    // Newer receipts in the timeline are sent.
    let third_receipts_event_id = event_id!("$third_receipts_event_id");
    let third_receipts = Receipts::new()
        .fully_read_marker(Some(third_receipts_event_id.to_owned()))
        .public_read_receipt(Some(third_receipts_event_id.to_owned()))
        .private_read_receipt(Some(third_receipts_event_id.to_owned()));

    ev_builder.add_joined_room(
        JoinedRoomBuilder::new(room_id)
            .add_timeline_event(sync_timeline_event!({
                "content": {
                    "body": "I'm User A",
                    "msgtype": "m.text",
                },
                "event_id": second_receipts_event_id,
                "origin_server_ts": 152046694,
                "sender": "@user_a:example.org",
                "type": "m.room.message",
            }))
            .add_timeline_event(sync_timeline_event!({
                "content": {
                    "body": "I'm User B",
                    "msgtype": "m.text",
                },
                "event_id": third_receipts_event_id,
                "origin_server_ts": 152049794,
                "sender": "@user_b:example.org",
                "type": "m.room.message",
            }))
            .add_ephemeral_event(EphemeralTestEvent::Custom(json!({
                "content": {
                    second_receipts_event_id: {
                        "m.read.private": {
                            own_user_id: {
                                "ts": 1436453550,
                            },
                        },
                        "m.read": {
                            own_user_id: {
                                "ts": 1436453550,
                            },
                        },
                    },
                },
                "type": "m.receipt",
            })))
            .add_account_data(RoomAccountDataTestEvent::Custom(json!({
                "content": {
                    "event_id": second_receipts_event_id,
                },
                "type": "m.fully_read",
            }))),
    );

    mock_sync(&server, ev_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    Mock::given(method("POST"))
        .and(path_regex(r"^/_matrix/client/r0/rooms/.*/read_markers$"))
        .and(header("authorization", "Bearer 1234"))
        .and(body_json(json!({
            "m.fully_read": third_receipts_event_id,
            "m.read": third_receipts_event_id,
            "m.read.private": third_receipts_event_id,
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
        .expect(1)
        .mount(&server)
        .await;

    timeline.send_multiple_receipts(third_receipts.clone()).await.unwrap();
    server.reset().await;

    // Older receipts in the timeline are not sent.
    ev_builder.add_joined_room(
        JoinedRoomBuilder::new(room_id)
            .add_ephemeral_event(EphemeralTestEvent::Custom(json!({
                "content": {
                    third_receipts_event_id: {
                        "m.read.private": {
                            own_user_id: {
                                "ts": 1436453550,
                            },
                        },
                        "m.read": {
                            own_user_id: {
                                "ts": 1436453550,
                            },
                        },
                    },
                },
                "type": "m.receipt",
            })))
            .add_account_data(RoomAccountDataTestEvent::Custom(json!({
                "content": {
                    "event_id": third_receipts_event_id,
                },
                "type": "m.fully_read",
            }))),
    );

    mock_sync(&server, ev_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    timeline.send_multiple_receipts(second_receipts.clone()).await.unwrap();
}

#[async_test]
async fn test_latest_user_read_receipt() {
    let room_id = room_id!("!a98sd12bjh:example.org");
    let (client, server) = logged_in_client_with_server().await;
    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

    let own_user_id = client.user_id().unwrap();

    let event_a_id = event_id!("$event_a");
    let event_b_id = event_id!("$event_b");
    let event_c_id = event_id!("$event_c");
    let event_d_id = event_id!("$event_d");
    let event_e_id = event_id!("$event_e");

    let mut ev_builder = SyncResponseBuilder::new();
    ev_builder.add_joined_room(JoinedRoomBuilder::new(room_id));

    mock_sync(&server, ev_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    let room = client.get_room(room_id).unwrap();
    let timeline = room.timeline().await.unwrap();
    let (items, _) = timeline.subscribe().await;

    assert!(items.is_empty());

    let user_receipt = timeline.latest_user_read_receipt(own_user_id).await;
    assert_matches!(user_receipt, None);

    // Only private receipt.
    ev_builder.add_joined_room(JoinedRoomBuilder::new(room_id).add_ephemeral_event(
        EphemeralTestEvent::Custom(json!({
            "content": {
                event_a_id: {
                    "m.read.private": {
                        own_user_id: {},
                    },
                },
            },
            "type": "m.receipt",
        })),
    ));

    mock_sync(&server, ev_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    let (user_receipt_id, _) = timeline.latest_user_read_receipt(own_user_id).await.unwrap();
    assert_eq!(user_receipt_id, event_a_id);

    // Private and public receipts without timestamp should return private
    // receipt.
    ev_builder.add_joined_room(JoinedRoomBuilder::new(room_id).add_ephemeral_event(
        EphemeralTestEvent::Custom(json!({
            "content": {
                event_b_id: {
                    "m.read": {
                        own_user_id: {},
                    },
                },
            },
            "type": "m.receipt",
        })),
    ));

    mock_sync(&server, ev_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    let (user_receipt_id, _) = timeline.latest_user_read_receipt(own_user_id).await.unwrap();
    assert_eq!(user_receipt_id, event_a_id);

    // Public receipt with bigger timestamp.
    ev_builder.add_joined_room(JoinedRoomBuilder::new(room_id).add_ephemeral_event(
        EphemeralTestEvent::Custom(json!({
            "content": {
                event_c_id: {
                    "m.read.private": {
                        own_user_id: {
                            "ts": 1,
                        },
                    },
                },
                event_d_id: {
                    "m.read": {
                        own_user_id: {
                            "ts": 10,
                        },
                    },
                },
            },
            "type": "m.receipt",
        })),
    ));

    mock_sync(&server, ev_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    let (user_receipt_id, _) = timeline.latest_user_read_receipt(own_user_id).await.unwrap();
    assert_eq!(user_receipt_id, event_d_id);

    // Private receipt with bigger timestamp.
    ev_builder.add_joined_room(JoinedRoomBuilder::new(room_id).add_ephemeral_event(
        EphemeralTestEvent::Custom(json!({
            "content": {
                event_e_id: {
                    "m.read.private": {
                        own_user_id: {
                            "ts": 100,
                        },
                    },
                },
            },
            "type": "m.receipt",
        })),
    ));

    mock_sync(&server, ev_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    let (user_receipt_id, _) = timeline.latest_user_read_receipt(own_user_id).await.unwrap();
    assert_eq!(user_receipt_id, event_e_id);
}
