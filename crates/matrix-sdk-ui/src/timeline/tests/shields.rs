use eyeball_im::VectorDiff;
use matrix_sdk_base::deserialized_responses::ShieldState;
use matrix_sdk_test::{async_test, ALICE};
use ruma::events::room::message::RoomMessageEventContent;
use stream_assert::assert_next_matches;

use crate::timeline::tests::TestTimeline;

#[async_test]
async fn test_no_shield_in_unencrypted_room() {
    let timeline = TestTimeline::new();
    let mut stream = timeline.subscribe().await;

    timeline
        .handle_live_message_event(
            &ALICE,
            RoomMessageEventContent::text_plain("Unencrypted message."),
        )
        .await;

    let item = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);
    let shield = item.as_event().unwrap().get_shield(false);
    assert!(shield.is_none());
}

#[async_test]
async fn test_sent_in_clear_shield() {
    let timeline = TestTimeline::with_is_room_encrypted(true);
    let mut stream = timeline.subscribe().await;

    timeline
        .handle_live_message_event(
            &ALICE,
            RoomMessageEventContent::text_plain("Unencrypted message."),
        )
        .await;

    let item = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);
    let shield = item.as_event().unwrap().get_shield(false);
    assert_eq!(shield, Some(ShieldState::Grey { message: "Sent in clear." }));
}
