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

use std::{fmt, sync::Arc};

use imbl::{vector, Vector};
use matrix_sdk::deserialized_responses::TimelineEvent;
use ruma::{
    assign,
    events::{
        relation::InReplyTo,
        room::{
            message,
            message::{MessageType, Relation, RoomMessageEventContent, SyncRoomMessageEvent},
        },
        AnyMessageLikeEventContent, AnySyncMessageLikeEvent, AnyTimelineEvent,
        BundledMessageLikeRelations,
    },
    html::RemoveReplyFallback,
    OwnedEventId, OwnedUserId, RoomVersionId, UserId,
};
use tracing::error;

use super::TimelineItemContent;
use crate::{
    timeline::{
        event_item::{EventTimelineItem, Profile, TimelineDetails},
        traits::RoomDataProvider,
        Error as TimelineError, TimelineItem,
    },
    DEFAULT_SANITIZER_MODE,
};

/// An `m.room.message` event or extensible event, including edits.
#[derive(Clone)]
pub struct Message {
    pub(in crate::timeline) msgtype: MessageType,
    pub(in crate::timeline) in_reply_to: Option<InReplyToDetails>,
    pub(in crate::timeline) threaded: bool,
    pub(in crate::timeline) edited: bool,
}

impl Message {
    /// Construct a `Message` from a `m.room.message` event.
    pub(in crate::timeline) fn from_event(
        c: RoomMessageEventContent,
        relations: BundledMessageLikeRelations<AnySyncMessageLikeEvent>,
        timeline_items: &Vector<Arc<TimelineItem>>,
    ) -> Self {
        let edited = relations.has_replacement();
        let edit = relations.replace.and_then(|r| match *r {
            AnySyncMessageLikeEvent::RoomMessage(SyncRoomMessageEvent::Original(ev)) => match ev
                .content
                .relates_to
            {
                Some(Relation::Replacement(re)) => Some(re),
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
        });

        let mut threaded = false;
        let in_reply_to = c.relates_to.and_then(|relation| match relation {
            message::Relation::Reply { in_reply_to } => {
                Some(InReplyToDetails::new(in_reply_to.event_id, timeline_items))
            }
            message::Relation::Thread(thread) => {
                threaded = true;
                thread
                    .in_reply_to
                    .map(|in_reply_to| InReplyToDetails::new(in_reply_to.event_id, timeline_items))
            }
            _ => None,
        });

        let msgtype = match edit {
            Some(mut e) => {
                // Edit's content is never supposed to contain the reply fallback.
                e.new_content.msgtype.sanitize(DEFAULT_SANITIZER_MODE, RemoveReplyFallback::No);
                e.new_content.msgtype
            }
            None => {
                let remove_reply_fallback = if in_reply_to.is_some() {
                    RemoveReplyFallback::Yes
                } else {
                    RemoveReplyFallback::No
                };

                let mut msgtype = c.msgtype;
                msgtype.sanitize(DEFAULT_SANITIZER_MODE, remove_reply_fallback);
                msgtype
            }
        };

        Self { msgtype, in_reply_to, threaded, edited }
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

    /// Get the event this message is replying to, if any.
    pub fn in_reply_to(&self) -> Option<&InReplyToDetails> {
        self.in_reply_to.as_ref()
    }

    /// Whether this message is part of a thread.
    pub fn is_threaded(&self) -> bool {
        self.threaded
    }

    /// Get the edit state of this message (has been edited: `true` /
    /// `false`).
    pub fn is_edited(&self) -> bool {
        self.edited
    }

    pub(in crate::timeline) fn to_content(&self) -> RoomMessageEventContent {
        // Like the `impl From<Message> for RoomMessageEventContent` below, but
        // takes &self and only copies what's needed.
        let relates_to = self.in_reply_to.as_ref().map(|details| message::Relation::Reply {
            in_reply_to: InReplyTo::new(details.event_id.clone()),
        });
        assign!(RoomMessageEventContent::new(self.msgtype.clone()), { relates_to })
    }

    pub(in crate::timeline) fn with_in_reply_to(&self, in_reply_to: InReplyToDetails) -> Self {
        Self { in_reply_to: Some(in_reply_to), ..self.clone() }
    }
}

impl From<Message> for RoomMessageEventContent {
    fn from(msg: Message) -> Self {
        let relates_to = msg.in_reply_to.map(|details| message::Relation::Reply {
            in_reply_to: InReplyTo::new(details.event_id),
        });
        assign!(Self::new(msg.msgtype), { relates_to })
    }
}

#[cfg(not(tarpaulin_include))]
impl fmt::Debug for Message {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Self { msgtype: _, in_reply_to, threaded, edited } = self;
        // since timeline items are logged, don't include all fields here so
        // people don't leak personal data in bug reports
        f.debug_struct("Message")
            .field("in_reply_to", in_reply_to)
            .field("threaded", threaded)
            .field("edited", edited)
            .finish_non_exhaustive()
    }
}

/// Details about an event being replied to.
#[derive(Clone, Debug)]
pub struct InReplyToDetails {
    /// The ID of the event.
    pub event_id: OwnedEventId,

    /// The details of the event.
    ///
    /// Use [`Timeline::fetch_details_for_event`] to fetch the data if it is
    /// unavailable.
    ///
    /// [`Timeline::fetch_details_for_event`]: crate::Timeline::fetch_details_for_event
    pub event: TimelineDetails<Box<RepliedToEvent>>,
}

impl InReplyToDetails {
    pub fn new(
        event_id: OwnedEventId,
        timeline_items: &Vector<Arc<TimelineItem>>,
    ) -> InReplyToDetails {
        let event = timeline_items
            .iter()
            .filter_map(|it| it.as_event())
            .find(|it| it.event_id() == Some(&*event_id))
            .map(|item| Box::new(RepliedToEvent::from_timeline_item(item)));

        InReplyToDetails { event_id, event: TimelineDetails::from_initial_value(event) }
    }
}

/// An event that is replied to.
#[derive(Clone, Debug)]
pub struct RepliedToEvent {
    pub(in crate::timeline) content: TimelineItemContent,
    pub(in crate::timeline) sender: OwnedUserId,
    pub(in crate::timeline) sender_profile: TimelineDetails<Profile>,
}

impl RepliedToEvent {
    /// Get the message of this event.
    pub fn content(&self) -> &TimelineItemContent {
        &self.content
    }

    /// Get the sender of this event.
    pub fn sender(&self) -> &UserId {
        &self.sender
    }

    /// Get the profile of the sender.
    pub fn sender_profile(&self) -> &TimelineDetails<Profile> {
        &self.sender_profile
    }

    pub(crate) fn from_timeline_item(timeline_item: &EventTimelineItem) -> Self {
        Self {
            content: timeline_item.content.clone(),
            sender: timeline_item.sender.clone(),
            sender_profile: timeline_item.sender_profile.clone(),
        }
    }

    pub(in crate::timeline) fn redact(&self, room_version: &RoomVersionId) -> Self {
        Self {
            content: self.content.redact(room_version),
            sender: self.sender.clone(),
            sender_profile: self.sender_profile.clone(),
        }
    }

    pub(in crate::timeline) async fn try_from_timeline_event<P: RoomDataProvider>(
        timeline_event: TimelineEvent,
        room_data_provider: &P,
    ) -> Result<Self, TimelineError> {
        let event = match timeline_event.event.deserialize() {
            Ok(AnyTimelineEvent::MessageLike(event)) => event,
            _ => {
                return Err(TimelineError::UnsupportedEvent);
            }
        };

        let Some(AnyMessageLikeEventContent::RoomMessage(c)) = event.original_content() else {
            return Err(TimelineError::UnsupportedEvent);
        };

        let content =
            TimelineItemContent::Message(Message::from_event(c, event.relations(), &vector![]));
        let sender = event.sender().to_owned();
        let sender_profile = TimelineDetails::from_initial_value(
            room_data_provider.profile_from_user_id(&sender).await,
        );

        Ok(Self { content, sender, sender_profile })
    }
}
