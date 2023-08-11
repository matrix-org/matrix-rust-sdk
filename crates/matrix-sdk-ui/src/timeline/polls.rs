/// Polls module.
///
/// This module handles rendering of MSC3381 polls in the timeline.
use imbl::HashMap;
use ruma::{
    events::{
        poll::{
            compile_unstable_poll_results,
            unstable_end::UnstablePollEndEventContent,
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
    EventId, MilliSecondsSinceUnixEpoch, OwnedEventId, OwnedUserId, ServerName,
};

/// Holds the state of a poll.
///
/// This struct should be created for each poll start event received and then
/// updated whenever any other poll response or poll end event is received
/// that relates to the same poll start event.
#[derive(Clone, Debug)]
pub struct PollState {
    pub(super) start_event: StartEvent,
    pub(super) response_events: Vec<ResponseEvent>,
    pub(super) end_event: Option<EndEvent>,
}

#[derive(Clone, Debug)]
pub(super) struct StartEvent {
    pub(super) sender: OwnedUserId,
    pub(super) timestamp: MilliSecondsSinceUnixEpoch,
    pub(super) content: UnstablePollStartEventContent,
}

#[derive(Clone, Debug)]
pub(super) struct ResponseEvent {
    pub(super) sender: OwnedUserId,
    pub(super) timestamp: MilliSecondsSinceUnixEpoch,
    pub(super) content: UnstablePollResponseEventContent,
}

#[derive(Clone, Debug)]
pub(super) struct EndEvent {
    pub(super) sender: OwnedUserId,
    pub(super) timestamp: MilliSecondsSinceUnixEpoch,
    pub(super) content: UnstablePollEndEventContent,
}

impl PollState {
    pub(super) fn new(
        sender: OwnedUserId,
        timestamp: MilliSecondsSinceUnixEpoch,
        content: UnstablePollStartEventContent,
    ) -> Self {
        Self {
            start_event: StartEvent { sender, timestamp, content },
            response_events: Vec::new(),
            end_event: None,
        }
    }

    pub(super) fn edit(
        &self,
        timestamp: MilliSecondsSinceUnixEpoch,
        replacement: &UnstablePollStartEventContentWithoutRelation,
    ) -> Result<Self, ()> {
        if self.response_events.is_empty() && self.end_event.is_none() {
            let mut clone = self.clone();
            clone.start_event.timestamp = timestamp;
            clone.start_event.content.poll_start = replacement.poll_start.clone();
            clone.start_event.content.text = replacement.text.clone();
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

    pub(super) fn end(
        &self,
        sender: &OwnedUserId,
        timestamp: &MilliSecondsSinceUnixEpoch,
        content: &UnstablePollEndEventContent,
    ) -> Self {
        let mut clone = self.clone();
        clone.end_event = Some(EndEvent {
            sender: sender.clone(),
            timestamp: timestamp.clone(),
            content: content.clone(),
        });
        clone
    }

    pub fn fallback_text(&self) -> Option<String> {
        self.start_event.content.text.clone()
    }

    pub fn results(&self) -> FfiPoll {
        let responses = self
            .response_events
            .iter()
            .map(|e| OriginalSyncUnstablePollResponseEvent {
                // Concocting an OriginalSyncUnstablePollResponseEvent as I'm not able to get it from the handle_event() fn.
                content: e.content.clone(),
                event_id: EventId::new(<&ServerName>::try_from("example.com").unwrap()), // TODO Remove example.com
                sender: e.sender.clone(),
                origin_server_ts: e.timestamp,
                unsigned: MessageLikeUnsigned::new(),
            })
            .collect::<Vec<OriginalSyncUnstablePollResponseEvent>>();

        let results = compile_unstable_poll_results(
            &self.start_event.content.poll_start,
            responses.iter().map(|i| i).collect::<Vec<&OriginalSyncUnstablePollResponseEvent>>(),
            None,
        );

        FfiPoll {
            question: self.start_event.content.poll_start.question.text.clone(),
            kind: match self.start_event.content.poll_start.kind {
                ruma::events::poll::start::PollKind::Undisclosed => FfiPollKind::Undisclosed,
                ruma::events::poll::start::PollKind::Disclosed => FfiPollKind::Disclosed,
                _ => FfiPollKind::Undisclosed, // Safe default. Should we keep it?
            },
            max_selections: self.start_event.content.poll_start.max_selections.into(),
            answers: self
                .start_event
                .content
                .poll_start
                .answers
                .iter()
                .map(|i| FfiPollAnswer { id: i.id.clone(), text: i.text.clone() })
                .collect(),
            votes: results
                .iter()
                .map(|i| (i.0.to_string(), i.1.into_iter().map(|i| i.to_string()).collect()))
                .collect(),
            end_time: if let Some(e) = self.end_event.clone() {
                Some(e.timestamp.0.into())
            } else {
                None
            },
        }
    }
}

impl Into<UnstablePollStartEventContent> for PollState {
    fn into(self) -> UnstablePollStartEventContent {
        let content = UnstablePollStartContentBlock::new(
            self.start_event.content.poll_start.question.text.clone(),
            self.start_event.content.poll_start.answers.clone(),
        );
        if let Some(text) = self.fallback_text() {
            UnstablePollStartEventContent::plain_text(text, content)
        } else {
            UnstablePollStartEventContent::new(content)
        }
    }
}

#[derive(Debug, Default)]
pub(super) struct PollCache {
    pub(super) pending_poll_responses: HashMap<OwnedEventId, Vec<ResponseEvent>>,
    pub(super) pending_poll_ends: HashMap<OwnedEventId, EndEvent>,
}

impl PollCache {
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
        sender: &OwnedUserId,
        timestamp: &MilliSecondsSinceUnixEpoch,
        content: &UnstablePollEndEventContent,
    ) {
        self.pending_poll_ends.insert(
            start_id.clone(),
            EndEvent {
                sender: sender.clone(),
                timestamp: timestamp.clone(),
                content: content.clone(),
            },
        );
    }

    /// Adds all response and end event in the cache to the given poll_state
    pub(super) fn apply(&mut self, start_event_id: &OwnedEventId, poll_state: &mut PollState) {
        if let Some(pending_responses) = self.pending_poll_responses.get_mut(start_event_id) {
            poll_state.response_events.append(pending_responses)
        }
        if let Some(pending_end) = self.pending_poll_ends.get_mut(start_event_id) {
            poll_state.end_event = Some(pending_end.clone())
        }
    }
}

#[derive(PartialEq, Eq, Hash, Clone, Debug)]
pub struct PollAnswerId(pub String);

pub struct FfiPoll {
    pub question: String,
    pub kind: FfiPollKind,
    pub max_selections: u64,
    pub answers: Vec<FfiPollAnswer>,
    pub votes: HashMap<String, Vec<String>>,
    pub end_time: Option<u64>,
}

pub enum FfiPollKind {
    Disclosed,
    Undisclosed,
}
pub struct FfiPollAnswer {
    pub id: String,
    pub text: String,
}
