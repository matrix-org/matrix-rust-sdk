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

use std::sync::Arc;

use indexmap::map::Entry;
use matrix_sdk_base::deserialized_responses::EncryptionInfo;
use ruma::{
    events::{
        reaction::ReactionEventContent,
        room::{
            message::{Relation, Replacement, RoomMessageEventContent},
            redaction::{
                OriginalSyncRoomRedactionEvent, RoomRedactionEventContent, SyncRoomRedactionEvent,
            },
        },
        AnyMessageLikeEventContent, AnyStateEventContent, AnySyncMessageLikeEvent,
        AnySyncTimelineEvent, Relations,
    },
    serde::Raw,
    uint, EventId, MilliSecondsSinceUnixEpoch, OwnedEventId, OwnedTransactionId, OwnedUserId,
    UserId,
};
use tracing::{debug, error, info, warn};

use super::{
    event_item::{BundledReactions, TimelineDetails},
    find_event, EventTimelineItem, Message, TimelineInner, TimelineItem, TimelineItemContent,
    TimelineKey,
};

impl TimelineInner {
    pub(super) fn handle_live_event(
        &self,
        raw: Raw<AnySyncTimelineEvent>,
        encryption_info: Option<EncryptionInfo>,
        own_user_id: &UserId,
    ) {
        self.handle_remote_event(raw, encryption_info, own_user_id, TimelineItemPosition::End)
    }

    pub(super) fn handle_local_event(
        &self,
        txn_id: OwnedTransactionId,
        content: AnyMessageLikeEventContent,
        own_user_id: &UserId,
    ) {
        let meta = TimelineEventMetadata {
            sender: own_user_id.to_owned(),
            origin_server_ts: None,
            raw_event: None,
            is_own_event: true,
            relations: None,
            // FIXME: Should we supply something here for encrypted rooms?
            encryption_info: None,
        };

        let flow = Flow::Local { txn_id };
        let kind = TimelineEventKind::Message { content };

        TimelineEventHandler::new(meta, flow, self).handle_event(kind)
    }

    pub(super) fn handle_back_paginated_event(
        &self,
        raw: Raw<AnySyncTimelineEvent>,
        encryption_info: Option<EncryptionInfo>,
        own_user_id: &UserId,
    ) {
        self.handle_remote_event(raw, encryption_info, own_user_id, TimelineItemPosition::Start)
    }

    fn handle_remote_event(
        &self,
        raw: Raw<AnySyncTimelineEvent>,
        encryption_info: Option<EncryptionInfo>,
        own_user_id: &UserId,
        position: TimelineItemPosition,
    ) {
        let event = match raw.deserialize() {
            Ok(ev) => ev,
            Err(_e) => {
                // TODO: Add some sort of error timeline item
                return;
            }
        };

        let sender = event.sender().to_owned();
        let is_own_event = sender == own_user_id;

        let meta = TimelineEventMetadata {
            raw_event: Some(raw),
            sender,
            origin_server_ts: Some(event.origin_server_ts()),
            is_own_event,
            relations: event.relations().cloned(),
            encryption_info,
        };
        let flow = Flow::Remote {
            event_id: event.event_id().to_owned(),
            txn_id: event.transaction_id().map(ToOwned::to_owned),
            position,
        };

        TimelineEventHandler::new(meta, flow, self).handle_event(event.into())
    }
}

enum Flow {
    Local {
        txn_id: OwnedTransactionId,
    },
    Remote {
        event_id: OwnedEventId,
        txn_id: Option<OwnedTransactionId>,
        position: TimelineItemPosition,
    },
}

impl Flow {
    fn to_key(&self) -> TimelineKey {
        match self {
            Self::Remote { event_id, .. } => TimelineKey::EventId(event_id.to_owned()),
            Self::Local { txn_id } => TimelineKey::TransactionId(txn_id.to_owned()),
        }
    }
}

struct TimelineEventMetadata {
    raw_event: Option<Raw<AnySyncTimelineEvent>>,
    sender: OwnedUserId,
    origin_server_ts: Option<MilliSecondsSinceUnixEpoch>,
    is_own_event: bool,
    relations: Option<Relations>,
    encryption_info: Option<EncryptionInfo>,
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

#[derive(Clone)]
enum TimelineEventKind {
    Message { content: AnyMessageLikeEventContent },
    RedactedMessage,
    Redaction { redacts: OwnedEventId, content: RoomRedactionEventContent },
    // FIXME: Split further for state keys of different type
    State { _content: AnyStateEventContent },
    RedactedState, // AnyRedactedStateEventContent
}

enum TimelineItemPosition {
    Start,
    End,
}

// Bundles together a few things that are needed throughout the different stages
// of handling an event (figuring out whether it should update an existing
// timeline item, transforming that item or creating a new one, updating the
// reactive Vec).
struct TimelineEventHandler<'a> {
    meta: TimelineEventMetadata,
    flow: Flow,
    timeline: &'a TimelineInner,
    event_added: bool,
}

impl<'a> TimelineEventHandler<'a> {
    fn new(meta: TimelineEventMetadata, flow: Flow, timeline: &'a TimelineInner) -> Self {
        Self { meta, flow, timeline, event_added: false }
    }

    fn handle_event(mut self, event_kind: TimelineEventKind) {
        match event_kind {
            TimelineEventKind::Message { content } => match content {
                AnyMessageLikeEventContent::Reaction(c) => self.handle_reaction(c),
                AnyMessageLikeEventContent::RoomMessage(c) => self.handle_room_message(c),
                // TODO
                _ => {}
            },
            TimelineEventKind::RedactedMessage => {
                self.add(NewEventTimelineItem::redacted_message());
            }
            TimelineEventKind::Redaction { redacts, content } => {
                self.handle_redaction(redacts, content)
            }
            // TODO: State events
            _ => {}
        }

        if !self.event_added {
            // TODO: Add event as raw
        }
    }

    fn handle_room_message(&mut self, content: RoomMessageEventContent) {
        match content.relates_to {
            Some(Relation::Replacement(re)) => {
                self.handle_room_message_edit(re);
            }
            _ => {
                self.add(NewEventTimelineItem::message(content, self.meta.relations.clone()));
            }
        }
    }

    fn handle_room_message_edit(&mut self, replacement: Replacement) {
        let event_id = &replacement.event_id;

        self.maybe_update_timeline_item(event_id, "edit", |item| {
            if self.meta.sender != item.sender() {
                info!(
                    %event_id, original_sender = %item.sender(), edit_sender = %self.meta.sender,
                    "Event tries to edit another user's timeline item, discarding"
                );
                return None;
            }

            let msg = match &item.content {
                TimelineItemContent::Message(msg) => msg,
                TimelineItemContent::RedactedMessage => {
                    info!(
                        %event_id,
                        "Event tries to edit a non-editable timeline item, discarding"
                    );
                    return None;
                }
            };

            let content = TimelineItemContent::Message(Message {
                msgtype: replacement.new_content.msgtype,
                in_reply_to: msg.in_reply_to.clone(),
                edited: true,
            });

            Some(item.with_content(content))
        });
    }

    // Redacted reaction events are no-ops so don't need to be handled
    fn handle_reaction(&mut self, c: ReactionEventContent) {
        let event_id: &EventId = &c.relates_to.event_id;

        // This lock should never be contended, same as the timeline item lock.
        // If this is ever run in parallel for some reason though, make sure the
        // reaction lock is held for the entire time of the timeline items being
        // locked so these two things can't get out of sync.
        let mut lock = self.timeline.reaction_map.lock().unwrap();

        let did_update = self.maybe_update_timeline_item(event_id, "reaction", |item| {
            // Handling of reactions on redacted events is an open question.
            // For now, ignore reactions on redacted events like Element does.
            if let TimelineItemContent::RedactedMessage = item.content {
                debug!(%event_id, "Ignoring reaction on redacted event");
                None
            } else {
                let mut reactions = item.reactions.clone();
                let reaction_details =
                    reactions.bundled.entry(c.relates_to.key.clone()).or_default();

                reaction_details.count += uint!(1);
                if let TimelineDetails::Ready(senders) = &mut reaction_details.senders {
                    senders.push(self.meta.sender.clone());
                }

                Some(item.with_reactions(reactions))
            }
        });

        if did_update {
            lock.insert(self.flow.to_key(), (self.meta.sender.clone(), c.relates_to));
        }
    }

    // Redacted redactions are no-ops (unfortunately)
    fn handle_redaction(&mut self, redacts: OwnedEventId, _content: RoomRedactionEventContent) {
        let mut did_update = false;

        // Don't release this lock until after update_timeline_item.
        // See first comment in handle_reaction for why.
        let mut lock = self.timeline.reaction_map.lock().unwrap();
        if let Some((sender, rel)) = lock.remove(&TimelineKey::EventId(redacts.clone())) {
            did_update = self.maybe_update_timeline_item(&rel.event_id, "redaction", |item| {
                let mut reactions = item.reactions.clone();

                let mut details_entry = match reactions.bundled.entry(rel.key) {
                    Entry::Occupied(o) => o,
                    Entry::Vacant(_) => return None,
                };
                let details = details_entry.get_mut();
                details.count -= uint!(1);

                if details.count == uint!(0) {
                    details_entry.remove();
                    return Some(item.with_reactions(reactions));
                }

                let senders = match &mut details.senders {
                    TimelineDetails::Ready(senders) => senders,
                    _ => {
                        // FIXME: We probably want to support this somehow in
                        //        the future, but right now it's not possible.
                        warn!(
                            "inconsistent state: shouldn't have a reaction_map entry for a \
                             timeline item with incomplete reactions"
                        );
                        return None;
                    }
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

                Some(item.with_reactions(reactions))
            });

            if !did_update {
                warn!("reaction_map out of sync with timeline items");
            }
        }

        // Even if the event being redacted is a reaction (found in
        // `reaction_map`), it can still be present in the timeline items
        // directly with the raw event timeline feature (not yet implemented).
        did_update |= self.update_timeline_item(&redacts, "redaction", |item| item.to_redacted());

        if !did_update {
            // We will want to know this when debugging redaction issues.
            debug!(redaction_key = ?self.flow.to_key(), %redacts, "redaction affected no event");
        }
    }

    fn add(&mut self, item: NewEventTimelineItem) {
        self.event_added = true;

        let NewEventTimelineItem { content, reactions } = item;
        let item = EventTimelineItem {
            key: self.flow.to_key(),
            event_id: None,
            sender: self.meta.sender.to_owned(),
            content,
            reactions,
            origin_server_ts: self.meta.origin_server_ts,
            is_own: self.meta.is_own_event,
            encryption_info: self.meta.encryption_info.clone(),
            raw: self.meta.raw_event.clone(),
        };

        let item = Arc::new(TimelineItem::Event(item));
        let mut lock = self.timeline.items.lock_mut();
        match &self.flow {
            Flow::Local { .. }
            | Flow::Remote { position: TimelineItemPosition::End, txn_id: None, .. } => {
                lock.push_cloned(item);
            }
            Flow::Remote { position: TimelineItemPosition::Start, txn_id: None, .. } => {
                lock.insert_cloned(0, item);
            }
            Flow::Remote { txn_id: Some(txn_id), event_id, position } => {
                if let Some((idx, _old_item)) = find_event(&lock, txn_id) {
                    // TODO: Check whether anything is different about the old and new item?
                    lock.set_cloned(idx, item);
                } else {
                    debug!(
                        %txn_id, %event_id,
                        "Received event with transaction ID, but didn't find matching timeline item"
                    );

                    match position {
                        TimelineItemPosition::Start => lock.insert_cloned(0, item),
                        TimelineItemPosition::End => lock.push_cloned(item),
                    }
                }
            }
        }
    }

    /// Returns whether an update happened
    fn maybe_update_timeline_item(
        &self,
        event_id: &EventId,
        action: &str,
        update: impl FnOnce(&EventTimelineItem) -> Option<EventTimelineItem>,
    ) -> bool {
        // No point in trying to update items with relations when back-
        // paginating, the event the relation applies to can't be processed yet.
        if matches!(self.flow, Flow::Remote { position: TimelineItemPosition::Start, .. }) {
            return false;
        }

        let mut lock = self.timeline.items.lock_mut();
        if let Some((idx, item)) = find_event(&lock, event_id) {
            if let Some(new_item) = update(item) {
                lock.set_cloned(idx, Arc::new(TimelineItem::Event(new_item)));
                return true;
            }
        } else {
            debug!(%event_id, "Timeline item not found, discarding {action}");
        }

        false
    }

    /// Returns whether an update happened
    fn update_timeline_item(
        &self,
        event_id: &EventId,
        action: &str,
        update: impl FnOnce(&EventTimelineItem) -> EventTimelineItem,
    ) -> bool {
        self.maybe_update_timeline_item(event_id, action, move |item| Some(update(item)))
    }
}

struct NewEventTimelineItem {
    content: TimelineItemContent,
    reactions: BundledReactions,
}

impl NewEventTimelineItem {
    // These constructors could also be `From` implementations, but that would
    // allow users to call them directly, which should not be supported
    pub(crate) fn message(c: RoomMessageEventContent, relations: Option<Relations>) -> Self {
        let edited = relations.as_ref().map_or(false, |r| r.replace.is_some());
        let content = TimelineItemContent::Message(Message {
            msgtype: c.msgtype,
            in_reply_to: c.relates_to.and_then(|rel| match rel {
                Relation::Reply { in_reply_to } => Some(in_reply_to.event_id),
                _ => None,
            }),
            edited,
        });

        let reactions =
            relations.and_then(|r| r.annotation).map(BundledReactions::from).unwrap_or_default();

        Self { content, reactions }
    }

    pub(crate) fn redacted_message() -> Self {
        Self {
            content: TimelineItemContent::RedactedMessage,
            reactions: BundledReactions::default(),
        }
    }
}
