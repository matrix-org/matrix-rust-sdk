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

use std::{io, sync::Arc};

use assert_matches::assert_matches;
use eyeball_im::VectorDiff;
use matrix_sdk::Error;
use matrix_sdk_test::{async_test, sync_timeline_event, ALICE, BOB};
use ruma::{
    event_id,
    events::{room::message::RoomMessageEventContent, AnyMessageLikeEventContent},
};
use stream_assert::assert_next_matches;

use super::TestTimeline;
use crate::timeline::event_item::EventSendState;

#[async_test]
async fn test_remote_echo_full_trip() {
    let timeline = TestTimeline::new();
    let mut stream = timeline.subscribe().await;

    // Given a local event…
    let txn_id = timeline
        .handle_local_event(AnyMessageLikeEventContent::RoomMessage(
            RoomMessageEventContent::text_plain("echo"),
        ))
        .await;

    // Scenario 1: The local event has not been sent yet to the server.
    let id = {
        let item = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);
        let event_item = item.as_event().unwrap();
        assert!(event_item.is_local_echo());
        assert_matches!(event_item.send_state(), Some(EventSendState::NotSentYet));
        assert!(!event_item.can_be_replied_to());
        item.unique_id()
    };

    {
        // The day divider comes in late.
        let day_divider = assert_next_matches!(stream, VectorDiff::PushFront { value } => value);
        assert!(day_divider.is_day_divider());
    }

    // Scenario 2: The local event has not been sent to the server successfully, it
    // has failed. In this case, there is no event ID.
    {
        let some_io_error = Error::Io(io::Error::new(io::ErrorKind::Other, "this is a test"));
        timeline
            .inner
            .update_event_send_state(
                &txn_id,
                EventSendState::SendingFailed { error: Arc::new(some_io_error) },
            )
            .await;

        let item = assert_next_matches!(stream, VectorDiff::Set { value, index: 1 } => value);
        let event_item = item.as_event().unwrap();
        assert!(event_item.is_local_echo());
        assert_matches!(event_item.send_state(), Some(EventSendState::SendingFailed { .. }));
        assert_eq!(item.unique_id(), id);
    }

    // Scenario 3: The local event has been sent successfully to the server and an
    // event ID has been received as part of the server's response.
    let event_id = event_id!("$W6mZSLWMmfuQQ9jhZWeTxFIM");
    let timestamp = {
        timeline
            .inner
            .update_event_send_state(
                &txn_id,
                EventSendState::Sent { event_id: event_id.to_owned() },
            )
            .await;

        let item = assert_next_matches!(stream, VectorDiff::Set { value, index: 1 } => value);
        let event_item = item.as_event().unwrap();
        assert!(event_item.is_local_echo());
        assert_matches!(event_item.send_state(), Some(EventSendState::Sent { .. }));
        assert_eq!(item.unique_id(), id);

        event_item.timestamp()
    };

    // Now, a sync has been run against the server, and an event with the same ID
    // comes in.
    timeline
        .handle_live_custom_event(sync_timeline_event!({
            "content": {
                "body": "echo",
                "msgtype": "m.text",
            },
            "sender": &*ALICE,
            "event_id": event_id,
            "origin_server_ts": timestamp,
            "type": "m.room.message",
        }))
        .await;

    // The local echo is replaced with the remote echo.
    let item = assert_next_matches!(stream, VectorDiff::Set { index: 1, value } => value);
    assert!(!item.as_event().unwrap().is_local_echo());
    assert_eq!(item.unique_id(), id);
}

#[async_test]
async fn test_remote_echo_new_position() {
    let timeline = TestTimeline::new();
    let mut stream = timeline.subscribe().await;

    // Given a local event…
    let txn_id = timeline
        .handle_local_event(AnyMessageLikeEventContent::RoomMessage(
            RoomMessageEventContent::text_plain("echo"),
        ))
        .await;

    let item = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);
    let txn_id_from_event = item.as_event().unwrap();
    assert_eq!(txn_id, txn_id_from_event.transaction_id().unwrap());

    let day_divider = assert_next_matches!(stream, VectorDiff::PushFront { value } => value);
    assert!(day_divider.is_day_divider());

    // … and another event that comes back before the remote echo
    timeline.handle_live_message_event(&BOB, RoomMessageEventContent::text_plain("test")).await;

    // … and is inserted before the local echo item
    let bob_message = assert_next_matches!(stream, VectorDiff::PushFront { value } => value);
    assert!(bob_message.is_remote_event());

    let day_divider = assert_next_matches!(stream, VectorDiff::PushFront { value } => value);
    assert!(day_divider.is_day_divider());

    // When the remote echo comes in…
    timeline
        .handle_live_custom_event(sync_timeline_event!({
            "content": {
                "body": "echo",
                "msgtype": "m.text",
            },
            "sender": &*ALICE,
            "event_id": "$eeG0HA0FAZ37wP8kXlNkxx3I",
            "origin_server_ts": 6,
            "type": "m.room.message",
            "unsigned": {
                "transaction_id": txn_id,
            },
        }))
        .await;

    // … the remote echo replaces the previous event.
    let item = assert_next_matches!(stream, VectorDiff::Set { index: 3, value } => value);
    assert!(!item.as_event().unwrap().is_local_echo());

    // … the day divider is removed (because both bob's and alice's message are from
    // the same day according to server timestamps).
    assert_next_matches!(stream, VectorDiff::Remove { index: 2 });
}

#[async_test]
async fn test_day_divider_duplication() {
    let timeline = TestTimeline::new();

    // Given two remote events from one day, and a local event from another day…
    timeline.handle_live_message_event(&BOB, RoomMessageEventContent::text_plain("A")).await;
    timeline.handle_live_message_event(&BOB, RoomMessageEventContent::text_plain("B")).await;
    timeline
        .handle_local_event(AnyMessageLikeEventContent::RoomMessage(
            RoomMessageEventContent::text_plain("C"),
        ))
        .await;

    let items = timeline.inner.items().await;
    assert_eq!(items.len(), 5);
    assert!(items[0].is_day_divider());
    assert!(items[1].is_remote_event());
    assert!(items[2].is_remote_event());
    assert!(items[3].is_day_divider());
    assert!(items[4].is_local_echo());

    // … when the second remote event is re-received (day still the same)
    let event_id = items[2].as_event().unwrap().event_id().unwrap();
    timeline
        .handle_live_custom_event(sync_timeline_event!({
            "content": {
                "body": "B",
                "msgtype": "m.text",
            },
            "sender": &*BOB,
            "event_id": event_id,
            "origin_server_ts": 1,
            "type": "m.room.message",
        }))
        .await;

    // … it should not impact the day dividers.
    let items = timeline.inner.items().await;
    assert_eq!(items.len(), 5);
    assert!(items[0].is_day_divider());
    assert!(items[1].is_remote_event());
    assert!(items[2].is_remote_event());
    assert!(items[3].is_day_divider());
    assert!(items[4].is_local_echo());
}
