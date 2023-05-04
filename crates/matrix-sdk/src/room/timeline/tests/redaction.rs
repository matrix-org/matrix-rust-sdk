use assert_matches::assert_matches;
use eyeball_im::VectorDiff;
use futures_util::StreamExt;
use matrix_sdk_test::async_test;
use ruma::events::{
    reaction::ReactionEventContent, relation::Annotation, room::message::RoomMessageEventContent,
};

use super::{TestTimeline, ALICE, BOB};

#[async_test]
async fn reaction_redaction() {
    let timeline = TestTimeline::new();
    let mut stream = timeline.subscribe().await;

    timeline.handle_live_message_event(&ALICE, RoomMessageEventContent::text_plain("hi!")).await;
    let _day_divider =
        assert_matches!(stream.next().await, Some(VectorDiff::PushBack { value }) => value);
    let item = assert_matches!(stream.next().await, Some(VectorDiff::PushBack { value }) => value);
    let event = item.as_event().unwrap();
    assert_eq!(event.reactions().len(), 0);

    let msg_event_id = event.event_id().unwrap();

    let rel = Annotation::new(msg_event_id.to_owned(), "+1".to_owned());
    timeline.handle_live_message_event(&BOB, ReactionEventContent::new(rel)).await;
    let item =
        assert_matches!(stream.next().await, Some(VectorDiff::Set { index: 1, value }) => value);
    let event = item.as_event().unwrap();
    assert_eq!(event.reactions().len(), 1);

    // TODO: After adding raw timeline items, check for one here

    let reaction_event_id = event.event_id().unwrap();

    timeline.handle_live_redaction(&BOB, reaction_event_id).await;
    let item =
        assert_matches!(stream.next().await, Some(VectorDiff::Set { index: 1, value }) => value);
    let event = item.as_event().unwrap();
    assert_eq!(event.reactions().len(), 0);
}
