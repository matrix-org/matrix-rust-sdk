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

//! An aggregation manager for the timeline.
//!
//! An aggregation is an event that relates to another event: for instance, a
//! reaction, a poll response, and so on and so forth.
//!
//! Because of the sync mechanisms and federation, it can happen that a related
//! event is received *before* receiving the event it relates to. Those events
//! must be accounted for, stashed somewhere, and reapplied later, if/when the
//! related-to event shows up.
//!
//! In addition to that, a room's event cache can also decide to move events
//! around, in its own internal representation (likely because it ran into some
//! duplicate events). When that happens, a timeline opened on the given room
//! will see a removal then re-insertion of the given event. If that event was
//! the target of aggregations, then those aggregations must be re-applied when
//! the given event is reinserted.
//!
//! To satisfy both requirements, the [`Aggregations`] "manager" object provided
//! by this module will take care of memoizing aggregations, for the entire
//! lifetime of the timeline (or until it's [`Aggregations::clear()`]'ed by some
//! caller). Aggregations are saved in memory, and have the same lifetime as
//! that of a timeline. This makes it possible to apply pending aggregations
//! to cater for the first use case, and to never lose any aggregations in the
//! second use case.

use std::collections::HashMap;

use ruma::{MilliSecondsSinceUnixEpoch, OwnedEventId, OwnedTransactionId, OwnedUserId};
use tracing::{trace, warn};

use super::{rfind_event_by_item_id, ObservableItemsTransaction};
use crate::timeline::{
    PollState, ReactionInfo, ReactionStatus, TimelineEventItemId, TimelineItem, TimelineItemContent,
};

/// Which kind of aggregation (related event) is this?
#[derive(Clone, Debug)]
pub(crate) enum AggregationKind {
    /// This is a response to a poll.
    PollResponse {
        /// Sender of the poll's response.
        sender: OwnedUserId,
        /// Timestamp at which the response has beens ent.
        timestamp: MilliSecondsSinceUnixEpoch,
        /// All the answers to the poll sent by the sender.
        answers: Vec<String>,
    },

    /// This is the marker of the end of a poll.
    PollEnd {
        /// Timestamp at which the poll ends, i.e. all the responses with a
        /// timestamp prior to this one should be taken into account
        /// (and all the responses with a timestamp after this one
        /// should be dropped).
        end_date: MilliSecondsSinceUnixEpoch,
    },

    /// This is a reaction to another event.
    Reaction {
        /// The reaction "key" displayed by the client, often an emoji.
        key: String,
        /// Sender of the reaction.
        sender: OwnedUserId,
        /// Timestamp at which the reaction has been sent.
        timestamp: MilliSecondsSinceUnixEpoch,
        /// The send status of the reaction this is, with handles to abort it if
        /// we can, etc.
        reaction_status: ReactionStatus,
    },
}

/// An aggregation is an event related to another event (for instance a
/// reaction, a poll's response, etc.).
///
/// It can be either a local or a remote echo.
#[derive(Clone, Debug)]
pub(crate) struct Aggregation {
    /// The kind of aggregation this represents.
    pub kind: AggregationKind,

    /// The own timeline identifier for an aggregation.
    ///
    /// It will be a transaction id when the aggregation is still a local echo,
    /// and it will transition into an event id when the aggregation is a
    /// remote echo (i.e. has been received in a sync response):
    pub own_id: TimelineEventItemId,
}

/// Get the poll state from a given [`TimelineItemContent`].
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
    /// Create a new [`Aggregation`].
    pub fn new(own_id: TimelineEventItemId, kind: AggregationKind) -> Self {
        Self { kind, own_id }
    }

    /// Apply an aggregation in-place to a given [`TimelineItemContent`].
    ///
    /// In case of success, returns an enum indicating whether the applied
    /// aggregation had an effect on the content; if it updated it, then the
    /// caller has the responsibility to reflect that change.
    ///
    /// In case of error, returns an error detailing why the aggregation
    /// couldn't be applied.
    pub fn apply(&self, content: &mut TimelineItemContent) -> ApplyAggregationResult {
        match &self.kind {
            AggregationKind::PollResponse { sender, timestamp, answers } => {
                let state = match poll_state_from_item(content) {
                    Ok(state) => state,
                    Err(err) => return ApplyAggregationResult::Error(err),
                };
                state.add_response(sender.clone(), *timestamp, answers.clone());
                ApplyAggregationResult::UpdatedItem
            }

            AggregationKind::PollEnd { end_date } => {
                let poll_state = match poll_state_from_item(content) {
                    Ok(state) => state,
                    Err(err) => return ApplyAggregationResult::Error(err),
                };
                if !poll_state.end(*end_date) {
                    return ApplyAggregationResult::Error(AggregationError::PollAlreadyEnded);
                }
                ApplyAggregationResult::UpdatedItem
            }

            AggregationKind::Reaction { key, sender, timestamp, reaction_status } => {
                let Some(reactions) = content.reactions_mut() else {
                    // These items don't hold reactions.
                    return ApplyAggregationResult::LeftItemIntact;
                };

                let previous_reaction = reactions.entry(key.clone()).or_default().insert(
                    sender.clone(),
                    ReactionInfo { timestamp: *timestamp, status: reaction_status.clone() },
                );

                let is_same = previous_reaction.is_some_and(|prev| {
                    prev.timestamp == *timestamp
                        && matches!(
                            (prev.status, reaction_status),
                            (ReactionStatus::LocalToLocal(_), ReactionStatus::LocalToLocal(_))
                                | (
                                    ReactionStatus::LocalToRemote(_),
                                    ReactionStatus::LocalToRemote(_),
                                )
                                | (
                                    ReactionStatus::RemoteToRemote(_),
                                    ReactionStatus::RemoteToRemote(_),
                                )
                        )
                });

                if is_same {
                    ApplyAggregationResult::LeftItemIntact
                } else {
                    ApplyAggregationResult::UpdatedItem
                }
            }
        }
    }

    /// Undo an aggregation in-place to a given [`TimelineItemContent`].
    ///
    /// In case of success, returns an enum indicating whether unapplying the
    /// aggregation had an effect on the content; if it updated it, then the
    /// caller has the responsibility to reflect that change.
    ///
    /// In case of error, returns an error detailing why the aggregation
    /// couldn't be unapplied.
    pub fn unapply(&self, content: &mut TimelineItemContent) -> ApplyAggregationResult {
        match &self.kind {
            AggregationKind::PollResponse { sender, timestamp, .. } => {
                let state = match poll_state_from_item(content) {
                    Ok(state) => state,
                    Err(err) => return ApplyAggregationResult::Error(err),
                };
                state.remove_response(sender, *timestamp);
                ApplyAggregationResult::UpdatedItem
            }

            AggregationKind::PollEnd { .. } => {
                // Assume we can't undo a poll end event at the moment.
                ApplyAggregationResult::Error(AggregationError::CantUndoPollEnd)
            }

            AggregationKind::Reaction { key, sender, .. } => {
                let Some(reactions) = content.reactions_mut() else {
                    // An item that doesn't hold any reactions.
                    return ApplyAggregationResult::LeftItemIntact;
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

                if previous_entry.is_some() {
                    ApplyAggregationResult::UpdatedItem
                } else {
                    ApplyAggregationResult::LeftItemIntact
                }
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
    /// Clear all the known aggregations from all the mappings.
    pub fn clear(&mut self) {
        self.related_events.clear();
        self.inverted_map.clear();
    }

    /// Add a given aggregation that relates to the [`TimelineItemContent`]
    /// identified by the given [`TimelineEventItemId`].
    pub fn add(&mut self, related_to: TimelineEventItemId, aggregation: Aggregation) {
        self.inverted_map.insert(aggregation.own_id.clone(), related_to.clone());
        self.related_events.entry(related_to).or_default().push(aggregation);
    }

    /// Is the given id one for a known aggregation to another event?
    ///
    /// If so, returns the target event identifier as well as the aggregation.
    /// The aggregation must be unapplied on the corresponding timeline
    /// item.
    #[must_use]
    pub fn try_remove_aggregation(
        &mut self,
        aggregation_id: &TimelineEventItemId,
    ) -> Option<(&TimelineEventItemId, Aggregation)> {
        let found = self.inverted_map.get(aggregation_id)?;

        // Find and remove the aggregation in the other mapping.
        let aggregation = if let Some(aggregations) = self.related_events.get_mut(found) {
            let removed = aggregations
                .iter()
                .position(|agg| agg.own_id == *aggregation_id)
                .map(|idx| aggregations.remove(idx));

            // If this was the last aggregation, remove the entry in the `related_events`
            // mapping.
            if aggregations.is_empty() {
                self.related_events.remove(found);
            }

            removed
        } else {
            None
        };

        if aggregation.is_none() {
            warn!("incorrect internal state: {aggregation_id:?} was present in the inverted map, not in related-to map.");
        }

        Some((found, aggregation?))
    }

    /// Apply all the aggregations to a [`TimelineItemContent`].
    ///
    /// Will return an error at the first aggregation that couldn't be applied;
    /// see [`Aggregation::apply`] which explains under which conditions it can
    /// happen.
    pub fn apply(
        &self,
        item_id: &TimelineEventItemId,
        content: &mut TimelineItemContent,
    ) -> Result<(), AggregationError> {
        let Some(aggregations) = self.related_events.get(item_id) else {
            return Ok(());
        };
        for a in aggregations {
            if let ApplyAggregationResult::Error(err) = a.apply(content) {
                return Err(err);
            }
        }
        Ok(())
    }

    /// Mark a target event as being sent (i.e. it transitions from an local
    /// transaction id to its remote event id counterpart), by updating the
    /// internal mappings.
    pub fn mark_target_as_sent(&mut self, txn_id: OwnedTransactionId, event_id: OwnedEventId) {
        let from = TimelineEventItemId::TransactionId(txn_id);
        let to = TimelineEventItemId::EventId(event_id);

        // Update the aggregations in the `related_events` field.
        if let Some(aggregations) = self.related_events.remove(&from) {
            // Update the inverted mappings (from aggregation's id, to the new target id).
            for a in &aggregations {
                if let Some(prev_target) = self.inverted_map.remove(&a.own_id) {
                    debug_assert_eq!(prev_target, from);
                    self.inverted_map.insert(a.own_id.clone(), to.clone());
                }
            }
            // Update the direct mapping of target -> aggregations.
            self.related_events.entry(to).or_default().extend(aggregations);
        }
    }

    /// Mark an aggregation event as being sent (i.e. it transitions from an
    /// local transaction id to its remote event id counterpart), by
    /// updating the internal mappings.
    ///
    /// When an aggregation has been marked as sent, it may need to be reapplied
    /// to the corresponding [`TimelineItemContent`]; in this case, a
    /// [`MarkAggregationSentResult::MarkedSent`] result with a set `update`
    /// will be returned, and must be applied.
    pub fn mark_aggregation_as_sent(
        &mut self,
        txn_id: OwnedTransactionId,
        event_id: OwnedEventId,
    ) -> MarkAggregationSentResult {
        let from = TimelineEventItemId::TransactionId(txn_id);
        let to = TimelineEventItemId::EventId(event_id.clone());

        let Some(target) = self.inverted_map.remove(&from) else {
            return MarkAggregationSentResult::NotFound;
        };

        let mut target_and_new_aggregation = MarkAggregationSentResult::MarkedSent { update: None };

        if let Some(aggregations) = self.related_events.get_mut(&target) {
            if let Some(found) = aggregations.iter_mut().find(|agg| agg.own_id == from) {
                found.own_id = to.clone();

                match &mut found.kind {
                    AggregationKind::PollResponse { .. } | AggregationKind::PollEnd { .. } => {
                        // Nothing particular to do.
                    }
                    AggregationKind::Reaction { reaction_status, .. } => {
                        // Mark the reaction as becoming remote, and signal that update to the
                        // caller.
                        *reaction_status = ReactionStatus::RemoteToRemote(event_id);
                        target_and_new_aggregation = MarkAggregationSentResult::MarkedSent {
                            update: Some((target.clone(), found.clone())),
                        };
                    }
                }
            }
        }

        self.inverted_map.insert(to, target);

        target_and_new_aggregation
    }
}

/// Find an item identified by the target identifier, and apply the aggregation
/// onto it.
///
/// Returns whether the aggregation has been applied or not (so as to increment
/// a number of updated result, for instance).
pub(crate) fn find_item_and_apply_aggregation(
    items: &mut ObservableItemsTransaction<'_>,
    target: &TimelineEventItemId,
    aggregation: Aggregation,
) -> bool {
    let Some((idx, event_item)) = rfind_event_by_item_id(items, target) else {
        trace!("couldn't find aggregation's target {target:?}");
        return false;
    };

    let mut new_content = event_item.content().clone();

    match aggregation.apply(&mut new_content) {
        ApplyAggregationResult::UpdatedItem => {
            trace!("applied aggregation");
            let new_item = event_item.with_content(new_content);
            items.replace(idx, TimelineItem::new(new_item, event_item.internal_id.to_owned()));
            true
        }
        ApplyAggregationResult::LeftItemIntact => {
            trace!("applying the aggregation had no effect");
            false
        }
        ApplyAggregationResult::Error(err) => {
            warn!("error when applying aggregation: {err}");
            false
        }
    }
}

/// The result of marking an aggregation as sent.
pub(crate) enum MarkAggregationSentResult {
    /// The aggregation has been found, and marked as sent.
    ///
    /// Optionally, it can include an [`Aggregation`] `update` to the matching
    /// [`TimelineItemContent`] item identified by the [`TimelineEventItemId`].
    MarkedSent { update: Option<(TimelineEventItemId, Aggregation)> },
    /// The aggregation was unknown to the aggregations manager, aka not found.
    NotFound,
}

/// The result of applying (or unapplying) an aggregation onto a timeline item.
pub(crate) enum ApplyAggregationResult {
    /// The item has been updated after applying the aggregation.
    UpdatedItem,

    /// The item hasn't been modified after applying the aggregation, because it
    /// was likely already applied prior to this.
    LeftItemIntact,

    /// An error happened while applying the aggregation.
    Error(AggregationError),
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
