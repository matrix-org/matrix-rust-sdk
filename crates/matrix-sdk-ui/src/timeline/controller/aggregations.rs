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
use matrix_sdk::{
    check_validity_of_replacement_events,
    deserialized_responses::EncryptionInfo,
    send_queue::{RoomSendQueueStorageError, SendHandle, SendReactionHandle, SendRedactionHandle},
};
use ruma::{
    MilliSecondsSinceUnixEpoch, OwnedEventId, OwnedTransactionId, OwnedUserId, UserId,
    events::{
        AnySyncTimelineEvent, beacon_info::BeaconInfoEventContent,
        poll::unstable_start::NewUnstablePollStartEventContentWithoutRelation,
        relation::Replacement, room::message::RoomMessageEventContentWithoutRelation,
    },
    room_version_rules::RoomVersionRules,
    serde::Raw,
};
use tracing::{error, info, trace, warn};

use super::{ObservableItemsTransaction, rfind_event_by_item_id};
use crate::timeline::{
    BeaconInfo, EventSendState, EventTimelineItem, LiveLocationState, MsgLikeContent, MsgLikeKind,
    PollState, ReactionInfo, TimelineEventItemId, TimelineItem, TimelineItemContent,
    event_item::beacon_info_matches,
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
    },

    /// An event has been redacted.
    ///
    /// Our own pending redactions are applied reversibly, sent or remote ones
    /// irreversibly; see [`Aggregation::is_local`].
    Redaction,

    /// An event has been edited.
    ///
    /// Note that edits can't be applied in isolation; we need to identify what
    /// the *latest* edit is, based on the event ordering. As such, they're
    /// handled exceptionally in `Aggregation::apply` and
    /// `Aggregation::unapply`, and the callers have the responsibility of
    /// considering all the edits and applying only the right one.
    Edit(PendingEdit),

    /// A location update for a live location sharing session (MSC3489).
    BeaconUpdate { location: BeaconInfo },

    /// A stop event for a live location sharing session (MSC3489).
    ///
    /// Carries the new (non-live) [`BeaconInfoEventContent`] that should
    /// replace the stored content on the target item, flipping
    /// [`LiveLocationState::is_live`] to `false`.
    ///
    /// Unlike [`BeaconUpdate`], a beacon stop is not reversible.
    BeaconStop { content: BeaconInfoEventContent },

    /// An m.rtc.decline event for an m.rtc.notification event
    CallDeclined {
        /// Sender of the decline.
        sender: OwnedUserId,
    },
}

/// The handle to abort an aggregation while it's still a local echo.
#[derive(Clone, Debug)]
pub(crate) enum AggregationSendHandle {
    /// The aggregation was queued as a regular event.
    Event(SendHandle),
    /// A reaction to a local echo, queued as a child request of that echo.
    Reaction(SendReactionHandle),
    /// A redaction, queued as a dedicated request.
    Redaction(SendRedactionHandle),
}

impl AggregationSendHandle {
    pub async fn abort(&self) -> Result<bool, RoomSendQueueStorageError> {
        match self {
            Self::Event(handle) => handle.abort().await,
            Self::Reaction(handle) => handle.abort().await,
            Self::Redaction(handle) => handle.abort().await,
        }
    }
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

    /// `None` when the aggregation came from the server; `Some` for one of our
    /// local echoes, with the same states as a standalone local event.
    pub send_state: Option<EventSendState>,

    /// Lets one of our local echoes be aborted while it's still pending.
    pub send_handle: Option<AggregationSendHandle>,
}

/// Get the poll state from a given [`TimelineItemContent`].
fn poll_state_from_item<'a>(
    event: &'a mut Cow<'_, EventTimelineItem>,
) -> Result<&'a mut PollState, AggregationError> {
    let content = event.to_mut().content_mut();

    if let TimelineItemContent::MsgLike(MsgLikeContent { kind: MsgLikeKind::Poll(state), .. }) =
        content
    {
        Ok(state)
    } else {
        Err(AggregationError::InvalidType {
            expected: "a poll".to_owned(),
            actual: content.debug_string().to_owned(),
        })
    }
}

/// Get the [`LiveLocationState`] from a given [`TimelineItemContent`], mutably.
fn live_location_state_from_item<'a>(
    event: &'a mut Cow<'_, EventTimelineItem>,
) -> Result<&'a mut LiveLocationState, AggregationError> {
    let content = event.to_mut().content_mut();

    if let TimelineItemContent::MsgLike(MsgLikeContent {
        kind: MsgLikeKind::LiveLocation(state),
        ..
    }) = content
    {
        Ok(state)
    } else {
        Err(AggregationError::InvalidType {
            expected: "a live location".to_owned(),
            actual: content.debug_string().to_owned(),
        })
    }
}

/// Gets the mutable list of users that did decline this notification event.
fn rtc_notification_declinations_from_item<'a>(
    event: &'a mut Cow<'_, EventTimelineItem>,
) -> Result<&'a mut Vec<OwnedUserId>, AggregationError> {
    let content = event.to_mut().content_mut();

    if let TimelineItemContent::RtcNotification { declined_by, .. } = content {
        Ok(declined_by)
    } else {
        Err(AggregationError::InvalidType {
            expected: "an rtc notification".to_owned(),
            actual: content.debug_string().to_owned(),
        })
    }
}

impl Aggregation {
    /// Create an aggregation received from the server.
    pub fn new(own_id: TimelineEventItemId, kind: AggregationKind) -> Self {
        Self { kind, own_id, send_state: None, send_handle: None }
    }

    /// Create an aggregation for one of our local echoes.
    pub fn new_local(
        own_id: TimelineEventItemId,
        kind: AggregationKind,
        send_handle: Option<AggregationSendHandle>,
    ) -> Self {
        Self {
            kind,
            own_id,
            send_state: Some(EventSendState::NotSentYet { progress: None }),
            send_handle,
        }
    }

    /// Whether this is one of our local echoes that hasn't been sent yet.
    pub fn is_local(&self) -> bool {
        !matches!(self.send_state, None | Some(EventSendState::Sent { .. }))
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
                let is_local = self.is_local();
                let is_local_redacted =
                    event.content().is_redacted() && event.unredacted_item.is_some();
                let is_remote_redacted =
                    event.content().is_redacted() && event.unredacted_item.is_none();
                if is_local && is_local_redacted || !is_local && is_remote_redacted {
                    if event.redaction_send_state.is_some() && self.send_state.is_none() {
                        // The remote echo of a redaction we sent: nothing pending anymore.
                        event.to_mut().redaction_send_state = None;
                        ApplyAggregationResult::UpdatedItem
                    } else {
                        ApplyAggregationResult::LeftItemIntact
                    }
                } else {
                    let mut new_item = event.redact(&rules.redaction, is_local);
                    new_item.redaction_send_state = self.send_state.clone();
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

            AggregationKind::Reaction { key, sender, timestamp } => {
                let Some(reactions) = event.content().reactions() else {
                    // An item that can't hold any reactions.
                    return ApplyAggregationResult::LeftItemIntact;
                };

                let previous_reaction = reactions.get(key).and_then(|by_user| by_user.get(sender));

                // Same reaction, same origin: already applied.
                let is_same = previous_reaction.is_some_and(|prev| {
                    prev.timestamp == *timestamp
                        && same_send_state_kind(prev.send_state.as_ref(), self.send_state.as_ref())
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
                        ReactionInfo { timestamp: *timestamp, send_state: self.send_state.clone() },
                    );

                    ApplyAggregationResult::UpdatedItem
                }
            }

            AggregationKind::Edit(_) => {
                // Let the caller handle the edit.
                ApplyAggregationResult::Edit
            }

            AggregationKind::BeaconUpdate { location } => {
                match live_location_state_from_item(event) {
                    Ok(state) => {
                        state.add_location(location.clone());
                        ApplyAggregationResult::UpdatedItem
                    }
                    Err(err) => ApplyAggregationResult::Error(err),
                }
            }

            AggregationKind::BeaconStop { content } => match live_location_state_from_item(event) {
                Ok(state) => {
                    state.stop(content.clone());
                    ApplyAggregationResult::UpdatedItem
                }
                Err(err) => ApplyAggregationResult::Error(err),
            },

            AggregationKind::CallDeclined { sender } => {
                match rtc_notification_declinations_from_item(event) {
                    Ok(declinations) => {
                        if declinations.contains(sender) {
                            ApplyAggregationResult::LeftItemIntact
                        } else {
                            declinations.push(sender.clone());
                            ApplyAggregationResult::UpdatedItem
                        }
                    }
                    Err(err) => ApplyAggregationResult::Error(err),
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
                if self.is_local() {
                    if event.unredacted_item.is_some() {
                        // Unapply local redaction.
                        *event = Cow::Owned(event.unredact());
                        ApplyAggregationResult::UpdatedItem
                    } else {
                        // Event isn't locally redacted. Nothing to do.
                        ApplyAggregationResult::LeftItemIntact
                    }
                } else {
                    // Remote redactions are not reversible.
                    ApplyAggregationResult::Error(AggregationError::CantUndoRedaction)
                }
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

            AggregationKind::BeaconUpdate { location } => {
                match live_location_state_from_item(event) {
                    Ok(state) => {
                        state.remove_location(location.ts);
                        ApplyAggregationResult::UpdatedItem
                    }
                    Err(err) => ApplyAggregationResult::Error(err),
                }
            }

            AggregationKind::BeaconStop { .. } => {
                // Stopping a live location share is not reversible.
                ApplyAggregationResult::Error(AggregationError::CantUndoBeaconStop)
            }

            AggregationKind::CallDeclined { .. } => {
                // One cannot un-decline a call
                ApplyAggregationResult::Error(AggregationError::CantUndoRtcDecline)
            }
        }
    }

    /// Reflect this aggregation's send state on the item it applies to,
    /// without reapplying its content. Returns whether the item changed.
    fn apply_send_state(
        &self,
        siblings: &[Aggregation],
        event: &mut Cow<'_, EventTimelineItem>,
    ) -> bool {
        match &self.kind {
            AggregationKind::Reaction { key, sender, .. } => {
                let has_entry = event
                    .content()
                    .reactions()
                    .and_then(|reactions| reactions.get(key)?.get(sender))
                    .is_some();
                if !has_entry {
                    return false;
                }
                let reactions =
                    event.to_mut().content_mut().reactions_mut().expect("reactions was Some above");
                if let Some(info) =
                    reactions.get_mut(key).and_then(|by_user| by_user.get_mut(sender))
                {
                    info.send_state = self.send_state.clone();
                }
                true
            }

            AggregationKind::Edit(_) => {
                event.to_mut().content_mut().set_edit_send_state(edit_send_state(siblings))
            }

            AggregationKind::Redaction => {
                event.to_mut().redaction_send_state = self.send_state.clone();
                true
            }

            AggregationKind::PollResponse { .. }
            | AggregationKind::PollEnd { .. }
            | AggregationKind::BeaconUpdate { .. }
            | AggregationKind::BeaconStop { .. }
            | AggregationKind::CallDeclined { .. } => false,
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

    /// A pending beacon-stop aggregation received before the corresponding live
    /// `beacon_info` start item has arrived.
    ///
    /// Keyed by the sender's user ID. When a live start item is eventually
    /// inserted via `add_item`, we check if the pending stop matches and
    /// promote it into [`Self::related_events`] so that [`Self::apply_all`]
    /// can apply it immediately.
    pending_beacon_stops: HashMap<OwnedUserId, Aggregation>,
}

impl Aggregations {
    /// Clear all the known aggregations from all the mappings.
    pub fn clear(&mut self) {
        self.related_events.clear();
        self.inverted_map.clear();
        self.pending_beacon_stops.clear();
    }

    /// Stash a [`AggregationKind::BeaconStop`] that arrived before its target
    /// live `beacon_info` item. It will be promoted into
    /// [`Self::related_events`] (and thus picked up by [`Self::apply_all`])
    /// when the live item is inserted via
    /// [`Self::promote_pending_beacon_stop`].
    pub fn add_pending_beacon_stop(&mut self, sender: OwnedUserId, aggregation: Aggregation) {
        self.pending_beacon_stops.insert(sender, aggregation);
    }

    /// Promote a matching stashed beacon-stop aggregation for `sender` into the
    /// regular aggregation map, now that the live start item's
    /// `target_event_id` is known.
    ///
    /// The pending stop's content must match the start event's content (except
    /// for the `live` field) for promotion to occur. If they don't match, the
    /// pending stop is discarded because it belongs to a different session.
    ///
    /// Should be called from `add_item` just before `apply_all`, when inserting
    /// a live `beacon_info` item.
    fn promote_pending_beacon_stop(
        &mut self,
        sender: &OwnedUserId,
        target_event_id: OwnedEventId,
        start_content: &BeaconInfoEventContent,
    ) {
        if !start_content.live {
            return;
        }

        let Some(stop) = self.pending_beacon_stops.remove(sender) else { return };

        let AggregationKind::BeaconStop { content: stop_content } = &stop.kind else {
            warn!("pending beacon stop has unexpected aggregation kind");
            return;
        };

        if !beacon_info_matches(start_content, stop_content) {
            trace!("discarding stale pending beacon stop (content mismatch)");
            return;
        }

        let target = TimelineEventItemId::EventId(target_event_id);
        self.add(target, stop);
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
        // The transition from states 1 to 2 is handled in `update_send_state()`.
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
                    let resolved = self
                        .related_events
                        .get(found)
                        .is_some_and(|aggregations| resolve_edits(aggregations, items, &mut cowed));
                    // Otherwise nothing is pending anymore.
                    // TODO likely need to change the item to indicate
                    // it's been un-edited etc.
                    if resolved || cowed.to_mut().content_mut().set_edit_send_state(None) {
                        items.replace(
                            item_pos,
                            TimelineItem::new(cowed.into_owned(), item.internal_id.to_owned()),
                        );
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
    /// If `sender` is provided alongside a remote `item_id`, any
    /// [`AggregationKind::BeaconStop`] events that arrived out-of-order (i.e.
    /// before the live `beacon_info` start item) are first promoted from the
    /// pending-stops stash into the regular aggregation map so they are picked
    /// up here together with every other pending aggregation for this item.
    ///
    /// Will return an error at the first aggregation that couldn't be applied;
    /// see [`Aggregation::apply`] which explains under which conditions it can
    /// happen.
    pub fn apply_all(
        &mut self,
        item_id: &TimelineEventItemId,
        sender: &OwnedUserId,
        event: &mut Cow<'_, EventTimelineItem>,
        items: &mut ObservableItemsTransaction<'_>,
        rules: &RoomVersionRules,
    ) -> Result<(), AggregationError> {
        // If a beacon-stop arrived before this live start item, it was stashed
        // in `pending_beacon_stops` keyed by sender. Promote it into
        // `related_events` under the now-known start event ID so the loop below
        // applies it together with any other pending aggregations.
        //
        // The promotion verifies that the pending stop's content matches the
        // start event's content to ensure we don't apply an old stop to a new
        // session.
        if let TimelineEventItemId::EventId(event_id) = item_id
            && let Some(live_location) = event.content().as_live_location_state()
        {
            self.promote_pending_beacon_stop(sender, event_id.clone(), &live_location.beacon_info);
        }

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

    /// Update the send state of one of our local aggregations, identified by
    /// its transaction id, and reflect it on the item it applies to.
    ///
    /// Returns `false` if no aggregation has this transaction id.
    pub fn update_send_state(
        &mut self,
        txn_id: OwnedTransactionId,
        send_state: EventSendState,
        items: &mut ObservableItemsTransaction<'_>,
        rules: &RoomVersionRules,
    ) -> bool {
        let from = TimelineEventItemId::TransactionId(txn_id);

        let Some(target) = self.inverted_map.get(&from).cloned() else {
            return false;
        };

        let sent_event_id =
            as_variant!(&send_state, EventSendState::Sent { event_id } => event_id.clone());

        if let Some(event_id) = &sent_event_id {
            let to = TimelineEventItemId::EventId(event_id.clone());
            let remote_echo_received = self
                .related_events
                .get(&target)
                .is_some_and(|aggs| aggs.iter().any(|agg| agg.own_id == to));
            if remote_echo_received {
                // The remote echo got there first: forget the local echo and let the remote
                // one settle the item.
                let remote = self.related_events.get_mut(&target).and_then(|aggs| {
                    aggs.retain(|agg| agg.own_id != from);
                    aggs.iter().find(|agg| agg.own_id == to).cloned()
                });
                self.inverted_map.remove(&from);
                if let Some(remote) = remote {
                    find_item_and_apply_aggregation(self, items, &target, remote, rules);
                }
                return true;
            }
        }

        let updated = {
            let Some(aggregations) = self.related_events.get_mut(&target) else {
                return false;
            };
            let Some(found) = aggregations.iter_mut().find(|agg| agg.own_id == from) else {
                return false;
            };

            found.send_state = Some(send_state);

            if let Some(event_id) = &sent_event_id {
                found.own_id = TimelineEventItemId::EventId(event_id.clone());
            }

            found.clone()
        };

        if let Some(event_id) = sent_event_id {
            self.inverted_map.remove(&from);
            self.inverted_map.insert(TimelineEventItemId::EventId(event_id), target.clone());
        }

        let sent_redaction = matches!(updated.kind, AggregationKind::Redaction)
            && matches!(updated.send_state, Some(EventSendState::Sent { .. }));

        if sent_redaction {
            // A sent redaction becomes irreversible: reapply it.
            find_item_and_apply_aggregation(self, items, &target, updated, rules);
        } else if let Some((idx, item)) = rfind_event_by_item_id(items, &target) {
            let siblings = self.related_events.get(&target).map(Vec::as_slice).unwrap_or(&[]);
            let mut cowed = Cow::Borrowed(&*item);
            if updated.apply_send_state(siblings, &mut cowed) {
                let new_item = TimelineItem::new(cowed.into_owned(), item.internal_id.to_owned());
                items.replace(idx, new_item);
            }
        } else {
            trace!("couldn't find aggregation's target {target:?} to reflect its send state");
        }

        true
    }

    /// Returns the id of the event this aggregation relates to, if it's a known
    /// aggregation.
    pub fn is_aggregation_of(&self, item: &TimelineEventItemId) -> Option<&TimelineEventItemId> {
        self.inverted_map.get(item)
    }

    /// Find the latest reaction with the given key sent by `sender` on
    /// `target`.
    pub fn find_reaction(
        &self,
        target: &TimelineEventItemId,
        key: &str,
        sender: &UserId,
    ) -> Option<&Aggregation> {
        self.related_events.get(target)?.iter().rev().find(|agg| {
            matches!(&agg.kind, AggregationKind::Reaction { key: k, sender: s, .. } if k == key && s == sender)
        })
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
    // A tuple of the best edit, if we have found one and a boolean indicating if
    // the edit is coming from a local echo. If it's from a local echo, we can't
    // validate it as we don't have a raw JSON, but this isn't that important as
    // we're sure we won't send ourselves invalid edits.
    let mut best_edit: Option<(PendingEdit, bool)> = None;
    let mut best_edit_pos = None;

    for a in aggregations {
        if let AggregationKind::Edit(pending_edit) = &a.kind {
            // One of our own edits is always the most recent, even once sent but not
            // echoed yet.
            if a.send_state.is_some() {
                best_edit = Some((pending_edit.clone(), true));
                break;
            }

            match &a.own_id {
                TimelineEventItemId::TransactionId(_) => {
                    // A local echo is always the most recent edit: use this one.
                    best_edit = Some((pending_edit.clone(), true));
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
                                best_edit = Some((pending_edit.clone(), false));
                                *best_edit_pos = pos;
                                trace!(?best_edit_pos, edit_id = ?a.own_id, "found better edit");
                            }
                        } else {
                            trace!(edit_id = ?a.own_id, "couldn't find timeline meta for edit event");

                            // The edit event isn't in the timeline, so it might be a bundled
                            // edit. In this case, record it as the best edit if and only if
                            // there wasn't any other.
                            if best_edit.is_none() {
                                best_edit = Some((pending_edit.clone(), false));
                                trace!(?best_edit_pos, edit_id = ?a.own_id, "found bundled edit");
                            }
                        }
                    } else {
                        // There wasn't any best edit yet, so record this one as being it, with
                        // its position.
                        best_edit = Some((pending_edit.clone(), false));
                        best_edit_pos = items.position_by_event_id(event_id);
                        trace!(?best_edit_pos, edit_id = ?a.own_id, "first best edit");
                    }
                }
            }
        }
    }

    if let Some((edit, is_local_echo)) = best_edit {
        if edit_item(event, edit, is_local_echo) {
            event.to_mut().content_mut().set_edit_send_state(edit_send_state(aggregations));
            true
        } else {
            false
        }
    } else {
        false
    }
}

/// Apply the selected edit to the given EventTimelineItem.
///
/// Returns true if the edit was applied, false otherwise (because the edit and
/// original timeline item types didn't match, for instance).
fn edit_item(
    item: &mut Cow<'_, EventTimelineItem>,
    edit: PendingEdit,
    is_local_echo: bool,
) -> bool {
    // We can receive edits from a local echo, i.e. the edit wasn't yet received
    // from the homeserver.
    //
    // Before we send an edit we check that the event is allowed to be edited and
    // that the replacement content is allowed.
    //
    // We don't have yet a full JSON of the event, so we can't do the validation
    // here.
    if !is_local_echo {
        let Some(original_json) = item.original_json() else {
            error!("The original event does not have the JSON field set.");
            return false;
        };

        let Some(edit_json) = &edit.edit_json else {
            error!(
                "The replacement event of a remotely received edit does not have the JSON field set."
            );
            return false;
        };

        match check_validity_of_replacement_events(
            original_json,
            item.encryption_info(),
            edit_json,
            edit.encryption_info.as_deref(),
        ) {
            Ok(content) => content,
            Err(e) => {
                warn!("Event wasn't replaced due to the replacement event being invalid: {e}");
                return false;
            }
        }
    }

    let TimelineItemContent::MsgLike(content) = item.content() else {
        info!("Edit of message event applies to {:?}, discarding", item.content().debug_string());
        return false;
    };

    let PendingEdit { kind: edit_kind, edit_json, encryption_info, bundled_item_owner: _ } = edit;

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

/// Whether two optional send states are of the same kind (ignoring their
/// payload, e.g. upload progress).
fn same_send_state_kind(a: Option<&EventSendState>, b: Option<&EventSendState>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(a), Some(b)) => std::mem::discriminant(a) == std::mem::discriminant(b),
        _ => false,
    }
}

/// The send state to expose for an item's edits: a failed edit blocks the
/// later ones, so it wins over pending, which wins over sent.
fn edit_send_state(aggregations: &[Aggregation]) -> Option<EventSendState> {
    let rank = |s: &EventSendState| match s {
        EventSendState::SendingFailed { .. } => 2,
        EventSendState::NotSentYet { .. } => 1,
        EventSendState::Sent { .. } => 0,
    };
    aggregations
        .iter()
        .filter(|a| matches!(a.kind, AggregationKind::Edit(_)))
        .filter_map(|a| a.send_state.as_ref())
        .max_by_key(|s| rank(s))
        .cloned()
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

    #[error("a beacon stop can't be unapplied")]
    CantUndoBeaconStop,

    #[error("a call decline can't be unapplied")]
    CantUndoRtcDecline,

    #[error(
        "trying to apply an aggregation of one type to an invalid target: \
         expected {expected}, actual {actual}"
    )]
    InvalidType { expected: String, actual: String },
}
