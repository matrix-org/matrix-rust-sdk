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

use chrono::{DateTime, Datelike, Local, TimeZone};
use futures_signals::signal_vec::MutableVecLockMut;
use indexmap::{map::Entry, IndexMap, IndexSet};
use matrix_sdk_base::deserialized_responses::EncryptionInfo;
use ruma::{
    events::{
        reaction::ReactionEventContent,
        relation::{Annotation, Replacement},
        room::{
            encrypted::{self, RoomEncryptedEventContent},
            member::{Change, RoomMemberEventContent},
            message::{self, MessageType, RoomMessageEventContent},
            redaction::{
                OriginalSyncRoomRedactionEvent, RoomRedactionEventContent, SyncRoomRedactionEvent,
            },
        },
        sticker::StickerEventContent,
        AnyMessageLikeEventContent, AnySyncMessageLikeEvent, AnySyncStateEvent,
        AnySyncTimelineEvent, BundledRelations, EventContent, FullStateEventContent,
        MessageLikeEventType, StateEventType, SyncStateEvent,
    },
    serde::Raw,
    EventId, MilliSecondsSinceUnixEpoch, OwnedEventId, OwnedTransactionId, OwnedUserId,
};
use tracing::{debug, error, field::debug, info, instrument, trace, warn};

use super::{
    event_item::{
        AnyOtherFullStateEventContent, BundledReactions, EventSendState, LocalEventTimelineItem,
        MemberProfileChange, OtherState, Profile, RemoteEventTimelineItem, RoomMembershipChange,
        Sticker,
    },
    find_read_marker, rfind_event_by_id, rfind_event_item, EventTimelineItem, InReplyToDetails,
    Message, ReactionGroup, TimelineInnerMetadata, TimelineItem, TimelineItemContent,
    VirtualTimelineItem,
};
use crate::{events::SyncTimelineEventWithoutContent, room::timeline::MembershipChange};

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

pub(super) struct TimelineEventMetadata {
    pub(super) sender: OwnedUserId,
    pub(super) sender_profile: Profile,
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
    RoomMember {
        user_id: OwnedUserId,
        content: FullStateEventContent<RoomMemberEventContent>,
        sender: OwnedUserId,
    },
    OtherState {
        state_key: String,
        content: AnyOtherFullStateEventContent,
    },
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
            AnySyncTimelineEvent::State(ev) => match ev {
                AnySyncStateEvent::RoomMember(ev) => match ev {
                    SyncStateEvent::Original(ev) => Self::RoomMember {
                        user_id: ev.state_key,
                        content: FullStateEventContent::Original {
                            content: ev.content,
                            prev_content: ev.unsigned.prev_content,
                        },
                        sender: ev.sender,
                    },
                    SyncStateEvent::Redacted(ev) => Self::RoomMember {
                        user_id: ev.state_key,
                        content: FullStateEventContent::Redacted(ev.content),
                        sender: ev.sender,
                    },
                },
                ev => Self::OtherState {
                    state_key: ev.state_key().to_owned(),
                    content: AnyOtherFullStateEventContent::with_event_content(ev.content()),
                },
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
    #[allow(clippy::type_complexity)]
    reaction_map: &'a mut HashMap<
        (Option<OwnedTransactionId>, Option<OwnedEventId>),
        (OwnedUserId, Annotation),
    >,
    pending_reactions: &'a mut HashMap<OwnedEventId, IndexSet<OwnedEventId>>,
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
            pending_reactions: &mut timeline_meta.pending_reactions,
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
                span.record("txn_id", debug(txn_id));
            }

            Flow::Remote { event_id, txn_id, position, .. } => {
                span.record("event_id", debug(event_id));
                span.record("position", debug(position));

                if let Some(txn_id) = txn_id {
                    span.record("txn_id", debug(txn_id));
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
                _ => {
                    debug!(
                        "Ignoring message-like event of type `{}`, not supported (yet)",
                        content.event_type()
                    );
                }
            },

            TimelineEventKind::RedactedMessage => {
                self.add(NewEventTimelineItem::redacted_message());
            }

            TimelineEventKind::Redaction { redacts, content } => {
                self.handle_redaction(redacts, content);
            }

            TimelineEventKind::RoomMember { user_id, content, sender } => {
                self.add(NewEventTimelineItem::room_member(user_id, content, sender));
            }

            TimelineEventKind::OtherState { state_key, content } => {
                self.add(NewEventTimelineItem::other_state(state_key, content));
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

    #[instrument(skip_all, fields(replacement_event_id = ?replacement.event_id))]
    fn handle_room_message_edit(&mut self, replacement: Replacement<MessageType>) {
        update_timeline_item!(self, &replacement.event_id, "edit", |event_item| {
            if self.meta.sender != event_item.sender() {
                info!(
                    original_sender = ?event_item.sender(), edit_sender = ?self.meta.sender,
                    "Edit event applies to another user's timeline item, discarding"
                );
                return None;
            }

            let msg = match &event_item.content() {
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
                TimelineItemContent::MembershipChange(_)
                | TimelineItemContent::ProfileChange(_)
                | TimelineItemContent::OtherState { .. } => {
                    info!("Edit event applies to a state event, discarding");
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
            Some(event_item.with_content(content))
        });
    }

    // Redacted reaction events are no-ops so don't need to be handled
    #[instrument(skip_all, fields(relates_to_event_id = ?c.relates_to.event_id))]
    fn handle_reaction(&mut self, c: ReactionEventContent) {
        let event_id: &EventId = &c.relates_to.event_id;
        let (reaction_id, old_txn_id) = match &self.flow {
            Flow::Local { txn_id, .. } => ((Some(txn_id.clone()), None), None),
            Flow::Remote { event_id, txn_id, .. } => {
                ((None, Some(event_id.clone())), txn_id.as_ref())
            }
        };

        if let Some((idx, event_item)) = rfind_event_by_id(self.timeline_items, event_id) {
            let EventTimelineItem::Remote(remote_event_item) = event_item else {
                error!("inconsistent state: reaction received on a non-remote event item");
                return;
            };

            // Handling of reactions on redacted events is an open question.
            // For now, ignore reactions on redacted events like Element does.
            if let TimelineItemContent::RedactedMessage = remote_event_item.content {
                debug!("Ignoring reaction on redacted event");
                return;
            } else {
                let mut reactions = remote_event_item.reactions.clone();
                let reaction_group = reactions.entry(c.relates_to.key.clone()).or_default();

                if let Some(txn_id) = old_txn_id {
                    // Remove the local echo from the related event.
                    if reaction_group.0.remove(&(Some(txn_id.clone()), None)).is_none() {
                        warn!(
                            "Received reaction with transaction ID, but didn't \
                             find matching reaction in the related event's reactions"
                        );
                    }
                }
                reaction_group.0.insert(reaction_id.clone(), self.meta.sender.clone());

                trace!("Adding reaction");
                self.timeline_items.set_cloned(
                    idx,
                    Arc::new(TimelineItem::Event(
                        remote_event_item.with_reactions(reactions).into(),
                    )),
                );
                self.result.items_updated += 1;
            }
        } else {
            trace!("Timeline item not found, adding reaction to the pending list");
            let (None, Some(reaction_event_id)) = &reaction_id else {
                error!("Adding local reaction echo to event absent from the timeline");
                return;
            };

            let pending = self.pending_reactions.entry(event_id.to_owned()).or_default();

            pending.insert(reaction_event_id.clone());
        }

        if let Flow::Remote { txn_id: Some(txn_id), .. } = &self.flow {
            // Remove the local echo from the reaction map.
            if self.reaction_map.remove(&(Some(txn_id.clone()), None)).is_none() {
                warn!(
                    "Received reaction with transaction ID, but didn't \
                     find matching reaction in reaction_map"
                );
            }
        }
        self.reaction_map.insert(reaction_id, (self.meta.sender.clone(), c.relates_to));
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
    #[instrument(skip_all, fields(redacts_event_id = ?redacts))]
    fn handle_redaction(&mut self, redacts: OwnedEventId, _content: RoomRedactionEventContent) {
        if let Some((_, rel)) = self.reaction_map.remove(&(None, Some(redacts.clone()))) {
            update_timeline_item!(self, &rel.event_id, "redaction", |event_item| {
                let EventTimelineItem::Remote(remote_event_item) = event_item else {
                    error!("inconsistent state: reaction received on a non-remote event item");
                    return None;
                };

                let mut reactions = remote_event_item.reactions.clone();

                let count = {
                    let Entry::Occupied(mut group_entry) = reactions.entry(rel.key.clone()) else {
                        return None;
                    };
                    let group = group_entry.get_mut();

                    if group.0.remove(&(None, Some(redacts.clone()))).is_none() {
                        error!(
                            "inconsistent state: reaction from reaction_map not in reaction list \
                             of timeline item"
                        );
                        return None;
                    }

                    group.len()
                };

                if count == 0 {
                    reactions.remove(&rel.key);
                }

                trace!("Removing reaction");
                Some(remote_event_item.with_reactions(reactions).into())
            });

            if !self.result.items_updated > 0 {
                if let Some(reactions) = self.pending_reactions.get_mut(&rel.event_id) {
                    if !reactions.remove(&redacts) {
                        error!(
                            "inconsistent state: reaction from reaction_map not in reaction list \
                             of pending_reactions"
                        );
                    }
                } else {
                    warn!("reaction_map out of sync with timeline items");
                }
            }
        }

        // Even if the event being redacted is a reaction (found in
        // `reaction_map`), it can still be present in the timeline items
        // directly with the raw event timeline feature (not yet implemented).
        update_timeline_item!(self, &redacts, "redaction", |event_item| {
            let EventTimelineItem::Remote(remote_event_item) = event_item else {
                    error!("inconsistent state: reaction received on a non-remote event item");
                    return None;
                };

            Some(remote_event_item.to_redacted().into())
        });

        if self.result.items_updated > 0 {
            // We will want to know this when debugging redaction issues.
            debug!(redaction_key = ?redacts, "redaction affected no event");
        }
    }

    /// Add a new event item in the timeline.
    fn add(&mut self, item: NewEventTimelineItem) {
        self.result.item_added = true;

        let NewEventTimelineItem { content } = item;

        let item = {
            let sender = self.meta.sender.to_owned();
            let sender_profile = self.meta.sender_profile.clone();
            let mut reactions = self.pending_reactions().unwrap_or_default();

            match &self.flow {
                Flow::Local { txn_id, timestamp } => {
                    EventTimelineItem::Local(LocalEventTimelineItem {
                        send_state: EventSendState::NotSentYet,
                        transaction_id: txn_id.to_owned(),
                        sender,
                        sender_profile,
                        timestamp: *timestamp,
                        content,
                    })
                }
                Flow::Remote { event_id, origin_server_ts, raw_event, .. } => {
                    // Drop pending reactions if the message is redacted.
                    if let TimelineItemContent::RedactedMessage = content {
                        if !reactions.is_empty() {
                            reactions = BundledReactions::default();
                        }
                    }

                    EventTimelineItem::Remote(RemoteEventTimelineItem {
                        event_id: event_id.clone(),
                        sender,
                        sender_profile,
                        timestamp: *origin_server_ts,
                        content,
                        reactions,
                        is_own: self.meta.is_own_event,
                        encryption_info: self.meta.encryption_info.clone(),
                        raw: raw_event.clone(),
                    })
                }
            }
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
                    self.timeline_items
                        .push_cloned(Arc::new(TimelineItem::day_divider(*timestamp)));
                }

                self.timeline_items.push_cloned(item);
            }

            Flow::Remote { position: TimelineItemPosition::Start, origin_server_ts, .. } => {
                // If there is a loading indicator at the top, check for / insert the day
                // divider at position 1 and the new event at 2 rather than 0 and 1.
                let offset = match self.timeline_items.first().and_then(|item| item.as_virtual()) {
                    Some(
                        VirtualTimelineItem::LoadingIndicator | VirtualTimelineItem::TimelineStart,
                    ) => 1,
                    _ => 0,
                };

                // Check if the earliest day divider has the same date as this event.
                if let Some(VirtualTimelineItem::DayDivider(divider_ts)) =
                    self.timeline_items.get(offset).and_then(|item| item.as_virtual())
                {
                    if let Some(day_divider_item) =
                        maybe_create_day_divider_from_timestamps(*divider_ts, *origin_server_ts)
                    {
                        self.timeline_items.insert_cloned(offset, Arc::new(day_divider_item));
                    }
                } else {
                    // The list must always start with a day divider.
                    self.timeline_items.insert_cloned(
                        offset,
                        Arc::new(TimelineItem::day_divider(*origin_server_ts)),
                    );
                }

                self.timeline_items.insert_cloned(offset + 1, item);
            }

            Flow::Remote {
                position: TimelineItemPosition::End,
                txn_id,
                event_id,
                origin_server_ts,
                ..
            } => {
                let result = rfind_event_item(self.timeline_items, |it| {
                    txn_id.is_some() && it.transaction_id() == txn_id.as_deref()
                        || it.event_id() == Some(event_id)
                });

                if let Some((idx, old_item)) = result {
                    if let EventTimelineItem::Remote(old_item) = old_item {
                        // Item was previously received by the server. Until we
                        // implement forwards pagination, this indicates a bug
                        // somewhere.
                        warn!(?item, ?old_item, "Received duplicate event");

                        // With /messages and /sync sometimes disagreeing on
                        // order of messages, we might want to change the
                        // position in some circumstances, but for now this
                        // should be good enough.
                        self.timeline_items.set_cloned(idx, item);
                        return;
                    };

                    if txn_id.is_none() {
                        // The event was created by this client, but the server
                        // sent it back without a transaction ID. This occurs
                        // very often right now due to a sliding-sync bug:
                        // https://github.com/matrix-org/sliding-sync/issues/3
                        // TODO: Raise log level once that bug is fixed
                        trace!("Received remote echo without transaction ID");
                    }

                    // Remove local echo, remote echo will be added below
                    // TODO: Check whether anything is different about the
                    //       old and new item?
                    self.timeline_items.remove(idx);
                } else if txn_id.is_some() {
                    warn!(
                        "Received event with transaction ID, but didn't \
                         find matching timeline item"
                    );
                }

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
                    self.timeline_items
                        .push_cloned(Arc::new(TimelineItem::day_divider(*origin_server_ts)));
                }

                self.timeline_items.push_cloned(item);
            }

            #[cfg(feature = "e2e-encryption")]
            Flow::Remote { position: TimelineItemPosition::Update(idx), .. } => {
                self.timeline_items.set_cloned(*idx, item);
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

    fn pending_reactions(&mut self) -> Option<BundledReactions> {
        match &self.flow {
            Flow::Local { .. } => None,
            Flow::Remote { event_id, .. } => {
                let reactions = self.pending_reactions.remove(event_id)?;
                let mut bundled = IndexMap::new();

                for reaction_event_id in reactions {
                    let reaction_id = (None, Some(reaction_event_id));
                    let Some((sender, annotation)) = self.reaction_map.get(&reaction_id) else {
                        error!(
                            "inconsistent state: reaction from pending_reactions not in reaction_map"
                        );
                        continue;
                    };

                    let group: &mut ReactionGroup =
                        bundled.entry(annotation.key.clone()).or_default();
                    group.0.insert(reaction_id, sender.clone());
                }

                Some(bundled)
            }
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
    let fully_read_event_idx = rfind_event_by_id(items_lock, fully_read_event).map(|(idx, _)| idx);

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
    if let Some((idx, item)) = rfind_event_by_id(timeline_items, event_id) {
        if let Some(new_item) = update(item) {
            timeline_items.set_cloned(idx, Arc::new(TimelineItem::Event(new_item)));
            *items_updated += 1;
        }
    } else {
        debug!("Timeline item not found, discarding {action}");
    }
}

/// Converts a timestamp since Unix Epoch to a local date and time.
fn timestamp_to_local_datetime(ts: MilliSecondsSinceUnixEpoch) -> DateTime<Local> {
    Local
        .timestamp_millis_opt(ts.0.into())
        // Only returns `None` if date is after Dec 31, 262143 BCE.
        .single()
        // Fallback to the current date to avoid issues with malicious
        // homeservers.
        .unwrap_or_else(Local::now)
}

fn datetime_to_ymd(datetime: DateTime<Local>) -> (i32, u32, u32) {
    (datetime.year(), datetime.month(), datetime.day())
}

/// Returns a new day divider item for the new timestamp if it is on a different
/// day than the old timestamp
fn maybe_create_day_divider_from_timestamps(
    old_ts: MilliSecondsSinceUnixEpoch,
    new_ts: MilliSecondsSinceUnixEpoch,
) -> Option<TimelineItem> {
    let old_date = timestamp_to_local_datetime(old_ts);
    let new_date = timestamp_to_local_datetime(new_ts);

    if datetime_to_ymd(old_date) != datetime_to_ymd(new_date) {
        Some(TimelineItem::day_divider(new_ts))
    } else {
        None
    }
}

struct NewEventTimelineItem {
    content: TimelineItemContent,
}

impl NewEventTimelineItem {
    // These constructors could also be `From` implementations, but that would
    // allow users to call them directly, which should not be supported
    fn message(c: RoomMessageEventContent, relations: BundledRelations) -> Self {
        let edited = relations.replace.is_some();
        let content = TimelineItemContent::Message(Message {
            msgtype: c.msgtype,
            in_reply_to: c.relates_to.and_then(InReplyToDetails::from_relation),
            edited,
        });

        Self::from_content(content)
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

    fn room_member(
        user_id: OwnedUserId,
        full_content: FullStateEventContent<RoomMemberEventContent>,
        sender: OwnedUserId,
    ) -> Self {
        use ruma::events::room::member::MembershipChange as MChange;
        let item_content = match &full_content {
            FullStateEventContent::Original { content, prev_content } => {
                let membership_change = content.membership_change(
                    prev_content.as_ref().map(|c| c.details()),
                    &sender,
                    &user_id,
                );

                if let MChange::ProfileChanged { displayname_change, avatar_url_change } =
                    membership_change
                {
                    TimelineItemContent::ProfileChange(MemberProfileChange {
                        user_id,
                        displayname_change: displayname_change.map(|c| Change {
                            new: c.new.map(ToOwned::to_owned),
                            old: c.old.map(ToOwned::to_owned),
                        }),
                        avatar_url_change: avatar_url_change.map(|c| Change {
                            new: c.new.map(ToOwned::to_owned),
                            old: c.old.map(ToOwned::to_owned),
                        }),
                    })
                } else {
                    let change = match membership_change {
                        MChange::None => MembershipChange::None,
                        MChange::Error => MembershipChange::Error,
                        MChange::Joined => MembershipChange::Joined,
                        MChange::Left => MembershipChange::Left,
                        MChange::Banned => MembershipChange::Banned,
                        MChange::Unbanned => MembershipChange::Unbanned,
                        MChange::Kicked => MembershipChange::Kicked,
                        MChange::Invited => MembershipChange::Invited,
                        MChange::KickedAndBanned => MembershipChange::KickedAndBanned,
                        MChange::InvitationAccepted => MembershipChange::InvitationAccepted,
                        MChange::InvitationRejected => MembershipChange::InvitationRejected,
                        MChange::InvitationRevoked => MembershipChange::InvitationRevoked,
                        MChange::Knocked => MembershipChange::Knocked,
                        MChange::KnockAccepted => MembershipChange::KnockAccepted,
                        MChange::KnockRetracted => MembershipChange::KnockRetracted,
                        MChange::KnockDenied => MembershipChange::KnockDenied,
                        MChange::ProfileChanged { .. } => unreachable!(),
                        _ => MembershipChange::NotImplemented,
                    };

                    TimelineItemContent::MembershipChange(RoomMembershipChange {
                        user_id,
                        content: full_content,
                        change: Some(change),
                    })
                }
            }
            FullStateEventContent::Redacted(_) => {
                TimelineItemContent::MembershipChange(RoomMembershipChange {
                    user_id,
                    content: full_content,
                    change: None,
                })
            }
        };

        Self::from_content(item_content)
    }

    fn other_state(state_key: String, content: AnyOtherFullStateEventContent) -> Self {
        Self::from_content(TimelineItemContent::OtherState(OtherState { state_key, content }))
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
        Self { content }
    }
}
