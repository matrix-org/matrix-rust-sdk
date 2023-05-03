use assert_matches::assert_matches;
use eyeball_im::VectorDiff;
use futures_util::StreamExt;
use matrix_sdk_test::async_test;
use ruma::{
    assign,
    events::{
        relation::Replacement,
        room::message::{
            self, MessageType, RedactedRoomMessageEventContent, RoomMessageEventContent,
        },
    },
};

use super::{TestTimeline, ALICE};

#[async_test]
async fn edit_redacted() {
    let timeline = TestTimeline::new();
    let mut stream = timeline.subscribe().await;

    timeline
        .handle_live_redacted_message_event(*ALICE, RedactedRoomMessageEventContent::new())
        .await;
    let _day_divider =
        assert_matches!(stream.next().await, Some(VectorDiff::PushBack { value }) => value);
    let item = assert_matches!(stream.next().await, Some(VectorDiff::PushBack { value }) => value);

    let redacted_event_id = item.as_event().unwrap().event_id().unwrap();

    let edit = assign!(RoomMessageEventContent::text_plain(" * test"), {
        relates_to: Some(message::Relation::Replacement(Replacement::new(
            redacted_event_id.to_owned(),
            MessageType::text_plain("test"),
        ))),
    });
    timeline.handle_live_message_event(&ALICE, edit).await;

    assert_eq!(timeline.inner.items().await.len(), 2);
}
