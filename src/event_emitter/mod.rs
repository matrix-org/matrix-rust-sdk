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

use crate::events::{
    fully_read::FullyReadEvent,
    ignored_user_list::IgnoredUserListEvent,
    presence::PresenceEvent,
    push_rules::PushRulesEvent,
    room::{
        aliases::AliasesEvent,
        avatar::AvatarEvent,
        canonical_alias::CanonicalAliasEvent,
        join_rules::JoinRulesEvent,
        member::MemberEvent,
        message::{feedback::FeedbackEvent, MessageEvent},
        name::NameEvent,
        power_levels::PowerLevelsEvent,
        redaction::RedactionEvent,
        tombstone::TombstoneEvent,
    },
};
use crate::models::Room;
use tokio::sync::RwLock;

/// This trait allows any type implementing `EventEmitter` to specify event callbacks for each event.
/// The `AsyncClient` calls each method when the corresponding event is received.
///
/// # Examples
/// ```
/// # use std::ops::Deref;
/// # use std::sync::Arc;
/// # use std::{env, process::exit};
/// # use url::Url;
/// use matrix_sdk::{
///     self,
///     events::{
///         room::message::{MessageEvent, MessageEventContent, TextMessageEventContent},
///     },
///     AsyncClient, AsyncClientConfig, EventEmitter, Room, SyncSettings,
/// };
/// use tokio::sync::RwLock;
///
/// struct EventCallback;
///
/// #[async_trait::async_trait]
/// impl EventEmitter for EventCallback {
///     async fn on_room_message(&self, room: Arc<RwLock<Room>>, event: &MessageEvent) {
///         if let MessageEvent {
///             content: MessageEventContent::Text(TextMessageEventContent { body: msg_body, .. }),
///             sender,
///             ..
///         } = event
///         {
///             let name = {
///                let room = room.read().await;
///                let member = room.members.get(&sender).unwrap();
///                member
///                    .display_name
///                    .as_ref()
///                    .map(ToString::to_string)
///                    .unwrap_or(sender.to_string())
///            };
///             println!("{}: {}", name, msg_body);
///         }
///     }
/// }
/// ```
#[async_trait::async_trait]
pub trait EventEmitter: Send + Sync {
    // ROOM EVENTS from `IncomingTimeline`
    /// Fires when `AsyncClient` receives a `RoomEvent::RoomMember` event.
    async fn on_room_member(&self, _: Arc<RwLock<Room>>, _: &MemberEvent) {}
    /// Fires when `AsyncClient` receives a `RoomEvent::RoomName` event.
    async fn on_room_name(&self, _: Arc<RwLock<Room>>, _: &NameEvent) {}
    /// Fires when `AsyncClient` receives a `RoomEvent::RoomCanonicalAlias` event.
    async fn on_room_canonical_alias(&self, _: Arc<RwLock<Room>>, _: &CanonicalAliasEvent) {}
    /// Fires when `AsyncClient` receives a `RoomEvent::RoomAliases` event.
    async fn on_room_aliases(&self, _: Arc<RwLock<Room>>, _: &AliasesEvent) {}
    /// Fires when `AsyncClient` receives a `RoomEvent::RoomAvatar` event.
    async fn on_room_avatar(&self, _: Arc<RwLock<Room>>, _: &AvatarEvent) {}
    /// Fires when `AsyncClient` receives a `RoomEvent::RoomMessage` event.
    async fn on_room_message(&self, _: Arc<RwLock<Room>>, _: &MessageEvent) {}
    /// Fires when `AsyncClient` receives a `RoomEvent::RoomMessageFeedback` event.
    async fn on_room_message_feedback(&self, _: Arc<RwLock<Room>>, _: &FeedbackEvent) {}
    /// Fires when `AsyncClient` receives a `RoomEvent::RoomRedaction` event.
    async fn on_room_redaction(&self, _: Arc<RwLock<Room>>, _: &RedactionEvent) {}
    /// Fires when `AsyncClient` receives a `RoomEvent::RoomPowerLevels` event.
    async fn on_room_power_levels(&self, _: Arc<RwLock<Room>>, _: &PowerLevelsEvent) {}
    /// Fires when `AsyncClient` receives a `RoomEvent::Tombstone` event.
    async fn on_room_tombstone(&self, _: Arc<RwLock<Room>>, _: &TombstoneEvent) {}

    // `RoomEvent`s from `IncomingState`
    /// Fires when `AsyncClient` receives a `StateEvent::RoomMember` event.
    async fn on_state_member(&self, _: Arc<RwLock<Room>>, _: &MemberEvent) {}
    /// Fires when `AsyncClient` receives a `StateEvent::RoomName` event.
    async fn on_state_name(&self, _: Arc<RwLock<Room>>, _: &NameEvent) {}
    /// Fires when `AsyncClient` receives a `StateEvent::RoomCanonicalAlias` event.
    async fn on_state_canonical_alias(&self, _: Arc<RwLock<Room>>, _: &CanonicalAliasEvent) {}
    /// Fires when `AsyncClient` receives a `StateEvent::RoomAliases` event.
    async fn on_state_aliases(&self, _: Arc<RwLock<Room>>, _: &AliasesEvent) {}
    /// Fires when `AsyncClient` receives a `StateEvent::RoomAvatar` event.
    async fn on_state_avatar(&self, _: Arc<RwLock<Room>>, _: &AvatarEvent) {}
    /// Fires when `AsyncClient` receives a `StateEvent::RoomPowerLevels` event.
    async fn on_state_power_levels(&self, _: Arc<RwLock<Room>>, _: &PowerLevelsEvent) {}
    /// Fires when `AsyncClient` receives a `StateEvent::RoomJoinRules` event.
    async fn on_state_join_rules(&self, _: Arc<RwLock<Room>>, _: &JoinRulesEvent) {}

    // `NonRoomEvent` (this is a type alias from ruma_events) from `IncomingAccountData`
    /// Fires when `AsyncClient` receives a `NonRoomEvent::RoomMember` event.
    async fn on_account_presence(&self, _: Arc<RwLock<Room>>, _: &PresenceEvent) {}
    /// Fires when `AsyncClient` receives a `NonRoomEvent::RoomName` event.
    async fn on_account_ignored_users(&self, _: Arc<RwLock<Room>>, _: &IgnoredUserListEvent) {}
    /// Fires when `AsyncClient` receives a `NonRoomEvent::RoomCanonicalAlias` event.
    async fn on_account_push_rules(&self, _: Arc<RwLock<Room>>, _: &PushRulesEvent) {}
    /// Fires when `AsyncClient` receives a `NonRoomEvent::RoomAliases` event.
    async fn on_account_data_fully_read(&self, _: Arc<RwLock<Room>>, _: &FullyReadEvent) {}

    // `PresenceEvent` is a struct so there is only the one method
    /// Fires when `AsyncClient` receives a `NonRoomEvent::RoomAliases` event.
    async fn on_presence_event(&self, _: Arc<RwLock<Room>>, _: &PresenceEvent) {}
}

#[cfg(test)]
mod test {
    use super::*;
    use std::sync::Arc;
    use tokio::sync::Mutex;
    #[derive(Clone)]
    pub struct EvEmitterTest(Arc<Mutex<Vec<String>>>);

    #[async_trait::async_trait]
    impl EventEmitter for EvEmitterTest {
        async fn on_room_member(&self, _: Arc<RwLock<Room>>, _: &MemberEvent) {
            self.0.lock().await.push("member".to_string())
        }
        async fn on_room_name(&self, _: Arc<RwLock<Room>>, _: &NameEvent) {
            self.0.lock().await.push("name".to_string())
        }
        async fn on_room_canonical_alias(&self, _: Arc<RwLock<Room>>, _: &CanonicalAliasEvent) {
            self.0.lock().await.push("canonical".to_string())
        }
        async fn on_room_aliases(&self, _: Arc<RwLock<Room>>, _: &AliasesEvent) {
            self.0.lock().await.push("aliases".to_string())
        }
        async fn on_room_avatar(&self, _: Arc<RwLock<Room>>, _: &AvatarEvent) {
            self.0.lock().await.push("avatar".to_string())
        }
        async fn on_room_message(&self, _: Arc<RwLock<Room>>, _: &MessageEvent) {
            self.0.lock().await.push("message".to_string())
        }
        async fn on_room_message_feedback(&self, _: Arc<RwLock<Room>>, _: &FeedbackEvent) {
            self.0.lock().await.push("feedback".to_string())
        }
        async fn on_room_redaction(&self, _: Arc<RwLock<Room>>, _: &RedactionEvent) {
            self.0.lock().await.push("redaction".to_string())
        }
        async fn on_room_power_levels(&self, _: Arc<RwLock<Room>>, _: &PowerLevelsEvent) {
            self.0.lock().await.push("power".to_string())
        }
        async fn on_room_tombstone(&self, _: Arc<RwLock<Room>>, _: &TombstoneEvent) {
            self.0.lock().await.push("tombstone".to_string())
        }

        async fn on_state_member(&self, _: Arc<RwLock<Room>>, _: &MemberEvent) {
            self.0.lock().await.push("state member".to_string())
        }
        async fn on_state_name(&self, _: Arc<RwLock<Room>>, _: &NameEvent) {
            self.0.lock().await.push("state name".to_string())
        }
        async fn on_state_canonical_alias(&self, _: Arc<RwLock<Room>>, _: &CanonicalAliasEvent) {
            self.0.lock().await.push("state canonical".to_string())
        }
        async fn on_state_aliases(&self, _: Arc<RwLock<Room>>, _: &AliasesEvent) {
            self.0.lock().await.push("state aliases".to_string())
        }
        async fn on_state_avatar(&self, _: Arc<RwLock<Room>>, _: &AvatarEvent) {
            self.0.lock().await.push("state avatar".to_string())
        }
        async fn on_state_power_levels(&self, _: Arc<RwLock<Room>>, _: &PowerLevelsEvent) {
            self.0.lock().await.push("state power".to_string())
        }
        async fn on_state_join_rules(&self, _: Arc<RwLock<Room>>, _: &JoinRulesEvent) {
            self.0.lock().await.push("state rules".to_string())
        }

        async fn on_account_presence(&self, _: Arc<RwLock<Room>>, _: &PresenceEvent) {
            self.0.lock().await.push("account presence".to_string())
        }
        async fn on_account_ignored_users(&self, _: Arc<RwLock<Room>>, _: &IgnoredUserListEvent) {
            self.0.lock().await.push("account ignore".to_string())
        }
        async fn on_account_push_rules(&self, _: Arc<RwLock<Room>>, _: &PushRulesEvent) {
            self.0.lock().await.push("".to_string())
        }
        async fn on_account_data_fully_read(&self, _: Arc<RwLock<Room>>, _: &FullyReadEvent) {
            self.0.lock().await.push("account read".to_string())
        }
        async fn on_presence_event(&self, _: Arc<RwLock<Room>>, _: &PresenceEvent) {
            self.0.lock().await.push("presence event".to_string())
        }
    }

    use crate::identifiers::UserId;
    use crate::{AsyncClient, Session, SyncSettings};

    use mockito::{mock, Matcher};
    use url::Url;

    use std::convert::TryFrom;
    use std::str::FromStr;
    use std::time::Duration;

    #[tokio::test]
    async fn event_emitter() {
        let homeserver = Url::from_str(&mockito::server_url()).unwrap();

        let session = Session {
            access_token: "1234".to_owned(),
            user_id: UserId::try_from("@example:example.com").unwrap(),
            device_id: "DEVICEID".to_owned(),
        };

        let _m = mock(
            "GET",
            Matcher::Regex(r"^/_matrix/client/r0/sync\?.*$".to_string()),
        )
        .with_status(200)
        .with_body_from_file("tests/data/sync.json")
        .create();

        let vec = Arc::new(Mutex::new(Vec::new()));
        let test_vec = Arc::clone(&vec);
        let emitter = Box::new(EvEmitterTest(vec)) as Box<(dyn EventEmitter)>;
        let mut client = AsyncClient::new(homeserver, Some(session)).unwrap();
        client.add_event_emitter(emitter).await;

        let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));
        let _response = client.sync(sync_settings).await.unwrap();

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
                "presence event",
            ],
        )
    }
}
