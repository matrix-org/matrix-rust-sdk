use assert_matches::assert_matches;
use eyeball_im::VectorDiff;
use matrix_sdk_base::deserialized_responses::{ShieldState, ShieldStateCode};
use matrix_sdk_test::{async_test, sync_timeline_event, ALICE};
use ruma::{
    event_id,
    events::{room::message::RoomMessageEventContent, AnyMessageLikeEventContent},
};
use stream_assert::assert_next_matches;

use crate::timeline::{tests::TestTimeline, EventSendState};

#[async_test]
async fn test_no_shield_in_unencrypted_room() {
    let timeline = TestTimeline::new();
    let mut stream = timeline.subscribe().await;
    let f = &timeline.factory;

    timeline.handle_live_event(f.text_msg("Unencrypted message.").sender(&ALICE)).await;

    let item = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);
    let shield = item.as_event().unwrap().get_shield(false);
    assert!(shield.is_none());
}

#[async_test]
async fn test_sent_in_clear_shield() {
    let timeline = TestTimeline::with_is_room_encrypted(true);
    let mut stream = timeline.subscribe().await;

    let f = &timeline.factory;
    timeline.handle_live_event(f.text_msg("Unencrypted message.").sender(&ALICE)).await;

    let item = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);
    let shield = item.as_event().unwrap().get_shield(false);
    assert_eq!(
        shield,
        Some(ShieldState::Red { code: ShieldStateCode::SentInClear, message: "Not encrypted." })
    );
}

#[async_test]
/// Note: This is a regression test for a bug where the SentInClear shield was
/// incorrectly shown on a local echo whilst sending and was then removed once
/// the local echo came back. There doesn't appear to be an obvious way to mock
/// trust in the timeline, so this test uses a message sent in the clear, making
/// sure the shield only appears once the remote echo is received.
async fn test_local_sent_in_clear_shield() {
    // Given an encrypted timeline.
    let timeline = TestTimeline::with_is_room_encrypted(true);
    let mut stream = timeline.subscribe().await;

    // When sending an unencrypted event.
    let event_id = event_id!("$W6mZSLWMmfuQQ9jhZWeTxFIM");
    let txn_id = timeline
        .handle_local_event(AnyMessageLikeEventContent::RoomMessage(
            RoomMessageEventContent::text_plain("Local message"),
        ))
        .await;
    let item = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);
    let event_item = item.as_event().unwrap();

    // Then it's local echo should not have a shield (as encryption info isn't
    // available).
    assert!(event_item.is_local_echo());
    let shield = event_item.get_shield(false);
    assert_eq!(shield, None);

    {
        // The day divider comes in late.
        let day_divider = assert_next_matches!(stream, VectorDiff::PushFront { value } => value);
        assert!(day_divider.is_day_divider());
    }

    // When the event is sent (but without a remote echo).
    timeline
        .controller
        .update_event_send_state(&txn_id, EventSendState::Sent { event_id: event_id.to_owned() })
        .await;
    let item = assert_next_matches!(stream, VectorDiff::Set { value, index: 1 } => value);
    let event_item = item.as_event().unwrap();
    let timestamp = event_item.timestamp();
    assert_matches!(event_item.send_state(), Some(EventSendState::Sent { .. }));

    // Then the local echo still should not have a shield.
    assert!(event_item.is_local_echo());
    let shield = event_item.get_shield(false);
    assert_eq!(shield, None);

    // When the remote echo comes in.
    timeline
        .handle_live_event(sync_timeline_event!({
            "content": {
                "body": "Local message",
                "msgtype": "m.text",
            },
            "sender": &*ALICE,
            "event_id": event_id,
            "origin_server_ts": timestamp,
            "type": "m.room.message",
        }))
        .await;
    let item = assert_next_matches!(stream, VectorDiff::Set { index: 1, value } => value);
    let event_item = item.as_event().unwrap();

    // Then the remote echo should now be showing the shield.
    assert!(!event_item.is_local_echo());
    let shield = event_item.get_shield(false);
    assert_eq!(
        shield,
        Some(ShieldState::Red { code: ShieldStateCode::SentInClear, message: "Not encrypted." })
    );
}
