use std::sync::Arc;

use crate::timeline::polls::PollState;
use crate::timeline::tests::{assert_no_more_updates, TestTimeline, ALICE, BOB};
use crate::timeline::{EventTimelineItem, TimelineItem};
use eyeball_im::VectorDiff;
use futures_core::Stream;
use futures_util::{FutureExt, StreamExt};
use matrix_sdk_test::async_test;
use ruma::events::poll::start::PollKind;
use ruma::events::poll::unstable_response::UnstablePollResponseEventContent;
use ruma::events::poll::unstable_start::{
    UnstablePollAnswer, UnstablePollAnswers, UnstablePollStartContentBlock,
    UnstablePollStartEventContent,
};
use ruma::events::AnyMessageLikeEventContent;
use ruma::{OwnedEventId, UInt, UserId};

#[async_test]
async fn start_poll_event_should_have_initial_poll_state() {
    let (_timeline, mut stream) = create_timeline_with_start_poll_event().await;
    let start_event = get_poll_start_event(&mut stream);
    let poll_state = PollState::try_from(start_event.content()).unwrap();

    assert_poll_description(&poll_state);
    assert_eq!(poll_state.response_events.is_empty(), true);

    assert_no_more_updates(&mut stream).await;
}

#[async_test]
async fn poll_response_event_should_update_poll_state() {
    let (timeline, mut stream) = create_timeline_with_start_poll_event().await;
    let start_event = get_poll_start_event(&mut stream);
    timeline
        .send_poll_response_event(
            &ALICE,
            vec!["id_up".to_string()],
            start_event.event_id().unwrap().to_owned(),
        )
        .await;
    let updated_start_event = get_updated_poll_event(&mut stream);
    let poll_state = PollState::try_from(updated_start_event.content()).unwrap();

    assert_poll_description(&poll_state);
    assert_eq!(poll_state.response_events.len(), 1);
    assert_eq!(poll_state.response_events[0].sender, ALICE.to_owned());
    assert_eq!(poll_state.response_events[0].timestamp.0, UInt::new(1).unwrap());
    assert_eq!(
        poll_state.response_events[0].content.poll_response.answers.first().unwrap(),
        "id_up"
    );

    let poll = poll_state.results();
    assert_eq!(poll.votes["id_up"], vec!["@alice:server.name"]);

    assert_no_more_updates(&mut stream).await;
}

#[async_test]
async fn two_poll_response_events_should_update_poll_state() {
    let (timeline, mut stream) = create_timeline_with_start_poll_event().await;
    let start_event = get_poll_start_event(&mut stream);
    timeline
        .send_poll_response_event(
            &ALICE,
            vec!["id_up".to_string()],
            start_event.event_id().unwrap().to_owned(),
        )
        .await;
    let start_event_after_1st_vote = get_updated_poll_event(&mut stream);
    let _poll_state_after_first_vote =
        PollState::try_from(start_event_after_1st_vote.content()).unwrap();
    timeline
        .send_poll_response_event(
            &BOB,
            vec!["id_up".to_string()],
            start_event.event_id().unwrap().to_owned(),
        )
        .await;
    let start_event_after_2nd_vote = get_updated_poll_event(&mut stream);
    let _poll_state_after_2nd_vote =
        PollState::try_from(start_event_after_2nd_vote.content()).unwrap();

    assert_poll_description(&_poll_state_after_2nd_vote);
    assert_eq!(_poll_state_after_2nd_vote.response_events.len(), 2);
    assert_eq!(_poll_state_after_2nd_vote.response_events[0].sender, ALICE.to_owned());
    assert_eq!(_poll_state_after_2nd_vote.response_events[0].timestamp.0, UInt::new(1).unwrap());
    assert_eq!(
        _poll_state_after_2nd_vote.response_events[0]
            .content
            .poll_response
            .answers
            .first()
            .unwrap(),
        "id_up"
    );
    assert_eq!(_poll_state_after_2nd_vote.response_events[1].sender, BOB.to_owned());
    assert_eq!(_poll_state_after_2nd_vote.response_events[1].timestamp.0, UInt::new(2).unwrap());
    assert_eq!(
        _poll_state_after_2nd_vote.response_events[1]
            .content
            .poll_response
            .answers
            .first()
            .unwrap(),
        "id_up"
    );

    let poll = _poll_state_after_2nd_vote.results();
    assert_eq!(poll.votes["id_up"], vec!["@alice:server.name", "@bob:other.server"]);

    assert_no_more_updates(&mut stream).await;
}

impl TestTimeline {
    async fn send_poll_start_event(self: &Self) {
        let mut content = UnstablePollStartContentBlock::new(
            "Up or down?",
            UnstablePollAnswers::try_from(vec![
                UnstablePollAnswer::new("id_up".to_string(), "Up".to_string()),
                UnstablePollAnswer::new("id_down".to_string(), "Down".to_string()),
            ])
            .unwrap(),
        );
        content.kind = PollKind::Disclosed;

        self.handle_live_message_event(
            &ALICE,
            AnyMessageLikeEventContent::UnstablePollStart(UnstablePollStartEventContent::new(
                content,
            )),
        )
        .await
    }

    async fn send_poll_response_event(
        self: &Self,
        sender: &UserId,
        answers: Vec<String>,
        poll_id: OwnedEventId,
    ) {
        self.handle_live_message_event(
            sender,
            AnyMessageLikeEventContent::UnstablePollResponse(
                UnstablePollResponseEventContent::new(answers, poll_id),
            ),
        )
        .await
    }
}

async fn create_timeline_with_start_poll_event(
) -> (TestTimeline, impl Stream<Item = VectorDiff<Arc<TimelineItem>>>) {
    let timeline = TestTimeline::new();
    let stream = timeline.subscribe().await;
    timeline.send_poll_start_event().await;
    (timeline, stream)
}

fn get_poll_start_event(
    stream: &mut (impl Stream<Item = VectorDiff<Arc<TimelineItem>>> + Unpin),
) -> EventTimelineItem {
    let _day_divider = stream.next().now_or_never().unwrap();
    match stream.next().now_or_never().unwrap().unwrap() {
        VectorDiff::PushBack { value } => value.as_event().unwrap().clone(),
        _ => panic!("Not a pushback"),
    }
}

fn get_updated_poll_event(
    stream: &mut (impl Stream<Item = VectorDiff<Arc<TimelineItem>>> + Unpin),
) -> EventTimelineItem {
    match stream.next().now_or_never().unwrap().unwrap() {
        VectorDiff::Set { value, .. } => value.as_event().unwrap().clone(),
        _ => panic!("Not a set"),
    }
}

fn assert_poll_description(poll_state: &PollState) {
    let start_content = poll_state.start_event.content.poll_start.clone();
    assert_eq!(start_content.question.text, "Up or down?");
    assert_eq!(start_content.kind, PollKind::Disclosed);
    assert_eq!(start_content.max_selections, UInt::new(1).unwrap());
    assert_eq!(start_content.answers.len(), 2);
    assert_eq!(start_content.answers[0].id, "id_up");
    assert_eq!(start_content.answers[0].text, "Up");
    assert_eq!(start_content.answers[1].id, "id_down");
    assert_eq!(start_content.answers[1].text, "Down");
}
