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

use std::time::Duration;

use assert_matches::assert_matches;
use assert_matches2::assert_let;
use eyeball_im::VectorDiff;
use futures_util::StreamExt;
use matrix_sdk::config::SyncSettings;
use matrix_sdk_test::{
    async_test, EventBuilder, JoinedRoomBuilder, SyncResponseBuilder, ALICE, BOB,
};
use matrix_sdk_ui::timeline::{RoomExt, TimelineDetails, TimelineItemContent};
use ruma::{
    assign, event_id,
    events::{
        poll::unstable_start::{
            NewUnstablePollStartEventContent, UnstablePollAnswer, UnstablePollAnswers,
            UnstablePollStartContentBlock,
        },
        relation::InReplyTo,
        room::message::{MessageType, Relation, ReplacementMetadata, RoomMessageEventContent},
    },
    room_id, user_id,
};
use serde_json::json;
use stream_assert::assert_next_matches;
use tokio::time::sleep;
use wiremock::{
    matchers::{method, path_regex},
    Mock, ResponseTemplate,
};

use crate::{logged_in_client, mock_encryption_state, mock_sync};

#[async_test]
async fn edit() {
    let room_id = room_id!("!a98sd12bjh:example.org");
    let (client, server) = logged_in_client().await;
    let event_builder = EventBuilder::new();
    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

    let mut ev_builder = SyncResponseBuilder::new();
    ev_builder.add_joined_room(JoinedRoomBuilder::new(room_id));

    mock_sync(&server, ev_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    let room = client.get_room(room_id).unwrap();
    let timeline = room.timeline().await;
    let (_, mut timeline_stream) = timeline.subscribe().await;

    let event_id = event_id!("$msda7m:localhost");
    ev_builder.add_joined_room(JoinedRoomBuilder::new(room_id).add_timeline_event(
        event_builder.make_sync_message_event_with_id(
            &ALICE,
            event_id,
            RoomMessageEventContent::text_plain("hello"),
        ),
    ));

    mock_sync(&server, ev_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    assert_let!(Some(VectorDiff::PushBack { value: day_divider }) = timeline_stream.next().await);
    assert!(day_divider.is_day_divider());
    assert_let!(Some(VectorDiff::PushBack { value: first }) = timeline_stream.next().await);
    assert_let!(TimelineItemContent::Message(msg) = first.as_event().unwrap().content());
    assert_matches!(msg.msgtype(), MessageType::Text(_));
    assert_matches!(msg.in_reply_to(), None);
    assert!(!msg.is_edited());

    ev_builder.add_joined_room(
        JoinedRoomBuilder::new(room_id)
            .add_timeline_event(event_builder.make_sync_message_event(
                &BOB,
                RoomMessageEventContent::text_html("Test", "<em>Test</em>"),
            ))
            .add_timeline_event(
                event_builder.make_sync_message_event(
                    &ALICE,
                    RoomMessageEventContent::text_plain("hi").make_replacement(
                        ReplacementMetadata::new(event_id.to_owned(), None),
                        None,
                    ),
                ),
            ),
    );

    mock_sync(&server, ev_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    assert_let!(Some(VectorDiff::PushBack { value: second }) = timeline_stream.next().await);
    let item = second.as_event().unwrap();
    assert!(item.event_id().is_some());
    assert!(!item.is_own());
    assert!(item.original_json().is_some());

    assert_let!(TimelineItemContent::Message(msg) = item.content());
    assert_matches!(msg.msgtype(), MessageType::Text(_));
    assert_matches!(msg.in_reply_to(), None);
    assert!(!msg.is_edited());

    // Implicit read receipt of Alice changed.
    assert_matches!(timeline_stream.next().await, Some(VectorDiff::Set { index: 1, .. }));

    assert_let!(Some(VectorDiff::Set { index: 1, value: edit }) = timeline_stream.next().await);
    assert_let!(TimelineItemContent::Message(edited) = edit.as_event().unwrap().content());
    assert_let!(MessageType::Text(text) = edited.msgtype());
    assert_eq!(text.body, "hi");
    assert_matches!(edited.in_reply_to(), None);
    assert!(edited.is_edited());
}

#[async_test]
async fn send_edit() {
    let room_id = room_id!("!a98sd12bjh:example.org");
    let (client, server) = logged_in_client().await;
    let event_builder = EventBuilder::new();
    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

    let mut sync_builder = SyncResponseBuilder::new();
    sync_builder.add_joined_room(JoinedRoomBuilder::new(room_id));

    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    let room = client.get_room(room_id).unwrap();
    let timeline = room.timeline().await;
    let (_, mut timeline_stream) =
        timeline.subscribe_filter_map(|item| item.as_event().cloned()).await;

    sync_builder.add_joined_room(JoinedRoomBuilder::new(room_id).add_timeline_event(
        event_builder.make_sync_message_event_with_id(
            // Same user as the logged_in_client
            user_id!("@example:localhost"),
            event_id!("$original_event"),
            RoomMessageEventContent::text_plain("Hello, World!"),
        ),
    ));

    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    let hello_world_item =
        assert_next_matches!(timeline_stream, VectorDiff::PushBack { value } => value);
    let hello_world_message = hello_world_item.content().as_message().unwrap();
    assert!(!hello_world_message.is_edited());
    assert!(hello_world_item.can_be_edited());

    mock_encryption_state(&server, false).await;
    Mock::given(method("PUT"))
        .and(path_regex(r"^/_matrix/client/r0/rooms/.*/send/.*"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({ "event_id": "$edit_event" })),
        )
        .expect(1)
        .mount(&server)
        .await;

    timeline
        .edit(RoomMessageEventContent::text_plain("Hello, Room!"), &hello_world_item)
        .await
        .unwrap();

    let edit_item =
        assert_next_matches!(timeline_stream, VectorDiff::Set { index: 0, value } => value);

    // The event itself is already known to the server. We don't currently have
    // a separate edit send state.
    assert_matches!(edit_item.send_state(), None);
    let edit_message = edit_item.content().as_message().unwrap();
    assert_eq!(edit_message.body(), "Hello, Room!");
    assert!(edit_message.is_edited());

    // The response to the mocked endpoint does not generate further timeline
    // updates, so just wait for a bit before verifying that the endpoint was
    // called.
    sleep(Duration::from_millis(200)).await;

    server.verify().await;
}

#[async_test]
async fn send_reply_edit() {
    let room_id = room_id!("!a98sd12bjh:example.org");
    let (client, server) = logged_in_client().await;
    let event_builder = EventBuilder::new();
    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

    let mut sync_builder = SyncResponseBuilder::new();
    sync_builder.add_joined_room(JoinedRoomBuilder::new(room_id));

    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    let room = client.get_room(room_id).unwrap();
    let timeline = room.timeline().await;
    let (_, mut timeline_stream) =
        timeline.subscribe_filter_map(|item| item.as_event().cloned()).await;

    let fst_event_id = event_id!("$original_event");
    sync_builder.add_joined_room(
        JoinedRoomBuilder::new(room_id)
            .add_timeline_event(event_builder.make_sync_message_event_with_id(
                &ALICE,
                fst_event_id,
                RoomMessageEventContent::text_plain("Hello, World!"),
            ))
            .add_timeline_event(event_builder.make_sync_message_event(
                // Same user as the logged_in_client
                user_id!("@example:localhost"),
                assign!(RoomMessageEventContent::text_plain("Hello, Alice!"), {
                    relates_to: Some(Relation::Reply {
                        in_reply_to: InReplyTo::new(fst_event_id.to_owned()),
                    })
                }),
            )),
    );

    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    // 'Hello, World!' message
    assert_next_matches!(timeline_stream, VectorDiff::PushBack { .. });

    // Reply message
    let reply_item = assert_next_matches!(timeline_stream, VectorDiff::PushBack { value } => value);
    let reply_message = reply_item.content().as_message().unwrap();
    assert!(!reply_message.is_edited());
    assert!(reply_item.can_be_edited());
    let in_reply_to = reply_message.in_reply_to().unwrap();
    assert_eq!(in_reply_to.event_id, fst_event_id);
    assert_matches!(in_reply_to.event, TimelineDetails::Ready(_));

    mock_encryption_state(&server, false).await;
    Mock::given(method("PUT"))
        .and(path_regex(r"^/_matrix/client/r0/rooms/.*/send/.*"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({ "event_id": "$edit_event" })),
        )
        .expect(1)
        .mount(&server)
        .await;

    timeline.edit(RoomMessageEventContent::text_plain("Hello, Room!"), &reply_item).await.unwrap();

    let edit_item =
        assert_next_matches!(timeline_stream, VectorDiff::Set { index: 1, value } => value);

    // The event itself is already known to the server. We don't currently have
    // a separate edit send state.
    assert_matches!(edit_item.send_state(), None);
    let edit_message = edit_item.content().as_message().unwrap();
    assert_eq!(edit_message.body(), "Hello, Room!");
    assert!(edit_message.is_edited());
    let in_reply_to = reply_message.in_reply_to().unwrap();
    assert_eq!(in_reply_to.event_id, fst_event_id);
    assert_matches!(in_reply_to.event, TimelineDetails::Ready(_));

    // The response to the mocked endpoint does not generate further timeline
    // updates, so just wait for a bit before verifying that the endpoint was
    // called.
    sleep(Duration::from_millis(200)).await;

    server.verify().await;
}

#[async_test]
async fn send_edit_poll() {
    let room_id = room_id!("!a98sd12bjh:example.org");
    let (client, server) = logged_in_client().await;
    let event_builder = EventBuilder::new();
    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

    let mut sync_builder = SyncResponseBuilder::new();
    sync_builder.add_joined_room(JoinedRoomBuilder::new(room_id));

    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    let room = client.get_room(room_id).unwrap();
    let timeline = room.timeline().await;
    let (_, mut timeline_stream) =
        timeline.subscribe_filter_map(|item| item.as_event().cloned()).await;

    let poll_answers = UnstablePollAnswers::try_from(vec![
        UnstablePollAnswer::new("0", "Yes"),
        UnstablePollAnswer::new("1", "no"),
    ])
    .unwrap();
    sync_builder.add_joined_room(JoinedRoomBuilder::new(room_id).add_timeline_event(
        event_builder.make_sync_message_event_with_id(
            // Same user as the logged_in_client
            user_id!("@example:localhost"),
            event_id!("$original_event"),
            NewUnstablePollStartEventContent::new(UnstablePollStartContentBlock::new(
                "Test",
                poll_answers,
            )),
        ),
    ));

    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    let poll_event = assert_next_matches!(timeline_stream, VectorDiff::PushBack { value } => value);
    assert_let!(TimelineItemContent::Poll(poll) = poll_event.content());
    let poll_results = poll.results();
    assert_eq!(poll_results.question, "Test");
    assert_eq!(poll_results.answers.len(), 2);
    assert!(!poll_results.has_been_edited);

    mock_encryption_state(&server, false).await;
    Mock::given(method("PUT"))
        .and(path_regex(r"^/_matrix/client/r0/rooms/.*/send/.*"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({ "event_id": "$edit_event" })),
        )
        .expect(1)
        .mount(&server)
        .await;

    let edited_poll_answers = UnstablePollAnswers::try_from(vec![
        UnstablePollAnswer::new("0", "Yes"),
        UnstablePollAnswer::new("1", "no"),
        UnstablePollAnswer::new("2", "Maybe"),
    ])
    .unwrap();
    let edited_poll =
        UnstablePollStartContentBlock::new("Edited Test".to_owned(), edited_poll_answers);
    timeline.edit_poll("poll_fallback_text", edited_poll, &poll_event).await.unwrap();

    let edit_item =
        assert_next_matches!(timeline_stream, VectorDiff::Set { index: 0, value } => value);

    // The event itself is already known to the server. We don't currently have
    // a separate edit send state.
    assert_matches!(edit_item.send_state(), None);

    assert_let!(TimelineItemContent::Poll(edited_poll) = edit_item.content());
    let edited_poll_results = edited_poll.results();
    assert_eq!(edited_poll_results.question, "Edited Test");
    assert_eq!(edited_poll_results.answers.len(), 3);
    assert!(edited_poll_results.has_been_edited);

    // The response to the mocked endpoint does not generate further timeline
    // updates, so just wait for a bit before verifying that the endpoint was
    // called.
    sleep(Duration::from_millis(200)).await;

    server.verify().await;
}
