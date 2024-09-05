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

use std::future::Future;

use matrix_sdk_base::{deserialized_responses::SyncTimelineEvent, SendOutsideWasm};
use ruma::{
    events::{
        poll::unstable_start::{
            ReplacementUnstablePollStartEventContent, UnstablePollStartContentBlock,
            UnstablePollStartEventContent,
        },
        room::message::{Relation, ReplacementMetadata, RoomMessageEventContentWithoutRelation},
        AnyMessageLikeEvent, AnyMessageLikeEventContent, AnySyncMessageLikeEvent,
        AnySyncTimelineEvent, AnyTimelineEvent, MessageLikeEvent, SyncMessageLikeEvent,
    },
    EventId, RoomId, UserId,
};
use thiserror::Error;
use tracing::{debug, instrument, trace, warn};

use crate::Room;

/// The new content that will replace the previous event's content.
pub enum EditedContent {
    /// The content is a `m.room.message`.
    RoomMessage(RoomMessageEventContentWithoutRelation),

    /// The content is a fallback text and an `org.matrix.msc3381.poll.start`.
    PollStart(String, UnstablePollStartContentBlock),
}

#[cfg(not(tarpaulin_include))]
impl std::fmt::Debug for EditedContent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::RoomMessage(_) => f.debug_tuple("RoomMessage").finish(),
            Self::PollStart(_, _) => f.debug_tuple("PollStart").finish(),
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
    #[error("The original event type ({target}) isn't the same as the parameter's new content type ({new_content})")]
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
        make_edit_event(self, self.room_id(), self.own_user_id(), event_id, new_content).await
    }
}

trait EventSource {
    fn get_event(
        &self,
        event_id: &EventId,
    ) -> impl Future<Output = Result<SyncTimelineEvent, EditError>> + SendOutsideWasm;
}

impl<'a> EventSource for &'a Room {
    async fn get_event(&self, event_id: &EventId) -> Result<SyncTimelineEvent, EditError> {
        match self.event_cache().await {
            Ok((event_cache, _drop_handles)) => {
                if let Some(event) = event_cache.event(event_id).await {
                    return Ok(event);
                }
                // Fallthrough: try with /event.
            }

            Err(err) => {
                debug!("error when getting the event cache: {err}");
            }
        }

        trace!("trying with /event now");
        self.event(event_id, None)
            .await
            .map(Into::into)
            .map_err(|err| EditError::Fetch(Box::new(err)))
    }
}

async fn make_edit_event<S: EventSource>(
    source: S,
    room_id: &RoomId,
    own_user_id: &UserId,
    event_id: &EventId,
    new_content: EditedContent,
) -> Result<AnyMessageLikeEventContent, EditError> {
    let target = source.get_event(event_id).await?;

    let event = target.event.deserialize().map_err(EditError::Deserialize)?;

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

            let mentions = original.content.mentions.clone();

            // Do a best effort at finding the replied-to original event.
            let replied_to_sync_timeline_event =
                if let Some(Relation::Reply { in_reply_to }) = original.content.relates_to {
                    source
                        .get_event(&in_reply_to.event_id)
                        .await
                        .map_err(|err| {
                            warn!("couldn't fetch the replied-to event, when editing: {err}");
                            err
                        })
                        .ok()
                } else {
                    None
                };

            let replied_to_original_room_msg = replied_to_sync_timeline_event
                .and_then(|sync_timeline_event| {
                    sync_timeline_event
                        .event
                        .deserialize()
                        .map_err(|err| warn!("unable to deserialize replied-to event: {err}"))
                        .ok()
                })
                .and_then(|event| {
                    if let AnyTimelineEvent::MessageLike(AnyMessageLikeEvent::RoomMessage(
                        MessageLikeEvent::Original(original),
                    )) = event.into_full_event(room_id.to_owned())
                    {
                        Some(original)
                    } else {
                        None
                    }
                });

            Ok(new_content
                .make_replacement(
                    ReplacementMetadata::new(event_id.to_owned(), mentions),
                    replied_to_original_room_msg.as_ref(),
                )
                .into())
        }
        EditedContent::PollStart(text, new_content) => {
            let AnySyncMessageLikeEvent::UnstablePollStart(SyncMessageLikeEvent::Original(_)) =
                message_like_event
            else {
                return Err(EditError::IncompatibleEditType {
                    target: message_like_event.event_type().to_string(),
                    new_content: "poll start",
                });
            };
            let replacement = UnstablePollStartEventContent::Replacement(
                ReplacementUnstablePollStartEventContent::plain_text(
                    text,
                    new_content,
                    event_id.to_owned(),
                ),
            );
            Ok(AnyMessageLikeEventContent::UnstablePollStart(replacement))
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use assert_matches2::{assert_let, assert_matches};
    use matrix_sdk_base::deserialized_responses::SyncTimelineEvent;
    use matrix_sdk_test::async_test;
    use ruma::{
        event_id,
        events::{
            room::message::{Relation, RoomMessageEventContentWithoutRelation},
            AnyMessageLikeEventContent, AnySyncTimelineEvent,
        },
        room_id,
        serde::Raw,
        user_id, EventId, OwnedEventId,
    };
    use serde_json::json;

    use super::{make_edit_event, EditError, EventSource};
    use crate::{room::edit::EditedContent, test_utils::events::EventFactory};

    #[derive(Default)]
    struct TestEventCache {
        events: BTreeMap<OwnedEventId, SyncTimelineEvent>,
    }

    impl EventSource for TestEventCache {
        async fn get_event(&self, event_id: &EventId) -> Result<SyncTimelineEvent, EditError> {
            Ok(self.events.get(event_id).unwrap().clone())
        }
    }

    #[async_test]
    async fn test_edit_state_event() {
        let event_id = event_id!("$1");
        let own_user_id = user_id!("@me:saucisse.bzh");

        let mut cache = TestEventCache::default();

        cache.events.insert(
            event_id.to_owned(),
            // TODO: use the EventFactory for state events too.
            SyncTimelineEvent::new(
                Raw::<AnySyncTimelineEvent>::from_json_string(
                    json!({
                        "content": {
                            "name": "The room name"
                        },
                        "event_id": event_id,
                        "sender": own_user_id,
                        "state_key": "",
                        "origin_server_ts": 1,
                        "type": "m.room.name",
                    })
                    .to_string(),
                )
                .unwrap(),
            ),
        );

        let room_id = room_id!("!galette:saucisse.bzh");
        let new_content = RoomMessageEventContentWithoutRelation::text_plain("the edit");

        assert_matches!(
            make_edit_event(
                cache,
                room_id,
                own_user_id,
                event_id,
                EditedContent::RoomMessage(new_content),
            )
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

        let room_id = room_id!("!galette:saucisse.bzh");
        let own_user_id = user_id!("@me:saucisse.bzh");
        let new_content = RoomMessageEventContentWithoutRelation::text_plain("the edit");

        assert_matches!(
            make_edit_event(
                cache,
                room_id,
                own_user_id,
                event_id,
                EditedContent::RoomMessage(new_content),
            )
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

        let room_id = room_id!("!galette:saucisse.bzh");
        let new_content = RoomMessageEventContentWithoutRelation::text_plain("the edit");

        let edit_event = make_edit_event(
            cache,
            room_id,
            own_user_id,
            event_id,
            EditedContent::RoomMessage(new_content),
        )
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

        let room_id = room_id!("!galette:saucisse.bzh");
        let new_content = RoomMessageEventContentWithoutRelation::text_plain("uh i mean hi too");

        let edit_event = make_edit_event(
            cache,
            room_id,
            own_user_id,
            resp_event_id,
            EditedContent::RoomMessage(new_content),
        )
        .await
        .unwrap();

        assert_let!(AnyMessageLikeEventContent::RoomMessage(msg) = &edit_event);
        // This is the fallback text, for clients not supporting edits.
        assert_eq!(
            msg.body(),
            r#"> <@steb:saucisse.bzh> hi

* uh i mean hi too"#
        );
        assert_let!(Some(Relation::Replacement(repl)) = &msg.relates_to);

        assert_eq!(repl.event_id, resp_event_id);
        assert_eq!(repl.new_content.msgtype.body(), "uh i mean hi too");
    }
}
