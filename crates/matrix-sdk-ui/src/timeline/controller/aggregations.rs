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
use tracing::warn;

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
        reaction_status: ReactionStatus,
    },
}

#[derive(Clone, Debug)]
pub(crate) struct Aggregation {
    pub kind: AggregationKind,
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

    /// Apply an aggregation in-place to a given [`TimelineItemContent`].
    ///
    /// In case of success, returns a boolean indicating whether applying the
    /// aggregation caused a change in the content. In case of error,
    /// returns an error detailing why the aggregation couldn't be applied.
    pub fn apply(&self, content: &mut TimelineItemContent) -> Result<bool, AggregationError> {
        match &self.kind {
            AggregationKind::PollResponse { sender, timestamp, answers } => {
                poll_state_from_item(content)?.add_response(
                    sender.clone(),
                    *timestamp,
                    answers.clone(),
                );
                Ok(true)
            }

            AggregationKind::PollEnd { end_date } => {
                let poll_state = poll_state_from_item(content)?;
                if !poll_state.end(*end_date) {
                    return Err(AggregationError::PollAlreadyEnded);
                }
                Ok(true)
            }

            AggregationKind::Reaction { key, sender, timestamp, reaction_status } => {
                let Some(reactions) = content.reactions_mut() else {
                    // These items don't hold reactions.
                    return Ok(false);
                };

                let previous_reaction = reactions.entry(key.clone()).or_default().insert(
                    sender.clone(),
                    ReactionInfo { timestamp: *timestamp, status: reaction_status.clone() },
                );

                let is_same = previous_reaction.is_some_and(|prev| {
                    prev.timestamp == *timestamp
                        && match (prev.status, reaction_status) {
                            (ReactionStatus::LocalToLocal(_), ReactionStatus::LocalToLocal(_)) => {
                                true
                            }
                            (
                                ReactionStatus::LocalToRemote(_),
                                ReactionStatus::LocalToRemote(_),
                            ) => true,
                            (
                                ReactionStatus::RemoteToRemote(_),
                                ReactionStatus::RemoteToRemote(_),
                            ) => true,
                            _ => false,
                        }
                });

                Ok(!is_same)
            }
        }
    }

    /// Undo an aggregation in-place to a given [`TimelineItemContent`].
    ///
    /// In case of success, returns a boolean indicating whether applying the
    /// aggregation caused a change in the content. In case of error,
    /// returns an error detailing why the aggregation couldn't be applied.
    pub fn unapply(&self, content: &mut TimelineItemContent) -> Result<bool, AggregationError> {
        match &self.kind {
            AggregationKind::PollResponse { sender, timestamp, .. } => {
                poll_state_from_item(content)?.remove_response(sender, *timestamp);
                Ok(true)
            }

            AggregationKind::PollEnd { .. } => {
                // Assume we can't undo a poll end event at the moment.
                return Err(AggregationError::CantUndoPollEnd);
            }

            AggregationKind::Reaction { key, sender, .. } => {
                let Some(reactions) = content.reactions_mut() else {
                    // An item that doesn't hold any reactions.
                    return Ok(false);
                };

                let by_user = reactions.get_mut(key);
                let previous_entry = if let Some(by_user) = by_user {
                    let prev = by_user.swap_remove(sender);
                    // If this was the last reaction, remove the entire map for this key.
                    if by_user.is_empty() {
                        reactions.swap_remove(key);
                    }
                    prev
                } else {
                    None
                };

                Ok(previous_entry.is_some())
            }
        }
    }
}

/// Manager for all known existing aggregations to all events in the timeline.
#[derive(Clone, Debug, Default)]
pub(crate) struct Aggregations {
    /// Mapping of a target event to its list of aggregations.
    related_events: HashMap<TimelineEventItemId, Vec<Aggregation>>,

    /// Mapping of a related event identifier to its target.
    inverted_map: HashMap<TimelineEventItemId, TimelineEventItemId>,
}

impl Aggregations {
    pub fn clear(&mut self) {
        self.related_events.clear();
        self.inverted_map.clear();
    }

    pub fn add(&mut self, related_to: OwnedEventId, aggregation: Aggregation) {
        self.inverted_map
            .insert(aggregation.own_id.clone(), TimelineEventItemId::EventId(related_to.clone()));
        self.related_events
            .entry(TimelineEventItemId::EventId(related_to))
            .or_default()
            .push(aggregation);
    }

    /// Is the given id one for a known aggregation to another event?
    ///
    /// If so, returns the target event identifier as well as the aggregation.
    pub fn try_remove_aggregation(
        &mut self,
        aggregation_id: &TimelineEventItemId,
    ) -> Option<(&TimelineEventItemId, Aggregation)> {
        let found = self.inverted_map.get(aggregation_id)?;

        // Find and remove the aggregation in the other mapping.
        let aggregation = self.related_events.get_mut(found).and_then(|aggregations| {
            if let Some(idx) = aggregations.iter().position(|agg| agg.own_id == *aggregation_id) {
                Some(aggregations.remove(idx))
            } else {
                None
            }
        });

        if aggregation.is_none() {
            warn!("unexpected missing aggregation {aggregation_id:?} (was present in the inverted map, not in the actual map)");
        }

        Some((found, aggregation?))
    }

    pub fn apply(
        &self,
        item_id: &TimelineEventItemId,
        content: &mut TimelineItemContent,
    ) -> Result<(), AggregationError> {
        let Some(aggregations) = self.related_events.get(item_id) else {
            return Ok(());
        };
        for a in aggregations {
            a.apply(content)?;
        }
        Ok(())
    }
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum AggregationError {
    #[error("trying to end a poll twice")]
    PollAlreadyEnded,

    #[error("a poll end can't be unapplied")]
    CantUndoPollEnd,

    #[error("trying to apply an aggregation of one type to an invalid target: expected {expected}, actual {actual}")]
    InvalidType { expected: String, actual: String },
}
