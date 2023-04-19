use std::{io, sync::Arc};

use assert_matches::assert_matches;
use eyeball_im::VectorDiff;
use futures_util::StreamExt;
use matrix_sdk_test::async_test;
use ruma::{
    event_id,
    events::{room::message::RoomMessageEventContent, AnyMessageLikeEventContent},
};
use serde_json::json;

use super::{TestTimeline, ALICE, BOB};
use crate::{room::timeline::event_item::EventSendState, Error};

#[async_test]
async fn remote_echo_full_trip() {
    let timeline = TestTimeline::new();
    let mut stream = timeline.subscribe().await;

    // Given a local event…
    let txn_id = timeline
        .handle_local_event(AnyMessageLikeEventContent::RoomMessage(
            RoomMessageEventContent::text_plain("echo"),
        ))
        .await;

    let _day_divider =
        assert_matches!(stream.next().await, Some(VectorDiff::PushBack { value }) => value);

    // Scenario 1: The local event has not been sent yet to the server.
    {
        let item =
            assert_matches!(stream.next().await, Some(VectorDiff::PushBack { value }) => value);
        let event = item.as_event().unwrap();
        assert_matches!(event.send_state(), Some(EventSendState::NotSentYet));
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

        let item = assert_matches!(
            stream.next().await,
            Some(VectorDiff::Set { value, index: 1 }) => value
        );
        let event = item.as_event().unwrap();
        assert_matches!(event.send_state(), Some(EventSendState::SendingFailed { .. }));
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

        let item = assert_matches!(
            stream.next().await,
            Some(VectorDiff::Set { value, index: 1 }) => value
        );
        let event_item = item.as_event().unwrap();
        assert_matches!(event_item.send_state(), Some(EventSendState::Sent { .. }));

        event_item.timestamp()
    };

    // Now, a sync has been run against the server, and an event with the same ID
    // comes in.
    timeline
        .handle_live_custom_event(json!({
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

    // The local echo is replaced with the remote echo
    let item =
        assert_matches!(stream.next().await, Some(VectorDiff::Set { index: 1, value }) => value);
    assert!(item.as_event().unwrap().as_remote().is_some());
}

#[async_test]
async fn remote_echo_new_position() {
    let timeline = TestTimeline::new();
    let mut stream = timeline.subscribe().await;

    // Given a local event…
    let txn_id = timeline
        .handle_local_event(AnyMessageLikeEventContent::RoomMessage(
            RoomMessageEventContent::text_plain("echo"),
        ))
        .await;

    let _day_divider =
        assert_matches!(stream.next().await, Some(VectorDiff::PushBack { value }) => value);

    let item = assert_matches!(stream.next().await, Some(VectorDiff::PushBack { value }) => value);
    let txn_id_from_event = item.as_event().unwrap();
    assert_eq!(txn_id, txn_id_from_event.transaction_id().unwrap());

    // … and another event that comes back before the remote echo
    timeline.handle_live_message_event(&BOB, RoomMessageEventContent::text_plain("test")).await;
    let _day_divider =
        assert_matches!(stream.next().await, Some(VectorDiff::PushBack { value }) => value);
    let _bob_message =
        assert_matches!(stream.next().await, Some(VectorDiff::PushBack { value }) => value);

    // When the remote echo comes in…
    timeline
        .handle_live_custom_event(json!({
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

    // … the local echo should be removed
    assert_matches!(stream.next().await, Some(VectorDiff::Remove { index: 1 }));
    // … along with its day divider
    assert_matches!(stream.next().await, Some(VectorDiff::Remove { index: 0 }));

    // … and the remote echo added (no new day divider because both bob's and
    // alice's message are from the same day according to server timestamps)
    let item = assert_matches!(stream.next().await, Some(VectorDiff::PushBack { value }) => value);
    assert!(item.as_event().unwrap().as_remote().is_some());
}
