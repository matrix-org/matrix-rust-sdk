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
    fully_read::FullyReadEvent,
    ignored_user_list::IgnoredUserListEvent,
    presence::PresenceEvent,
    push_rules::PushRulesEvent,
    receipt::ReceiptEvent,
    room::{
        aliases::AliasesEvent,
        avatar::AvatarEvent,
        canonical_alias::CanonicalAliasEvent,
        join_rules::JoinRulesEvent,
        member::{MemberEvent, MemberEventContent},
        message::{feedback::FeedbackEvent, MessageEvent},
        name::NameEvent,
        power_levels::PowerLevelsEvent,
        redaction::RedactionEvent,
        tombstone::TombstoneEvent,
    },
    stripped::{
        StrippedRoomAliases, StrippedRoomAvatar, StrippedRoomCanonicalAlias, StrippedRoomJoinRules,
        StrippedRoomMember, StrippedRoomName, StrippedRoomPowerLevels,
    },
    typing::TypingEvent,
    CustomEvent, CustomRoomEvent, CustomStateEvent,
};
use crate::{Room, RoomState};
use matrix_sdk_common_macros::async_trait;

/// Type alias for `RoomState` enum when passed to `EventEmitter` methods.
pub type SyncRoom = RoomState<Arc<RwLock<Room>>>;

/// This represents the various "unrecognized" events.
#[derive(Clone, Copy, Debug)]
pub enum CustomOrRawEvent<'c> {
    /// When an event can not be deserialized by ruma.
    ///
    /// This will be mostly obsolete when ruma-events is updated.
    RawJson(&'c RawJsonValue),
    /// A custom event.
    Custom(&'c CustomEvent),
    /// A custom room event.
    CustomRoom(&'c CustomRoomEvent),
    /// A custom state event.
    CustomState(&'c CustomStateEvent),
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
/// #         room::message::{MessageEvent, MessageEventContent, TextMessageEventContent},
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
///     async fn on_room_message(&self, room: SyncRoom, event: &MessageEvent) {
///         if let SyncRoom::Joined(room) = room {
///             if let MessageEvent {
///                 content: MessageEventContent::Text(TextMessageEventContent { body: msg_body, .. }),
///                 sender,
///                 ..
///             } = event
///             {
///                 let name = {
///                    let room = room.read().await;
///                    let member = room.members.get(&sender).unwrap();
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
    async fn on_room_member(&self, _: SyncRoom, _: &MemberEvent) {}
    /// Fires when `Client` receives a `RoomEvent::RoomName` event.
    async fn on_room_name(&self, _: SyncRoom, _: &NameEvent) {}
    /// Fires when `Client` receives a `RoomEvent::RoomCanonicalAlias` event.
    async fn on_room_canonical_alias(&self, _: SyncRoom, _: &CanonicalAliasEvent) {}
    /// Fires when `Client` receives a `RoomEvent::RoomAliases` event.
    async fn on_room_aliases(&self, _: SyncRoom, _: &AliasesEvent) {}
    /// Fires when `Client` receives a `RoomEvent::RoomAvatar` event.
    async fn on_room_avatar(&self, _: SyncRoom, _: &AvatarEvent) {}
    /// Fires when `Client` receives a `RoomEvent::RoomMessage` event.
    async fn on_room_message(&self, _: SyncRoom, _: &MessageEvent) {}
    /// Fires when `Client` receives a `RoomEvent::RoomMessageFeedback` event.
    async fn on_room_message_feedback(&self, _: SyncRoom, _: &FeedbackEvent) {}
    /// Fires when `Client` receives a `RoomEvent::RoomRedaction` event.
    async fn on_room_redaction(&self, _: SyncRoom, _: &RedactionEvent) {}
    /// Fires when `Client` receives a `RoomEvent::RoomPowerLevels` event.
    async fn on_room_power_levels(&self, _: SyncRoom, _: &PowerLevelsEvent) {}
    /// Fires when `Client` receives a `RoomEvent::Tombstone` event.
    async fn on_room_tombstone(&self, _: SyncRoom, _: &TombstoneEvent) {}

    // `RoomEvent`s from `IncomingState`
    /// Fires when `Client` receives a `StateEvent::RoomMember` event.
    async fn on_state_member(&self, _: SyncRoom, _: &MemberEvent) {}
    /// Fires when `Client` receives a `StateEvent::RoomName` event.
    async fn on_state_name(&self, _: SyncRoom, _: &NameEvent) {}
    /// Fires when `Client` receives a `StateEvent::RoomCanonicalAlias` event.
    async fn on_state_canonical_alias(&self, _: SyncRoom, _: &CanonicalAliasEvent) {}
    /// Fires when `Client` receives a `StateEvent::RoomAliases` event.
    async fn on_state_aliases(&self, _: SyncRoom, _: &AliasesEvent) {}
    /// Fires when `Client` receives a `StateEvent::RoomAvatar` event.
    async fn on_state_avatar(&self, _: SyncRoom, _: &AvatarEvent) {}
    /// Fires when `Client` receives a `StateEvent::RoomPowerLevels` event.
    async fn on_state_power_levels(&self, _: SyncRoom, _: &PowerLevelsEvent) {}
    /// Fires when `Client` receives a `StateEvent::RoomJoinRules` event.
    async fn on_state_join_rules(&self, _: SyncRoom, _: &JoinRulesEvent) {}

    // `AnyStrippedStateEvent`s
    /// Fires when `Client` receives a `AnyStrippedStateEvent::StrippedRoomMember` event.
    async fn on_stripped_state_member(
        &self,
        _: SyncRoom,
        _: &StrippedRoomMember,
        _: Option<MemberEventContent>,
    ) {
    }
    /// Fires when `Client` receives a `AnyStrippedStateEvent::StrippedRoomName` event.
    async fn on_stripped_state_name(&self, _: SyncRoom, _: &StrippedRoomName) {}
    /// Fires when `Client` receives a `AnyStrippedStateEvent::StrippedRoomCanonicalAlias` event.
    async fn on_stripped_state_canonical_alias(&self, _: SyncRoom, _: &StrippedRoomCanonicalAlias) {
    }
    /// Fires when `Client` receives a `AnyStrippedStateEvent::StrippedRoomAliases` event.
    async fn on_stripped_state_aliases(&self, _: SyncRoom, _: &StrippedRoomAliases) {}
    /// Fires when `Client` receives a `AnyStrippedStateEvent::StrippedRoomAvatar` event.
    async fn on_stripped_state_avatar(&self, _: SyncRoom, _: &StrippedRoomAvatar) {}
    /// Fires when `Client` receives a `AnyStrippedStateEvent::StrippedRoomPowerLevels` event.
    async fn on_stripped_state_power_levels(&self, _: SyncRoom, _: &StrippedRoomPowerLevels) {}
    /// Fires when `Client` receives a `AnyStrippedStateEvent::StrippedRoomJoinRules` event.
    async fn on_stripped_state_join_rules(&self, _: SyncRoom, _: &StrippedRoomJoinRules) {}

    // `NonRoomEvent` (this is a type alias from ruma_events)
    /// Fires when `Client` receives a `NonRoomEvent::RoomPresence` event.
    async fn on_non_room_presence(&self, _: SyncRoom, _: &PresenceEvent) {}
    /// Fires when `Client` receives a `NonRoomEvent::RoomName` event.
    async fn on_non_room_ignored_users(&self, _: SyncRoom, _: &IgnoredUserListEvent) {}
    /// Fires when `Client` receives a `NonRoomEvent::RoomCanonicalAlias` event.
    async fn on_non_room_push_rules(&self, _: SyncRoom, _: &PushRulesEvent) {}
    /// Fires when `Client` receives a `NonRoomEvent::RoomAliases` event.
    async fn on_non_room_fully_read(&self, _: SyncRoom, _: &FullyReadEvent) {}
    /// Fires when `Client` receives a `NonRoomEvent::Typing` event.
    async fn on_non_room_typing(&self, _: SyncRoom, _: &TypingEvent) {}
    /// Fires when `Client` receives a `NonRoomEvent::Receipt` event.
    ///
    /// This is always a read receipt.
    async fn on_non_room_receipt(&self, _: SyncRoom, _: &ReceiptEvent) {}

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
        async fn on_room_member(&self, _: SyncRoom, _: &MemberEvent) {
            self.0.lock().await.push("member".to_string())
        }
        async fn on_room_name(&self, _: SyncRoom, _: &NameEvent) {
            self.0.lock().await.push("name".to_string())
        }
        async fn on_room_canonical_alias(&self, _: SyncRoom, _: &CanonicalAliasEvent) {
            self.0.lock().await.push("canonical".to_string())
        }
        async fn on_room_aliases(&self, _: SyncRoom, _: &AliasesEvent) {
            self.0.lock().await.push("aliases".to_string())
        }
        async fn on_room_avatar(&self, _: SyncRoom, _: &AvatarEvent) {
            self.0.lock().await.push("avatar".to_string())
        }
        async fn on_room_message(&self, _: SyncRoom, _: &MessageEvent) {
            self.0.lock().await.push("message".to_string())
        }
        async fn on_room_message_feedback(&self, _: SyncRoom, _: &FeedbackEvent) {
            self.0.lock().await.push("feedback".to_string())
        }
        async fn on_room_redaction(&self, _: SyncRoom, _: &RedactionEvent) {
            self.0.lock().await.push("redaction".to_string())
        }
        async fn on_room_power_levels(&self, _: SyncRoom, _: &PowerLevelsEvent) {
            self.0.lock().await.push("power".to_string())
        }
        async fn on_room_tombstone(&self, _: SyncRoom, _: &TombstoneEvent) {
            self.0.lock().await.push("tombstone".to_string())
        }

        async fn on_state_member(&self, _: SyncRoom, _: &MemberEvent) {
            self.0.lock().await.push("state member".to_string())
        }
        async fn on_state_name(&self, _: SyncRoom, _: &NameEvent) {
            self.0.lock().await.push("state name".to_string())
        }
        async fn on_state_canonical_alias(&self, _: SyncRoom, _: &CanonicalAliasEvent) {
            self.0.lock().await.push("state canonical".to_string())
        }
        async fn on_state_aliases(&self, _: SyncRoom, _: &AliasesEvent) {
            self.0.lock().await.push("state aliases".to_string())
        }
        async fn on_state_avatar(&self, _: SyncRoom, _: &AvatarEvent) {
            self.0.lock().await.push("state avatar".to_string())
        }
        async fn on_state_power_levels(&self, _: SyncRoom, _: &PowerLevelsEvent) {
            self.0.lock().await.push("state power".to_string())
        }
        async fn on_state_join_rules(&self, _: SyncRoom, _: &JoinRulesEvent) {
            self.0.lock().await.push("state rules".to_string())
        }

        async fn on_stripped_state_member(
            &self,
            _: SyncRoom,
            _: &StrippedRoomMember,
            _: Option<MemberEventContent>,
        ) {
            self.0
                .lock()
                .await
                .push("stripped state member".to_string())
        }
        async fn on_stripped_state_name(&self, _: SyncRoom, _: &StrippedRoomName) {
            self.0.lock().await.push("stripped state name".to_string())
        }
        async fn on_stripped_state_canonical_alias(
            &self,
            _: SyncRoom,
            _: &StrippedRoomCanonicalAlias,
        ) {
            self.0
                .lock()
                .await
                .push("stripped state canonical".to_string())
        }
        async fn on_stripped_state_aliases(&self, _: SyncRoom, _: &StrippedRoomAliases) {
            self.0
                .lock()
                .await
                .push("stripped state aliases".to_string())
        }
        async fn on_stripped_state_avatar(&self, _: SyncRoom, _: &StrippedRoomAvatar) {
            self.0
                .lock()
                .await
                .push("stripped state avatar".to_string())
        }
        async fn on_stripped_state_power_levels(&self, _: SyncRoom, _: &StrippedRoomPowerLevels) {
            self.0.lock().await.push("stripped state power".to_string())
        }
        async fn on_stripped_state_join_rules(&self, _: SyncRoom, _: &StrippedRoomJoinRules) {
            self.0.lock().await.push("stripped state rules".to_string())
        }

        async fn on_non_room_presence(&self, _: SyncRoom, _: &PresenceEvent) {
            self.0.lock().await.push("account presence".to_string())
        }
        async fn on_non_room_ignored_users(&self, _: SyncRoom, _: &IgnoredUserListEvent) {
            self.0.lock().await.push("account ignore".to_string())
        }
        async fn on_non_room_push_rules(&self, _: SyncRoom, _: &PushRulesEvent) {
            self.0.lock().await.push("account push rules".to_string())
        }
        async fn on_non_room_fully_read(&self, _: SyncRoom, _: &FullyReadEvent) {
            self.0.lock().await.push("account read".to_string())
        }
        async fn on_non_room_typing(&self, _: SyncRoom, _: &TypingEvent) {
            self.0.lock().await.push("typing event".to_string())
        }
        async fn on_presence_event(&self, _: SyncRoom, _: &PresenceEvent) {
            self.0.lock().await.push("presence event".to_string())
        }
        async fn on_unrecognized_event(&self, _: SyncRoom, _: &CustomOrRawEvent<'_>) {
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
                "account read",
                "account ignore",
                "presence event"
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
                "typing event"
            ],
        )
    }
}
