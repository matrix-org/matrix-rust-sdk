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

use crate::timeline::{PollState, TimelineEventItemId, TimelineItemContent};

#[derive(Clone, Debug)]
pub(crate) enum Aggregation {
    PollResponse {
        sender: OwnedUserId,
        timestamp: MilliSecondsSinceUnixEpoch,
        answers: Vec<String>,
    },

    PollEnd {
        end_date: MilliSecondsSinceUnixEpoch,
    },
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
    pub fn apply(&self, content: &mut TimelineItemContent) -> Result<(), AggregationError> {
        match self {
            Aggregation::PollResponse { sender, timestamp, answers } => {
                poll_state_from_item(content)?.add_response(
                    sender.clone(),
                    *timestamp,
                    answers.clone(),
                );
            }
            Aggregation::PollEnd { end_date } => {
                let poll_state = poll_state_from_item(content)?;
                if !poll_state.end(*end_date) {
                    return Err(AggregationError::PollAlreadyEnded);
                }
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

    pub fn add(&mut self, event_id: OwnedEventId, aggregation: Aggregation) {
        self.stashed.entry(TimelineEventItemId::EventId(event_id)).or_default().push(aggregation);
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
