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

use as_variant::as_variant;
use ruma::{
    OwnedEventId, UserId,
    events::{
        AnySyncTimelineEvent,
        room::{
            encrypted::Relation as EncryptedRelation,
            message::{
                AddMentions, ForwardThread, ReplyMetadata, ReplyWithinThread,
                RoomMessageEventContent, RoomMessageEventContentWithoutRelation,
            },
        },
    },
};
use thiserror::Error;
use tracing::instrument;

use super::{EventSource, Room};

/// Information needed to reply to an event.
#[derive(Debug)]
pub struct Reply {
    /// The event ID of the event to reply to.
    pub event_id: OwnedEventId,
    /// Whether to enforce a thread relation.
    pub enforce_thread: EnforceThread,
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
///
/// [`Relation::Thread`]: ruma::events::room::message::Relation::Thread
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
        make_reply_event(self, self.own_user_id(), content, reply).await
    }
}

async fn make_reply_event<S: EventSource>(
    source: S,
    own_user_id: &UserId,
    content: RoomMessageEventContentWithoutRelation,
    reply: Reply,
) -> Result<RoomMessageEventContent, ReplyError> {
    let event =
        source.get_event(&reply.event_id).await.map_err(|err| ReplyError::Fetch(Box::new(err)))?;

    let raw_event = event.into_raw();
    let event = raw_event.deserialize().map_err(|_| ReplyError::Deserialization)?;

    let relation = as_variant!(&event, AnySyncTimelineEvent::MessageLike)
        .ok_or(ReplyError::StateEvent)?
        .original_content()
        .and_then(|content| content.relation());
    let thread =
        relation.as_ref().and_then(|relation| as_variant!(relation, EncryptedRelation::Thread));

    let reply_metadata = ReplyMetadata::new(event.event_id(), event.sender(), thread);

    // [The specification](https://spec.matrix.org/v1.10/client-server-api/#user-and-room-mentions) says:
    //
    // > Users should not add their own Matrix ID to the `m.mentions` property as
    // > outgoing messages cannot self-notify.
    //
    // If the replied to event has been written by the current user, let's toggle to
    // `AddMentions::No`.
    let mention_the_sender =
        if own_user_id == event.sender() { AddMentions::No } else { AddMentions::Yes };

    let content = match reply.enforce_thread {
        EnforceThread::Threaded(is_reply) => {
            content.make_for_thread(reply_metadata, is_reply, mention_the_sender)
        }
        EnforceThread::MaybeThreaded => {
            content.make_reply_to(reply_metadata, ForwardThread::Yes, mention_the_sender)
        }
        EnforceThread::Unthreaded => {
            content.make_reply_to(reply_metadata, ForwardThread::No, mention_the_sender)
        }
    };

    Ok(content)
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
            AnySyncTimelineEvent,
            room::message::{Relation, ReplyWithinThread, RoomMessageEventContentWithoutRelation},
        },
        serde::Raw,
        user_id,
    };
    use serde_json::json;

    use super::{EnforceThread, EventSource, Reply, ReplyError, make_reply_event};
    use crate::{Error, event_cache::EventCacheError};

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

        let content = RoomMessageEventContentWithoutRelation::text_plain("the reply");

        assert_matches!(
            make_reply_event(
                cache,
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

        let content = RoomMessageEventContentWithoutRelation::text_plain("the reply");

        assert_matches!(
            make_reply_event(
                cache,
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

        let content = RoomMessageEventContentWithoutRelation::text_plain("the reply");

        assert_matches!(
            make_reply_event(
                cache,
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

        let content = RoomMessageEventContentWithoutRelation::text_plain("the reply");

        let reply_event = make_reply_event(
            cache,
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

        let content = RoomMessageEventContentWithoutRelation::text_plain("the reply");

        let reply_event = make_reply_event(
            cache,
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

        let content = RoomMessageEventContentWithoutRelation::text_plain("the reply");

        let reply_event = make_reply_event(
            cache,
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

        let content = RoomMessageEventContentWithoutRelation::text_plain("the reply");

        let reply_event = make_reply_event(
            cache,
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

        let content = RoomMessageEventContentWithoutRelation::text_plain("the reply");

        let reply_event = make_reply_event(
            cache,
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

    #[async_test]
    async fn test_reply_forwarding_thread_for_poll_start() {
        let thread_root = event_id!("$thread_root");
        let event_id = event_id!("$thread_reply");
        let own_user_id = user_id!("@me:saucisse.bzh");

        let mut cache = TestEventCache::default();
        let f = EventFactory::new();

        cache.events.insert(
            event_id.to_owned(),
            f.poll_start(
                "would you rather… A) eat a pineapple pizza, B) drink pickle juice",
                "would you rather…",
                vec!["eat a pineapple pizza", "drink pickle juice"],
            )
            .in_thread(thread_root, thread_root)
            .event_id(event_id)
            .sender(own_user_id)
            .into(),
        );

        let content = RoomMessageEventContentWithoutRelation::text_plain("the reply");

        let reply_event = make_reply_event(
            cache,
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
}
