// Copyright 2020 Damir Jelić
// Copyright 2020 The Matrix.org Foundation C.I.C.
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

use matrix_sdk_common::locks::RwLock;
use serde_json::value::RawValue as RawJsonValue;

use crate::events::{
    custom::CustomEventContent,
    fully_read::FullyReadEventContent,
    ignored_user_list::IgnoredUserListEventContent,
    presence::PresenceEvent,
    push_rules::PushRulesEventContent,
    receipt::ReceiptEventContent,
    room::{
        aliases::AliasesEventContent,
        avatar::AvatarEventContent,
        canonical_alias::CanonicalAliasEventContent,
        join_rules::JoinRulesEventContent,
        member::MemberEventContent,
        message::{feedback::FeedbackEventContent, MessageEventContent as MsgEventContent},
        name::NameEventContent,
        power_levels::PowerLevelsEventContent,
        redaction::RedactionEventStub,
        tombstone::TombstoneEventContent,
    },
    typing::TypingEventContent,
    BasicEvent, EphemeralRoomEvent, MessageEventStub, StateEventStub, StrippedStateEventStub,
};
use crate::{Room, RoomState};
use matrix_sdk_common_macros::async_trait;

/// Type alias for `RoomState` enum when passed to `EventEmitter` methods.
pub type SyncRoom = RoomState<Arc<RwLock<Room>>>;

/// This represents the various "unrecognized" events.
#[derive(Clone, Copy, Debug)]
pub enum CustomOrRawEvent<'c> {
    /// When an event can not be deserialized by ruma.
    RawJson(&'c RawJsonValue),
    /// A custom basic event.
    Basic(&'c BasicEvent<CustomEventContent>),
    /// A custom basic event.
    EphemeralRoom(&'c EphemeralRoomEvent<CustomEventContent>),
    /// A custom room event.
    Message(&'c MessageEventStub<CustomEventContent>),
    /// A custom state event.
    State(&'c StateEventStub<CustomEventContent>),
    /// A custom stripped state event.
    StrippedState(&'c StrippedStateEventStub<CustomEventContent>),
}

/// This trait allows any type implementing `EventEmitter` to specify event callbacks for each event.
/// The `Client` calls each method when the corresponding event is received.
///
/// # Examples
/// ```
/// # use std::ops::Deref;
/// # use std::sync::Arc;
/// # use std::{env, process::exit};
/// # use matrix_sdk_base::{
/// #     self,
/// #     events::{
/// #         room::message::{MessageEventContent, TextMessageEventContent},
/// #         MessageEventStub
/// #     },
/// #     EventEmitter, SyncRoom
/// # };
/// # use matrix_sdk_common::locks::RwLock;
/// # use matrix_sdk_common_macros::async_trait;
///
/// struct EventCallback;
///
/// #[async_trait]
/// impl EventEmitter for EventCallback {
///     async fn on_room_message(&self, room: SyncRoom, event: &MessageEventStub<MessageEventContent>) {
///         if let SyncRoom::Joined(room) = room {
///             if let MessageEventStub {
///                 content: MessageEventContent::Text(TextMessageEventContent { body: msg_body, .. }),
///                 sender,
///                 ..
///             } = event
///             {
///                 let name = {
///                    let room = room.read().await;
///                    let member = room.joined_members.get(&sender).unwrap();
///                    member
///                        .display_name
///                        .as_ref()
///                        .map(ToString::to_string)
///                        .unwrap_or(sender.to_string())
///                };
///                 println!("{}: {}", name, msg_body);
///             }
///         }
///     }
/// }
/// ```
#[async_trait]
pub trait EventEmitter: Send + Sync {
    // ROOM EVENTS from `IncomingTimeline`
    /// Fires when `Client` receives a `RoomEvent::RoomMember` event.
    async fn on_room_member(&self, _: SyncRoom, _: &StateEventStub<MemberEventContent>) {}
    /// Fires when `Client` receives a `RoomEvent::RoomName` event.
    async fn on_room_name(&self, _: SyncRoom, _: &StateEventStub<NameEventContent>) {}
    /// Fires when `Client` receives a `RoomEvent::RoomCanonicalAlias` event.
    async fn on_room_canonical_alias(
        &self,
        _: SyncRoom,
        _: &StateEventStub<CanonicalAliasEventContent>,
    ) {
    }
    /// Fires when `Client` receives a `RoomEvent::RoomAliases` event.
    async fn on_room_aliases(&self, _: SyncRoom, _: &StateEventStub<AliasesEventContent>) {}
    /// Fires when `Client` receives a `RoomEvent::RoomAvatar` event.
    async fn on_room_avatar(&self, _: SyncRoom, _: &StateEventStub<AvatarEventContent>) {}
    /// Fires when `Client` receives a `RoomEvent::RoomMessage` event.
    async fn on_room_message(&self, _: SyncRoom, _: &MessageEventStub<MsgEventContent>) {}
    /// Fires when `Client` receives a `RoomEvent::RoomMessageFeedback` event.
    async fn on_room_message_feedback(
        &self,
        _: SyncRoom,
        _: &MessageEventStub<FeedbackEventContent>,
    ) {
    }
    /// Fires when `Client` receives a `RoomEvent::RoomRedaction` event.
    async fn on_room_redaction(&self, _: SyncRoom, _: &RedactionEventStub) {}
    /// Fires when `Client` receives a `RoomEvent::RoomPowerLevels` event.
    async fn on_room_power_levels(&self, _: SyncRoom, _: &StateEventStub<PowerLevelsEventContent>) {
    }
    /// Fires when `Client` receives a `RoomEvent::Tombstone` event.
    async fn on_room_join_rules(&self, _: SyncRoom, _: &StateEventStub<JoinRulesEventContent>) {}
    /// Fires when `Client` receives a `RoomEvent::Tombstone` event.
    async fn on_room_tombstone(&self, _: SyncRoom, _: &StateEventStub<TombstoneEventContent>) {}

    // `RoomEvent`s from `IncomingState`
    /// Fires when `Client` receives a `StateEvent::RoomMember` event.
    async fn on_state_member(&self, _: SyncRoom, _: &StateEventStub<MemberEventContent>) {}
    /// Fires when `Client` receives a `StateEvent::RoomName` event.
    async fn on_state_name(&self, _: SyncRoom, _: &StateEventStub<NameEventContent>) {}
    /// Fires when `Client` receives a `StateEvent::RoomCanonicalAlias` event.
    async fn on_state_canonical_alias(
        &self,
        _: SyncRoom,
        _: &StateEventStub<CanonicalAliasEventContent>,
    ) {
    }
    /// Fires when `Client` receives a `StateEvent::RoomAliases` event.
    async fn on_state_aliases(&self, _: SyncRoom, _: &StateEventStub<AliasesEventContent>) {}
    /// Fires when `Client` receives a `StateEvent::RoomAvatar` event.
    async fn on_state_avatar(&self, _: SyncRoom, _: &StateEventStub<AvatarEventContent>) {}
    /// Fires when `Client` receives a `StateEvent::RoomPowerLevels` event.
    async fn on_state_power_levels(
        &self,
        _: SyncRoom,
        _: &StateEventStub<PowerLevelsEventContent>,
    ) {
    }
    /// Fires when `Client` receives a `StateEvent::RoomJoinRules` event.
    async fn on_state_join_rules(&self, _: SyncRoom, _: &StateEventStub<JoinRulesEventContent>) {}

    // `AnyStrippedStateEvent`s
    /// Fires when `Client` receives a `AnyStrippedStateEvent::StrippedRoomMember` event.
    async fn on_stripped_state_member(
        &self,
        _: SyncRoom,
        _: &StrippedStateEventStub<MemberEventContent>,
        _: Option<MemberEventContent>,
    ) {
    }
    /// Fires when `Client` receives a `AnyStrippedStateEvent::StrippedRoomName` event.
    async fn on_stripped_state_name(
        &self,
        _: SyncRoom,
        _: &StrippedStateEventStub<NameEventContent>,
    ) {
    }
    /// Fires when `Client` receives a `AnyStrippedStateEvent::StrippedRoomCanonicalAlias` event.
    async fn on_stripped_state_canonical_alias(
        &self,
        _: SyncRoom,
        _: &StrippedStateEventStub<CanonicalAliasEventContent>,
    ) {
    }
    /// Fires when `Client` receives a `AnyStrippedStateEvent::StrippedRoomAliases` event.
    async fn on_stripped_state_aliases(
        &self,
        _: SyncRoom,
        _: &StrippedStateEventStub<AliasesEventContent>,
    ) {
    }
    /// Fires when `Client` receives a `AnyStrippedStateEvent::StrippedRoomAvatar` event.
    async fn on_stripped_state_avatar(
        &self,
        _: SyncRoom,
        _: &StrippedStateEventStub<AvatarEventContent>,
    ) {
    }
    /// Fires when `Client` receives a `AnyStrippedStateEvent::StrippedRoomPowerLevels` event.
    async fn on_stripped_state_power_levels(
        &self,
        _: SyncRoom,
        _: &StrippedStateEventStub<PowerLevelsEventContent>,
    ) {
    }
    /// Fires when `Client` receives a `AnyStrippedStateEvent::StrippedRoomJoinRules` event.
    async fn on_stripped_state_join_rules(
        &self,
        _: SyncRoom,
        _: &StrippedStateEventStub<JoinRulesEventContent>,
    ) {
    }

    // `NonRoomEvent` (this is a type alias from ruma_events)
    /// Fires when `Client` receives a `NonRoomEvent::RoomPresence` event.
    async fn on_non_room_presence(&self, _: SyncRoom, _: &PresenceEvent) {}
    /// Fires when `Client` receives a `NonRoomEvent::RoomName` event.
    async fn on_non_room_ignored_users(
        &self,
        _: SyncRoom,
        _: &BasicEvent<IgnoredUserListEventContent>,
    ) {
    }
    /// Fires when `Client` receives a `NonRoomEvent::RoomCanonicalAlias` event.
    async fn on_non_room_push_rules(&self, _: SyncRoom, _: &BasicEvent<PushRulesEventContent>) {}
    /// Fires when `Client` receives a `NonRoomEvent::RoomAliases` event.
    async fn on_non_room_fully_read(
        &self,
        _: SyncRoom,
        _: &EphemeralRoomEvent<FullyReadEventContent>,
    ) {
    }
    /// Fires when `Client` receives a `NonRoomEvent::Typing` event.
    async fn on_non_room_typing(&self, _: SyncRoom, _: &EphemeralRoomEvent<TypingEventContent>) {}
    /// Fires when `Client` receives a `NonRoomEvent::Receipt` event.
    ///
    /// This is always a read receipt.
    async fn on_non_room_receipt(&self, _: SyncRoom, _: &EphemeralRoomEvent<ReceiptEventContent>) {}

    // `PresenceEvent` is a struct so there is only the one method
    /// Fires when `Client` receives a `NonRoomEvent::RoomAliases` event.
    async fn on_presence_event(&self, _: SyncRoom, _: &PresenceEvent) {}

    /// Fires when `Client` receives a `Event::Custom` event or if deserialization fails
    /// because the event was unknown to ruma.
    ///
    /// The only guarantee this method can give about the event is that it is valid JSON.
    async fn on_unrecognized_event(&self, _: SyncRoom, _: &CustomOrRawEvent<'_>) {}
}

#[cfg(test)]
mod test {
    use super::*;
    use matrix_sdk_common::locks::Mutex;
    use matrix_sdk_common_macros::async_trait;
    use matrix_sdk_test::{async_test, sync_response, SyncResponseFile};
    use std::sync::Arc;

    #[cfg(target_arch = "wasm32")]
    pub use wasm_bindgen_test::*;

    #[derive(Clone)]
    pub struct EvEmitterTest(Arc<Mutex<Vec<String>>>);

    #[async_trait]
    impl EventEmitter for EvEmitterTest {
        async fn on_room_member(&self, _: SyncRoom, _: &StateEventStub<MemberEventContent>) {
            self.0.lock().await.push("member".to_string())
        }
        async fn on_room_name(&self, _: SyncRoom, _: &StateEventStub<NameEventContent>) {
            self.0.lock().await.push("name".to_string())
        }
        async fn on_room_canonical_alias(
            &self,
            _: SyncRoom,
            _: &StateEventStub<CanonicalAliasEventContent>,
        ) {
            self.0.lock().await.push("canonical".to_string())
        }
        async fn on_room_aliases(&self, _: SyncRoom, _: &StateEventStub<AliasesEventContent>) {
            self.0.lock().await.push("aliases".to_string())
        }
        async fn on_room_avatar(&self, _: SyncRoom, _: &StateEventStub<AvatarEventContent>) {
            self.0.lock().await.push("avatar".to_string())
        }
        async fn on_room_message(&self, _: SyncRoom, _: &MessageEventStub<MsgEventContent>) {
            self.0.lock().await.push("message".to_string())
        }
        async fn on_room_message_feedback(
            &self,
            _: SyncRoom,
            _: &MessageEventStub<FeedbackEventContent>,
        ) {
            self.0.lock().await.push("feedback".to_string())
        }
        async fn on_room_redaction(&self, _: SyncRoom, _: &RedactionEventStub) {
            self.0.lock().await.push("redaction".to_string())
        }
        async fn on_room_power_levels(
            &self,
            _: SyncRoom,
            _: &StateEventStub<PowerLevelsEventContent>,
        ) {
            self.0.lock().await.push("power".to_string())
        }
        async fn on_room_tombstone(&self, _: SyncRoom, _: &StateEventStub<TombstoneEventContent>) {
            self.0.lock().await.push("tombstone".to_string())
        }

        async fn on_state_member(&self, _: SyncRoom, _: &StateEventStub<MemberEventContent>) {
            self.0.lock().await.push("state member".to_string())
        }
        async fn on_state_name(&self, _: SyncRoom, _: &StateEventStub<NameEventContent>) {
            self.0.lock().await.push("state name".to_string())
        }
        async fn on_state_canonical_alias(
            &self,
            _: SyncRoom,
            _: &StateEventStub<CanonicalAliasEventContent>,
        ) {
            self.0.lock().await.push("state canonical".to_string())
        }
        async fn on_state_aliases(&self, _: SyncRoom, _: &StateEventStub<AliasesEventContent>) {
            self.0.lock().await.push("state aliases".to_string())
        }
        async fn on_state_avatar(&self, _: SyncRoom, _: &StateEventStub<AvatarEventContent>) {
            self.0.lock().await.push("state avatar".to_string())
        }
        async fn on_state_power_levels(
            &self,
            _: SyncRoom,
            _: &StateEventStub<PowerLevelsEventContent>,
        ) {
            self.0.lock().await.push("state power".to_string())
        }
        async fn on_state_join_rules(
            &self,
            _: SyncRoom,
            _: &StateEventStub<JoinRulesEventContent>,
        ) {
            self.0.lock().await.push("state rules".to_string())
        }

        // `AnyStrippedStateEvent`s
        /// Fires when `Client` receives a `AnyStrippedStateEvent::StrippedRoomMember` event.
        async fn on_stripped_state_member(
            &self,
            _: SyncRoom,
            _: &StrippedStateEventStub<MemberEventContent>,
            _: Option<MemberEventContent>,
        ) {
            self.0
                .lock()
                .await
                .push("stripped state member".to_string())
        }
        /// Fires when `Client` receives a `AnyStrippedStateEvent::StrippedRoomName` event.
        async fn on_stripped_state_name(
            &self,
            _: SyncRoom,
            _: &StrippedStateEventStub<NameEventContent>,
        ) {
            self.0.lock().await.push("stripped state name".to_string())
        }
        /// Fires when `Client` receives a `AnyStrippedStateEvent::StrippedRoomCanonicalAlias` event.
        async fn on_stripped_state_canonical_alias(
            &self,
            _: SyncRoom,
            _: &StrippedStateEventStub<CanonicalAliasEventContent>,
        ) {
            self.0
                .lock()
                .await
                .push("stripped state canonical".to_string())
        }
        /// Fires when `Client` receives a `AnyStrippedStateEvent::StrippedRoomAliases` event.
        async fn on_stripped_state_aliases(
            &self,
            _: SyncRoom,
            _: &StrippedStateEventStub<AliasesEventContent>,
        ) {
            self.0
                .lock()
                .await
                .push("stripped state aliases".to_string())
        }
        /// Fires when `Client` receives a `AnyStrippedStateEvent::StrippedRoomAvatar` event.
        async fn on_stripped_state_avatar(
            &self,
            _: SyncRoom,
            _: &StrippedStateEventStub<AvatarEventContent>,
        ) {
            self.0
                .lock()
                .await
                .push("stripped state avatar".to_string())
        }
        /// Fires when `Client` receives a `AnyStrippedStateEvent::StrippedRoomPowerLevels` event.
        async fn on_stripped_state_power_levels(
            &self,
            _: SyncRoom,
            _: &StrippedStateEventStub<PowerLevelsEventContent>,
        ) {
            self.0.lock().await.push("stripped state power".to_string())
        }
        /// Fires when `Client` receives a `AnyStrippedStateEvent::StrippedRoomJoinRules` event.
        async fn on_stripped_state_join_rules(
            &self,
            _: SyncRoom,
            _: &StrippedStateEventStub<JoinRulesEventContent>,
        ) {
            self.0.lock().await.push("stripped state rules".to_string())
        }

        async fn on_non_room_presence(&self, _: SyncRoom, _: &PresenceEvent) {
            self.0.lock().await.push("presence".to_string())
        }
        async fn on_non_room_ignored_users(
            &self,
            _: SyncRoom,
            _: &BasicEvent<IgnoredUserListEventContent>,
        ) {
            self.0.lock().await.push("account ignore".to_string())
        }
        async fn on_non_room_push_rules(&self, _: SyncRoom, _: &BasicEvent<PushRulesEventContent>) {
            self.0.lock().await.push("account push rules".to_string())
        }
        async fn on_non_room_fully_read(
            &self,
            _: SyncRoom,
            _: &EphemeralRoomEvent<FullyReadEventContent>,
        ) {
            self.0.lock().await.push("account read".to_string())
        }
        async fn on_non_room_typing(
            &self,
            _: SyncRoom,
            _: &EphemeralRoomEvent<TypingEventContent>,
        ) {
            self.0.lock().await.push("typing event".to_string())
        }
        async fn on_non_room_receipt(
            &self,
            _: SyncRoom,
            _: &EphemeralRoomEvent<ReceiptEventContent>,
        ) {
            self.0.lock().await.push("receipt event".to_string())
        }
        async fn on_presence_event(&self, _: SyncRoom, _: &PresenceEvent) {
            self.0.lock().await.push("presence event".to_string())
        }
        async fn on_unrecognized_event(&self, _: SyncRoom, event: &CustomOrRawEvent<'_>) {
            println!("{:#?}", event);
            self.0.lock().await.push("unrecognized event".to_string())
        }
    }

    use crate::identifiers::UserId;
    use crate::{BaseClient, Session};

    use std::convert::TryFrom;

    async fn get_client() -> BaseClient {
        let session = Session {
            access_token: "1234".to_owned(),
            user_id: UserId::try_from("@example:example.com").unwrap(),
            device_id: "DEVICEID".to_owned(),
        };
        let client = BaseClient::new().unwrap();
        client.restore_login(session).await.unwrap();
        client
    }

    #[async_test]
    async fn event_emitter_joined() {
        let vec = Arc::new(Mutex::new(Vec::new()));
        let test_vec = Arc::clone(&vec);
        let emitter = Box::new(EvEmitterTest(vec));

        let client = get_client().await;
        client.add_event_emitter(emitter).await;

        let mut response = sync_response(SyncResponseFile::Default);
        client.receive_sync_response(&mut response).await.unwrap();

        let v = test_vec.lock().await;
        assert_eq!(
            v.as_slice(),
            [
                "state rules",
                "state member",
                "state aliases",
                "state power",
                "state canonical",
                "state member",
                "state member",
                "message",
                "account ignore",
                "presence event",
                "receipt event",
                "account read",
            ],
        )
    }

    #[async_test]
    async fn event_emitter_invite() {
        let vec = Arc::new(Mutex::new(Vec::new()));
        let test_vec = Arc::clone(&vec);
        let emitter = Box::new(EvEmitterTest(vec));

        let client = get_client().await;
        client.add_event_emitter(emitter).await;

        let mut response = sync_response(SyncResponseFile::Invite);
        client.receive_sync_response(&mut response).await.unwrap();

        let v = test_vec.lock().await;
        assert_eq!(
            v.as_slice(),
            ["stripped state name", "stripped state member"],
        )
    }

    #[async_test]
    async fn event_emitter_leave() {
        let vec = Arc::new(Mutex::new(Vec::new()));
        let test_vec = Arc::clone(&vec);
        let emitter = Box::new(EvEmitterTest(vec));

        let client = get_client().await;
        client.add_event_emitter(emitter).await;

        let mut response = sync_response(SyncResponseFile::Leave);
        client.receive_sync_response(&mut response).await.unwrap();

        let v = test_vec.lock().await;
        assert_eq!(
            v.as_slice(),
            [
                "state rules",
                "state member",
                "state aliases",
                "state power",
                "state canonical",
                "state member",
                "state member",
                "message"
            ],
        )
    }

    #[async_test]
    async fn event_emitter_more_events() {
        let vec = Arc::new(Mutex::new(Vec::new()));
        let test_vec = Arc::clone(&vec);
        let emitter = Box::new(EvEmitterTest(vec));

        let client = get_client().await;
        client.add_event_emitter(emitter).await;

        let mut response = sync_response(SyncResponseFile::All);
        client.receive_sync_response(&mut response).await.unwrap();

        let v = test_vec.lock().await;
        assert_eq!(
            v.as_slice(),
            [
                "message",
                "unrecognized event",
                "redaction",
                "unrecognized event",
                "unrecognized event",
                "receipt event",
                "typing event"
            ],
        )
    }
}
