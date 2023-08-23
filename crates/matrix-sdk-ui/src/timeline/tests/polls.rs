use crate::timeline::polls::PollState;
use crate::timeline::tests::{TestTimeline, ALICE, BOB};
use crate::timeline::{EventTimelineItem, TimelineItemContent};
use matrix_sdk_test::async_test;
use ruma::events::poll::unstable_end::UnstablePollEndEventContent;
use ruma::events::poll::unstable_response::UnstablePollResponseEventContent;
use ruma::events::poll::unstable_start::{
    UnstablePollStartContentBlock, UnstablePollStartEventContent,
};
use ruma::events::relation::Replacement;
use ruma::events::room::message::Relation;
use ruma::events::AnyMessageLikeEventContent;
use ruma::serde::Raw;
use ruma::{server_name, EventId, OwnedEventId, UserId};

#[async_test]
async fn poll_is_displayed() {
    let timeline = TestTimeline::new();

    timeline.send_poll_start(&ALICE, fakes::poll_a()).await;
    let poll_state = timeline.poll_state().await;

    assert_poll_start_eq(&poll_state.start_event_content.poll_start, &fakes::poll_a());
    assert_eq!(poll_state.response_data.is_empty(), true);
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
async fn voting_does_update_a_poll() {
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
async fn events_received_before_start_are_cached() {
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
    timeline.send_poll_response(&BOB, vec!["id_up"], &poll_id).await;

    let results = timeline.poll_state().await.results();
    assert_eq!(results.votes["id_up"], vec![BOB.to_string()]);
    assert_eq!(results.votes["id_down"], vec![ALICE.to_string()]);
}

#[async_test]
async fn multiple_end_events_are_discarded() {
    let timeline = TestTimeline::new();
    timeline.send_poll_start(&ALICE, fakes::poll_a()).await;
    let poll_id = timeline.poll_event().await.event_id().unwrap().to_owned();

    // Poll finishes
    timeline.send_poll_end(&ALICE, "ENDED", &poll_id).await;
    let results = timeline.poll_state().await.results();
    assert_eq!(results.end_time.is_some(), true);

    let first_end_time = results.end_time.unwrap();

    // Another poll end event arrives, but it should be discarded
    // and therefore the poll's end time should not change
    timeline.send_poll_end(&ALICE, "ENDED", &poll_id).await;
    let results = timeline.poll_state().await.results();
    assert_eq!(results.end_time.unwrap(), first_end_time);
}

impl TestTimeline {
    async fn events(&self) -> Vec<EventTimelineItem> {
        self.inner
            .items()
            .await
            .iter()
            .filter_map(|item| match item.as_event() {
                Some(event) => Some(event.clone()),
                None => None,
            })
            .collect()
    }

    async fn poll_event(&self) -> EventTimelineItem {
        self.events().await.first().unwrap().clone()
    }

    async fn poll_state(&self) -> PollState {
        self.events().await.first().unwrap().clone().poll_state()
    }

    async fn send_poll_start(&self, sender: &UserId, content: UnstablePollStartContentBlock) {
        let event_content = AnyMessageLikeEventContent::UnstablePollStart(
            UnstablePollStartEventContent::new(content),
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
            UnstablePollStartEventContent::new(content),
        );
        let event = self.make_message_event_with_id(sender, event_content, event_id.to_owned());
        let raw = Raw::new(&event).unwrap().cast();
        self.handle_live_event(raw).await;
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
        let mut content = UnstablePollStartEventContent::new(content);
        content.relates_to = Some(Relation::Replacement(Replacement::new(
            original_id.to_owned(),
            content.clone().into(),
        )));
        let event_content = AnyMessageLikeEventContent::UnstablePollStart(content);
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
    assert_eq!(a.answers[0].id, a.answers[0].id);
    assert_eq!(a.answers[0].text, a.answers[0].text);
    assert_eq!(a.answers[1].id, a.answers[1].id);
    assert_eq!(a.answers[1].text, a.answers[1].text);
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
