use assert_matches2::assert_let;
use fakes::poll_a2;
use matrix_sdk_test::{ALICE, BOB, async_test};
use ruma::{
    EventId, OwnedEventId, UserId, event_id,
    events::poll::unstable_start::{
        NewUnstablePollStartEventContent, ReplacementUnstablePollStartEventContent,
        UnstablePollStartContentBlock, UnstablePollStartEventContent,
    },
    server_name,
};

use crate::timeline::{EventTimelineItem, event_item::PollState, tests::TestTimeline};

#[async_test]
async fn test_poll_is_displayed() {
    let timeline = TestTimeline::new();

    timeline.send_poll_start(&ALICE, fakes::poll_a()).await;
    let poll_state = timeline.poll_state().await;

    assert_poll_start_eq(&poll_state.poll_start, &fakes::poll_a());
    assert!(poll_state.response_data.is_empty());
}

#[async_test]
async fn test_edited_poll_is_displayed() {
    let timeline = TestTimeline::new();

    timeline.send_poll_start(&ALICE, fakes::poll_a()).await;
    let event = timeline.poll_event().await;
    let event_id = event.event_id().unwrap();
    timeline.send_poll_edit(&ALICE, event_id, fakes::poll_b()).await;
    let poll_state = event.content().as_poll().unwrap();
    let edited_poll_state = timeline.poll_state().await;

    assert_poll_start_eq(&poll_state.poll_start, &fakes::poll_a());
    assert_poll_start_eq(&edited_poll_state.poll_start, &fakes::poll_b());
    assert!(!poll_state.has_been_edited);
    assert!(edited_poll_state.has_been_edited);
}

#[async_test]
async fn test_voting_adds_the_vote_to_the_results() {
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
async fn test_ending_a_poll_sets_end_time_to_results() {
    let timeline = TestTimeline::new();
    timeline.send_poll_start(&ALICE, fakes::poll_a()).await;
    let poll_id = timeline.poll_event().await.event_id().unwrap().to_owned();

    // Poll finishes
    timeline.send_poll_end(&ALICE, "ENDED", &poll_id).await;
    let results = timeline.poll_state().await.results();

    assert!(results.end_time.is_some());
}

#[async_test]
async fn test_only_the_last_vote_from_a_user_is_counted() {
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
async fn test_votes_after_end_are_discarded() {
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
async fn test_multiple_end_events_are_discarded() {
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
async fn test_a_somewhat_complex_voting_session_yields_the_expected_outcome() {
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
async fn test_events_received_before_start_are_not_lost() {
    let timeline = TestTimeline::new();
    let poll_id: OwnedEventId = EventId::new(server_name!("dummy.server"));

    // Alice votes
    timeline.send_poll_response(&ALICE, vec!["0"], &poll_id).await;

    // Now Bob also votes
    timeline.send_poll_response(&BOB, vec!["0"], &poll_id).await;

    // Alice changes her mind and votes again
    timeline.send_poll_response(&ALICE, vec!["1"], &poll_id).await;

    // Poll finishes
    timeline.send_poll_end(&ALICE, "ENDED", &poll_id).await;

    // Now the start event arrives
    let start_ev = poll_a2(&timeline.factory).sender(&ALICE).event_id(&poll_id);
    timeline.handle_live_event(start_ev).await;

    // Now Bob votes again but his vote won't count
    timeline.send_poll_response(&BOB, vec!["1"], &poll_id).await;

    let results = timeline.poll_state().await.results();
    assert_eq!(results.votes["0"], vec![BOB.to_string()]);
    assert_eq!(results.votes["1"], vec![ALICE.to_string()]);
}

#[async_test]
async fn test_adding_response_doesnt_clear_latest_json_edit() {
    let timeline = TestTimeline::new();

    // Alice sends the poll.
    timeline.send_poll_start(&ALICE, fakes::poll_a()).await;

    // Alice edits the poll.
    let poll_item = timeline.poll_event().await;
    let poll_id = poll_item.event_id().unwrap();
    timeline.send_poll_edit(&ALICE, poll_id, fakes::poll_b()).await;
    // Sanity check: the poll has a latest edit JSON.
    assert!(timeline.event_items().await[0].latest_edit_json().is_some());

    // Now Bob also votes
    timeline.send_poll_response(&BOB, vec!["0"], poll_id).await;

    // The poll still has a latest edit JSON.
    assert!(timeline.event_items().await[0].latest_edit_json().is_some());
}

#[async_test]
async fn test_ending_poll_doesnt_clear_latest_json_edit() {
    let timeline = TestTimeline::new();

    // Alice sends the poll.
    timeline.send_poll_start(&ALICE, fakes::poll_a()).await;

    let poll_item = timeline.poll_event().await;
    let poll_id = poll_item.event_id().unwrap();

    // Alice edits the poll.
    timeline.send_poll_edit(&ALICE, poll_id, fakes::poll_b()).await;
    // Sanity check: the poll has a latest edit JSON.
    assert!(timeline.event_items().await[0].latest_edit_json().is_some());

    // Now the poll is ended.
    timeline.send_poll_end(&ALICE, "ended", poll_id).await;

    // The poll still has a latest edit JSON.
    assert!(timeline.event_items().await[0].latest_edit_json().is_some());
}

#[async_test]
async fn test_poll_contains_relations() {
    let timeline = TestTimeline::new();

    let thread_root_id = event_id!("$thread_root");
    let in_reply_to_id = event_id!("$in_reply_to");

    let poll_start = poll_a2(&timeline.factory)
        .in_thread(thread_root_id, in_reply_to_id)
        .sender(&ALICE)
        .event_id(event_id!("$poll"));

    timeline.handle_live_event(poll_start).await;

    let poll = timeline.event_items().await[0].clone();

    assert_eq!(poll.content().thread_root(), Some(thread_root_id.to_owned()));

    assert_let!(Some(details) = poll.content().in_reply_to());
    assert_eq!(details.event_id, in_reply_to_id);
}

impl TestTimeline {
    async fn event_items(&self) -> Vec<EventTimelineItem> {
        self.controller.items().await.iter().filter_map(|item| item.as_event().cloned()).collect()
    }

    async fn poll_event(&self) -> EventTimelineItem {
        self.event_items().await[0].clone()
    }

    async fn poll_state(&self) -> PollState {
        self.event_items().await[0].content().as_poll().unwrap().clone()
    }

    async fn send_poll_start(&self, sender: &UserId, content: UnstablePollStartContentBlock) {
        let event_content =
            UnstablePollStartEventContent::from(NewUnstablePollStartEventContent::new(content));
        self.handle_live_event(self.factory.event(event_content).sender(sender)).await;
    }

    async fn send_poll_response(&self, sender: &UserId, answers: Vec<&str>, poll_id: &EventId) {
        self.handle_live_event(self.factory.poll_response(answers, poll_id).sender(sender)).await
    }

    async fn send_poll_end(&self, sender: &UserId, text: &str, poll_id: &EventId) {
        self.handle_live_event(self.factory.poll_end(text, poll_id).sender(sender)).await
    }

    async fn send_poll_edit(
        &self,
        sender: &UserId,
        original_id: &EventId,
        content: UnstablePollStartContentBlock,
    ) {
        let event_content = UnstablePollStartEventContent::from(
            ReplacementUnstablePollStartEventContent::new(content, original_id.to_owned()),
        );
        self.handle_live_event(self.factory.event(event_content).sender(sender)).await
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
    use matrix_sdk_test::event_factory::{EventBuilder, EventFactory};
    use ruma::events::poll::{
        start::PollKind,
        unstable_start::{
            UnstablePollAnswer, UnstablePollAnswers, UnstablePollStartContentBlock,
            UnstablePollStartEventContent,
        },
    };

    pub fn poll_a2(f: &EventFactory) -> EventBuilder<UnstablePollStartEventContent> {
        f.poll_start("Up or down?", "Up or down?", vec!["Up", "Down"])
    }

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
