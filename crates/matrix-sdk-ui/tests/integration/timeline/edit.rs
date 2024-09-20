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

use as_variant::as_variant;
use assert_matches::assert_matches;
use assert_matches2::assert_let;
use eyeball_im::VectorDiff;
use futures_util::{FutureExt, StreamExt};
use matrix_sdk::{
    config::SyncSettings,
    room::edit::EditedContent,
    test_utils::{events::EventFactory, logged_in_client_with_server},
    Client,
};
use matrix_sdk_test::{
    async_test, mocks::mock_encryption_state, JoinedRoomBuilder, SyncResponseBuilder, ALICE, BOB,
};
use matrix_sdk_ui::{
    timeline::{
        Error, EventSendState, RoomExt, TimelineDetails, TimelineEventItemId, TimelineItemContent,
    },
    Timeline,
};
use ruma::{
    event_id,
    events::{
        poll::unstable_start::{
            NewUnstablePollStartEventContent, ReplacementUnstablePollStartEventContent,
            UnstablePollAnswer, UnstablePollAnswers, UnstablePollStartContentBlock,
            UnstablePollStartEventContent,
        },
        room::message::{
            MessageType, RoomMessageEventContent, RoomMessageEventContentWithoutRelation,
            TextMessageEventContent,
        },
        AnyMessageLikeEventContent, AnyTimelineEvent,
    },
    owned_event_id, room_id,
    serde::Raw,
    OwnedRoomId,
};
use serde_json::json;
use stream_assert::assert_next_matches;
use tokio::{task::yield_now, time::sleep};
use wiremock::{
    matchers::{header, method, path_regex},
    Mock, MockServer, ResponseTemplate,
};

use crate::mock_sync;

#[async_test]
async fn test_edit() {
    let room_id = room_id!("!a98sd12bjh:example.org");
    let (client, server) = logged_in_client_with_server().await;
    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

    let f = EventFactory::new();

    let mut sync_builder = SyncResponseBuilder::new();
    sync_builder.add_joined_room(JoinedRoomBuilder::new(room_id));

    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    mock_encryption_state(&server, false).await;

    let room = client.get_room(room_id).unwrap();
    let timeline = room.timeline().await.unwrap();
    let (_, mut timeline_stream) = timeline.subscribe().await;

    let event_id = event_id!("$msda7m:localhost");
    sync_builder.add_joined_room(
        JoinedRoomBuilder::new(room_id)
            .add_timeline_event(f.text_msg("hello").sender(&ALICE).event_id(event_id)),
    );

    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    assert_let!(Some(VectorDiff::PushBack { value: first }) = timeline_stream.next().await);
    let item = first.as_event().unwrap();
    assert_eq!(item.read_receipts().len(), 1, "implicit read receipt");
    assert_let!(TimelineItemContent::Message(msg) = item.content());
    assert_matches!(msg.msgtype(), MessageType::Text(_));
    assert_matches!(msg.in_reply_to(), None);
    assert!(!msg.is_edited());

    assert_let!(Some(VectorDiff::PushFront { value: day_divider }) = timeline_stream.next().await);
    assert!(day_divider.is_day_divider());

    sync_builder.add_joined_room(
        JoinedRoomBuilder::new(room_id)
            .add_timeline_event(f.text_html("Test", "<em>Test</em>").sender(&BOB))
            .add_timeline_event(
                f.text_msg("* hi")
                    .sender(&ALICE)
                    .edit(event_id, RoomMessageEventContent::text_plain("hi").into()),
            ),
    );

    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    assert_let!(Some(VectorDiff::PushBack { value: second }) = timeline_stream.next().await);
    let item = second.as_event().unwrap();
    assert!(item.event_id().is_some());
    assert!(!item.is_own());
    assert!(item.original_json().is_some());
    assert_eq!(item.read_receipts().len(), 1, "implicit read receipt");

    assert_let!(TimelineItemContent::Message(msg) = item.content());
    assert_let!(MessageType::Text(TextMessageEventContent { body, .. }) = msg.msgtype());
    assert_eq!(body, "Test");
    assert_matches!(msg.in_reply_to(), None);
    assert!(!msg.is_edited());

    // No more implicit read receipt in Alice's message, because they edited
    // something after the second event.
    assert_let!(Some(VectorDiff::Set { index: 1, value: item }) = timeline_stream.next().await);
    let item = item.as_event().unwrap();
    assert_let!(TimelineItemContent::Message(msg) = item.content());
    assert_let!(MessageType::Text(text) = msg.msgtype());
    assert_eq!(text.body, "hello");
    assert_matches!(msg.in_reply_to(), None);
    assert!(!msg.is_edited());
    assert_eq!(item.read_receipts().len(), 0, "no more implicit read receipt");

    // ... so Alice's read receipt moves to Bob's message.
    assert_let!(Some(VectorDiff::Set { index: 2, value: second }) = timeline_stream.next().await);
    let item = second.as_event().unwrap();
    assert!(item.event_id().is_some());
    assert!(!item.is_own());
    assert!(item.original_json().is_some());
    assert_eq!(item.read_receipts().len(), 2, "should carry alice and bob's read receipts");

    // The text changes in Alice's message.
    assert_let!(Some(VectorDiff::Set { index: 1, value: edit }) = timeline_stream.next().await);
    let item = edit.as_event().unwrap();
    assert_let!(TimelineItemContent::Message(edited) = item.content());
    assert_let!(MessageType::Text(text) = edited.msgtype());
    assert_eq!(text.body, "hi");
    assert_matches!(edited.in_reply_to(), None);
    assert!(edited.is_edited());
}

#[async_test]
async fn test_edit_local_echo() {
    let room_id = room_id!("!a98sd12bjh:example.org");
    let (client, server) = logged_in_client_with_server().await;
    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

    let mut sync_builder = SyncResponseBuilder::new();
    sync_builder.add_joined_room(JoinedRoomBuilder::new(room_id));

    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    mock_encryption_state(&server, false).await;

    let room = client.get_room(room_id).unwrap();
    let timeline = room.timeline().await.unwrap();
    let (_, mut timeline_stream) = timeline.subscribe().await;

    sync_builder.add_joined_room(JoinedRoomBuilder::new(room_id));

    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    mock_encryption_state(&server, false).await;
    let mounted_send = Mock::given(method("PUT"))
        .and(path_regex(r"^/_matrix/client/r0/rooms/.*/send/.*"))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(413).set_body_json(json!({
            "errcode": "M_TOO_LARGE",
        })))
        .expect(1)
        .mount_as_scoped(&server)
        .await;

    // Redacting a local event works.
    timeline.send(RoomMessageEventContent::text_plain("hello, just you").into()).await.unwrap();

    assert_let!(Some(VectorDiff::PushBack { value: item }) = timeline_stream.next().await);

    let internal_id = item.unique_id();

    let item = item.as_event().unwrap();
    assert_matches!(item.send_state(), Some(EventSendState::NotSentYet));

    assert_let!(Some(VectorDiff::PushFront { value: day_divider }) = timeline_stream.next().await);
    assert!(day_divider.is_day_divider());

    // We haven't set a route for sending events, so this will fail.

    assert_let!(Some(VectorDiff::Set { index: 1, value: item }) = timeline_stream.next().await);

    let item = item.as_event().unwrap();
    assert!(item.is_local_echo());
    assert!(item.is_editable());

    assert_matches!(
        item.send_state(),
        Some(EventSendState::SendingFailed { is_recoverable: false, .. })
    );

    assert!(timeline_stream.next().now_or_never().is_none());

    // Set up the success response before editing, since edit causes an immediate
    // retry (the room's send queue is not blocked, since the one event it couldn't
    // send failed in an unrecoverable way).
    drop(mounted_send);
    Mock::given(method("PUT"))
        .and(path_regex(r"^/_matrix/client/r0/rooms/.*/send/.*"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "event_id": "$1" })))
        .expect(1)
        .mount(&server)
        .await;

    // Let's edit the local echo.
    let did_edit = timeline
        .edit(
            item,
            EditedContent::RoomMessage(RoomMessageEventContent::text_plain("hello, world").into()),
        )
        .await
        .unwrap();

    // We could edit the local echo, since it was in the failed state.
    assert!(did_edit);

    // Observe local echo being replaced.
    assert_let!(Some(VectorDiff::Set { index: 1, value: item }) = timeline_stream.next().await);

    assert_eq!(item.unique_id(), internal_id);

    let item = item.as_event().unwrap();
    assert!(item.is_local_echo());

    // The send state has been reset.
    assert_matches!(item.send_state(), Some(EventSendState::NotSentYet));

    let edit_message = item.content().as_message().unwrap();
    assert_eq!(edit_message.body(), "hello, world");

    // Observe the event being sent, and replacing the local echo.
    assert_let!(Some(VectorDiff::Set { index: 1, value: item }) = timeline_stream.next().await);

    let item = item.as_event().unwrap();
    assert!(item.is_local_echo());

    let edit_message = item.content().as_message().unwrap();
    assert_eq!(edit_message.body(), "hello, world");

    // No new updates.
    assert!(timeline_stream.next().now_or_never().is_none());
}

#[async_test]
async fn test_send_edit() {
    let room_id = room_id!("!a98sd12bjh:example.org");
    let (client, server) = logged_in_client_with_server().await;
    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

    let mut sync_builder = SyncResponseBuilder::new();
    sync_builder.add_joined_room(JoinedRoomBuilder::new(room_id));

    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    mock_encryption_state(&server, false).await;

    let room = client.get_room(room_id).unwrap();
    let timeline = room.timeline().await.unwrap();
    let (_, mut timeline_stream) =
        timeline.subscribe_filter_map(|item| item.as_event().cloned()).await;

    let f = EventFactory::new();
    sync_builder.add_joined_room(
        JoinedRoomBuilder::new(room_id).add_timeline_event(
            f.text_msg("Hello, World!")
                .sender(client.user_id().unwrap())
                .event_id(event_id!("$original_event")),
        ),
    );

    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    let hello_world_item =
        assert_next_matches!(timeline_stream, VectorDiff::PushBack { value } => value);
    let hello_world_message = hello_world_item.content().as_message().unwrap();
    assert!(!hello_world_message.is_edited());
    assert!(hello_world_item.is_editable());

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
        .edit(
            &hello_world_item,
            EditedContent::RoomMessage(RoomMessageEventContentWithoutRelation::text_plain(
                "Hello, Room!",
            )),
        )
        .await
        .unwrap();

    // Let the send queue handle the event.
    yield_now().await;

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
async fn test_send_reply_edit() {
    let room_id = room_id!("!a98sd12bjh:example.org");
    let (client, server) = logged_in_client_with_server().await;
    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

    let mut sync_builder = SyncResponseBuilder::new();
    sync_builder.add_joined_room(JoinedRoomBuilder::new(room_id));

    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    mock_encryption_state(&server, false).await;

    let room = client.get_room(room_id).unwrap();
    let timeline = room.timeline().await.unwrap();
    let (_, mut timeline_stream) =
        timeline.subscribe_filter_map(|item| item.as_event().cloned()).await;

    let f = EventFactory::new();
    let event_id = event_id!("$original_event");
    sync_builder.add_joined_room(
        JoinedRoomBuilder::new(room_id)
            .add_timeline_event(f.text_msg("Hello, World!").sender(&ALICE).event_id(event_id))
            .add_timeline_event(
                f.text_msg("Hello, Alice!").reply_to(event_id).sender(client.user_id().unwrap()),
            ),
    );

    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    // 'Hello, World!' message.
    assert_next_matches!(timeline_stream, VectorDiff::PushBack { .. });

    // Reply message.
    let reply_item = assert_next_matches!(timeline_stream, VectorDiff::PushBack { value } => value);
    let reply_message = reply_item.content().as_message().unwrap();
    assert!(!reply_message.is_edited());
    assert!(reply_item.is_editable());
    let in_reply_to = reply_message.in_reply_to().unwrap();
    assert_eq!(in_reply_to.event_id, event_id);
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

    timeline
        .edit(
            &reply_item,
            EditedContent::RoomMessage(RoomMessageEventContentWithoutRelation::text_plain(
                "Hello, Room!",
            )),
        )
        .await
        .unwrap();

    // Let the send queue handle the event.
    yield_now().await;

    let edit_item =
        assert_next_matches!(timeline_stream, VectorDiff::Set { index: 1, value } => value);

    // The event itself is already known to the server. We don't currently have
    // a separate edit send state.
    assert_matches!(edit_item.send_state(), None);
    let edit_message = edit_item.content().as_message().unwrap();
    assert_eq!(edit_message.body(), "Hello, Room!");
    assert!(edit_message.is_edited());
    let in_reply_to = reply_message.in_reply_to().unwrap();
    assert_eq!(in_reply_to.event_id, event_id);
    assert_matches!(in_reply_to.event, TimelineDetails::Ready(_));

    // The response to the mocked endpoint does not generate further timeline
    // updates, so just wait for a bit before verifying that the endpoint was
    // called.
    sleep(Duration::from_millis(200)).await;

    server.verify().await;
}

#[async_test]
async fn test_send_edit_poll() {
    let room_id = room_id!("!a98sd12bjh:example.org");
    let (client, server) = logged_in_client_with_server().await;
    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

    let mut sync_builder = SyncResponseBuilder::new();
    sync_builder.add_joined_room(JoinedRoomBuilder::new(room_id));

    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    mock_encryption_state(&server, false).await;

    let room = client.get_room(room_id).unwrap();
    let timeline = room.timeline().await.unwrap();
    let (_, mut timeline_stream) =
        timeline.subscribe_filter_map(|item| item.as_event().cloned()).await;

    let poll_answers = UnstablePollAnswers::try_from(vec![
        UnstablePollAnswer::new("0", "Yes"),
        UnstablePollAnswer::new("1", "no"),
    ])
    .unwrap();

    let f = EventFactory::new();
    sync_builder.add_joined_room(
        JoinedRoomBuilder::new(room_id).add_timeline_event(
            f.event(NewUnstablePollStartEventContent::new(UnstablePollStartContentBlock::new(
                "Test",
                poll_answers,
            )))
            .sender(client.user_id().unwrap())
            .event_id(event_id!("$original_event")),
        ),
    );

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
    timeline
        .edit(
            &poll_event,
            EditedContent::PollStart {
                fallback_text: "poll_fallback_text".to_owned(),
                new_content: edited_poll,
            },
        )
        .await
        .unwrap();

    // Let the send queue handle the event.
    yield_now().await;

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

#[async_test]
async fn test_send_edit_when_timeline_is_clear() {
    let room_id = room_id!("!a98sd12bjh:example.org");
    let (client, server) = logged_in_client_with_server().await;
    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

    let mut sync_builder = SyncResponseBuilder::new();
    sync_builder.add_joined_room(JoinedRoomBuilder::new(room_id));

    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    mock_encryption_state(&server, false).await;

    let room = client.get_room(room_id).unwrap();
    let timeline = room.timeline().await.unwrap();
    let (_, mut timeline_stream) =
        timeline.subscribe_filter_map(|item| item.as_event().cloned()).await;

    let f = EventFactory::new();
    let raw_original_event = f
        .text_msg("Hello, World!")
        .sender(client.user_id().unwrap())
        .event_id(event_id!("$original_event"))
        .into_raw_sync();
    sync_builder.add_joined_room(
        JoinedRoomBuilder::new(room_id).add_timeline_event(raw_original_event.clone()),
    );

    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    let hello_world_item =
        assert_next_matches!(timeline_stream, VectorDiff::PushBack { value } => value);
    let hello_world_message = hello_world_item.content().as_message().unwrap();
    assert!(!hello_world_message.is_edited());
    assert!(hello_world_item.is_editable());

    // Clear the event cache (hence the timeline) to make sure the old item does not
    // need to be available in it for the edit to work.
    client.event_cache().add_initial_events(room_id, vec![], None).await.unwrap();
    client.event_cache().empty_immutable_cache().await;

    yield_now().await;
    assert_next_matches!(timeline_stream, VectorDiff::Clear);

    mock_encryption_state(&server, false).await;
    Mock::given(method("PUT"))
        .and(path_regex(r"^/_matrix/client/r0/rooms/.*/send/.*"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({ "event_id": "$edit_event" })),
        )
        .expect(1)
        .mount(&server)
        .await;

    // Since we assume we can't use the timeline item directly in this use case, the
    // API will fetch the event from the server directly so we need to mock the
    // response.
    Mock::given(method("GET"))
        .and(path_regex(r"^/_matrix/client/r0/rooms/.*/event/"))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(200).set_body_json(raw_original_event.json()))
        .expect(1)
        .named("event_1")
        .mount(&server)
        .await;

    timeline
        .edit(
            &hello_world_item,
            EditedContent::RoomMessage(RoomMessageEventContentWithoutRelation::text_plain(
                "Hello, Room!",
            )),
        )
        .await
        .unwrap();

    // Since verifying the content would mean mocking the sliding sync response with
    // what we are already expecting, because this test would require to paginate
    // again the timeline, testing the content change would not be meaningful.
    // Use an integration test for the full case.

    // The response to the mocked endpoint does not generate further timeline
    // updates, so just wait for a bit before verifying that the endpoint was
    // called.
    sleep(Duration::from_millis(200)).await;

    server.verify().await;
}

#[async_test]
async fn test_edit_local_echo_with_unsupported_content() {
    let room_id = room_id!("!a98sd12bjh:example.org");
    let (client, server) = logged_in_client_with_server().await;
    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

    let mut sync_builder = SyncResponseBuilder::new();
    sync_builder.add_joined_room(JoinedRoomBuilder::new(room_id));

    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    mock_encryption_state(&server, false).await;

    let room = client.get_room(room_id).unwrap();
    let timeline = room.timeline().await.unwrap();
    let (_, mut timeline_stream) = timeline.subscribe().await;

    sync_builder.add_joined_room(JoinedRoomBuilder::new(room_id));

    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    mock_encryption_state(&server, false).await;
    let mounted_send = Mock::given(method("PUT"))
        .and(path_regex(r"^/_matrix/client/r0/rooms/.*/send/.*"))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(413).set_body_json(json!({
            "errcode": "M_TOO_LARGE",
        })))
        .expect(1)
        .mount_as_scoped(&server)
        .await;

    timeline.send(RoomMessageEventContent::text_plain("hello, just you").into()).await.unwrap();

    assert_let!(Some(VectorDiff::PushBack { value: item }) = timeline_stream.next().await);

    let item = item.as_event().unwrap();
    assert_matches!(item.send_state(), Some(EventSendState::NotSentYet));

    assert_let!(Some(VectorDiff::PushFront { value: day_divider }) = timeline_stream.next().await);
    assert!(day_divider.is_day_divider());

    // We haven't set a route for sending events, so this will fail.

    assert_let!(Some(VectorDiff::Set { index: 1, value: item }) = timeline_stream.next().await);

    let item = item.as_event().unwrap();
    assert!(item.is_local_echo());
    assert!(item.is_editable());

    assert_matches!(
        item.send_state(),
        Some(EventSendState::SendingFailed { is_recoverable: false, .. })
    );

    assert!(timeline_stream.next().now_or_never().is_none());

    // Set up the success response before editing, since edit causes an immediate
    // retry (the room's send queue is not blocked, since the one event it couldn't
    // send failed in an unrecoverable way).
    drop(mounted_send);
    Mock::given(method("PUT"))
        .and(path_regex(r"^/_matrix/client/r0/rooms/.*/send/.*"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "event_id": "$1" })))
        .mount(&server)
        .await;

    let answers = vec![UnstablePollAnswer::new("A", "Answer A")].try_into().unwrap();
    let poll_content_block = UnstablePollStartContentBlock::new("question", answers);
    let poll_start_content = EditedContent::PollStart {
        fallback_text: "edited".to_owned(),
        new_content: poll_content_block.clone(),
    };

    // Let's edit the local echo (message) with an unsupported type (poll start).
    let did_edit = timeline.edit(item, poll_start_content).await.unwrap();

    // We couldn't edit the local echo, since their content types didn't match
    assert!(!did_edit);

    timeline
        .send(AnyMessageLikeEventContent::UnstablePollStart(UnstablePollStartEventContent::New(
            NewUnstablePollStartEventContent::new(poll_content_block),
        )))
        .await
        .unwrap();

    assert_let!(Some(VectorDiff::PushBack { value: item }) = timeline_stream.next().await);

    let item = item.as_event().unwrap();
    assert_matches!(item.send_state(), Some(EventSendState::NotSentYet));

    // Let's edit the local echo (poll start) with an unsupported type (message).
    let did_edit = timeline
        .edit(
            item,
            EditedContent::RoomMessage(RoomMessageEventContentWithoutRelation::text_plain(
                "edited",
            )),
        )
        .await
        .unwrap();

    // We couldn't edit the local echo, since their content types didn't match
    assert!(!did_edit);
}

struct PendingEditHelper {
    client: Client,
    server: MockServer,
    timeline: Timeline,
    sync_builder: SyncResponseBuilder,
    sync_settings: SyncSettings,
    room_id: OwnedRoomId,
}

impl PendingEditHelper {
    async fn new() -> Self {
        let room_id = room_id!("!a98sd12bjh:example.org");
        let (client, server) = logged_in_client_with_server().await;
        let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

        let mut sync_builder = SyncResponseBuilder::new();
        sync_builder.add_joined_room(JoinedRoomBuilder::new(room_id));

        mock_sync(&server, sync_builder.build_json_sync_response(), None).await;
        let _response = client.sync_once(sync_settings.clone()).await.unwrap();
        server.reset().await;

        mock_encryption_state(&server, false).await;

        {
            // Fill the initial prev-batch token to avoid waiting for it later.
            let ec = client.event_cache();
            ec.subscribe().unwrap();
            ec.add_initial_events(room_id, vec![], Some("prev-batch-token".to_owned()))
                .await
                .unwrap();
        }

        let room = client.get_room(room_id).unwrap();
        let timeline = room.timeline().await.unwrap();

        Self { client, server, timeline, sync_builder, sync_settings, room_id: room_id.to_owned() }
    }

    async fn handle_sync(&mut self, joined_room_builder: JoinedRoomBuilder) {
        self.sync_builder.add_joined_room(joined_room_builder);

        mock_sync(&self.server, self.sync_builder.build_json_sync_response(), None).await;
        let _response = self.client.sync_once(self.sync_settings.clone()).await.unwrap();

        self.server.reset().await;
    }

    async fn handle_backpagination(&mut self, events: Vec<Raw<AnyTimelineEvent>>, batch_size: u16) {
        Mock::given(method("GET"))
            .and(path_regex(r"^/_matrix/client/r0/rooms/.*/messages$"))
            .and(header("authorization", "Bearer 1234"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "start": "123",
                "end": "yolo",
                "chunk": events,
                "state": []
            })))
            .expect(1)
            .mount(&self.server)
            .await;

        self.timeline.live_paginate_backwards(batch_size).await.unwrap();

        self.server.reset().await;
    }
}

#[async_test]
async fn test_pending_edit() {
    let mut h = PendingEditHelper::new().await;
    let f = EventFactory::new();

    let (_, mut timeline_stream) = h.timeline.subscribe().await;

    // When I receive an edit event for an event I don't know about…
    let original_event_id = event_id!("$original");
    let edit_event_id = event_id!("$edit");

    h.handle_sync(
        JoinedRoomBuilder::new(&h.room_id).add_timeline_event(
            f.text_msg("* hello")
                .sender(&ALICE)
                .event_id(edit_event_id)
                .edit(original_event_id, RoomMessageEventContent::text_plain("[edit]").into()),
        ),
    )
    .await;

    // Nothing happens.
    assert!(timeline_stream.next().now_or_never().is_none());

    // But when I receive the original event after a bit…
    h.handle_sync(
        JoinedRoomBuilder::new(&h.room_id)
            .add_timeline_event(f.text_msg("hi").sender(&ALICE).event_id(original_event_id)),
    )
    .await;

    // Then I get the edited content immediately.
    assert_let!(Some(VectorDiff::PushBack { value }) = timeline_stream.next().await);
    let msg = value.as_event().unwrap().content().as_message().unwrap();
    assert!(msg.is_edited());
    assert_eq!(msg.body(), "[edit]");

    // The day divider.
    assert_next_matches!(timeline_stream, VectorDiff::PushFront { value } => {
        assert!(value.is_day_divider());
    });

    // And nothing else.
    assert!(timeline_stream.next().now_or_never().is_none());
}

#[async_test]
async fn test_pending_edit_overrides() {
    let mut h = PendingEditHelper::new().await;
    let f = EventFactory::new();

    let (_, mut timeline_stream) = h.timeline.subscribe().await;

    // When I receive multiple edit events for an event I don't know about…
    let original_event_id = event_id!("$original");
    let edit_event_id = event_id!("$edit");
    let edit_event_id2 = event_id!("$edit2");
    h.handle_sync(
        JoinedRoomBuilder::new(&h.room_id)
            .add_timeline_event(
                f.text_msg("* hello")
                    .sender(&ALICE)
                    .event_id(edit_event_id)
                    .edit(original_event_id, RoomMessageEventContent::text_plain("hello").into()),
            )
            .add_timeline_event(
                f.text_msg("* bonjour")
                    .sender(&ALICE)
                    .event_id(edit_event_id2)
                    .edit(original_event_id, RoomMessageEventContent::text_plain("bonjour").into()),
            ),
    )
    .await;

    // Nothing happens.
    assert!(timeline_stream.next().now_or_never().is_none());

    // And then I receive the original event after a bit…
    h.handle_sync(
        JoinedRoomBuilder::new(&h.room_id)
            .add_timeline_event(f.text_msg("hi").sender(&ALICE).event_id(original_event_id)),
    )
    .await;

    // Then I get the latest edited content immediately.
    assert_let!(Some(VectorDiff::PushBack { value }) = timeline_stream.next().await);
    let msg = value.as_event().unwrap().content().as_message().unwrap();
    assert!(msg.is_edited());
    assert_eq!(msg.body(), "bonjour");

    // The day divider.
    assert_next_matches!(timeline_stream, VectorDiff::PushFront { value } => {
        assert!(value.is_day_divider());
    });

    // And nothing else.
    assert!(timeline_stream.next().now_or_never().is_none());
}

#[async_test]
async fn test_pending_edit_from_backpagination() {
    let mut h = PendingEditHelper::new().await;
    let f = EventFactory::new();

    let (_, mut timeline_stream) = h.timeline.subscribe().await;

    // When I receive an edit from a back-pagination for an event I don't know
    // about…
    let original_event_id = event_id!("$original");
    let edit_event_id = event_id!("$edit");
    h.handle_backpagination(
        vec![f
            .text_msg("* hello")
            .sender(&ALICE)
            .event_id(edit_event_id)
            .room(&h.room_id)
            .edit(original_event_id, RoomMessageEventContent::text_plain("hello").into())
            .into()],
        10,
    )
    .await;

    // Nothing happens.
    assert!(timeline_stream.next().now_or_never().is_none());

    // And then I receive the original event after a bit…
    h.handle_sync(
        JoinedRoomBuilder::new(&h.room_id)
            .add_timeline_event(f.text_msg("hi").sender(&ALICE).event_id(original_event_id)),
    )
    .await;

    // Then I get the latest edited content immediately.
    assert_let!(Some(VectorDiff::PushBack { value }) = timeline_stream.next().await);
    let msg = value.as_event().unwrap().content().as_message().unwrap();
    assert!(msg.is_edited());
    assert_eq!(msg.body(), "hello");

    // The day divider.
    assert_next_matches!(timeline_stream, VectorDiff::PushFront { value } => {
        assert!(value.is_day_divider());
    });

    // And nothing else.
    assert!(timeline_stream.next().now_or_never().is_none());
}

#[async_test]
async fn test_pending_edit_from_backpagination_doesnt_override_pending_edit_from_sync() {
    let mut h = PendingEditHelper::new().await;
    let f = EventFactory::new();

    let (_, mut timeline_stream) = h.timeline.subscribe().await;

    // When I receive an edit live from a sync for an event I don't know about…
    let original_event_id = event_id!("$original");
    let edit_event_id = event_id!("$edit");
    h.handle_sync(
        JoinedRoomBuilder::new(&h.room_id).add_timeline_event(
            f.text_msg("* hello")
                .sender(&ALICE)
                .event_id(edit_event_id)
                .edit(original_event_id, RoomMessageEventContent::text_plain("[edit]").into()),
        ),
    )
    .await;

    // And then I receive an edit from a back-pagination for the same event…
    let edit_event_id2 = event_id!("$edit2");
    h.handle_backpagination(
        vec![f
            .text_msg("* aloha")
            .sender(&ALICE)
            .event_id(edit_event_id2)
            .room(&h.room_id)
            .edit(original_event_id, RoomMessageEventContent::text_plain("aloha").into())
            .into()],
        10,
    )
    .await;

    // Nothing happens.
    assert!(timeline_stream.next().now_or_never().is_none());

    // And then I receive the original event after a bit…
    h.handle_sync(
        JoinedRoomBuilder::new(&h.room_id)
            .add_timeline_event(f.text_msg("hi").sender(&ALICE).event_id(original_event_id)),
    )
    .await;

    // Then I get the edit from the sync, even if the back-pagination happened
    // after.
    assert_let!(Some(VectorDiff::PushBack { value }) = timeline_stream.next().await);
    let msg = value.as_event().unwrap().content().as_message().unwrap();
    assert!(msg.is_edited());
    assert_eq!(msg.body(), "[edit]");

    // The day divider.
    assert_next_matches!(timeline_stream, VectorDiff::PushFront { value } => {
        assert!(value.is_day_divider());
    });

    // And nothing else.
    assert!(timeline_stream.next().now_or_never().is_none());
}

#[async_test]
async fn test_pending_poll_edit() {
    let mut h = PendingEditHelper::new().await;
    let f = EventFactory::new();

    let (_, mut timeline_stream) = h.timeline.subscribe().await;

    // When I receive an edit event for an event I don't know about…
    let original_event_id = event_id!("$original");
    let edit_event_id = event_id!("$edit");

    let new_start = UnstablePollStartContentBlock::new(
        "Edited poll",
        UnstablePollAnswers::try_from(vec![
            UnstablePollAnswer::new("0", "Yes"),
            UnstablePollAnswer::new("1", "No"),
        ])
        .unwrap(),
    );

    h.handle_sync(
        JoinedRoomBuilder::new(&h.room_id).add_timeline_event(
            f.event(ReplacementUnstablePollStartEventContent::new(
                new_start,
                original_event_id.to_owned(),
            ))
            .sender(&ALICE)
            .event_id(edit_event_id),
        ),
    )
    .await;

    // Nothing happens.
    assert!(timeline_stream.next().now_or_never().is_none());

    // But when I receive the original event after a bit…
    let event_content = NewUnstablePollStartEventContent::new(UnstablePollStartContentBlock::new(
        "Original poll",
        UnstablePollAnswers::try_from(vec![
            UnstablePollAnswer::new("0", "f yeah"),
            UnstablePollAnswer::new("1", "noooope"),
        ])
        .unwrap(),
    ));

    h.handle_sync(
        JoinedRoomBuilder::new(&h.room_id)
            .add_timeline_event(f.event(event_content).sender(&ALICE).event_id(original_event_id)),
    )
    .await;

    // Then I get the edited content immediately.
    assert_let!(Some(VectorDiff::PushBack { value }) = timeline_stream.next().await);
    let poll = as_variant!(value.as_event().unwrap().content(), TimelineItemContent::Poll).unwrap();
    assert!(poll.is_edit());

    let results = poll.results();
    assert_eq!(results.question, "Edited poll");
    assert_eq!(results.answers[0].text, "Yes");
    assert_eq!(results.answers[1].text, "No");

    // The day divider.
    assert_next_matches!(timeline_stream, VectorDiff::PushFront { value } => {
        assert!(value.is_day_divider());
    });

    // And nothing else.
    assert!(timeline_stream.next().now_or_never().is_none());
}

#[async_test]
async fn test_send_edit_by_event_id() {
    let room_id = room_id!("!a98sd12bjh:example.org");
    let (client, server) = logged_in_client_with_server().await;
    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

    let mut sync_builder = SyncResponseBuilder::new();
    sync_builder.add_joined_room(JoinedRoomBuilder::new(room_id));

    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    mock_encryption_state(&server, false).await;

    let room = client.get_room(room_id).unwrap();
    let timeline = room.timeline().await.unwrap();
    let (_, mut timeline_stream) =
        timeline.subscribe_filter_map(|item| item.as_event().cloned()).await;

    let f = EventFactory::new();
    sync_builder.add_joined_room(
        JoinedRoomBuilder::new(room_id).add_timeline_event(
            f.text_msg("Hello, World!")
                .sender(client.user_id().unwrap())
                .event_id(event_id!("$original_event")),
        ),
    );

    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    let hello_world_item =
        assert_next_matches!(timeline_stream, VectorDiff::PushBack { value } => value);
    let hello_world_message = hello_world_item.content().as_message().unwrap();
    assert!(!hello_world_message.is_edited());
    assert!(hello_world_item.is_editable());

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
        .edit_by_id(
            &hello_world_item.identifier(),
            EditedContent::RoomMessage(RoomMessageEventContentWithoutRelation::text_plain(
                "Hello, Room!",
            )),
        )
        .await
        .unwrap();

    // Let the send queue handle the event.
    yield_now().await;

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
async fn test_send_edit_by_non_existing_event_id() {
    let room_id = room_id!("!a98sd12bjh:example.org");
    let (client, server) = logged_in_client_with_server().await;
    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

    let mut sync_builder = SyncResponseBuilder::new();
    sync_builder.add_joined_room(JoinedRoomBuilder::new(room_id));

    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    mock_encryption_state(&server, false).await;

    let room = client.get_room(room_id).unwrap();
    let timeline = room.timeline().await.unwrap();

    mock_encryption_state(&server, false).await;

    let error = timeline
        .edit_by_id(
            &TimelineEventItemId::EventId(owned_event_id!("$123:example.com")),
            EditedContent::RoomMessage(RoomMessageEventContentWithoutRelation::text_plain(
                "Hello, Room!",
            )),
        )
        .await
        .err()
        .unwrap();
    assert_matches!(error, Error::EventNotInTimeline(_));
}

#[async_test]
async fn test_edit_local_echo_by_id() {
    let room_id = room_id!("!a98sd12bjh:example.org");
    let (client, server) = logged_in_client_with_server().await;
    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

    let mut sync_builder = SyncResponseBuilder::new();
    sync_builder.add_joined_room(JoinedRoomBuilder::new(room_id));

    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    mock_encryption_state(&server, false).await;

    let room = client.get_room(room_id).unwrap();
    let timeline = room.timeline().await.unwrap();
    let (_, mut timeline_stream) = timeline.subscribe().await;

    sync_builder.add_joined_room(JoinedRoomBuilder::new(room_id));

    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    mock_encryption_state(&server, false).await;
    let mounted_send = Mock::given(method("PUT"))
        .and(path_regex(r"^/_matrix/client/r0/rooms/.*/send/.*"))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(413).set_body_json(json!({
            "errcode": "M_TOO_LARGE",
        })))
        .expect(1)
        .mount_as_scoped(&server)
        .await;

    // Redacting a local event works.
    timeline.send(RoomMessageEventContent::text_plain("hello, just you").into()).await.unwrap();

    assert_let!(Some(VectorDiff::PushBack { value: item }) = timeline_stream.next().await);

    let internal_id = item.unique_id();

    let item = item.as_event().unwrap();
    assert_matches!(item.send_state(), Some(EventSendState::NotSentYet));

    assert_let!(Some(VectorDiff::PushFront { value: day_divider }) = timeline_stream.next().await);
    assert!(day_divider.is_day_divider());

    // We haven't set a route for sending events, so this will fail.

    assert_let!(Some(VectorDiff::Set { index: 1, value: item }) = timeline_stream.next().await);

    let item = item.as_event().unwrap();
    assert!(item.is_local_echo());
    assert!(item.is_editable());

    assert_matches!(
        item.send_state(),
        Some(EventSendState::SendingFailed { is_recoverable: false, .. })
    );

    assert!(timeline_stream.next().now_or_never().is_none());

    // Set up the success response before editing, since edit causes an immediate
    // retry (the room's send queue is not blocked, since the one event it couldn't
    // send failed in an unrecoverable way).
    drop(mounted_send);
    Mock::given(method("PUT"))
        .and(path_regex(r"^/_matrix/client/r0/rooms/.*/send/.*"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "event_id": "$1" })))
        .expect(1)
        .mount(&server)
        .await;

    // Let's edit the local echo.
    let did_edit = timeline
        .edit_by_id(
            &item.identifier(),
            EditedContent::RoomMessage(RoomMessageEventContent::text_plain("hello, world").into()),
        )
        .await
        .unwrap();

    // We could edit the local echo, since it was in the failed state.
    assert!(did_edit);

    // Observe local echo being replaced.
    assert_let!(Some(VectorDiff::Set { index: 1, value: item }) = timeline_stream.next().await);

    assert_eq!(item.unique_id(), internal_id);

    let item = item.as_event().unwrap();
    assert!(item.is_local_echo());

    // The send state has been reset.
    assert_matches!(item.send_state(), Some(EventSendState::NotSentYet));

    let edit_message = item.content().as_message().unwrap();
    assert_eq!(edit_message.body(), "hello, world");

    // Observe the event being sent, and replacing the local echo.
    assert_let!(Some(VectorDiff::Set { index: 1, value: item }) = timeline_stream.next().await);

    let item = item.as_event().unwrap();
    assert!(item.is_local_echo());

    let edit_message = item.content().as_message().unwrap();
    assert_eq!(edit_message.body(), "hello, world");

    // No new updates.
    assert!(timeline_stream.next().now_or_never().is_none());
}

#[async_test]
async fn test_send_edit_by_non_existing_local_id() {
    let room_id = room_id!("!a98sd12bjh:example.org");
    let (client, server) = logged_in_client_with_server().await;
    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

    let mut sync_builder = SyncResponseBuilder::new();
    sync_builder.add_joined_room(JoinedRoomBuilder::new(room_id));

    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    mock_encryption_state(&server, false).await;

    let room = client.get_room(room_id).unwrap();
    let timeline = room.timeline().await.unwrap();

    mock_encryption_state(&server, false).await;

    let error = timeline
        .edit_by_id(
            &TimelineEventItemId::TransactionId("something".into()),
            EditedContent::RoomMessage(RoomMessageEventContentWithoutRelation::text_plain(
                "Hello, Room!",
            )),
        )
        .await
        .err()
        .unwrap();
    assert_matches!(error, Error::EventNotInTimeline(_));
}
