/// Polls module.
///
/// This module handles rendering of MSC3381 polls in the timeline.
use imbl::HashMap;
use ruma::{
    events::{
        poll::{
            compile_unstable_poll_results,
            start::PollKind,
            unstable_response::{
                OriginalSyncUnstablePollResponseEvent, UnstablePollResponseEventContent,
            },
            unstable_start::{
                UnstablePollStartContentBlock, UnstablePollStartEventContent,
                UnstablePollStartEventContentWithoutRelation,
            },
        },
        MessageLikeUnsigned,
    },
    server_name, EventId, MilliSecondsSinceUnixEpoch, OwnedEventId, OwnedUserId,
};

/// Holds the state of a poll.
///
/// This struct should be created for each poll start event handled and then
/// updated whenever handling any poll response or poll end event that relates
/// to the same poll start event.
#[derive(Clone, Debug)]
pub struct PollState {
    pub(super) start_event_content: UnstablePollStartEventContent,
    pub(super) response_events: Vec<ResponseEvent>,
    pub(super) end_event_timestamp: Option<MilliSecondsSinceUnixEpoch>,
}

#[derive(Clone, Debug)]
pub(super) struct ResponseEvent {
    pub(super) sender: OwnedUserId,
    pub(super) timestamp: MilliSecondsSinceUnixEpoch,
    pub(super) content: UnstablePollResponseEventContent,
}

impl PollState {
    pub(super) fn new(content: UnstablePollStartEventContent) -> Self {
        Self { start_event_content: content, response_events: vec![], end_event_timestamp: None }
    }

    pub(super) fn edit(
        &self,
        replacement: &UnstablePollStartEventContentWithoutRelation,
    ) -> Result<Self, ()> {
        if self.response_events.is_empty() && self.end_event_timestamp.is_none() {
            let mut clone = self.clone();
            clone.start_event_content.poll_start = replacement.poll_start.clone();
            clone.start_event_content.text = replacement.text.clone();
            Ok(clone)
        } else {
            Err(())
        }
    }

    pub(super) fn add_response(
        &self,
        sender: &OwnedUserId,
        timestamp: &MilliSecondsSinceUnixEpoch,
        content: &UnstablePollResponseEventContent,
    ) -> Self {
        let mut clone = self.clone();
        clone.response_events.push(ResponseEvent {
            sender: sender.clone(),
            timestamp: timestamp.clone(),
            content: content.clone(),
        });
        clone
    }

    pub(super) fn end(&self, timestamp: &MilliSecondsSinceUnixEpoch) -> Self {
        let mut clone = self.clone();
        clone.end_event_timestamp = Some(timestamp.clone());
        clone
    }

    pub fn fallback_text(&self) -> Option<String> {
        self.start_event_content.text.clone()
    }

    pub fn results(&self) -> PollResult {
        let responses = self
            .response_events
            .iter()
            .map(|e| OriginalSyncUnstablePollResponseEvent {
                // TODO Concocting an OriginalSyncUnstablePollResponseEvent as I'm not able to get it from the handle_event() fn.
                content: e.content.clone(),
                event_id: EventId::new(server_name!("no-server.local")), // TODO better fake server name?
                sender: e.sender.clone(),
                origin_server_ts: e.timestamp,
                unsigned: MessageLikeUnsigned::new(),
            })
            .collect::<Vec<OriginalSyncUnstablePollResponseEvent>>();

        let results = compile_unstable_poll_results(
            &self.start_event_content.poll_start,
            responses.iter().map(|i| i).collect::<Vec<&OriginalSyncUnstablePollResponseEvent>>(), // TODO: Why .iter() twice?
            None,
        );

        PollResult {
            question: self.start_event_content.poll_start.question.text.clone(),
            kind: self.start_event_content.poll_start.kind.clone(),
            max_selections: self.start_event_content.poll_start.max_selections.into(),
            answers: self
                .start_event_content
                .poll_start
                .answers
                .iter()
                .map(|i| PollResultAnswer { id: i.id.clone(), text: i.text.clone() })
                .collect(),
            votes: results
                .iter()
                .map(|i| (i.0.to_string(), i.1.into_iter().map(|i| i.to_string()).collect()))
                .collect(),
            end_time: self.end_event_timestamp.map(|millis| millis.0.into()),
        }
    }
}

impl From<PollState> for UnstablePollStartEventContent {
    fn from(value: PollState) -> Self {
        let content = UnstablePollStartContentBlock::new(
            value.start_event_content.poll_start.question.text.clone(),
            value.start_event_content.poll_start.answers.clone(),
        );
        if let Some(text) = value.fallback_text() {
            UnstablePollStartEventContent::plain_text(text, content)
        } else {
            UnstablePollStartEventContent::new(content)
        }
    }
}

/// Acts as a cache for poll response and poll end events handled before their
/// start event has been handled.
#[derive(Debug, Default)]
pub(super) struct PollPendingEvents {
    pub(super) pending_poll_responses: HashMap<OwnedEventId, Vec<ResponseEvent>>,
    pub(super) pending_poll_ends: HashMap<OwnedEventId, MilliSecondsSinceUnixEpoch>,
}

impl PollPendingEvents {
    pub(super) fn add_response(
        &mut self,
        start_id: &OwnedEventId,
        sender: &OwnedUserId,
        timestamp: &MilliSecondsSinceUnixEpoch,
        content: &UnstablePollResponseEventContent,
    ) {
        self.pending_poll_responses.entry(start_id.clone()).or_insert(vec![]).push(ResponseEvent {
            sender: sender.clone(),
            timestamp: timestamp.clone(),
            content: content.clone(),
        });
    }

    pub(super) fn add_end(
        &mut self,
        start_id: &OwnedEventId,
        timestamp: &MilliSecondsSinceUnixEpoch,
    ) {
        self.pending_poll_ends.insert(start_id.clone(), timestamp.clone());
    }

    /// Dumps all response and end events presnet in the cache that belong to
    /// the given start_event_id into the given poll_state.
    pub(super) fn apply(&mut self, start_event_id: &OwnedEventId, poll_state: &mut PollState) {
        if let Some(pending_responses) = self.pending_poll_responses.get_mut(start_event_id) {
            poll_state.response_events.append(pending_responses)
        }
        if let Some(pending_end) = self.pending_poll_ends.get_mut(start_event_id) {
            poll_state.end_event_timestamp = Some(pending_end.clone())
        }
    }
}

pub struct PollResult {
    pub question: String,
    pub kind: PollKind,
    pub max_selections: u64,
    pub answers: Vec<PollResultAnswer>,
    pub votes: HashMap<String, Vec<String>>,
    pub end_time: Option<u64>,
}

pub struct PollResultAnswer {
    pub id: String,
    pub text: String,
}
