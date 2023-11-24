//! This module handles rendering of MSC3381 polls in the timeline.

use std::collections::HashMap;

use ruma::{
    events::poll::{
        compile_unstable_poll_results,
        start::PollKind,
        unstable_response::UnstablePollResponseEventContent,
        unstable_start::{
            NewUnstablePollStartEventContent, NewUnstablePollStartEventContentWithoutRelation,
            UnstablePollStartContentBlock,
        },
        PollResponseData,
    },
    EventId, MilliSecondsSinceUnixEpoch, OwnedEventId, OwnedUserId, UserId,
};

/// Holds the state of a poll.
///
/// This struct should be created for each poll start event handled and then
/// updated whenever handling any poll response or poll end event that relates
/// to the same poll start event.
#[derive(Clone, Debug)]
pub struct PollState {
    pub(super) start_event_content: NewUnstablePollStartEventContent,
    pub(super) response_data: Vec<ResponseData>,
    pub(super) end_event_timestamp: Option<MilliSecondsSinceUnixEpoch>,
    pub(super) has_been_edited: bool,
}

#[derive(Clone, Debug)]
pub(super) struct ResponseData {
    pub(super) sender: OwnedUserId,
    pub(super) timestamp: MilliSecondsSinceUnixEpoch,
    pub(super) answers: Vec<String>,
}

impl PollState {
    pub(super) fn new(content: NewUnstablePollStartEventContent) -> Self {
        Self {
            start_event_content: content,
            response_data: vec![],
            end_event_timestamp: None,
            has_been_edited: false,
        }
    }

    pub(super) fn edit(
        &self,
        replacement: &NewUnstablePollStartEventContentWithoutRelation,
    ) -> Result<Self, ()> {
        if self.end_event_timestamp.is_none() {
            let mut clone = self.clone();
            clone.start_event_content.poll_start = replacement.poll_start.clone();
            clone.start_event_content.text = replacement.text.clone();
            clone.has_been_edited = true;
            Ok(clone)
        } else {
            Err(())
        }
    }

    pub(super) fn add_response(
        &self,
        sender: &UserId,
        timestamp: MilliSecondsSinceUnixEpoch,
        content: &UnstablePollResponseEventContent,
    ) -> Self {
        let mut clone = self.clone();
        clone.response_data.push(ResponseData {
            sender: sender.to_owned(),
            timestamp,
            answers: content.poll_response.answers.clone(),
        });
        clone
    }

    /// Marks the poll as ended.
    ///
    /// If the poll has already ended, returns `Err(())`.
    pub(super) fn end(&self, timestamp: MilliSecondsSinceUnixEpoch) -> Result<Self, ()> {
        if self.end_event_timestamp.is_none() {
            let mut clone = self.clone();
            clone.end_event_timestamp = Some(timestamp);
            Ok(clone)
        } else {
            Err(())
        }
    }

    pub fn fallback_text(&self) -> Option<String> {
        self.start_event_content.text.clone()
    }

    pub fn results(&self) -> PollResult {
        let results = compile_unstable_poll_results(
            &self.start_event_content.poll_start,
            self.response_data.iter().map(|response_data| PollResponseData {
                sender: &response_data.sender,
                origin_server_ts: response_data.timestamp,
                selections: &response_data.answers,
            }),
            self.end_event_timestamp,
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
                .map(|i| ((*i.0).to_owned(), i.1.iter().map(|i| i.to_string()).collect()))
                .collect(),
            end_time: self.end_event_timestamp.map(|millis| millis.0.into()),
            has_been_edited: self.has_been_edited,
        }
    }
}

impl From<PollState> for NewUnstablePollStartEventContent {
    fn from(value: PollState) -> Self {
        let content = UnstablePollStartContentBlock::new(
            value.start_event_content.poll_start.question.text.clone(),
            value.start_event_content.poll_start.answers.clone(),
        );
        if let Some(text) = value.fallback_text() {
            NewUnstablePollStartEventContent::plain_text(text, content)
        } else {
            NewUnstablePollStartEventContent::new(content)
        }
    }
}

/// Acts as a cache for poll response and poll end events handled before their
/// start event has been handled.
#[derive(Debug, Default)]
pub(super) struct PollPendingEvents {
    pub(super) pending_poll_responses: HashMap<OwnedEventId, Vec<ResponseData>>,
    pub(super) pending_poll_ends: HashMap<OwnedEventId, MilliSecondsSinceUnixEpoch>,
}

impl PollPendingEvents {
    pub(super) fn add_response(
        &mut self,
        start_id: &EventId,
        sender: &UserId,
        timestamp: MilliSecondsSinceUnixEpoch,
        content: &UnstablePollResponseEventContent,
    ) {
        self.pending_poll_responses.entry(start_id.to_owned()).or_default().push(ResponseData {
            sender: sender.to_owned(),
            timestamp,
            answers: content.poll_response.answers.clone(),
        });
    }

    pub(super) fn add_end(&mut self, start_id: &EventId, timestamp: MilliSecondsSinceUnixEpoch) {
        self.pending_poll_ends.insert(start_id.to_owned(), timestamp);
    }

    /// Dumps all response and end events present in the cache that belong to
    /// the given start_event_id into the given poll_state.
    pub(super) fn apply(&mut self, start_event_id: &EventId, poll_state: &mut PollState) {
        if let Some(pending_responses) = self.pending_poll_responses.get_mut(start_event_id) {
            poll_state.response_data.append(pending_responses);
        }
        if let Some(pending_end) = self.pending_poll_ends.get(start_event_id) {
            poll_state.end_event_timestamp = Some(*pending_end)
        }
    }
}

#[derive(Debug)]
pub struct PollResult {
    pub question: String,
    pub kind: PollKind,
    pub max_selections: u64,
    pub answers: Vec<PollResultAnswer>,
    pub votes: HashMap<String, Vec<String>>,
    pub end_time: Option<u64>,
    pub has_been_edited: bool,
}

#[derive(Debug)]
pub struct PollResultAnswer {
    pub id: String,
    pub text: String,
}
