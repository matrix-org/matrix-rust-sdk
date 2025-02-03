// Copyright 2025 The Matrix.org Foundation C.I.C.
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

use std::collections::HashMap;

use ruma::{MilliSecondsSinceUnixEpoch, OwnedEventId, OwnedUserId};

use crate::timeline::{
    PollState, ReactionInfo, ReactionStatus, TimelineEventItemId, TimelineItemContent,
};

#[derive(Clone, Debug)]
pub(crate) enum AggregationKind {
    PollResponse {
        sender: OwnedUserId,
        timestamp: MilliSecondsSinceUnixEpoch,
        answers: Vec<String>,
    },

    PollEnd {
        end_date: MilliSecondsSinceUnixEpoch,
    },

    Reaction {
        key: String,
        sender: OwnedUserId,
        timestamp: MilliSecondsSinceUnixEpoch,
    },
}

#[derive(Clone, Debug)]
pub(crate) struct Aggregation {
    kind: AggregationKind,
    pub own_id: TimelineEventItemId,
}

fn poll_state_from_item(
    content: &mut TimelineItemContent,
) -> Result<&mut PollState, AggregationError> {
    match content {
        TimelineItemContent::Poll(poll_state) => Ok(poll_state),
        _ => Err(AggregationError::InvalidType {
            expected: "a poll".to_owned(),
            actual: content.debug_string().to_owned(),
        }),
    }
}

impl Aggregation {
    pub fn new(own_id: TimelineEventItemId, kind: AggregationKind) -> Self {
        Self { kind, own_id }
    }

    pub fn apply(&self, content: &mut TimelineItemContent) -> Result<(), AggregationError> {
        match &self.kind {
            AggregationKind::PollResponse { sender, timestamp, answers } => {
                poll_state_from_item(content)?.add_response(
                    sender.clone(),
                    *timestamp,
                    answers.clone(),
                );
            }

            AggregationKind::PollEnd { end_date } => {
                let poll_state = poll_state_from_item(content)?;
                if !poll_state.end(*end_date) {
                    return Err(AggregationError::PollAlreadyEnded);
                }
            }

            AggregationKind::Reaction { key, sender, timestamp } => {
                let reactions = match content {
                    TimelineItemContent::Message(message) => &mut message.reactions,
                    TimelineItemContent::Poll(poll_state) => &mut poll_state.reactions,
                    TimelineItemContent::Sticker(sticker) => &mut sticker.reactions,

                    TimelineItemContent::RedactedMessage
                    | TimelineItemContent::UnableToDecrypt(..)
                    | TimelineItemContent::MembershipChange(..)
                    | TimelineItemContent::ProfileChange(..)
                    | TimelineItemContent::OtherState(..)
                    | TimelineItemContent::FailedToParseMessageLike { .. }
                    | TimelineItemContent::FailedToParseState { .. }
                    | TimelineItemContent::CallInvite
                    | TimelineItemContent::CallNotify => {
                        // These items don't hold reactions.
                        return Ok(());
                    }
                };

                let status = match &self.own_id {
                    TimelineEventItemId::TransactionId(_) => {
                        // TODO?
                        ReactionStatus::LocalToRemote(None)
                    }
                    TimelineEventItemId::EventId(owned_event_id) => {
                        ReactionStatus::RemoteToRemote(owned_event_id.clone())
                    }
                };

                reactions
                    .entry(key.clone())
                    .or_default()
                    .insert(sender.clone(), ReactionInfo { timestamp: *timestamp, status });
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Default)]
pub(crate) struct Aggregations {
    stashed: HashMap<TimelineEventItemId, Vec<Aggregation>>,
}

impl Aggregations {
    pub fn clear(&mut self) {
        self.stashed.clear();
    }

    pub fn add(&mut self, related_to: OwnedEventId, aggregation: Aggregation) {
        self.stashed.entry(TimelineEventItemId::EventId(related_to)).or_default().push(aggregation);
    }

    pub fn get_mut(&mut self, related_to: &TimelineEventItemId) -> Option<&mut Vec<Aggregation>> {
        self.stashed.get_mut(related_to)
    }

    pub fn apply(
        &self,
        item_id: &TimelineEventItemId,
        content: &mut TimelineItemContent,
    ) -> Result<bool, AggregationError> {
        let Some(aggregations) = self.stashed.get(item_id) else {
            return Ok(false);
        };
        for a in aggregations {
            a.apply(content)?;
        }
        Ok(true)
    }
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum AggregationError {
    #[error("trying to end a poll twice")]
    PollAlreadyEnded,

    #[error("trying to apply an aggregation of one type to an invalid target: expected {expected}, actual {actual}")]
    InvalidType { expected: String, actual: String },
}
