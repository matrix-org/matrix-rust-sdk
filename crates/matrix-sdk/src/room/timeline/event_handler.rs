// Copyright 2022 The Matrix.org Foundation C.I.C.
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

use std::{collections::HashMap, sync::Arc};

use chrono::{Datelike, Local, TimeZone};
use futures_signals::signal_vec::MutableVecLockMut;
use indexmap::map::Entry;
use matrix_sdk_base::deserialized_responses::EncryptionInfo;
use ruma::{
    events::{
        reaction::ReactionEventContent,
        relation::{Annotation, Replacement},
        room::{
            encrypted::{self, RoomEncryptedEventContent},
            message::{self, MessageType, RoomMessageEventContent},
            redaction::{
                OriginalSyncRoomRedactionEvent, RoomRedactionEventContent, SyncRoomRedactionEvent,
            },
        },
        sticker::StickerEventContent,
        AnyMessageLikeEventContent, AnyStateEventContent, AnySyncMessageLikeEvent,
        AnySyncTimelineEvent, BundledRelations, MessageLikeEventType, StateEventType,
    },
    serde::Raw,
    uint, EventId, MilliSecondsSinceUnixEpoch, OwnedEventId, OwnedTransactionId, OwnedUserId,
};
use tracing::{
    debug, error,
    field::{debug, display},
    info, instrument, trace, warn,
};

use super::{
    event_item::{BundledReactions, Sticker, TimelineDetails},
    find_event_by_id, find_event_by_txn_id, find_read_marker, EventTimelineItem, Message,
    TimelineInnerMetadata, TimelineItem, TimelineItemContent, TimelineKey, VirtualTimelineItem,
};
use crate::events::SyncTimelineEventWithoutContent;

pub(super) enum Flow {
    Local {
        txn_id: OwnedTransactionId,
        timestamp: MilliSecondsSinceUnixEpoch,
    },
    Remote {
        event_id: OwnedEventId,
        txn_id: Option<OwnedTransactionId>,
        origin_server_ts: MilliSecondsSinceUnixEpoch,
        raw_event: Raw<AnySyncTimelineEvent>,
        position: TimelineItemPosition,
    },
}

impl Flow {
    fn to_key(&self) -> TimelineKey {
        match self {
            Self::Remote { event_id, .. } => TimelineKey::EventId(event_id.to_owned()),
            Self::Local { txn_id, .. } => TimelineKey::TransactionId(txn_id.to_owned()),
        }
    }

    fn timestamp(&self) -> MilliSecondsSinceUnixEpoch {
        match self {
            Flow::Local { timestamp, .. } => *timestamp,
            Flow::Remote { origin_server_ts, .. } => *origin_server_ts,
        }
    }

    fn raw_event(&self) -> Option<&Raw<AnySyncTimelineEvent>> {
        match self {
            Flow::Local { .. } => None,
            Flow::Remote { raw_event, .. } => Some(raw_event),
        }
    }
}

pub(super) struct TimelineEventMetadata {
    pub(super) sender: OwnedUserId,
    pub(super) is_own_event: bool,
    pub(super) relations: BundledRelations,
    pub(super) encryption_info: Option<EncryptionInfo>,
}

#[derive(Clone)]
pub(super) enum TimelineEventKind {
    Message {
        content: AnyMessageLikeEventContent,
    },
    RedactedMessage,
    Redaction {
        redacts: OwnedEventId,
        content: RoomRedactionEventContent,
    },
    // FIXME: Split further for state keys of different type
    State {
        _content: AnyStateEventContent,
    },
    RedactedState, // AnyRedactedStateEventContent
    FailedToParseMessageLike {
        event_type: MessageLikeEventType,
        error: Arc<serde_json::Error>,
    },
    FailedToParseState {
        event_type: StateEventType,
        state_key: String,
        error: Arc<serde_json::Error>,
    },
}

impl TimelineEventKind {
    pub(super) fn failed_to_parse(
        event: SyncTimelineEventWithoutContent,
        error: serde_json::Error,
    ) -> Self {
        let error = Arc::new(error);
        match event {
            SyncTimelineEventWithoutContent::OriginalMessageLike(ev) => {
                Self::FailedToParseMessageLike { event_type: ev.content.event_type, error }
            }
            SyncTimelineEventWithoutContent::RedactedMessageLike(ev) => {
                Self::FailedToParseMessageLike { event_type: ev.content.event_type, error }
            }
            SyncTimelineEventWithoutContent::OriginalState(ev) => Self::FailedToParseState {
                event_type: ev.content.event_type,
                state_key: ev.state_key,
                error,
            },
            SyncTimelineEventWithoutContent::RedactedState(ev) => Self::FailedToParseState {
                event_type: ev.content.event_type,
                state_key: ev.state_key,
                error,
            },
        }
    }
}

impl From<AnySyncTimelineEvent> for TimelineEventKind {
    fn from(event: AnySyncTimelineEvent) -> Self {
        match event {
            AnySyncTimelineEvent::MessageLike(AnySyncMessageLikeEvent::RoomRedaction(
                SyncRoomRedactionEvent::Original(OriginalSyncRoomRedactionEvent {
                    redacts,
                    content,
                    ..
                }),
            )) => Self::Redaction { redacts, content },
            AnySyncTimelineEvent::MessageLike(ev) => match ev.original_content() {
                Some(content) => Self::Message { content },
                None => Self::RedactedMessage,
            },
            AnySyncTimelineEvent::State(ev) => match ev.original_content() {
                Some(_content) => Self::State { _content },
                None => Self::RedactedState,
            },
        }
    }
}

#[derive(Debug)]
pub(super) enum TimelineItemPosition {
    Start,
    End,
    #[cfg(feature = "e2e-encryption")]
    Update(usize),
}

#[derive(Default)]
pub(super) struct HandleEventResult {
    pub(super) item_added: bool,
    pub(super) items_updated: u16,
}

// Bundles together a few things that are needed throughout the different stages
// of handling an event (figuring out whether it should update an existing
// timeline item, transforming that item or creating a new one, updating the
// reactive Vec).
pub(super) struct TimelineEventHandler<'a, 'i> {
    meta: TimelineEventMetadata,
    flow: Flow,
    timeline_items: &'a mut MutableVecLockMut<'i, Arc<TimelineItem>>,
    reaction_map: &'a mut HashMap<TimelineKey, (OwnedUserId, Annotation)>,
    fully_read_event: &'a mut Option<OwnedEventId>,
    fully_read_event_in_timeline: &'a mut bool,
    result: HandleEventResult,
}

// This is a macro instead of a method plus free fn so the borrow checker can
// "see through" it, allowing borrows of TimelineEventHandler fields other than
// `timeline_items` and `items_updated` in the update closure.
macro_rules! update_timeline_item {
    ($this:ident, $event_id:expr, $action:expr, $update:expr) => {
        _update_timeline_item(
            &mut *$this.timeline_items,
            &mut $this.result.items_updated,
            $event_id,
            $action,
            $update,
        )
    };
}

impl<'a, 'i> TimelineEventHandler<'a, 'i> {
    pub(super) fn new(
        event_meta: TimelineEventMetadata,
        flow: Flow,
        timeline_items: &'a mut MutableVecLockMut<'i, Arc<TimelineItem>>,
        timeline_meta: &'a mut TimelineInnerMetadata,
    ) -> Self {
        Self {
            meta: event_meta,
            flow,
            timeline_items,
            reaction_map: &mut timeline_meta.reaction_map,
            fully_read_event: &mut timeline_meta.fully_read_event,
            fully_read_event_in_timeline: &mut timeline_meta.fully_read_event_in_timeline,
            result: HandleEventResult::default(),
        }
    }

    /// Handle an event.
    ///
    /// Returns the number of timeline updates that were made.
    #[instrument(skip_all, fields(txn_id, event_id, position))]
    pub(super) fn handle_event(mut self, event_kind: TimelineEventKind) -> HandleEventResult {
        let span = tracing::Span::current();
        match &self.flow {
            Flow::Local { txn_id, .. } => {
                span.record("txn_id", display(txn_id));
            }
            Flow::Remote { event_id, txn_id, position, .. } => {
                span.record("event_id", display(event_id));
                span.record("position", debug(position));
                if let Some(txn_id) = txn_id {
                    span.record("txn_id", display(txn_id));
                }
            }
        }

        match event_kind {
            TimelineEventKind::Message { content } => match content {
                AnyMessageLikeEventContent::Reaction(c) => {
                    self.handle_reaction(c);
                }
                AnyMessageLikeEventContent::RoomMessage(RoomMessageEventContent {
                    relates_to: Some(message::Relation::Replacement(re)),
                    ..
                }) => {
                    self.handle_room_message_edit(re);
                }
                AnyMessageLikeEventContent::RoomMessage(c) => {
                    self.add(NewEventTimelineItem::message(c, self.meta.relations.clone()));
                }
                AnyMessageLikeEventContent::RoomEncrypted(c) => self.handle_room_encrypted(c),
                AnyMessageLikeEventContent::Sticker(c) => {
                    self.add(NewEventTimelineItem::sticker(c));
                }
                // TODO
                _ => {}
            },
            TimelineEventKind::RedactedMessage => {
                self.add(NewEventTimelineItem::redacted_message());
            }
            TimelineEventKind::Redaction { redacts, content } => {
                self.handle_redaction(redacts, content)
            }
            TimelineEventKind::State { .. } | TimelineEventKind::RedactedState => {
                // TODO
            }
            TimelineEventKind::FailedToParseMessageLike { event_type, error } => {
                self.add(NewEventTimelineItem::failed_to_parse_message_like(event_type, error));
            }
            TimelineEventKind::FailedToParseState { event_type, state_key, error } => {
                self.add(NewEventTimelineItem::failed_to_parse_state(event_type, state_key, error));
            }
        }

        if !self.result.item_added {
            // TODO: Add event as raw
        }

        self.result
    }

    #[instrument(skip_all, fields(replacement_event_id = %replacement.event_id))]
    fn handle_room_message_edit(&mut self, replacement: Replacement<MessageType>) {
        update_timeline_item!(self, &replacement.event_id, "edit", |item| {
            if self.meta.sender != item.sender() {
                info!(
                    original_sender = %item.sender(), edit_sender = %self.meta.sender,
                    "Edit event applies to another user's timeline item, discarding"
                );
                return None;
            }

            let msg = match &item.content {
                TimelineItemContent::Message(msg) => msg,
                TimelineItemContent::RedactedMessage => {
                    info!("Edit event applies to a redacted message, discarding");
                    return None;
                }
                TimelineItemContent::Sticker(_) => {
                    info!("Edit event applies to a sticker, discarding");
                    return None;
                }
                TimelineItemContent::UnableToDecrypt(_) => {
                    info!("Edit event applies to event that couldn't be decrypted, discarding");
                    return None;
                }
                TimelineItemContent::FailedToParseMessageLike { .. }
                | TimelineItemContent::FailedToParseState { .. } => {
                    info!("Edit event applies to event that couldn't be parsed, discarding");
                    return None;
                }
            };

            let content = TimelineItemContent::Message(Message {
                msgtype: replacement.new_content,
                in_reply_to: msg.in_reply_to.clone(),
                edited: true,
            });

            trace!("Applying edit");
            Some(item.with_content(content))
        });
    }

    // Redacted reaction events are no-ops so don't need to be handled
    #[instrument(skip_all, fields(relates_to_event_id = %c.relates_to.event_id))]
    fn handle_reaction(&mut self, c: ReactionEventContent) {
        let event_id: &EventId = &c.relates_to.event_id;

        update_timeline_item!(self, event_id, "reaction", |item| {
            // Handling of reactions on redacted events is an open question.
            // For now, ignore reactions on redacted events like Element does.
            if let TimelineItemContent::RedactedMessage = item.content {
                debug!("Ignoring reaction on redacted event");
                None
            } else {
                let mut reactions = item.reactions.clone();
                let reaction_details =
                    reactions.bundled.entry(c.relates_to.key.clone()).or_default();

                reaction_details.count += uint!(1);
                if let TimelineDetails::Ready(senders) = &mut reaction_details.senders {
                    senders.push(self.meta.sender.clone());
                }

                trace!("Adding reaction");
                Some(item.with_reactions(reactions))
            }
        });

        if self.result.items_updated > 0 {
            self.reaction_map.insert(self.flow.to_key(), (self.meta.sender.clone(), c.relates_to));
        }
    }

    #[instrument(skip_all)]
    fn handle_room_encrypted(&mut self, c: RoomEncryptedEventContent) {
        match c.relates_to {
            Some(encrypted::Relation::Replacement(_) | encrypted::Relation::Annotation(_)) => {
                // Do nothing for these, as they would not produce a new
                // timeline item when decrypted either
                debug!("Ignoring aggregating event that failed to decrypt");
            }
            _ => {
                self.add(NewEventTimelineItem::unable_to_decrypt(c));
            }
        }
    }

    // Redacted redactions are no-ops (unfortunately)
    #[instrument(skip_all, fields(redacts_event_id = %redacts))]
    fn handle_redaction(&mut self, redacts: OwnedEventId, _content: RoomRedactionEventContent) {
        if let Some((sender, rel)) =
            self.reaction_map.remove(&TimelineKey::EventId(redacts.clone()))
        {
            update_timeline_item!(self, &rel.event_id, "redaction", |item| {
                let mut reactions = item.reactions.clone();

                let Entry::Occupied(mut details_entry) = reactions.bundled.entry(rel.key) else {
                    return None;
                };
                let details = details_entry.get_mut();
                details.count -= uint!(1);

                if details.count == uint!(0) {
                    details_entry.remove();
                    return Some(item.with_reactions(reactions));
                }

                let TimelineDetails::Ready(senders) = &mut details.senders else {
                    // FIXME: We probably want to support this somehow in
                    //        the future, but right now it's not possible.
                    warn!(
                        "inconsistent state: shouldn't have a reaction_map entry for a \
                            timeline item with incomplete reactions"
                    );
                    return None;
                };

                if let Some(idx) = senders.iter().position(|s| *s == sender) {
                    senders.remove(idx);
                } else {
                    error!(
                        "inconsistent state: sender from reaction_map not in reaction sender list \
                         of timeline item"
                    );
                    return None;
                }

                if u64::from(details.count) != senders.len() as u64 {
                    error!("inconsistent state: reaction count differs from number of senders");
                    // Can't make things worse by updating the item, so no early
                    // return here.
                }

                trace!("Removing reaction");
                Some(item.with_reactions(reactions))
            });

            if !self.result.items_updated > 0 {
                warn!("reaction_map out of sync with timeline items");
            }
        }

        // Even if the event being redacted is a reaction (found in
        // `reaction_map`), it can still be present in the timeline items
        // directly with the raw event timeline feature (not yet implemented).
        update_timeline_item!(self, &redacts, "redaction", |item| Some(item.to_redacted()));

        if self.result.items_updated > 0 {
            // We will want to know this when debugging redaction issues.
            debug!(redaction_key = ?self.flow.to_key(), "redaction affected no event");
        }
    }

    fn add(&mut self, item: NewEventTimelineItem) {
        self.result.item_added = true;

        let NewEventTimelineItem { content, reactions } = item;
        let item = EventTimelineItem {
            key: self.flow.to_key(),
            event_id: None,
            sender: self.meta.sender.to_owned(),
            content,
            reactions,
            timestamp: self.flow.timestamp(),
            is_own: self.meta.is_own_event,
            encryption_info: self.meta.encryption_info.clone(),
            raw: self.flow.raw_event().cloned(),
        };

        let item = Arc::new(TimelineItem::Event(item));
        match &self.flow {
            Flow::Local { timestamp, .. } => {
                // Check if the latest event has the same date as this event.
                if let Some(latest_event) = self
                    .timeline_items
                    .iter()
                    .rfind(|item| item.as_event().is_some())
                    .and_then(|item| item.as_event())
                {
                    let old_ts = latest_event.timestamp();

                    if let Some(day_divider_item) =
                        maybe_create_day_divider_from_timestamps(old_ts, *timestamp)
                    {
                        self.timeline_items.push_cloned(Arc::new(day_divider_item));
                    }
                } else {
                    // If there is no event item, there is no day divider yet.
                    let (year, month, day) = timestamp_to_ymd(*timestamp);
                    self.timeline_items
                        .push_cloned(Arc::new(TimelineItem::day_divider(year, month, day)));
                }

                self.timeline_items.push_cloned(item);
            }
            Flow::Remote { txn_id, event_id, position, origin_server_ts, .. } => {
                if let Some(txn_id) = txn_id {
                    if let Some((idx, _old_item)) =
                        find_event_by_txn_id(self.timeline_items, txn_id)
                    {
                        // TODO: Check whether anything is different about the
                        //       old and new item?
                        self.timeline_items.set_cloned(idx, item);
                        return;
                    } else {
                        warn!(
                            "Received event with transaction ID, but didn't \
                             find matching timeline item"
                        );
                    }
                }

                if let Some((idx, old_item)) = find_event_by_id(self.timeline_items, event_id) {
                    // This occurs very often right now due to a sliding-sync
                    // bug: https://github.com/matrix-org/sliding-sync/issues/3
                    // TODO: Use warn log level once that bug is fixed.
                    trace!(
                        ?item,
                        ?old_item,
                        "Received event with an ID we already have a timeline item for"
                    );

                    // With /messages and /sync sometimes disagreeing on order
                    // of messages, we might want to change the position in some
                    // circumstances, but for now this should be good enough.
                    self.timeline_items.set_cloned(idx, item);
                    return;
                }

                match position {
                    TimelineItemPosition::Start => {
                        // If there is a loading indicator at the top, check for / insert the day
                        // divider at position 1 and the new event at 2 rather than 0 and 1.
                        let offset =
                            match self.timeline_items.first().and_then(|item| item.as_virtual()) {
                                Some(
                                    VirtualTimelineItem::LoadingIndicator
                                    | VirtualTimelineItem::TimelineStart,
                                ) => 1,
                                _ => 0,
                            };

                        // Check if the earliest day divider has the same date as this event.
                        if let Some(VirtualTimelineItem::DayDivider { year, month, day }) =
                            self.timeline_items.get(offset).and_then(|item| item.as_virtual())
                        {
                            if let Some(day_divider_item) = maybe_create_day_divider_from_ymd(
                                (*year, *month, *day),
                                timestamp_to_ymd(*origin_server_ts),
                            ) {
                                self.timeline_items
                                    .insert_cloned(offset, Arc::new(day_divider_item));
                            }
                        } else {
                            // The list must always start with a day divider.
                            let (year, month, day) = timestamp_to_ymd(*origin_server_ts);
                            self.timeline_items.insert_cloned(
                                offset,
                                Arc::new(TimelineItem::day_divider(year, month, day)),
                            );
                        }

                        self.timeline_items.insert_cloned(offset + 1, item)
                    }
                    TimelineItemPosition::End => {
                        // Check if the latest event has the same date as this event.
                        if let Some(latest_event) =
                            self.timeline_items.iter().rev().find_map(|item| item.as_event())
                        {
                            let old_ts = latest_event.timestamp();

                            if let Some(day_divider_item) =
                                maybe_create_day_divider_from_timestamps(old_ts, *origin_server_ts)
                            {
                                self.timeline_items.push_cloned(Arc::new(day_divider_item));
                            }
                        } else {
                            // If there is not event item, there is no day divider yet.
                            let (year, month, day) = timestamp_to_ymd(*origin_server_ts);
                            self.timeline_items
                                .push_cloned(Arc::new(TimelineItem::day_divider(year, month, day)));
                        }

                        self.timeline_items.push_cloned(item)
                    }
                    #[cfg(feature = "e2e-encryption")]
                    TimelineItemPosition::Update(idx) => self.timeline_items.set_cloned(*idx, item),
                }
            }
        }

        // See if we got the event corresponding to the read marker now.
        if !*self.fully_read_event_in_timeline {
            update_read_marker(
                self.timeline_items,
                self.fully_read_event.as_deref(),
                self.fully_read_event_in_timeline,
            );
        }
    }
}

pub(crate) fn update_read_marker(
    items_lock: &mut MutableVecLockMut<'_, Arc<TimelineItem>>,
    fully_read_event: Option<&EventId>,
    fully_read_event_in_timeline: &mut bool,
) {
    let Some(fully_read_event) = fully_read_event else { return };
    let read_marker_idx = find_read_marker(items_lock);
    let fully_read_event_idx = find_event_by_id(items_lock, fully_read_event).map(|(idx, _)| idx);
    match (read_marker_idx, fully_read_event_idx) {
        (None, None) => {}
        (None, Some(idx)) => {
            *fully_read_event_in_timeline = true;
            items_lock.insert_cloned(idx + 1, Arc::new(TimelineItem::read_marker()));
        }
        (Some(_), None) => {
            // Keep the current position of the read marker, hopefully we
            // should have a new position later.
            *fully_read_event_in_timeline = false;
        }
        (Some(from), Some(to)) => {
            *fully_read_event_in_timeline = true;

            // The read marker can't move backwards.
            if from < to {
                items_lock.move_from_to(from, to);
            }
        }
    }
}

fn _update_timeline_item(
    timeline_items: &mut MutableVecLockMut<'_, Arc<TimelineItem>>,
    items_updated: &mut u16,
    event_id: &EventId,
    action: &str,
    update: impl FnOnce(&EventTimelineItem) -> Option<EventTimelineItem>,
) {
    if let Some((idx, item)) = find_event_by_id(timeline_items, event_id) {
        if let Some(new_item) = update(item) {
            timeline_items.set_cloned(idx, Arc::new(TimelineItem::Event(new_item)));
            *items_updated += 1;
        }
    } else {
        debug!("Timeline item not found, discarding {action}");
    }
}

/// Converts a timestamp to a `(year, month, day)` tuple.
fn timestamp_to_ymd(ts: MilliSecondsSinceUnixEpoch) -> (i32, u32, u32) {
    let datetime = Local
        .timestamp_millis_opt(ts.0.into())
        // Only returns `None` if date is after Dec 31, 262143 BCE.
        .single()
        // Fallback to the current date to avoid issues with malicious
        // homeservers.
        .unwrap_or_else(Local::now);

    (datetime.year(), datetime.month(), datetime.day())
}

/// Returns a new day divider item for the new timestamp if it is on a different
/// day than the old timestamp
fn maybe_create_day_divider_from_timestamps(
    old_ts: MilliSecondsSinceUnixEpoch,
    new_ts: MilliSecondsSinceUnixEpoch,
) -> Option<TimelineItem> {
    maybe_create_day_divider_from_ymd(timestamp_to_ymd(old_ts), timestamp_to_ymd(new_ts))
}

/// Returns a new day divider item for the new YMD `(year, month, day)` tuple if
/// it is on a different day than the old YMD tuple.
fn maybe_create_day_divider_from_ymd(
    old_ymd: (i32, u32, u32),
    new_ymd: (i32, u32, u32),
) -> Option<TimelineItem> {
    let (old_year, old_month, old_day) = old_ymd;
    let (new_year, new_month, new_day) = new_ymd;
    if old_year != new_year || old_month != new_month || old_day != new_day {
        Some(TimelineItem::day_divider(new_year, new_month, new_day))
    } else {
        None
    }
}

struct NewEventTimelineItem {
    content: TimelineItemContent,
    reactions: BundledReactions,
}

impl NewEventTimelineItem {
    // These constructors could also be `From` implementations, but that would
    // allow users to call them directly, which should not be supported
    fn message(c: RoomMessageEventContent, relations: BundledRelations) -> Self {
        let edited = relations.replace.is_some();
        let content = TimelineItemContent::Message(Message {
            msgtype: c.msgtype,
            in_reply_to: c.relates_to.and_then(|rel| match rel {
                message::Relation::Reply { in_reply_to } => Some(in_reply_to.event_id),
                _ => None,
            }),
            edited,
        });

        let reactions = relations.annotation.map(BundledReactions::from).unwrap_or_default();

        Self { content, reactions }
    }

    fn unable_to_decrypt(content: RoomEncryptedEventContent) -> Self {
        Self::from_content(TimelineItemContent::UnableToDecrypt(content.into()))
    }

    fn redacted_message() -> Self {
        Self::from_content(TimelineItemContent::RedactedMessage)
    }

    fn sticker(content: StickerEventContent) -> Self {
        Self::from_content(TimelineItemContent::Sticker(Sticker { content }))
    }

    fn failed_to_parse_message_like(
        event_type: MessageLikeEventType,
        error: Arc<serde_json::Error>,
    ) -> NewEventTimelineItem {
        Self::from_content(TimelineItemContent::FailedToParseMessageLike { event_type, error })
    }

    fn failed_to_parse_state(
        event_type: StateEventType,
        state_key: String,
        error: Arc<serde_json::Error>,
    ) -> NewEventTimelineItem {
        Self::from_content(TimelineItemContent::FailedToParseState { event_type, state_key, error })
    }

    fn from_content(content: TimelineItemContent) -> Self {
        Self { content, reactions: BundledReactions::default() }
    }
}
