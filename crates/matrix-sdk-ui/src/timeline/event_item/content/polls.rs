// Copyright 2024 The Matrix.org Foundation C.I.C.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! This module handles rendering of MSC3381 polls in the timeline.

use std::collections::HashMap;

use ruma::{
    MilliSecondsSinceUnixEpoch, OwnedUserId, UserId,
    events::poll::{
        PollResponseData, compile_unstable_poll_results,
        start::PollKind,
        unstable_start::{
            NewUnstablePollStartEventContent, NewUnstablePollStartEventContentWithoutRelation,
            UnstablePollStartContentBlock,
        },
    },
};

/// Holds the state of a poll.
///
/// This struct should be created for each poll start event handled and then
/// updated whenever handling any poll response or poll end event that relates
/// to the same poll start event.
#[derive(Clone, Debug)]
pub struct PollState {
    /// Text representation of the message, for clients that don't support
    /// polls.
    pub(in crate::timeline) fallback_text: Option<String>,
    /// The poll content of the message.
    pub(in crate::timeline) poll_start: UnstablePollStartContentBlock,
    pub(in crate::timeline) response_data: Vec<ResponseData>,
    pub(in crate::timeline) end_event_timestamp: Option<MilliSecondsSinceUnixEpoch>,
    pub(in crate::timeline) has_been_edited: bool,
}

#[derive(Clone, Debug)]
pub(in crate::timeline) struct ResponseData {
    pub sender: OwnedUserId,
    pub timestamp: MilliSecondsSinceUnixEpoch,
    pub answers: Vec<String>,
}

impl PollState {
    pub(crate) fn new(
        poll_start: UnstablePollStartContentBlock,
        fallback_text: Option<String>,
    ) -> Self {
        Self {
            fallback_text,
            poll_start,
            response_data: vec![],
            end_event_timestamp: None,
            has_been_edited: false,
        }
    }

    /// Applies an edit to a poll, returns `None` if the poll was already marked
    /// as finished.
    pub(crate) fn edit(
        &self,
        replacement: NewUnstablePollStartEventContentWithoutRelation,
    ) -> Option<Self> {
        if self.end_event_timestamp.is_none() {
            let mut clone = self.clone();
            clone.poll_start = replacement.poll_start;
            clone.fallback_text = replacement.text;
            clone.has_been_edited = true;
            Some(clone)
        } else {
            None
        }
    }

    /// Reverts an edit applied with [`Self::edit`], by restoring the fields it
    /// replaced from `unedited`.
    ///
    /// Responses are left alone: they're part of the state an edit carries
    /// over, so the ones received since the edit was applied are kept.
    pub(crate) fn unedit(&self, unedited: &Self) -> Self {
        let mut clone = self.clone();
        clone.poll_start = unedited.poll_start.clone();
        clone.fallback_text = unedited.fallback_text.clone();
        clone.has_been_edited = unedited.has_been_edited;
        clone
    }

    /// Add a response to a poll.
    pub(crate) fn add_response(
        &mut self,
        sender: OwnedUserId,
        timestamp: MilliSecondsSinceUnixEpoch,
        answers: Vec<String>,
    ) {
        self.response_data.push(ResponseData { sender, timestamp, answers });
    }

    /// Remove a response from the poll, as identified by its sender and
    /// timestamp values.
    pub(crate) fn remove_response(
        &mut self,
        sender: &UserId,
        timestamp: MilliSecondsSinceUnixEpoch,
    ) {
        if let Some(idx) = self
            .response_data
            .iter()
            .position(|resp| resp.sender == sender && resp.timestamp == timestamp)
        {
            self.response_data.remove(idx);
        }
    }

    /// Marks the poll as ended.
    ///
    /// Returns false if the poll was already ended, true otherwise.
    pub(crate) fn end(&mut self, timestamp: MilliSecondsSinceUnixEpoch) -> bool {
        if self.end_event_timestamp.is_none() {
            self.end_event_timestamp = Some(timestamp);
            true
        } else {
            false
        }
    }

    /// Get the text representation of the poll, for clients that don't support
    /// them.
    pub fn fallback_text(&self) -> Option<String> {
        self.fallback_text.clone()
    }

    /// Compute the current results of the poll.
    pub fn results(&self) -> PollResult {
        let results = compile_unstable_poll_results(
            &self.poll_start,
            self.response_data.iter().map(|response_data| PollResponseData {
                sender: &response_data.sender,
                origin_server_ts: response_data.timestamp,
                selections: &response_data.answers,
            }),
            self.end_event_timestamp,
        );

        PollResult {
            question: self.poll_start.question.text.clone(),
            kind: self.poll_start.kind.clone(),
            max_selections: self.poll_start.max_selections.into(),
            answers: self
                .poll_start
                .answers
                .iter()
                .map(|i| PollResultAnswer { id: i.id.clone(), text: i.text.clone() })
                .collect(),
            votes: results
                .into_iter()
                .map(|(id, users)| {
                    (id.to_owned(), users.into_iter().map(ToOwned::to_owned).collect())
                })
                .collect(),
            end_time: self.end_event_timestamp,
            has_been_edited: self.has_been_edited,
        }
    }

    /// Returns true whether this poll has been edited.
    pub fn is_edit(&self) -> bool {
        self.has_been_edited
    }
}

impl From<PollState> for NewUnstablePollStartEventContent {
    fn from(value: PollState) -> Self {
        let content = UnstablePollStartContentBlock::new(
            value.poll_start.question.text.clone(),
            value.poll_start.answers.clone(),
        );
        if let Some(text) = value.fallback_text() {
            NewUnstablePollStartEventContent::plain_text(text, content)
        } else {
            NewUnstablePollStartEventContent::new(content)
        }
    }
}

/// The results of a poll.
#[derive(Debug)]
pub struct PollResult {
    /// The question of the poll.
    pub question: String,
    /// The kind of the poll.
    pub kind: PollKind,
    /// The maximum number of responses a user is able to select.
    pub max_selections: u64,
    /// The possible answers to the poll.
    pub answers: Vec<PollResultAnswer>,
    /// The current votes of the poll.
    ///
    /// This is a map from answer ID to the list of user IDs that voted for that
    /// answer.
    pub votes: HashMap<String, Vec<OwnedUserId>>,
    /// The time when the poll ended.
    ///
    /// If this is set the poll has ended, otherwise the poll is still ongoing.
    pub end_time: Option<MilliSecondsSinceUnixEpoch>,
    /// Whether the poll was edited.
    pub has_been_edited: bool,
}

/// A possible answer of a poll.
#[derive(Debug)]
pub struct PollResultAnswer {
    /// The ID of the answer.
    ///
    /// This is the string used to identify an answer when voting.
    pub id: String,
    /// The text representation of the answer.
    ///
    /// This is the string to present to the user in the UI.
    pub text: String,
}
