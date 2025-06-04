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

//! Facilities to reply to existing events.

use ruma::{
    events::{
        relation::Thread,
        room::{
            encrypted::Relation as EncryptedRelation,
            message::{
                AddMentions, ForwardThread, OriginalRoomMessageEvent, Relation, ReplyWithinThread,
                RoomMessageEventContent, RoomMessageEventContentWithoutRelation,
            },
        },
        AnySyncMessageLikeEvent, AnySyncTimelineEvent, SyncMessageLikeEvent,
    },
    serde::Raw,
    EventId, MilliSecondsSinceUnixEpoch, OwnedEventId, OwnedUserId, RoomId, UserId,
};
use serde::Deserialize;
use thiserror::Error;
use tracing::{error, instrument};

use super::{EventSource, Room};

/// Information needed to reply to an event.
#[derive(Debug)]
pub struct Reply {
    /// The event ID of the event to reply to.
    pub event_id: OwnedEventId,
    /// Whether to enforce a thread relation.
    pub enforce_thread: EnforceThread,
}

/// Content information needed to reply to an event.
#[derive(Debug, Clone)]
struct RepliedToInfo {
    /// The event ID of the event to reply to.
    event_id: OwnedEventId,
    /// The sender of the event to reply to.
    sender: OwnedUserId,
    /// The timestamp of the event to reply to.
    timestamp: MilliSecondsSinceUnixEpoch,
    /// The content of the event to reply to.
    content: ReplyContent,
}

/// The content of a reply.
#[derive(Debug, Clone)]
enum ReplyContent {
    /// Content of a message event.
    Message(Box<RoomMessageEventContent>),
    /// Content of any other kind of event stored as raw JSON.
    Raw(Raw<AnySyncTimelineEvent>),
}

/// Errors specific to unsupported replies.
#[derive(Debug, Error)]
pub enum ReplyError {
    /// We couldn't fetch the remote event with /room/event.
    #[error("Couldn't fetch the remote event: {0}")]
    Fetch(Box<crate::Error>),
    /// The event to reply to could not be deserialized.
    #[error("failed to deserialize event to reply to")]
    Deserialization,
    /// State events cannot be replied to.
    #[error("tried to reply to a state event")]
    StateEvent,
}

/// Whether or not to enforce a [`Relation::Thread`] when sending a reply.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EnforceThread {
    /// A thread relation is enforced. If the original message does not have a
    /// thread relation itself, a new thread is started.
    Threaded(ReplyWithinThread),

    /// A thread relation is not enforced. If the original message has a thread
    /// relation, it is forwarded.
    MaybeThreaded,

    /// A thread relation is not enforced. If the original message has a thread
    /// relation, it is *not* forwarded.
    Unthreaded,
}

impl Room {
    /// Create a new reply event for the target event id with the specified
    /// content.
    ///
    /// The event can then be sent with [`Room::send`] or a
    /// [`crate::send_queue::RoomSendQueue`].
    ///
    /// # Arguments
    ///
    /// * `content` - The content to reply with
    /// * `event_id` - ID of the event to reply to
    /// * `enforce_thread` - Whether to enforce a thread relation
    #[instrument(skip(self, content), fields(room = %self.room_id()))]
    pub async fn make_reply_event(
        &self,
        content: RoomMessageEventContentWithoutRelation,
        reply: Reply,
    ) -> Result<RoomMessageEventContent, ReplyError> {
        make_reply_event(self, self.room_id(), self.own_user_id(), content, reply).await
    }
}

async fn make_reply_event<S: EventSource>(
    source: S,
    room_id: &RoomId,
    own_user_id: &UserId,
    content: RoomMessageEventContentWithoutRelation,
    reply: Reply,
) -> Result<RoomMessageEventContent, ReplyError> {
    let replied_to_info = replied_to_info_from_event_id(source, &reply.event_id).await?;

    // [The specification](https://spec.matrix.org/v1.10/client-server-api/#user-and-room-mentions) says:
    //
    // > Users should not add their own Matrix ID to the `m.mentions` property as
    // > outgoing messages cannot self-notify.
    //
    // If the replied to event has been written by the current user, let's toggle to
    // `AddMentions::No`.
    let mention_the_sender =
        if own_user_id == replied_to_info.sender { AddMentions::No } else { AddMentions::Yes };

    let content = match replied_to_info.content {
        ReplyContent::Message(replied_to_content) => {
            let event = OriginalRoomMessageEvent {
                event_id: replied_to_info.event_id,
                sender: replied_to_info.sender,
                origin_server_ts: replied_to_info.timestamp,
                room_id: room_id.to_owned(),
                content: *replied_to_content,
                unsigned: Default::default(),
            };

            match reply.enforce_thread {
                EnforceThread::Threaded(is_reply) => {
                    content.make_for_thread(&event, is_reply, mention_the_sender)
                }
                EnforceThread::MaybeThreaded => {
                    content.make_reply_to(&event, ForwardThread::Yes, mention_the_sender)
                }
                EnforceThread::Unthreaded => {
                    content.make_reply_to(&event, ForwardThread::No, mention_the_sender)
                }
            }
        }

        ReplyContent::Raw(raw_event) => {
            match reply.enforce_thread {
                EnforceThread::Threaded(is_reply) => {
                    // Some of the code below technically belongs into ruma. However,
                    // reply fallbacks have been removed in Matrix 1.13 which means
                    // both match arms can use the successor of make_for_thread in
                    // the next ruma release.
                    #[derive(Deserialize)]
                    struct ContentDeHelper {
                        #[serde(rename = "m.relates_to")]
                        relates_to: Option<EncryptedRelation>,
                    }

                    let previous_content =
                        raw_event.get_field::<ContentDeHelper>("content").ok().flatten();

                    let mut content = if is_reply == ReplyWithinThread::Yes {
                        content.make_reply_to_raw(
                            &raw_event,
                            replied_to_info.event_id.to_owned(),
                            room_id,
                            ForwardThread::No,
                            mention_the_sender,
                        )
                    } else {
                        content.into()
                    };

                    let thread_root = if let Some(EncryptedRelation::Thread(thread)) =
                        previous_content.as_ref().and_then(|c| c.relates_to.as_ref())
                    {
                        thread.event_id.to_owned()
                    } else {
                        replied_to_info.event_id.to_owned()
                    };

                    let thread = if is_reply == ReplyWithinThread::Yes {
                        Thread::reply(thread_root, replied_to_info.event_id)
                    } else {
                        Thread::plain(thread_root, replied_to_info.event_id)
                    };

                    content.relates_to = Some(Relation::Thread(thread));
                    content
                }

                EnforceThread::MaybeThreaded => content.make_reply_to_raw(
                    &raw_event,
                    replied_to_info.event_id,
                    room_id,
                    ForwardThread::Yes,
                    mention_the_sender,
                ),

                EnforceThread::Unthreaded => content.make_reply_to_raw(
                    &raw_event,
                    replied_to_info.event_id,
                    room_id,
                    ForwardThread::No,
                    mention_the_sender,
                ),
            }
        }
    };

    Ok(content)
}

async fn replied_to_info_from_event_id<S: EventSource>(
    source: S,
    event_id: &EventId,
) -> Result<RepliedToInfo, ReplyError> {
    let event = source.get_event(event_id).await.map_err(|err| ReplyError::Fetch(Box::new(err)))?;

    let raw_event = event.into_raw();
    let event = raw_event.deserialize().map_err(|_| ReplyError::Deserialization)?;

    let reply_content = match &event {
        AnySyncTimelineEvent::MessageLike(event) => {
            if let AnySyncMessageLikeEvent::RoomMessage(SyncMessageLikeEvent::Original(
                original_event,
            )) = event
            {
                ReplyContent::Message(Box::new(original_event.content.clone()))
            } else {
                ReplyContent::Raw(raw_event)
            }
        }
        AnySyncTimelineEvent::State(_) => return Err(ReplyError::StateEvent),
    };

    Ok(RepliedToInfo {
        event_id: event_id.to_owned(),
        sender: event.sender().to_owned(),
        timestamp: event.origin_server_ts(),
        content: reply_content,
    })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use assert_matches2::{assert_let, assert_matches};
    use matrix_sdk_base::deserialized_responses::TimelineEvent;
    use matrix_sdk_test::{async_test, event_factory::EventFactory};
    use ruma::{
        event_id,
        events::{
            room::message::{Relation, ReplyWithinThread, RoomMessageEventContentWithoutRelation},
            AnySyncTimelineEvent,
        },
        room_id,
        serde::Raw,
        user_id, EventId, OwnedEventId,
    };
    use serde_json::json;

    use super::{make_reply_event, EnforceThread, EventSource, Reply, ReplyError};
    use crate::{event_cache::EventCacheError, Error};

    #[derive(Default)]
    struct TestEventCache {
        events: BTreeMap<OwnedEventId, TimelineEvent>,
    }

    impl EventSource for TestEventCache {
        async fn get_event(&self, event_id: &EventId) -> Result<TimelineEvent, Error> {
            self.events
                .get(event_id)
                .cloned()
                .ok_or(Error::EventCache(Box::new(EventCacheError::ClientDropped)))
        }
    }

    #[async_test]
    async fn test_cannot_reply_to_unknown_event() {
        let event_id = event_id!("$1");
        let own_user_id = user_id!("@me:saucisse.bzh");

        let mut cache = TestEventCache::default();
        let f = EventFactory::new();
        cache.events.insert(
            event_id.to_owned(),
            f.text_msg("hi").event_id(event_id).sender(own_user_id).into(),
        );

        let room_id = room_id!("!galette:saucisse.bzh");
        let content = RoomMessageEventContentWithoutRelation::text_plain("the reply");

        assert_matches!(
            make_reply_event(
                cache,
                room_id,
                own_user_id,
                content,
                Reply {
                    event_id: event_id!("$2").into(),
                    enforce_thread: EnforceThread::Unthreaded
                },
            )
            .await,
            Err(ReplyError::Fetch(_))
        );
    }

    #[async_test]
    async fn test_cannot_reply_to_invalid_event() {
        let event_id = event_id!("$1");
        let own_user_id = user_id!("@me:saucisse.bzh");

        let mut cache = TestEventCache::default();

        cache.events.insert(
            event_id.to_owned(),
            TimelineEvent::from_plaintext(
                Raw::<AnySyncTimelineEvent>::from_json_string(
                    json!({
                        "content": {
                            "body": "hi"
                        },
                        "event_id": event_id,
                        "origin_server_ts": 1,
                        "type": "m.room.message",
                        // Invalid because sender is missing
                    })
                    .to_string(),
                )
                .unwrap(),
            ),
        );

        let room_id = room_id!("!galette:saucisse.bzh");
        let content = RoomMessageEventContentWithoutRelation::text_plain("the reply");

        assert_matches!(
            make_reply_event(
                cache,
                room_id,
                own_user_id,
                content,
                Reply { event_id: event_id.into(), enforce_thread: EnforceThread::Unthreaded },
            )
            .await,
            Err(ReplyError::Deserialization)
        );
    }

    #[async_test]
    async fn test_cannot_reply_to_state_event() {
        let event_id = event_id!("$1");
        let own_user_id = user_id!("@me:saucisse.bzh");

        let mut cache = TestEventCache::default();
        let f = EventFactory::new();
        cache.events.insert(
            event_id.to_owned(),
            f.room_name("lobby").event_id(event_id).sender(own_user_id).into(),
        );

        let room_id = room_id!("!galette:saucisse.bzh");
        let content = RoomMessageEventContentWithoutRelation::text_plain("the reply");

        assert_matches!(
            make_reply_event(
                cache,
                room_id,
                own_user_id,
                content,
                Reply { event_id: event_id.into(), enforce_thread: EnforceThread::Unthreaded },
            )
            .await,
            Err(ReplyError::StateEvent)
        );
    }

    #[async_test]
    async fn test_reply_unthreaded() {
        let event_id = event_id!("$1");
        let own_user_id = user_id!("@me:saucisse.bzh");

        let mut cache = TestEventCache::default();
        let f = EventFactory::new();
        cache.events.insert(
            event_id.to_owned(),
            f.text_msg("hi").event_id(event_id).sender(own_user_id).into(),
        );

        let room_id = room_id!("!galette:saucisse.bzh");
        let content = RoomMessageEventContentWithoutRelation::text_plain("the reply");

        let reply_event = make_reply_event(
            cache,
            room_id,
            own_user_id,
            content,
            Reply { event_id: event_id.into(), enforce_thread: EnforceThread::Unthreaded },
        )
        .await
        .unwrap();

        assert_let!(Some(Relation::Reply { in_reply_to }) = &reply_event.relates_to);

        assert_eq!(in_reply_to.event_id, event_id);
    }

    #[async_test]
    async fn test_start_thread() {
        let event_id = event_id!("$1");
        let own_user_id = user_id!("@me:saucisse.bzh");

        let mut cache = TestEventCache::default();
        let f = EventFactory::new();
        cache.events.insert(
            event_id.to_owned(),
            f.text_msg("hi").event_id(event_id).sender(own_user_id).into(),
        );

        let room_id = room_id!("!galette:saucisse.bzh");
        let content = RoomMessageEventContentWithoutRelation::text_plain("the reply");

        let reply_event = make_reply_event(
            cache,
            room_id,
            own_user_id,
            content,
            Reply {
                event_id: event_id.into(),
                enforce_thread: EnforceThread::Threaded(ReplyWithinThread::No),
            },
        )
        .await
        .unwrap();

        assert_let!(Some(Relation::Thread(thread)) = &reply_event.relates_to);

        assert_eq!(thread.event_id, event_id);
        assert_eq!(thread.in_reply_to.as_ref().unwrap().event_id, event_id);
        assert!(thread.is_falling_back);
    }

    #[async_test]
    async fn test_reply_on_thread() {
        let thread_root = event_id!("$1");
        let event_id = event_id!("$2");
        let own_user_id = user_id!("@me:saucisse.bzh");

        let mut cache = TestEventCache::default();
        let f = EventFactory::new();
        cache.events.insert(
            thread_root.to_owned(),
            f.text_msg("hi").event_id(thread_root).sender(own_user_id).into(),
        );
        cache.events.insert(
            event_id.to_owned(),
            f.text_msg("ho")
                .in_thread(thread_root, thread_root)
                .event_id(event_id)
                .sender(own_user_id)
                .into(),
        );

        let room_id = room_id!("!galette:saucisse.bzh");
        let content = RoomMessageEventContentWithoutRelation::text_plain("the reply");

        let reply_event = make_reply_event(
            cache,
            room_id,
            own_user_id,
            content,
            Reply {
                event_id: event_id.into(),
                enforce_thread: EnforceThread::Threaded(ReplyWithinThread::No),
            },
        )
        .await
        .unwrap();

        assert_let!(Some(Relation::Thread(thread)) = &reply_event.relates_to);

        assert_eq!(thread.event_id, thread_root);
        assert_eq!(thread.in_reply_to.as_ref().unwrap().event_id, event_id);
        assert!(thread.is_falling_back);
    }

    #[async_test]
    async fn test_reply_on_thread_as_reply() {
        let thread_root = event_id!("$1");
        let event_id = event_id!("$2");
        let own_user_id = user_id!("@me:saucisse.bzh");

        let mut cache = TestEventCache::default();
        let f = EventFactory::new();
        cache.events.insert(
            thread_root.to_owned(),
            f.text_msg("hi").event_id(thread_root).sender(own_user_id).into(),
        );
        cache.events.insert(
            event_id.to_owned(),
            f.text_msg("ho")
                .in_thread(thread_root, thread_root)
                .event_id(event_id)
                .sender(own_user_id)
                .into(),
        );

        let room_id = room_id!("!galette:saucisse.bzh");
        let content = RoomMessageEventContentWithoutRelation::text_plain("the reply");

        let reply_event = make_reply_event(
            cache,
            room_id,
            own_user_id,
            content,
            Reply {
                event_id: event_id.into(),
                enforce_thread: EnforceThread::Threaded(ReplyWithinThread::Yes),
            },
        )
        .await
        .unwrap();

        assert_let!(Some(Relation::Thread(thread)) = &reply_event.relates_to);

        assert_eq!(thread.event_id, thread_root);
        assert_eq!(thread.in_reply_to.as_ref().unwrap().event_id, event_id);
        assert!(!thread.is_falling_back);
    }

    #[async_test]
    async fn test_reply_forwarding_thread() {
        let thread_root = event_id!("$1");
        let event_id = event_id!("$2");
        let own_user_id = user_id!("@me:saucisse.bzh");

        let mut cache = TestEventCache::default();
        let f = EventFactory::new();
        cache.events.insert(
            thread_root.to_owned(),
            f.text_msg("hi").event_id(thread_root).sender(own_user_id).into(),
        );
        cache.events.insert(
            event_id.to_owned(),
            f.text_msg("ho")
                .in_thread(thread_root, thread_root)
                .event_id(event_id)
                .sender(own_user_id)
                .into(),
        );

        let room_id = room_id!("!galette:saucisse.bzh");
        let content = RoomMessageEventContentWithoutRelation::text_plain("the reply");

        let reply_event = make_reply_event(
            cache,
            room_id,
            own_user_id,
            content,
            Reply { event_id: event_id.into(), enforce_thread: EnforceThread::MaybeThreaded },
        )
        .await
        .unwrap();

        assert_let!(Some(Relation::Thread(thread)) = &reply_event.relates_to);

        assert_eq!(thread.event_id, thread_root);
        assert_eq!(thread.in_reply_to.as_ref().unwrap().event_id, event_id);
        assert!(thread.is_falling_back);
    }
}
