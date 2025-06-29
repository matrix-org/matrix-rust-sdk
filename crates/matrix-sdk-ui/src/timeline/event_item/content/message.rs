// Copyright 2023 The Matrix.org Foundation C.I.C.
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

//! Timeline item content bits for `m.room.message` events.

use std::fmt;

use ruma::{
    OwnedEventId,
    events::{
        AnySyncMessageLikeEvent, AnySyncTimelineEvent, BundledMessageLikeRelations, Mentions,
        poll::unstable_start::{
            NewUnstablePollStartEventContentWithoutRelation, SyncUnstablePollStartEvent,
            UnstablePollStartEventContent,
        },
        room::message::{
            MessageType, Relation, RoomMessageEventContentWithoutRelation, SyncRoomMessageEvent,
        },
    },
    html::RemoveReplyFallback,
    serde::Raw,
};
use tracing::{error, trace};

use crate::DEFAULT_SANITIZER_MODE;

/// An `m.room.message` event or extensible event, including edits.
#[derive(Clone)]
pub struct Message {
    pub(in crate::timeline) msgtype: MessageType,
    pub(in crate::timeline) edited: bool,
    pub(in crate::timeline) mentions: Option<Mentions>,
}

impl Message {
    /// Construct a `Message` from a `m.room.message` event.
    pub(in crate::timeline) fn from_event(
        mut msgtype: MessageType,
        mentions: Option<Mentions>,
        edit: Option<RoomMessageEventContentWithoutRelation>,
        remove_reply_fallback: RemoveReplyFallback,
    ) -> Self {
        msgtype.sanitize(DEFAULT_SANITIZER_MODE, remove_reply_fallback);

        let mut ret = Self { msgtype, edited: false, mentions };

        if let Some(edit) = edit {
            ret.apply_edit(edit);
        }

        ret
    }

    /// Apply an edit to the current message.
    pub(crate) fn apply_edit(&mut self, mut new_content: RoomMessageEventContentWithoutRelation) {
        trace!("applying edit to a Message");
        // Edit's content is never supposed to contain the reply fallback.
        new_content.msgtype.sanitize(DEFAULT_SANITIZER_MODE, RemoveReplyFallback::No);
        self.msgtype = new_content.msgtype;
        self.mentions = new_content.mentions;
        self.edited = true;
    }

    /// Get the `msgtype`-specific data of this message.
    pub fn msgtype(&self) -> &MessageType {
        &self.msgtype
    }

    /// Get a reference to the message body.
    ///
    /// Shorthand for `.msgtype().body()`.
    pub fn body(&self) -> &str {
        self.msgtype.body()
    }

    /// Get the edit state of this message (has been edited: `true` /
    /// `false`).
    pub fn is_edited(&self) -> bool {
        self.edited
    }

    /// Get the mentions of this message.
    pub fn mentions(&self) -> Option<&Mentions> {
        self.mentions.as_ref()
    }
}

/// Extracts the raw json of the edit event part of bundled relations.
///
/// Note: while we had access to the deserialized event earlier, events are not
/// serializable, by design of Ruma, so we can't extract a bundled related event
/// and serialize it back to a raw JSON event.
pub(crate) fn extract_bundled_edit_event_json(
    raw: &Raw<AnySyncTimelineEvent>,
) -> Option<Raw<AnySyncTimelineEvent>> {
    // Follow the `unsigned`.`m.relations`.`m.replace` path.
    let raw_unsigned: Raw<serde_json::Value> = raw.get_field("unsigned").ok()??;
    let raw_relations: Raw<serde_json::Value> = raw_unsigned.get_field("m.relations").ok()??;
    raw_relations.get_field::<Raw<AnySyncTimelineEvent>>("m.replace").ok()?
}

/// Extracts a replacement for a room message, if present in the bundled
/// relations , along with the event ID of the replacement event.
pub(crate) fn extract_room_msg_edit_content(
    relations: BundledMessageLikeRelations<AnySyncMessageLikeEvent>,
) -> Option<(OwnedEventId, RoomMessageEventContentWithoutRelation)> {
    match *relations.replace? {
        AnySyncMessageLikeEvent::RoomMessage(SyncRoomMessageEvent::Original(ev)) => match ev
            .content
            .relates_to
        {
            Some(Relation::Replacement(re)) => {
                trace!("found a bundled edit event in a room message");
                Some((ev.event_id, re.new_content))
            }
            _ => {
                error!("got m.room.message event with an edit without a valid m.replace relation");
                None
            }
        },

        AnySyncMessageLikeEvent::RoomMessage(SyncRoomMessageEvent::Redacted(_)) => None,

        _ => {
            error!("got m.room.message event with an edit of a different event type");
            None
        }
    }
}

/// Extracts a replacement for a room message, if present in the bundled
/// relations, along with the event ID of the replacement event.
pub(crate) fn extract_poll_edit_content(
    relations: BundledMessageLikeRelations<AnySyncMessageLikeEvent>,
) -> Option<(OwnedEventId, NewUnstablePollStartEventContentWithoutRelation)> {
    match *relations.replace? {
        AnySyncMessageLikeEvent::UnstablePollStart(SyncUnstablePollStartEvent::Original(ev)) => {
            match ev.content {
                UnstablePollStartEventContent::Replacement(re) => {
                    trace!("found a bundled edit event in a poll");
                    Some((ev.event_id, re.relates_to.new_content))
                }
                _ => {
                    error!("got new poll start event in a bundled edit");
                    None
                }
            }
        }

        AnySyncMessageLikeEvent::UnstablePollStart(SyncUnstablePollStartEvent::Redacted(_)) => None,

        _ => {
            error!("got poll edit event with an edit of a different event type");
            None
        }
    }
}

#[cfg(not(tarpaulin_include))]
impl fmt::Debug for Message {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Self { msgtype: _, edited, mentions: _ } = self;
        // since timeline items are logged, don't include all fields here so
        // people don't leak personal data in bug reports
        f.debug_struct("Message").field("edited", edited).finish_non_exhaustive()
    }
}
