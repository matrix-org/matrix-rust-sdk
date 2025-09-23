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

//! Facilities to edit existing events.

use ruma::{
    EventId, UserId,
    events::{
        AnyMessageLikeEventContent, AnySyncMessageLikeEvent, AnySyncTimelineEvent, Mentions,
        SyncMessageLikeEvent,
        poll::unstable_start::{
            ReplacementUnstablePollStartEventContent, UnstablePollStartContentBlock,
            UnstablePollStartEventContent,
        },
        room::message::{
            FormattedBody, MessageType, ReplacementMetadata, RoomMessageEventContent,
            RoomMessageEventContentWithoutRelation,
        },
    },
};
use thiserror::Error;
use tracing::instrument;

use super::EventSource;
use crate::Room;

/// The new content that will replace the previous event's content.
pub enum EditedContent {
    /// The content is a `m.room.message`.
    RoomMessage(RoomMessageEventContentWithoutRelation),

    /// Tweak a caption for a `m.room.message` that's a media.
    MediaCaption {
        /// New caption for the media.
        ///
        /// Set to `None` to remove an existing caption.
        caption: Option<String>,

        /// New formatted caption for the media.
        ///
        /// Set to `None` to remove an existing formatted caption.
        formatted_caption: Option<FormattedBody>,

        /// New set of intentional mentions to be included in the edited
        /// caption.
        mentions: Option<Mentions>,
    },

    /// The content is a new poll start.
    PollStart {
        /// New fallback text for the poll.
        fallback_text: String,
        /// New start block for the poll.
        new_content: UnstablePollStartContentBlock,
    },
}

#[cfg(not(tarpaulin_include))]
impl std::fmt::Debug for EditedContent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::RoomMessage(_) => f.debug_tuple("RoomMessage").finish(),
            Self::MediaCaption { .. } => f.debug_tuple("MediaCaption").finish(),
            Self::PollStart { .. } => f.debug_tuple("PollStart").finish(),
        }
    }
}

/// An error occurring while editing an event.
#[derive(Debug, Error)]
pub enum EditError {
    /// We tried to edit a state event, which is not allowed, per spec.
    #[error("State events can't be edited")]
    StateEvent,

    /// We tried to edit an event which sender isn't the current user, which is
    /// forbidden, per spec.
    #[error("You're not the author of the event you'd like to edit.")]
    NotAuthor,

    /// We couldn't fetch the remote event with /room/event.
    #[error("Couldn't fetch the remote event: {0}")]
    Fetch(Box<crate::Error>),

    /// We couldn't properly deserialize the target event.
    #[error(transparent)]
    Deserialize(#[from] serde_json::Error),

    /// We tried to edit an event of type A with content of type B.
    #[error(
        "The original event type ({target}) isn't the same as \
         the parameter's new content type ({new_content})"
    )]
    IncompatibleEditType {
        /// The type of the target event.
        target: String,
        /// The type of the new content.
        new_content: &'static str,
    },
}

impl Room {
    /// Create a new edit event for the target event id with the new content.
    ///
    /// The event can then be sent with [`Room::send`] or a
    /// [`crate::send_queue::RoomSendQueue`].
    #[instrument(skip(self, new_content), fields(room = %self.room_id()))]
    pub async fn make_edit_event(
        &self,
        event_id: &EventId,
        new_content: EditedContent,
    ) -> Result<AnyMessageLikeEventContent, EditError> {
        make_edit_event(self, self.own_user_id(), event_id, new_content).await
    }
}

async fn make_edit_event<S: EventSource>(
    source: S,
    own_user_id: &UserId,
    event_id: &EventId,
    new_content: EditedContent,
) -> Result<AnyMessageLikeEventContent, EditError> {
    let target = source.get_event(event_id).await.map_err(|err| EditError::Fetch(Box::new(err)))?;

    let event = target.raw().deserialize().map_err(EditError::Deserialize)?;

    // The event must be message-like.
    let AnySyncTimelineEvent::MessageLike(message_like_event) = event else {
        return Err(EditError::StateEvent);
    };

    // The event must have been sent by the current user.
    if message_like_event.sender() != own_user_id {
        return Err(EditError::NotAuthor);
    }

    match new_content {
        EditedContent::RoomMessage(new_content) => {
            // Handle edits of m.room.message.
            let AnySyncMessageLikeEvent::RoomMessage(SyncMessageLikeEvent::Original(original)) =
                message_like_event
            else {
                return Err(EditError::IncompatibleEditType {
                    target: message_like_event.event_type().to_string(),
                    new_content: "room message",
                });
            };

            let mentions = original.content.mentions;

            let replacement = new_content
                .make_replacement(ReplacementMetadata::new(event_id.to_owned(), mentions));

            Ok(replacement.into())
        }

        EditedContent::MediaCaption { caption, formatted_caption, mentions } => {
            // Handle edits of m.room.message.
            let AnySyncMessageLikeEvent::RoomMessage(SyncMessageLikeEvent::Original(original)) =
                message_like_event
            else {
                return Err(EditError::IncompatibleEditType {
                    target: message_like_event.event_type().to_string(),
                    new_content: "caption for a media room message",
                });
            };

            let original_mentions = original.content.mentions.clone();

            let mut prev_content = original.content;

            if !update_media_caption(&mut prev_content, caption, formatted_caption, mentions) {
                return Err(EditError::IncompatibleEditType {
                    target: prev_content.msgtype.msgtype().to_owned(),
                    new_content: "caption for a media room message",
                });
            }

            let replacement = prev_content
                .make_replacement(ReplacementMetadata::new(event_id.to_owned(), original_mentions));

            Ok(replacement.into())
        }

        EditedContent::PollStart { fallback_text, new_content } => {
            if !matches!(
                message_like_event,
                AnySyncMessageLikeEvent::UnstablePollStart(SyncMessageLikeEvent::Original(_))
            ) {
                return Err(EditError::IncompatibleEditType {
                    target: message_like_event.event_type().to_string(),
                    new_content: "poll start",
                });
            }

            let replacement = UnstablePollStartEventContent::Replacement(
                ReplacementUnstablePollStartEventContent::plain_text(
                    fallback_text,
                    new_content,
                    event_id.to_owned(),
                ),
            );

            Ok(replacement.into())
        }
    }
}

/// Sets the caption of a media event content.
///
/// Why a macro over a plain function: the event content types all differ from
/// each other, and it would require adding a trait and implementing it for all
/// event types instead of having this simple macro.
macro_rules! set_caption {
    ($event:expr, $caption:expr) => {
        let filename = $event.filename().to_owned();
        // As a reminder:
        // - body and no filename set means the body is the filename
        // - body and filename set means the body is the caption, and filename is the
        //   filename.
        if let Some(caption) = $caption {
            $event.filename = Some(filename);
            $event.body = caption;
        } else {
            $event.filename = None;
            $event.body = filename;
        }
    };
}

/// Sets the caption of a [`RoomMessageEventContent`].
///
/// Returns true if the event represented a media event (and thus the captions
/// could be updated), false otherwise.
pub(crate) fn update_media_caption(
    content: &mut RoomMessageEventContent,
    caption: Option<String>,
    formatted_caption: Option<FormattedBody>,
    mentions: Option<Mentions>,
) -> bool {
    content.mentions = mentions;

    match &mut content.msgtype {
        MessageType::Audio(event) => {
            set_caption!(event, caption);
            event.formatted = formatted_caption;
            true
        }
        MessageType::File(event) => {
            set_caption!(event, caption);
            event.formatted = formatted_caption;
            true
        }
        #[cfg(feature = "unstable-msc4274")]
        MessageType::Gallery(event) => {
            event.body = caption.unwrap_or_default();
            event.formatted = formatted_caption;
            true
        }
        MessageType::Image(event) => {
            set_caption!(event, caption);
            event.formatted = formatted_caption;
            true
        }
        MessageType::Video(event) => {
            set_caption!(event, caption);
            event.formatted = formatted_caption;
            true
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use assert_matches2::{assert_let, assert_matches};
    use matrix_sdk_base::deserialized_responses::TimelineEvent;
    use matrix_sdk_test::{async_test, event_factory::EventFactory};
    use ruma::{
        EventId, OwnedEventId, event_id,
        events::{
            AnyMessageLikeEventContent, AnySyncTimelineEvent, Mentions,
            room::message::{MessageType, Relation, RoomMessageEventContentWithoutRelation},
        },
        owned_mxc_uri, owned_user_id, user_id,
    };

    use super::{EditError, EventSource, make_edit_event};
    use crate::{Error, room::edit::EditedContent};

    #[derive(Default)]
    struct TestEventCache {
        events: BTreeMap<OwnedEventId, TimelineEvent>,
    }

    impl EventSource for TestEventCache {
        async fn get_event(&self, event_id: &EventId) -> Result<TimelineEvent, Error> {
            Ok(self.events.get(event_id).unwrap().clone())
        }
    }

    #[async_test]
    async fn test_edit_state_event() {
        let event_id = event_id!("$1");
        let own_user_id = user_id!("@me:saucisse.bzh");

        let mut cache = TestEventCache::default();
        let f = EventFactory::new();
        cache.events.insert(
            event_id.to_owned(),
            f.room_name("The room name").event_id(event_id).sender(own_user_id).into(),
        );

        let new_content = RoomMessageEventContentWithoutRelation::text_plain("the edit");

        assert_matches!(
            make_edit_event(cache, own_user_id, event_id, EditedContent::RoomMessage(new_content),)
                .await,
            Err(EditError::StateEvent)
        );
    }

    #[async_test]
    async fn test_edit_event_other_user() {
        let event_id = event_id!("$1");
        let f = EventFactory::new();

        let mut cache = TestEventCache::default();

        cache.events.insert(
            event_id.to_owned(),
            f.text_msg("hi").event_id(event_id).sender(user_id!("@other:saucisse.bzh")).into(),
        );

        let own_user_id = user_id!("@me:saucisse.bzh");
        let new_content = RoomMessageEventContentWithoutRelation::text_plain("the edit");

        assert_matches!(
            make_edit_event(cache, own_user_id, event_id, EditedContent::RoomMessage(new_content),)
                .await,
            Err(EditError::NotAuthor)
        );
    }

    #[async_test]
    async fn test_make_edit_event_success() {
        let event_id = event_id!("$1");
        let own_user_id = user_id!("@me:saucisse.bzh");

        let mut cache = TestEventCache::default();
        let f = EventFactory::new();
        cache.events.insert(
            event_id.to_owned(),
            f.text_msg("hi").event_id(event_id).sender(own_user_id).into(),
        );

        let new_content = RoomMessageEventContentWithoutRelation::text_plain("the edit");

        let edit_event =
            make_edit_event(cache, own_user_id, event_id, EditedContent::RoomMessage(new_content))
                .await
                .unwrap();

        assert_let!(AnyMessageLikeEventContent::RoomMessage(msg) = &edit_event);
        // This is the fallback text, for clients not supporting edits.
        assert_eq!(msg.body(), "* the edit");
        assert_let!(Some(Relation::Replacement(repl)) = &msg.relates_to);

        assert_eq!(repl.event_id, event_id);
        assert_eq!(repl.new_content.msgtype.body(), "the edit");
    }

    #[async_test]
    async fn test_make_edit_caption_for_non_media_room_message() {
        let event_id = event_id!("$1");
        let own_user_id = user_id!("@me:saucisse.bzh");

        let mut cache = TestEventCache::default();
        let f = EventFactory::new();
        cache.events.insert(
            event_id.to_owned(),
            f.text_msg("hello world").event_id(event_id).sender(own_user_id).into(),
        );

        let err = make_edit_event(
            cache,
            own_user_id,
            event_id,
            EditedContent::MediaCaption {
                caption: Some("yo".to_owned()),
                formatted_caption: None,
                mentions: None,
            },
        )
        .await
        .unwrap_err();

        assert_let!(EditError::IncompatibleEditType { target, new_content } = err);
        assert_eq!(target, "m.text");
        assert_eq!(new_content, "caption for a media room message");
    }

    #[async_test]
    async fn test_add_caption_for_media() {
        let event_id = event_id!("$1");
        let own_user_id = user_id!("@me:saucisse.bzh");

        let filename = "rickroll.gif";

        let mut cache = TestEventCache::default();
        let f = EventFactory::new();
        cache.events.insert(
            event_id.to_owned(),
            f.image(filename.to_owned(), owned_mxc_uri!("mxc://sdk.rs/rickroll"))
                .event_id(event_id)
                .sender(own_user_id)
                .into(),
        );

        let edit_event = make_edit_event(
            cache,
            own_user_id,
            event_id,
            EditedContent::MediaCaption {
                caption: Some("Best joke ever".to_owned()),
                formatted_caption: None,
                mentions: None,
            },
        )
        .await
        .unwrap();

        assert_let!(AnyMessageLikeEventContent::RoomMessage(msg) = edit_event);
        assert_let!(MessageType::Image(image) = msg.msgtype);

        assert_eq!(image.filename(), filename);
        assert_eq!(image.caption(), Some("* Best joke ever")); // Fallback for a replacement 🤷
        assert!(image.formatted_caption().is_none());

        assert_let!(Some(Relation::Replacement(repl)) = msg.relates_to);
        assert_let!(MessageType::Image(new_image) = repl.new_content.msgtype);
        assert_eq!(new_image.filename(), filename);
        assert_eq!(new_image.caption(), Some("Best joke ever"));
        assert!(new_image.formatted_caption().is_none());
    }

    #[async_test]
    async fn test_remove_caption_for_media() {
        let event_id = event_id!("$1");
        let own_user_id = user_id!("@me:saucisse.bzh");

        let filename = "rickroll.gif";

        let mut cache = TestEventCache::default();
        let f = EventFactory::new();

        let event = f
            .image(filename.to_owned(), owned_mxc_uri!("mxc://sdk.rs/rickroll"))
            .caption(Some("caption".to_owned()), None)
            .event_id(event_id)
            .sender(own_user_id)
            .into_event();

        {
            // Sanity checks.
            let event = event.raw().deserialize().unwrap();
            assert_let!(AnySyncTimelineEvent::MessageLike(event) = event);
            assert_let!(
                AnyMessageLikeEventContent::RoomMessage(msg) = event.original_content().unwrap()
            );
            assert_let!(MessageType::Image(image) = msg.msgtype);
            assert_eq!(image.filename(), filename);
            assert_eq!(image.caption(), Some("caption"));
            assert!(image.formatted_caption().is_none());
        }

        cache.events.insert(event_id.to_owned(), event);

        let edit_event = make_edit_event(
            cache,
            own_user_id,
            event_id,
            // Remove the caption by setting it to None.
            EditedContent::MediaCaption { caption: None, formatted_caption: None, mentions: None },
        )
        .await
        .unwrap();

        assert_let!(AnyMessageLikeEventContent::RoomMessage(msg) = edit_event);
        assert_let!(MessageType::Image(image) = msg.msgtype);

        assert_eq!(image.filename(), "* rickroll.gif"); // Fallback for a replacement 🤷
        assert!(image.caption().is_none());
        assert!(image.formatted_caption().is_none());

        assert_let!(Some(Relation::Replacement(repl)) = msg.relates_to);
        assert_let!(MessageType::Image(new_image) = repl.new_content.msgtype);
        assert_eq!(new_image.filename(), "rickroll.gif");
        assert!(new_image.caption().is_none());
        assert!(new_image.formatted_caption().is_none());
    }

    #[async_test]
    async fn test_add_media_caption_mention() {
        let event_id = event_id!("$1");
        let own_user_id = user_id!("@me:saucisse.bzh");

        let filename = "rickroll.gif";

        let mut cache = TestEventCache::default();
        let f = EventFactory::new();

        // Start with a media event that has no mentions.
        let event = f
            .image(filename.to_owned(), owned_mxc_uri!("mxc://sdk.rs/rickroll"))
            .event_id(event_id)
            .sender(own_user_id)
            .into_event();

        {
            // Sanity checks.
            let event = event.raw().deserialize().unwrap();
            assert_let!(AnySyncTimelineEvent::MessageLike(event) = event);
            assert_let!(
                AnyMessageLikeEventContent::RoomMessage(msg) = event.original_content().unwrap()
            );
            assert_matches!(msg.mentions, None);
        }

        cache.events.insert(event_id.to_owned(), event);

        // Add an intentional mention in the caption.
        let mentioned_user_id = owned_user_id!("@crepe:saucisse.bzh");
        let edit_event = {
            let mentions = Mentions::with_user_ids([mentioned_user_id.clone()]);
            make_edit_event(
                cache,
                own_user_id,
                event_id,
                EditedContent::MediaCaption {
                    caption: None,
                    formatted_caption: None,
                    mentions: Some(mentions),
                },
            )
            .await
            .unwrap()
        };

        assert_let!(AnyMessageLikeEventContent::RoomMessage(msg) = edit_event);
        assert_let!(MessageType::Image(image) = msg.msgtype);

        assert!(image.caption().is_none());
        assert!(image.formatted_caption().is_none());

        // The raw event contains the mention.
        assert_let!(Some(mentions) = msg.mentions);
        assert!(!mentions.room);
        assert_eq!(
            mentions.user_ids.into_iter().collect::<Vec<_>>(),
            vec![mentioned_user_id.clone()]
        );

        assert_let!(Some(Relation::Replacement(repl)) = msg.relates_to);
        assert_let!(MessageType::Image(new_image) = repl.new_content.msgtype);
        assert!(new_image.caption().is_none());
        assert!(new_image.formatted_caption().is_none());

        // The replacement contains the mention.
        assert_let!(Some(mentions) = repl.new_content.mentions);
        assert!(!mentions.room);
        assert_eq!(mentions.user_ids.into_iter().collect::<Vec<_>>(), vec![mentioned_user_id]);
    }

    #[async_test]
    async fn test_make_edit_event_success_with_response() {
        let event_id = event_id!("$1");
        let resp_event_id = event_id!("$resp");
        let own_user_id = user_id!("@me:saucisse.bzh");

        let mut cache = TestEventCache::default();
        let f = EventFactory::new();

        cache.events.insert(
            event_id.to_owned(),
            f.text_msg("hi").event_id(event_id).sender(user_id!("@steb:saucisse.bzh")).into(),
        );

        cache.events.insert(
            resp_event_id.to_owned(),
            f.text_msg("you're the hi")
                .event_id(resp_event_id)
                .sender(own_user_id)
                .reply_to(event_id)
                .into(),
        );

        let new_content = RoomMessageEventContentWithoutRelation::text_plain("uh i mean hi too");

        let edit_event = make_edit_event(
            cache,
            own_user_id,
            resp_event_id,
            EditedContent::RoomMessage(new_content),
        )
        .await
        .unwrap();

        assert_let!(AnyMessageLikeEventContent::RoomMessage(msg) = &edit_event);
        // This is the fallback text, for clients not supporting edits.
        assert_eq!(msg.body(), "* uh i mean hi too");
        assert_let!(Some(Relation::Replacement(repl)) = &msg.relates_to);

        assert_eq!(repl.event_id, resp_event_id);
        assert_eq!(repl.new_content.msgtype.body(), "uh i mean hi too");
    }
}
