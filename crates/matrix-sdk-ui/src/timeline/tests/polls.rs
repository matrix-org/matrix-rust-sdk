use matrix_sdk_test::async_test;
use ruma::{
    events::{
        poll::{
            unstable_end::UnstablePollEndEventContent,
            unstable_response::UnstablePollResponseEventContent,
            unstable_start::{
                NewUnstablePollStartEventContent, ReplacementUnstablePollStartEventContent,
                UnstablePollStartContentBlock,
            },
        },
        AnyMessageLikeEventContent,
    },
    server_name, EventId, OwnedEventId, UserId,
};

use crate::timeline::{
    polls::PollState,
    tests::{TestTimeline, ALICE, BOB},
    EventTimelineItem, TimelineItemContent,
};

#[async_test]
async fn poll_is_displayed() {
    let timeline = TestTimeline::new();

    timeline.send_poll_start(&ALICE, fakes::poll_a()).await;
    let poll_state = timeline.poll_state().await;

    assert_poll_start_eq(&poll_state.start_event_content.poll_start, &fakes::poll_a());
    assert!(poll_state.response_data.is_empty());
}

#[async_test]
async fn edited_poll_is_displayed() {
    let timeline = TestTimeline::new();

    timeline.send_poll_start(&ALICE, fakes::poll_a()).await;
    let event = timeline.poll_event().await;
    let event_id = event.event_id().unwrap();
    timeline.send_poll_edit(&ALICE, event_id, fakes::poll_b()).await;
    let edited_poll_state = timeline.poll_state().await;

    assert_poll_start_eq(&event.poll_state().start_event_content.poll_start, &fakes::poll_a());
    assert_poll_start_eq(&edited_poll_state.start_event_content.poll_start, &fakes::poll_b());
}

#[async_test]
async fn voting_adds_the_vote_to_the_results() {
    let timeline = TestTimeline::new();
    timeline.send_poll_start(&ALICE, fakes::poll_a()).await;
    let poll_id = timeline.poll_event().await.event_id().unwrap().to_owned();

    // Alice votes
    timeline.send_poll_response(&ALICE, vec!["id_up"], &poll_id).await;
    let results = timeline.poll_state().await.results();

    assert_eq!(results.votes["id_up"], vec![ALICE.to_string()]);
    assert!(results.end_time.is_none());
}

#[async_test]
async fn ending_a_poll_sets_end_time_to_results() {
    let timeline = TestTimeline::new();
    timeline.send_poll_start(&ALICE, fakes::poll_a()).await;
    let poll_id = timeline.poll_event().await.event_id().unwrap().to_owned();

    // Poll finishes
    timeline.send_poll_end(&ALICE, "ENDED", &poll_id).await;
    let results = timeline.poll_state().await.results();

    assert!(results.end_time.is_some());
}

#[async_test]
async fn only_the_last_vote_from_a_user_is_counted() {
    let timeline = TestTimeline::new();
    timeline.send_poll_start(&ALICE, fakes::poll_a()).await;
    let poll_id = timeline.poll_event().await.event_id().unwrap().to_owned();

    // Alice votes
    timeline.send_poll_response(&ALICE, vec!["id_up"], &poll_id).await;
    let results = timeline.poll_state().await.results();
    assert_eq!(results.votes["id_up"], vec![ALICE.to_string()]);

    // Alice changes her mind and votes again
    timeline.send_poll_response(&ALICE, vec!["id_down"], &poll_id).await;
    let results = timeline.poll_state().await.results();
    assert_eq!(results.votes["id_down"], vec![ALICE.to_string()]);
}

#[async_test]
async fn votes_after_end_are_discarded() {
    let timeline = TestTimeline::new();
    timeline.send_poll_start(&ALICE, fakes::poll_a()).await;
    let poll_id = timeline.poll_event().await.event_id().unwrap().to_owned();

    // Poll finishes
    timeline.send_poll_end(&ALICE, "ENDED", &poll_id).await;

    // Alice votes but it's too late, her vote won't count
    timeline.send_poll_response(&ALICE, vec!["id_up"], &poll_id).await;
    let results = timeline.poll_state().await.results();
    for (_, votes) in results.votes.iter() {
        assert!(votes.is_empty());
    }
}

#[async_test]
async fn multiple_end_events_are_discarded() {
    let timeline = TestTimeline::new();
    timeline.send_poll_start(&ALICE, fakes::poll_a()).await;
    let poll_id = timeline.poll_event().await.event_id().unwrap().to_owned();

    // Poll finishes
    timeline.send_poll_end(&ALICE, "ENDED", &poll_id).await;
    let results = timeline.poll_state().await.results();
    assert!(results.end_time.is_some());

    let first_end_time = results.end_time.unwrap();

    // Another poll end event arrives, but it should be discarded
    // and therefore the poll's end time should not change
    timeline.send_poll_end(&ALICE, "ENDED", &poll_id).await;
    let results = timeline.poll_state().await.results();
    assert_eq!(results.end_time.unwrap(), first_end_time);
}

#[async_test]
async fn a_somewhat_complex_voting_session_yields_the_expected_outcome() {
    let timeline = TestTimeline::new();
    timeline.send_poll_start(&ALICE, fakes::poll_a()).await;
    let poll_id = timeline.poll_event().await.event_id().unwrap().to_owned();

    // Alice votes
    timeline.send_poll_response(&ALICE, vec!["id_up"], &poll_id).await;
    let results = timeline.poll_state().await.results();
    assert_eq!(results.votes["id_up"], vec![ALICE.to_string()]);

    // Now Bob also votes
    timeline.send_poll_response(&BOB, vec!["id_up"], &poll_id).await;
    let results = timeline.poll_state().await.results();
    assert_eq!(results.votes["id_up"], vec![ALICE.to_string(), BOB.to_string()]);

    // Alice changes her mind and votes again
    timeline.send_poll_response(&ALICE, vec!["id_down"], &poll_id).await;
    let results = timeline.poll_state().await.results();
    assert_eq!(results.votes["id_up"], vec![BOB.to_string()]);
    assert_eq!(results.votes["id_down"], vec![ALICE.to_string()]);

    // Poll finishes
    timeline.send_poll_end(&ALICE, "ENDED", &poll_id).await;

    // Now Bob also changes his mind but it's too late, his vote won't count
    timeline.send_poll_response(&BOB, vec!["id_down"], &poll_id).await;
    let results = timeline.poll_state().await.results();
    assert_eq!(results.votes["id_up"], vec![BOB.to_string()]);
    assert_eq!(results.votes["id_down"], vec![ALICE.to_string()]);
}

#[async_test]
async fn events_received_before_start_are_not_lost() {
    let timeline = TestTimeline::new();
    let poll_id: OwnedEventId = EventId::new(server_name!("dummy.server"));

    // Alice votes
    timeline.send_poll_response(&ALICE, vec!["id_up"], &poll_id).await;

    // Now Bob also votes
    timeline.send_poll_response(&BOB, vec!["id_up"], &poll_id).await;

    // Alice changes her mind and votes again
    timeline.send_poll_response(&ALICE, vec!["id_down"], &poll_id).await;

    // Poll finishes
    timeline.send_poll_end(&ALICE, "ENDED", &poll_id).await;

    // Now the start event arrives
    timeline.send_poll_start_with_id(&ALICE, &poll_id, fakes::poll_a()).await;

    // Now Bob votes again but his vote won't count
    timeline.send_poll_response(&BOB, vec!["id_down"], &poll_id).await;

    let results = timeline.poll_state().await.results();
    assert_eq!(results.votes["id_up"], vec![BOB.to_string()]);
    assert_eq!(results.votes["id_down"], vec![ALICE.to_string()]);
}

impl TestTimeline {
    async fn event_items(&self) -> Vec<EventTimelineItem> {
        self.inner.items().await.iter().filter_map(|item| item.as_event().cloned()).collect()
    }

    async fn poll_event(&self) -> EventTimelineItem {
        self.event_items().await.first().unwrap().clone()
    }

    async fn poll_state(&self) -> PollState {
        self.event_items().await.first().unwrap().clone().poll_state()
    }

    async fn send_poll_start(&self, sender: &UserId, content: UnstablePollStartContentBlock) {
        let event_content = AnyMessageLikeEventContent::UnstablePollStart(
            NewUnstablePollStartEventContent::new(content).into(),
        );
        self.handle_live_message_event(sender, event_content).await;
    }

    async fn send_poll_start_with_id(
        &self,
        sender: &UserId,
        event_id: &EventId,
        content: UnstablePollStartContentBlock,
    ) {
        let event_content = AnyMessageLikeEventContent::UnstablePollStart(
            NewUnstablePollStartEventContent::new(content).into(),
        );
        let event_id = event_id.to_owned();
        let event = self.event_builder.make_message_event_with_id(sender, event_content, event_id);
        self.handle_live_event(event).await;
    }

    async fn send_poll_response(&self, sender: &UserId, answers: Vec<&str>, poll_id: &EventId) {
        let event_content = AnyMessageLikeEventContent::UnstablePollResponse(
            UnstablePollResponseEventContent::new(
                answers.into_iter().map(|i| i.to_owned()).collect(),
                poll_id.to_owned(),
            ),
        );
        self.handle_live_message_event(sender, event_content).await
    }

    async fn send_poll_end(&self, sender: &UserId, text: &str, poll_id: &EventId) {
        let event_content = AnyMessageLikeEventContent::UnstablePollEnd(
            UnstablePollEndEventContent::new(text, poll_id.to_owned()),
        );
        self.handle_live_message_event(sender, event_content).await
    }

    async fn send_poll_edit(
        &self,
        sender: &UserId,
        original_id: &EventId,
        content: UnstablePollStartContentBlock,
    ) {
        let content =
            ReplacementUnstablePollStartEventContent::new(content, original_id.to_owned());
        let event_content = AnyMessageLikeEventContent::UnstablePollStart(content.into());
        self.handle_live_message_event(sender, event_content).await
    }
}

impl EventTimelineItem {
    fn poll_state(self) -> PollState {
        match self.content() {
            TimelineItemContent::Poll(poll_state) => poll_state.clone(),
            _ => panic!("Not a poll state"),
        }
    }
}

fn assert_poll_start_eq(a: &UnstablePollStartContentBlock, b: &UnstablePollStartContentBlock) {
    assert_eq!(a.question.text, b.question.text);
    assert_eq!(a.kind, b.kind);
    assert_eq!(a.max_selections, b.max_selections);
    assert_eq!(a.answers.len(), a.answers.len());
    a.answers.iter().zip(b.answers.iter()).all(|(a, b)| {
        assert_eq!(a.id, b.id);
        assert_eq!(a.text, b.text);
        true
    });
}

mod fakes {
    use ruma::events::poll::{
        start::PollKind,
        unstable_start::{UnstablePollAnswer, UnstablePollAnswers, UnstablePollStartContentBlock},
    };

    pub fn poll_a() -> UnstablePollStartContentBlock {
        let mut content = UnstablePollStartContentBlock::new(
            "Up or down?",
            UnstablePollAnswers::try_from(vec![
                UnstablePollAnswer::new("id_up", "Up"),
                UnstablePollAnswer::new("id_down", "Down"),
            ])
            .unwrap(),
        );
        content.kind = PollKind::Disclosed;
        content
    }

    pub fn poll_b() -> UnstablePollStartContentBlock {
        let mut content = UnstablePollStartContentBlock::new(
            "Left or right?",
            UnstablePollAnswers::try_from(vec![
                UnstablePollAnswer::new("id_left", "Left"),
                UnstablePollAnswer::new("id_right", "Right"),
            ])
            .unwrap(),
        );
        content.kind = PollKind::Disclosed;
        content
    }
}
