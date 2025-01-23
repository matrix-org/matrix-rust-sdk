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

use ruma::{EventId, MilliSecondsSinceUnixEpoch, OwnedEventId, OwnedUserId};

use crate::timeline::PollState;

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

impl Aggregation {
    pub fn apply_poll(&self, poll_state: &mut PollState) -> Result<(), AggregationError> {
        match self {
            Aggregation::PollResponse { sender, timestamp, answers } => {
                poll_state.add_response(sender.clone(), *timestamp, answers.clone());
            }
            Aggregation::PollEnd { end_date } => {
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
    stashed: HashMap<OwnedEventId, Vec<Aggregation>>,
}

impl Aggregations {
    pub fn clear(&mut self) {
        self.stashed.clear();
    }

    pub fn add(&mut self, event_id: OwnedEventId, aggregation: Aggregation) {
        self.stashed.entry(event_id).or_default().push(aggregation);
    }

    pub fn apply_poll(
        &self,
        event_id: &EventId,
        poll_state: &mut PollState,
    ) -> Result<(), AggregationError> {
        let Some(aggregations) = self.stashed.get(event_id) else {
            return Ok(());
        };
        for a in aggregations {
            a.apply_poll(poll_state)?;
        }
        Ok(())
    }
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum AggregationError {
    #[error("trying to end a poll twice")]
    PollAlreadyEnded,
}
