// Copyright 2026 The Matrix.org Foundation C.I.C.
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
// See the License for that specific language governing permissions and
// limitations under the License.

use std::sync::Arc;

use matrix_sdk::deserialized_responses::EncryptionInfo;
use matrix_sdk_common::{SendOutsideWasm, SyncOutsideWasm};
use ruma::{
    events::{AnySyncMessageLikeEvent, MessageLikeEventType},
    serde::Raw,
};
use serde::Deserialize;
use serde_json::value::RawValue as RawJsonValue;
use tokio::sync::mpsc;
use tracing::warn;

use super::Room;
use crate::{TaskHandle, encryption::EventEncryptionInfo, runtime::get_runtime_handle};

/// A message-like room event, as exposed over FFI.
#[derive(uniffi::Record)]
pub struct RoomMessageLikeEvent {
    /// The event type, e.g. `m.reaction`.
    pub event_type: MessageLikeEventType,
    /// The event id.
    pub event_id: String,
    /// The event sender.
    pub sender: String,
    /// When the event was sent, in milliseconds since the Unix epoch.
    pub timestamp: u64,
    /// The `content` of the event, as a JSON string, decrypted if the event was
    /// encrypted.
    pub content_json: String,
    /// The encryption info of the event, if it was encrypted.
    pub encryption_info: Option<EventEncryptionInfo>,
}

impl RoomMessageLikeEvent {
    fn from_raw(
        raw: &Raw<AnySyncMessageLikeEvent>,
        encryption_info: Option<&EncryptionInfo>,
    ) -> Result<Self, serde_json::Error> {
        /// The subset of a message-like event that is exposed over FFI.
        #[derive(Deserialize)]
        struct RoomMessageLikeEventHelper<'a> {
            #[serde(rename = "type")]
            event_type: MessageLikeEventType,
            event_id: String,
            sender: String,
            #[serde(borrow)]
            content: &'a RawJsonValue,
            origin_server_ts: u64,
        }

        let helper: RoomMessageLikeEventHelper<'_> = serde_json::from_str(raw.json().get())?;

        Ok(Self {
            event_type: helper.event_type,
            event_id: helper.event_id,
            sender: helper.sender,
            timestamp: helper.origin_server_ts,
            content_json: helper.content.get().to_owned(),
            encryption_info: encryption_info.map(Into::into),
        })
    }
}

/// A listener for the message-like events of a single type, registered with
/// [`Room::subscribe_to_message_like_events`].
#[matrix_sdk_ffi_macros::export(callback_interface)]
pub trait RoomMessageLikeEventsListener: SyncOutsideWasm + SendOutsideWasm {
    fn on_event(&self, event: RoomMessageLikeEvent);
}

#[matrix_sdk_ffi_macros::export]
impl Room {
    /// Subscribe to the message-like events of the given type in this room.
    ///
    /// The listener is called with every event of that type that a sync brings
    /// into the room's timeline from now on, decrypted if it was encrypted.
    ///
    /// This is built on the SDK's sync event handlers, so events come from sync
    /// only (not from pagination).
    /// An event that couldn't be decrypted during sync will not be reported
    /// even if it gets decrypted later.
    ///
    /// Use the returned [`TaskHandle`] to cancel the subscription.
    ///
    /// # Arguments
    ///
    /// - `event_type` - The type of the events to listen to. Build one from its
    ///   string representation with
    ///   `messageLikeEventTypeFromString("org.example.type")`.
    pub fn subscribe_to_message_like_events(
        self: Arc<Self>,
        event_type: MessageLikeEventType,
        listener: Box<dyn RoomMessageLikeEventsListener>,
    ) -> Arc<TaskHandle> {
        let (sender, mut receiver) = mpsc::unbounded_channel();

        // Handlers are keyed by a compile-time event type, and this one is only
        // known at runtime, so take every message-like event and compare here.
        let handle = self.inner.add_event_handler(
            move |raw: Raw<AnySyncMessageLikeEvent>, encryption_info: Option<EncryptionInfo>| {
                let matches_type = raw
                    .get_field::<MessageLikeEventType>("type")
                    .ok()
                    .flatten()
                    .is_some_and(|t| t == event_type);

                if matches_type {
                    // Ignore the result: it only fails once the task below has
                    // ended, which drops the guard and unregisters this
                    // handler.
                    let _ = sender.send((raw, encryption_info));
                }

                async {}
            },
        );
        let drop_guard = self.inner.client().event_handler_drop_guard(handle);

        // Call the listener from a task rather than from the handler, so that
        // a slow foreign callback never holds up sync processing.
        Arc::new(TaskHandle::new(get_runtime_handle().spawn(async move {
            // Keep the handler registered for as long as the task runs.
            // Cancelling the `TaskHandle` drops the guard, which unregisters
            // it.
            let _drop_guard = drop_guard;

            while let Some((raw, encryption_info)) = receiver.recv().await {
                match RoomMessageLikeEvent::from_raw(&raw, encryption_info.as_ref()) {
                    Ok(event) => listener.on_event(event),
                    Err(error) => warn!("Skipping malformed message-like event: {error}"),
                }
            }
        })))
    }
}

#[cfg(all(test, not(target_family = "wasm")))]
mod tests {
    use std::time::Duration;

    use matrix_sdk::{
        ruma::{
            event_id,
            events::{StaticEventContent, macros::EventContent},
            room_id, user_id,
        },
        test_utils::mocks::MatrixMockServer,
    };
    use matrix_sdk_test::{JoinedRoomBuilder, event_factory::EventFactory};
    use serde::Serialize;
    use serde_json::json;

    use super::*;

    /// A custom event type, which has no variant of its own in
    /// `MessageLikeEventType`.
    #[derive(Clone, Debug, Serialize, EventContent)]
    #[ruma_event(type = "io.element.call.reaction", kind = MessageLike)]
    struct CallReactionEventContent {
        emoji: String,
        name: String,
    }

    impl CallReactionEventContent {
        fn new(emoji: &str, name: &str) -> Self {
            Self { emoji: emoji.to_owned(), name: name.to_owned() }
        }
    }

    fn call_reaction_type() -> MessageLikeEventType {
        CallReactionEventContent::TYPE.into()
    }

    /// Forwards every event it is called with to a channel.
    struct Collector(mpsc::UnboundedSender<RoomMessageLikeEvent>);

    impl Collector {
        /// A listener to subscribe with, and the receiving end of its channel.
        fn new() -> (Box<Self>, mpsc::UnboundedReceiver<RoomMessageLikeEvent>) {
            let (sender, receiver) = mpsc::unbounded_channel();
            (Box::new(Self(sender)), receiver)
        }
    }

    impl RoomMessageLikeEventsListener for Collector {
        fn on_event(&self, event: RoomMessageLikeEvent) {
            let _ = self.0.send(event);
        }
    }

    async fn next_event(
        receiver: &mut mpsc::UnboundedReceiver<RoomMessageLikeEvent>,
    ) -> RoomMessageLikeEvent {
        tokio::time::timeout(Duration::from_secs(5), receiver.recv())
            .await
            .expect("no event within 5 seconds")
            .expect("the listener was dropped")
    }

    // The tests below end with an event the subscription does report, and check
    // that it comes next: events are delivered in order, so anything wrongly
    // reported would come before it. Waiting to see that nothing arrives would
    // be slower, and flaky.

    #[tokio::test]
    async fn test_subscribe_to_message_like_events() {
        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;

        let room_id = room_id!("!test:example.org");
        let room = Arc::new(Room::new(server.sync_joined_room(&client, room_id).await, None));

        let (listener, mut reactions) = Collector::new();
        let _reactions_handle =
            room.clone().subscribe_to_message_like_events(MessageLikeEventType::Reaction, listener);
        let (listener, mut call_reactions) = Collector::new();
        let _call_reactions_handle =
            room.clone().subscribe_to_message_like_events(call_reaction_type(), listener);

        let alice = user_id!("@alice:localhost");
        let member_event_id = event_id!("$member");
        let f = EventFactory::new().room(room_id).sender(alice);

        server
            .sync_room(
                &client,
                JoinedRoomBuilder::new(room_id)
                    // Alice raises her hand…
                    .add_timeline_event(
                        f.reaction(member_event_id, "🖐️").event_id(event_id!("$raise_hand")),
                    )
                    // …events of other types are not reported…
                    .add_timeline_event(f.text_msg("hello"))
                    .add_timeline_event(f.custom_state_event())
                    // …and she sends a call reaction.
                    .add_timeline_event(
                        f.event(CallReactionEventContent::new("🎉", "party"))
                            .event_id(event_id!("$party"))
                            .server_ts(1),
                    )
                    .add_timeline_event(
                        f.reaction(member_event_id, "👍").event_id(event_id!("$last")),
                    ),
            )
            .await;

        let event = next_event(&mut reactions).await;
        assert_eq!(event.event_type, MessageLikeEventType::Reaction);
        assert_eq!(event.event_id, "$raise_hand");
        assert_eq!(event.sender, alice.as_str());
        assert!(event.encryption_info.is_none());
        let content: serde_json::Value = serde_json::from_str(&event.content_json).unwrap();
        assert_eq!(content["m.relates_to"]["key"], "🖐️");
        assert_eq!(next_event(&mut reactions).await.event_id, "$last");

        let event = next_event(&mut call_reactions).await;
        assert_eq!(event.event_type, call_reaction_type());
        assert_eq!(event.event_id, "$party");
        assert_eq!(event.timestamp, 1);
        let content: serde_json::Value = serde_json::from_str(&event.content_json).unwrap();
        assert_eq!(content, json!({ "emoji": "🎉", "name": "party" }));
    }

    #[tokio::test]
    async fn test_subscribe_to_message_like_events_ignores_state_events() {
        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;

        let room_id = room_id!("!test:example.org");
        let room = Arc::new(Room::new(server.sync_joined_room(&client, room_id).await, None));

        let (listener, mut events) = Collector::new();
        let _handle = room.clone().subscribe_to_message_like_events(call_reaction_type(), listener);

        let f = EventFactory::new().room(room_id).sender(user_id!("@alice:localhost"));
        let content = CallReactionEventContent::new("🎉", "party");

        server
            .sync_room(
                &client,
                JoinedRoomBuilder::new(room_id)
                    // A state event with the subscribed type is not a message-like event.
                    .add_timeline_event(
                        f.event(content.clone()).state_key("").event_id(event_id!("$state")),
                    )
                    .add_timeline_event(f.event(content).event_id(event_id!("$message"))),
            )
            .await;

        assert_eq!(next_event(&mut events).await.event_id, "$message");
    }

    #[tokio::test]
    async fn test_subscribe_to_message_like_events_only_reports_later_events() {
        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;

        let room_id = room_id!("!test:example.org");
        let room = Arc::new(Room::new(server.sync_joined_room(&client, room_id).await, None));

        let f = EventFactory::new().room(room_id).sender(user_id!("@alice:localhost"));
        let target = event_id!("$target");

        // A reaction received before subscribing is not replayed.
        server
            .sync_room(
                &client,
                JoinedRoomBuilder::new(room_id)
                    .add_timeline_event(f.reaction(target, "👍").event_id(event_id!("$old"))),
            )
            .await;

        let (listener, mut events) = Collector::new();
        let _handle =
            room.clone().subscribe_to_message_like_events(MessageLikeEventType::Reaction, listener);

        server
            .sync_room(
                &client,
                JoinedRoomBuilder::new(room_id)
                    .add_timeline_event(f.reaction(target, "👍").event_id(event_id!("$new"))),
            )
            .await;

        assert_eq!(next_event(&mut events).await.event_id, "$new");
    }

    #[tokio::test]
    async fn test_cancelling_a_message_like_events_subscription_drops_the_listener() {
        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;

        let room_id = room_id!("!test:example.org");
        let room = Arc::new(Room::new(server.sync_joined_room(&client, room_id).await, None));

        let (listener, mut events) = Collector::new();
        let handle =
            room.clone().subscribe_to_message_like_events(MessageLikeEventType::Reaction, listener);
        handle.cancel();

        // The task owned the listener and the handler's drop guard, so both go
        // with it.
        let closed = tokio::time::timeout(Duration::from_secs(5), events.recv()).await;
        assert!(matches!(closed, Ok(None)), "the listener outlived the cancelled subscription");
    }
}
