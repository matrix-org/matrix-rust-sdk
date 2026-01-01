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

use std::{borrow::Cow, collections::HashMap, sync::Arc};

use as_variant::as_variant;
use matrix_sdk::deserialized_responses::EncryptionInfo;
use ruma::{
    MilliSecondsSinceUnixEpoch, OwnedEventId, OwnedTransactionId, OwnedUserId,
    events::{
        AnySyncTimelineEvent,
        poll::unstable_start::NewUnstablePollStartEventContentWithoutRelation,
        relation::Replacement, room::message::RoomMessageEventContentWithoutRelation,
    },
    room_version_rules::RoomVersionRules,
    serde::Raw,
};
use tracing::{info, trace, warn};

use super::{ObservableItemsTransaction, rfind_event_by_item_id};
use crate::timeline::{
    EventTimelineItem, MsgLikeContent, MsgLikeKind, PollState, ReactionInfo, ReactionStatus,
    TimelineEventItemId, TimelineItem, TimelineItemContent,
};

#[derive(Clone)]
pub(in crate::timeline) enum PendingEditKind {
    RoomMessage(Replacement<RoomMessageEventContentWithoutRelation>),
    Poll(Replacement<NewUnstablePollStartEventContentWithoutRelation>),
}

impl std::fmt::Debug for PendingEditKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::RoomMessage(_) => f.debug_struct("RoomMessage").finish_non_exhaustive(),
            Self::Poll(_) => f.debug_struct("Poll").finish_non_exhaustive(),
        }
    }
}

#[derive(Clone, Debug)]
pub(in crate::timeline) struct PendingEdit {
    /// The kind of edit this is.
    pub kind: PendingEditKind,

    /// The raw JSON for the edit.
    pub edit_json: Option<Raw<AnySyncTimelineEvent>>,

    /// The encryption info for this edit.
    pub encryption_info: Option<Arc<EncryptionInfo>>,

    /// If provided, this is the identifier of a remote event item that included
    /// this bundled edit.
    pub bundled_item_owner: Option<OwnedEventId>,
}

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

    /// An event has been redacted.
    Redaction,

    /// An event has been edited.
    ///
    /// Note that edits can't be applied in isolation; we need to identify what
    /// the *latest* edit is, based on the event ordering. As such, they're
    /// handled exceptionally in `Aggregation::apply` and
    /// `Aggregation::unapply`, and the callers have the responsibility of
    /// considering all the edits and applying only the right one.
    Edit(PendingEdit),
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
fn poll_state_from_item<'a>(
    event: &'a mut Cow<'_, EventTimelineItem>,
) -> Result<&'a mut PollState, AggregationError> {
    if event.content().is_poll() {
        // It was a poll! Now return the state as mutable.
        let state = as_variant!(
            event.to_mut().content_mut(),
            TimelineItemContent::MsgLike(MsgLikeContent { kind: MsgLikeKind::Poll(s), ..}) => s
        )
        .expect("it was a poll just above");
        Ok(state)
    } else {
        Err(AggregationError::InvalidType {
            expected: "a poll".to_owned(),
            actual: event.content().debug_string().to_owned(),
        })
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
    fn apply(
        &self,
        event: &mut Cow<'_, EventTimelineItem>,
        rules: &RoomVersionRules,
    ) -> ApplyAggregationResult {
        match &self.kind {
            AggregationKind::PollResponse { sender, timestamp, answers } => {
                match poll_state_from_item(event) {
                    Ok(state) => {
                        state.add_response(sender.clone(), *timestamp, answers.clone());
                        ApplyAggregationResult::UpdatedItem
                    }
                    Err(err) => ApplyAggregationResult::Error(err),
                }
            }

            AggregationKind::Redaction => {
                if event.content().is_redacted() {
                    ApplyAggregationResult::LeftItemIntact
                } else {
                    let new_item = event.redact(&rules.redaction);
                    *event = Cow::Owned(new_item);
                    ApplyAggregationResult::UpdatedItem
                }
            }

            AggregationKind::PollEnd { end_date } => match poll_state_from_item(event) {
                Ok(state) => {
                    if !state.end(*end_date) {
                        return ApplyAggregationResult::Error(AggregationError::PollAlreadyEnded);
                    }
                    ApplyAggregationResult::UpdatedItem
                }
                Err(err) => ApplyAggregationResult::Error(err),
            },

            AggregationKind::Reaction { key, sender, timestamp, reaction_status } => {
                let Some(reactions) = event.content().reactions() else {
                    // An item that can't hold any reactions.
                    return ApplyAggregationResult::LeftItemIntact;
                };

                let previous_reaction = reactions.get(key).and_then(|by_user| by_user.get(sender));

                // If the reaction was already added to the item, we don't need to add it back.
                //
                // Search for a previous reaction that would be equivalent.

                let is_same = previous_reaction.is_some_and(|prev| {
                    prev.timestamp == *timestamp
                        && matches!(
                            (&prev.status, reaction_status),
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
                    let reactions = event
                        .to_mut()
                        .content_mut()
                        .reactions_mut()
                        .expect("reactions was Some above");

                    reactions.entry(key.clone()).or_default().insert(
                        sender.clone(),
                        ReactionInfo { timestamp: *timestamp, status: reaction_status.clone() },
                    );

                    ApplyAggregationResult::UpdatedItem
                }
            }

            AggregationKind::Edit(_) => {
                // Let the caller handle the edit.
                ApplyAggregationResult::Edit
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
    fn unapply(&self, event: &mut Cow<'_, EventTimelineItem>) -> ApplyAggregationResult {
        match &self.kind {
            AggregationKind::PollResponse { sender, timestamp, .. } => {
                let state = match poll_state_from_item(event) {
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

            AggregationKind::Redaction => {
                // Redactions are not reversible.
                ApplyAggregationResult::Error(AggregationError::CantUndoRedaction)
            }

            AggregationKind::Reaction { key, sender, .. } => {
                let Some(reactions) = event.content().reactions() else {
                    // An item that can't hold any reactions.
                    return ApplyAggregationResult::LeftItemIntact;
                };

                // We only need to remove the previous reaction if it was there.
                //
                // Search for it.

                let had_entry =
                    reactions.get(key).and_then(|by_user| by_user.get(sender)).is_some();

                if had_entry {
                    let reactions = event
                        .to_mut()
                        .content_mut()
                        .reactions_mut()
                        .expect("reactions was some above");
                    let by_user = reactions.get_mut(key);
                    if let Some(by_user) = by_user {
                        by_user.swap_remove(sender);
                        // If this was the last reaction, remove the entire map for this key.
                        if by_user.is_empty() {
                            reactions.swap_remove(key);
                        }
                    }
                    ApplyAggregationResult::UpdatedItem
                } else {
                    ApplyAggregationResult::LeftItemIntact
                }
            }

            AggregationKind::Edit(_) => {
                // Let the caller handle the edit.
                ApplyAggregationResult::Edit
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
        // If the aggregation is a redaction, it invalidates all the other aggregations;
        // remove them.
        if matches!(aggregation.kind, AggregationKind::Redaction) {
            for agg in self.related_events.remove(&related_to).unwrap_or_default() {
                self.inverted_map.remove(&agg.own_id);
            }
        }

        // If there was any redaction among the current aggregation, adding a new one
        // should be a noop.
        if let Some(previous_aggregations) = self.related_events.get(&related_to)
            && previous_aggregations
                .iter()
                .any(|agg| matches!(agg.kind, AggregationKind::Redaction))
        {
            return;
        }

        self.inverted_map.insert(aggregation.own_id.clone(), related_to.clone());

        // We can have 3 different states for the same aggregation in related_events, in
        // chronological order:
        //
        // 1. The local echo with a transaction ID.
        // 2. The local echo with the event ID returned by the server after sending the
        //    event.
        // 3. The remote echo received via sync.
        //
        // The transition from states 1 to 2 is handled in `mark_aggregation_as_sent()`.
        // So here we need to handle the transition from states 2 to 3. We need to
        // replace the local echo by the remote echo, which might have more data, like
        // the raw JSON.
        let related_events = self.related_events.entry(related_to).or_default();
        if let Some(pos) = related_events.iter().position(|agg| agg.own_id == aggregation.own_id) {
            related_events.remove(pos);
        }
        related_events.push(aggregation);
    }

    /// Is the given id one for a known aggregation to another event?
    ///
    /// If so, unapplies it by replacing the corresponding related item, if
    /// needs be.
    ///
    /// Returns true if an aggregation was found. This doesn't mean
    /// the underlying item has been updated, if it was missing from the
    /// timeline for instance.
    ///
    /// May return an error if it found an aggregation, but it couldn't be
    /// properly applied.
    pub fn try_remove_aggregation(
        &mut self,
        aggregation_id: &TimelineEventItemId,
        items: &mut ObservableItemsTransaction<'_>,
    ) -> Result<bool, AggregationError> {
        let Some(found) = self.inverted_map.get(aggregation_id) else { return Ok(false) };

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

        let Some(aggregation) = aggregation else {
            warn!(
                "incorrect internal state: {aggregation_id:?} was present in the inverted map, \
                 not in related-to map."
            );
            return Ok(false);
        };

        if let Some((item_pos, item)) = rfind_event_by_item_id(items, found) {
            let mut cowed = Cow::Borrowed(&*item);
            match aggregation.unapply(&mut cowed) {
                ApplyAggregationResult::UpdatedItem => {
                    trace!("removed aggregation");
                    items.replace(
                        item_pos,
                        TimelineItem::new(cowed.into_owned(), item.internal_id.to_owned()),
                    );
                }
                ApplyAggregationResult::LeftItemIntact => {}
                ApplyAggregationResult::Error(err) => {
                    warn!("error when unapplying aggregation: {err}");
                }
                ApplyAggregationResult::Edit => {
                    // This edit has been removed; try to find another that still applies.
                    if let Some(aggregations) = self.related_events.get(found) {
                        if resolve_edits(aggregations, items, &mut cowed) {
                            items.replace(
                                item_pos,
                                TimelineItem::new(cowed.into_owned(), item.internal_id.to_owned()),
                            );
                        } else {
                            // No other edit was found, leave the item as is.
                            // TODO likely need to change the item to indicate
                            // it's been un-edited etc.
                        }
                    } else {
                        // No other edits apply.
                    }
                }
            }
        } else {
            info!("missing related-to item ({found:?}) for aggregation {aggregation_id:?}");
        }

        Ok(true)
    }

    /// Apply all the aggregations to a [`TimelineItemContent`].
    ///
    /// Will return an error at the first aggregation that couldn't be applied;
    /// see [`Aggregation::apply`] which explains under which conditions it can
    /// happen.
    ///
    /// Returns a boolean indicating whether at least one aggregation was
    /// applied.
    pub fn apply_all(
        &self,
        item_id: &TimelineEventItemId,
        event: &mut Cow<'_, EventTimelineItem>,
        items: &mut ObservableItemsTransaction<'_>,
        rules: &RoomVersionRules,
    ) -> Result<(), AggregationError> {
        let Some(aggregations) = self.related_events.get(item_id) else {
            return Ok(());
        };

        let mut has_edits = false;

        for a in aggregations {
            match a.apply(event, rules) {
                ApplyAggregationResult::Edit => {
                    has_edits = true;
                }
                ApplyAggregationResult::UpdatedItem | ApplyAggregationResult::LeftItemIntact => {}
                ApplyAggregationResult::Error(err) => return Err(err),
            }
        }

        if has_edits {
            resolve_edits(aggregations, items, event);
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
    /// to the corresponding [`TimelineItemContent`]; this is why we're also
    /// passing the context to apply an aggregation here.
    pub fn mark_aggregation_as_sent(
        &mut self,
        txn_id: OwnedTransactionId,
        event_id: OwnedEventId,
        items: &mut ObservableItemsTransaction<'_>,
        rules: &RoomVersionRules,
    ) -> bool {
        let from = TimelineEventItemId::TransactionId(txn_id);
        let to = TimelineEventItemId::EventId(event_id.clone());

        let Some(target) = self.inverted_map.remove(&from) else {
            return false;
        };

        if let Some(aggregations) = self.related_events.get_mut(&target)
            && let Some(found) = aggregations.iter_mut().find(|agg| agg.own_id == from)
        {
            found.own_id = to.clone();

            match &mut found.kind {
                AggregationKind::PollResponse { .. }
                | AggregationKind::PollEnd { .. }
                | AggregationKind::Edit(..)
                | AggregationKind::Redaction => {
                    // Nothing particular to do.
                }

                AggregationKind::Reaction { reaction_status, .. } => {
                    // Mark the reaction as becoming remote, and signal that update to the
                    // caller.
                    *reaction_status = ReactionStatus::RemoteToRemote(event_id);

                    let found = found.clone();
                    find_item_and_apply_aggregation(self, items, &target, found, rules);
                }
            }
        }

        self.inverted_map.insert(to, target);
        true
    }
}

/// Look at all the edits of a given event, and apply the most recent one, if
/// found.
///
/// Returns true if an edit was found and applied, false otherwise.
fn resolve_edits(
    aggregations: &[Aggregation],
    items: &ObservableItemsTransaction<'_>,
    event: &mut Cow<'_, EventTimelineItem>,
) -> bool {
    let mut best_edit: Option<PendingEdit> = None;
    let mut best_edit_pos = None;

    for a in aggregations {
        if let AggregationKind::Edit(pending_edit) = &a.kind {
            match &a.own_id {
                TimelineEventItemId::TransactionId(_) => {
                    // A local echo is always the most recent edit: use this one.
                    best_edit = Some(pending_edit.clone());
                    break;
                }

                TimelineEventItemId::EventId(event_id) => {
                    if let Some(best_edit_pos) = &mut best_edit_pos {
                        // Find the position of the timeline owning the edit: either the bundled
                        // item owner if this was a bundled edit, or the edit event itself.
                        let pos = items.position_by_event_id(
                            pending_edit.bundled_item_owner.as_ref().unwrap_or(event_id),
                        );

                        if let Some(pos) = pos {
                            // If the edit is more recent (higher index) than the previous best
                            // edit we knew about, use this one.
                            if pos > *best_edit_pos {
                                best_edit = Some(pending_edit.clone());
                                *best_edit_pos = pos;
                                trace!(?best_edit_pos, edit_id = ?a.own_id, "found better edit");
                            }
                        } else {
                            trace!(edit_id = ?a.own_id, "couldn't find timeline meta for edit event");

                            // The edit event isn't in the timeline, so it might be a bundled
                            // edit. In this case, record it as the best edit if and only if
                            // there wasn't any other.
                            if best_edit.is_none() {
                                best_edit = Some(pending_edit.clone());
                                trace!(?best_edit_pos, edit_id = ?a.own_id, "found bundled edit");
                            }
                        }
                    } else {
                        // There wasn't any best edit yet, so record this one as being it, with
                        // its position.
                        best_edit = Some(pending_edit.clone());
                        best_edit_pos = items.position_by_event_id(event_id);
                        trace!(?best_edit_pos, edit_id = ?a.own_id, "first best edit");
                    }
                }
            }
        }
    }

    if let Some(edit) = best_edit { edit_item(event, edit) } else { false }
}

/// Apply the selected edit to the given EventTimelineItem.
///
/// Returns true if the edit was applied, false otherwise (because the edit and
/// original timeline item types didn't match, for instance).
fn edit_item(item: &mut Cow<'_, EventTimelineItem>, edit: PendingEdit) -> bool {
    let PendingEdit { kind: edit_kind, edit_json, encryption_info, bundled_item_owner: _ } = edit;

    if let Some(event_json) = &edit_json {
        let Some(edit_sender) = event_json.get_field::<OwnedUserId>("sender").ok().flatten() else {
            info!("edit event didn't have a sender; likely a malformed event");
            return false;
        };

        if edit_sender != item.sender() {
            info!(
                original_sender = %item.sender(),
                %edit_sender,
                "Edit event applies to another user's timeline item, discarding"
            );
            return false;
        }
    }

    let TimelineItemContent::MsgLike(content) = item.content() else {
        info!("Edit of message event applies to {:?}, discarding", item.content().debug_string());
        return false;
    };

    match (edit_kind, content) {
        (
            PendingEditKind::RoomMessage(replacement),
            MsgLikeContent { kind: MsgLikeKind::Message(msg), .. },
        ) => {
            // First combination: it's a message edit for a message. Good.
            let mut new_msg = msg.clone();
            new_msg.apply_edit(replacement.new_content);

            let new_item = item.with_content_and_latest_edit(
                TimelineItemContent::MsgLike(content.with_kind(MsgLikeKind::Message(new_msg))),
                edit_json,
            );
            *item = Cow::Owned(new_item);
        }

        (
            PendingEditKind::Poll(replacement),
            MsgLikeContent { kind: MsgLikeKind::Poll(poll_state), .. },
        ) => {
            // Second combination: it's a poll edit for a poll. Good.
            if let Some(new_poll_state) = poll_state.edit(replacement.new_content) {
                let new_item = item.with_content_and_latest_edit(
                    TimelineItemContent::MsgLike(
                        content.with_kind(MsgLikeKind::Poll(new_poll_state)),
                    ),
                    edit_json,
                );
                *item = Cow::Owned(new_item);
            } else {
                // The poll has ended, so we can't edit it anymore.
                return false;
            }
        }

        (edit_kind, _) => {
            // Invalid combination.
            info!(
                content = item.content().debug_string(),
                edit = format!("{:?}", edit_kind),
                "Mismatch between edit type and content type",
            );
            return false;
        }
    }

    if let Some(encryption_info) = encryption_info {
        *item = Cow::Owned(item.with_encryption_info(Some(encryption_info)));
    }

    true
}

/// Find an item identified by the target identifier, and apply the aggregation
/// onto it.
///
/// Returns the updated [`EventTimelineItem`] if the aggregation was applied, or
/// `None` otherwise.
pub(crate) fn find_item_and_apply_aggregation(
    aggregations: &Aggregations,
    items: &mut ObservableItemsTransaction<'_>,
    target: &TimelineEventItemId,
    aggregation: Aggregation,
    rules: &RoomVersionRules,
) -> Option<EventTimelineItem> {
    let Some((idx, event_item)) = rfind_event_by_item_id(items, target) else {
        trace!("couldn't find aggregation's target {target:?}");
        return None;
    };

    let mut cowed = Cow::Borrowed(&*event_item);
    match aggregation.apply(&mut cowed, rules) {
        ApplyAggregationResult::UpdatedItem => {
            trace!("applied aggregation");
            let new_event_item = cowed.into_owned();
            let new_item =
                TimelineItem::new(new_event_item.clone(), event_item.internal_id.to_owned());
            items.replace(idx, new_item);
            Some(new_event_item)
        }
        ApplyAggregationResult::Edit => {
            if let Some(aggregations) = aggregations.related_events.get(target)
                && resolve_edits(aggregations, items, &mut cowed)
            {
                let new_event_item = cowed.into_owned();
                let new_item =
                    TimelineItem::new(new_event_item.clone(), event_item.internal_id.to_owned());
                items.replace(idx, new_item);
                return Some(new_event_item);
            }
            None
        }
        ApplyAggregationResult::LeftItemIntact => {
            trace!("applying the aggregation had no effect");
            None
        }
        ApplyAggregationResult::Error(err) => {
            warn!("error when applying aggregation: {err}");
            None
        }
    }
}

/// The result of applying (or unapplying) an aggregation onto a timeline item.
enum ApplyAggregationResult {
    /// The passed `Cow<EventTimelineItem>` has been cloned and updated.
    UpdatedItem,

    /// An edit must be included in the edit set and resolved later, using the
    /// relative position of the edits.
    Edit,

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

    #[error("a redaction can't be unapplied")]
    CantUndoRedaction,

    #[error(
        "trying to apply an aggregation of one type to an invalid target: \
         expected {expected}, actual {actual}"
    )]
    InvalidType { expected: String, actual: String },
}
