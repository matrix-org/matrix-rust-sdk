// Copyright 2024 The Matrix.org Foundation C.I.C.
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

use imbl::Vector;
use matrix_sdk::{
    crypto::types::events::UtdCause,
    deserialized_responses::{TimelineEvent, TimelineEventKind},
    Room,
};
use ruma::{
    events::{
        poll::unstable_start::UnstablePollStartEventContent, AnyMessageLikeEventContent,
        AnySyncTimelineEvent,
    },
    html::RemoveReplyFallback,
    OwnedEventId, OwnedUserId, UserId,
};
use tracing::{debug, instrument, warn};

use super::TimelineItemContent;
use crate::timeline::{
    event_item::{
        content::{MsgLikeContent, MsgLikeKind},
        extract_room_msg_edit_content, EventTimelineItem, Profile, TimelineDetails,
    },
    traits::RoomDataProvider,
    EncryptedMessage, Error as TimelineError, Message, PollState, ReactionsByKeyBySender, Sticker,
    TimelineItem,
};

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
    content: TimelineItemContent,
    sender: OwnedUserId,
    sender_profile: TimelineDetails<Profile>,
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

    /// Create a [`RepliedToEvent`] from a loaded event timeline item.
    pub fn from_timeline_item(timeline_item: &EventTimelineItem) -> Self {
        Self {
            content: timeline_item.content.clone(),
            sender: timeline_item.sender.clone(),
            sender_profile: timeline_item.sender_profile.clone(),
        }
    }

    /// Try to create a `RepliedToEvent` from a `TimelineEvent` by providing the
    /// room.
    pub async fn try_from_timeline_event_for_room(
        timeline_event: TimelineEvent,
        room_data_provider: &Room,
    ) -> Result<Self, TimelineError> {
        Self::try_from_timeline_event(timeline_event, room_data_provider).await
    }

    #[instrument(skip_all)]
    pub(in crate::timeline) async fn try_from_timeline_event<P: RoomDataProvider>(
        timeline_event: TimelineEvent,
        room_data_provider: &P,
    ) -> Result<Self, TimelineError> {
        let event = match timeline_event.raw().deserialize() {
            Ok(AnySyncTimelineEvent::MessageLike(event)) => event,
            Ok(_) => {
                warn!("can't get details, event isn't a message-like event");
                return Err(TimelineError::UnsupportedEvent);
            }
            Err(err) => {
                warn!("can't get details, event couldn't be deserialized: {err}");
                return Err(TimelineError::UnsupportedEvent);
            }
        };

        debug!(event_type = %event.event_type(), "got deserialized event");

        let content = match event.original_content() {
            Some(content) => match content {
                AnyMessageLikeEventContent::RoomMessage(c) => {
                    // Assume we're not interested in reactions and thread info in this context:
                    // this is information for an embedded (replied-to) event, that will usually not
                    // include detailed information like reactions.
                    let reactions = ReactionsByKeyBySender::default();
                    let thread_root = None;

                    TimelineItemContent::MsgLike(MsgLikeContent {
                        kind: MsgLikeKind::Message(Message::from_event(
                            c,
                            extract_room_msg_edit_content(event.relations()),
                            RemoveReplyFallback::Yes,
                        )),
                        reactions,
                        thread_root,
                        in_reply_to: None,
                    })
                }

                AnyMessageLikeEventContent::Sticker(content) => {
                    // Assume we're not interested in reactions or thread info in this context.
                    // (See above an explanation as to why that's the case.)
                    let reactions = ReactionsByKeyBySender::default();
                    let thread_root = None;

                    TimelineItemContent::MsgLike(MsgLikeContent {
                        kind: MsgLikeKind::Sticker(Sticker { content }),
                        reactions,
                        thread_root,
                        in_reply_to: None,
                    })
                }

                AnyMessageLikeEventContent::RoomEncrypted(content) => {
                    let utd_cause = match &timeline_event.kind {
                        TimelineEventKind::UnableToDecrypt { utd_info, .. } => UtdCause::determine(
                            timeline_event.raw(),
                            room_data_provider.crypto_context_info().await,
                            utd_info,
                        ),
                        _ => UtdCause::Unknown,
                    };

                    TimelineItemContent::MsgLike(MsgLikeContent::unable_to_decrypt(
                        EncryptedMessage::from_content(content, utd_cause),
                    ))
                }

                AnyMessageLikeEventContent::UnstablePollStart(
                    UnstablePollStartEventContent::New(content),
                ) => {
                    // Assume we're not interested in reactions or thread info in this context.
                    // (See above an explanation as to why that's the case.)
                    let reactions = ReactionsByKeyBySender::default();
                    let thread_root = None;

                    // TODO: could we provide the bundled edit here?
                    let poll_state = PollState::new(content, None);
                    TimelineItemContent::MsgLike(MsgLikeContent {
                        kind: MsgLikeKind::Poll(poll_state),
                        reactions,
                        thread_root,
                        in_reply_to: None,
                    })
                }

                _ => {
                    warn!("unsupported event type");
                    return Err(TimelineError::UnsupportedEvent);
                }
            },

            None => TimelineItemContent::MsgLike(MsgLikeContent::redacted()),
        };

        let sender = event.sender().to_owned();
        let sender_profile = TimelineDetails::from_initial_value(
            room_data_provider.profile_from_user_id(&sender).await,
        );

        Ok(Self { content, sender, sender_profile })
    }
}
