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

use std::collections::{BTreeMap, HashMap};
use std::convert::TryFrom;

#[cfg(feature = "messages")]
use super::message::MessageQueue;
use super::RoomMember;

use crate::api::r0::sync::sync_events::{RoomSummary, UnreadNotificationsCount};
use crate::events::collections::all::{RoomEvent, StateEvent};
use crate::events::presence::PresenceEvent;
use crate::events::room::{
    aliases::AliasesEvent,
    canonical_alias::CanonicalAliasEvent,
    encryption::EncryptionEvent,
    member::{MemberEvent, MembershipChange},
    name::NameEvent,
    power_levels::{NotificationPowerLevels, PowerLevelsEvent, PowerLevelsEventContent},
    tombstone::TombstoneEvent,
};
use crate::events::stripped::{AnyStrippedStateEvent, StrippedRoomName};
use crate::events::EventType;

#[cfg(feature = "messages")]
use crate::events::room::message::MessageEvent;

use crate::identifiers::{RoomAliasId, RoomId, UserId};

use crate::js_int::{Int, UInt};
use serde::{Deserialize, Serialize};
#[derive(Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(test, derive(Clone))]
/// `RoomName` allows the calculation of a text room name.
pub struct RoomName {
    /// The displayed name of the room.
    name: Option<String>,
    /// The canonical alias of the room ex. `#room-name:example.com` and port number.
    canonical_alias: Option<RoomAliasId>,
    /// List of `RoomAliasId`s the room has been given.
    aliases: Vec<RoomAliasId>,
    /// Users which can be used to generate a room name if the room does not have
    /// one. Required if room name or canonical aliases are not set or empty.
    pub heroes: Vec<String>,
    /// Number of users whose membership status is `join`.
    /// Required if field has changed since last sync; otherwise, it may be
    /// omitted.
    pub joined_member_count: Option<UInt>,
    /// Number of users whose membership status is `invite`.
    /// Required if field has changed since last sync; otherwise, it may be
    /// omitted.
    pub invited_member_count: Option<UInt>,
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(Clone))]
pub struct PowerLevels {
    /// The level required to ban a user.
    pub ban: Int,
    /// The level required to send specific event types.
    ///
    /// This is a mapping from event type to power level required.
    pub events: BTreeMap<EventType, Int>,
    /// The default level required to send message events.
    pub events_default: Int,
    /// The level required to invite a user.
    pub invite: Int,
    /// The level required to kick a user.
    pub kick: Int,
    /// The level required to redact an event.
    pub redact: Int,
    /// The default level required to send state events.
    pub state_default: Int,
    /// The default power level for every user in the room.
    pub users_default: Int,
    /// The power level requirements for specific notification types.
    ///
    /// This is a mapping from `key` to power level for that notifications key.
    pub notifications: Int,
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(Clone))]
pub struct Tombstone {
    /// A server-defined message.
    body: String,
    /// The room that is now active.
    replacement: RoomId,
}

#[derive(Debug, PartialEq, Serialize, Deserialize)]
#[cfg_attr(test, derive(Clone))]
/// A Matrix room.
pub struct Room {
    /// The unique id of the room.
    pub room_id: RoomId,
    /// The name of the room, clients use this to represent a room.
    pub room_name: RoomName,
    /// The mxid of our own user.
    pub own_user_id: UserId,
    /// The mxid of the room creator.
    pub creator: Option<UserId>,
    /// The map of room members.
    pub members: HashMap<UserId, RoomMember>,
    /// A queue of messages, holds no more than 10 of the most recent messages.
    ///
    /// This is helpful when using a `StateStore` to avoid multiple requests
    /// to the server for messages.
    #[cfg(feature = "messages")]
    #[cfg_attr(docsrs, doc(cfg(feature = "messages")))]
    #[serde(with = "super::message::ser_deser")]
    pub messages: MessageQueue,
    /// A list of users that are currently typing.
    pub typing_users: Vec<UserId>,
    /// The power level requirements for specific actions in this room
    pub power_levels: Option<PowerLevels>,
    // TODO when encryption events are handled we store algorithm used and rotation time.
    /// A flag indicating if the room is encrypted.
    pub encrypted: bool,
    /// Number of unread notifications with highlight flag set.
    pub unread_highlight: Option<UInt>,
    /// Number of unread notifications.
    pub unread_notifications: Option<UInt>,
    /// The tombstone state of this room.
    pub tombstone: Option<Tombstone>,
    /// The map of display names
    display_names: HashMap<UserId, String>,
}

impl RoomName {
    pub fn push_alias(&mut self, alias: RoomAliasId) -> bool {
        self.aliases.push(alias);
        true
    }

    pub fn set_canonical(&mut self, alias: RoomAliasId) -> bool {
        self.canonical_alias = Some(alias);
        true
    }

    pub fn set_name(&mut self, name: &str) -> bool {
        self.name = Some(name.to_string());
        true
    }

    pub fn calculate_name(&self, members: &HashMap<UserId, RoomMember>) -> String {
        // https://matrix.org/docs/spec/client_server/latest#calculating-the-display-name-for-a-room.
        // the order in which we check for a name ^^
        if let Some(name) = &self.name {
            let name = name.trim();
            name.to_string()
        } else if let Some(alias) = &self.canonical_alias {
            let alias = alias.alias().trim();
            alias.to_string()
        } else if !self.aliases.is_empty() && !self.aliases[0].alias().is_empty() {
            self.aliases[0].alias().trim().to_string()
        } else {
            let joined = self.joined_member_count.unwrap_or(UInt::MIN);
            let invited = self.invited_member_count.unwrap_or(UInt::MIN);
            let heroes = UInt::new(self.heroes.len() as u64).unwrap();
            let one = UInt::new(1).unwrap();

            let invited_joined = if invited + joined == UInt::MIN {
                UInt::MIN
            } else {
                invited + joined - one
            };

            // TODO this should use `self.heroes but it is always empty??
            if heroes >= invited_joined {
                let mut names = members
                    .values()
                    .take(3)
                    .map(|mem| {
                        mem.display_name
                            .clone()
                            .unwrap_or_else(|| mem.user_id.localpart().to_string())
                    })
                    .collect::<Vec<String>>();
                // stabilize ordering
                names.sort();
                names.join(", ")
            } else if heroes < invited_joined && invited + joined > one {
                let mut names = members
                    .values()
                    .take(3)
                    .map(|mem| {
                        mem.display_name
                            .clone()
                            .unwrap_or_else(|| mem.user_id.localpart().to_string())
                    })
                    .collect::<Vec<String>>();
                names.sort();
                // TODO what length does the spec want us to use here and in the `else`
                format!("{}, and {} others", names.join(", "), (joined + invited))
            } else {
                format!("Empty Room (was {} others)", members.len())
            }
        }
    }
}

impl Room {
    /// Create a new room.
    ///
    /// # Arguments
    ///
    /// * `room_id` - The unique id of the room.
    ///
    /// * `own_user_id` - The mxid of our own user.
    pub fn new(room_id: &RoomId, own_user_id: &UserId) -> Self {
        Room {
            room_id: room_id.clone(),
            room_name: RoomName::default(),
            own_user_id: own_user_id.clone(),
            creator: None,
            members: HashMap::new(),
            #[cfg(feature = "messages")]
            messages: MessageQueue::new(),
            typing_users: Vec::new(),
            power_levels: None,
            encrypted: false,
            unread_highlight: None,
            unread_notifications: None,
            tombstone: None,
            display_names: HashMap::new(),
        }
    }

    /// Return the display name of the room.
    pub fn display_name(&self) -> String {
        self.room_name.calculate_name(&self.members)
    }

    /// Is the room a encrypted room.
    pub fn is_encrypted(&self) -> bool {
        self.encrypted
    }

    /// Get the resolved display name for a member of this room.
    pub fn member_display_name(&self, id: &UserId) -> Option<&str> {
        self.display_names.get(id).map(|s| s.as_str())
    }

    fn add_member(&mut self, event: &MemberEvent) -> bool {
        if self
            .members
            .contains_key(&UserId::try_from(event.state_key.as_str()).unwrap())
        {
            return false;
        }

        let member = RoomMember::new(event);

        // find all users that share the same display name as the joining user
        let users_with_same_name: Vec<_> = self
            .display_names
            .iter()
            .filter(|(_, v)| {
                member
                    .display_name
                    .as_ref()
                    .map(|n| &n == v)
                    .unwrap_or(false)
            })
            .map(|(k, _)| k)
            .cloned()
            .collect();

        // if there is no other user with the same display name -> just use the display name
        if users_with_same_name.is_empty() {
            let display_name = member
                .display_name
                .as_ref()
                .map(|s| s.to_string())
                .unwrap_or_else(|| format!("{}", member.user_id));
            self.display_names
                .insert(member.user_id.clone(), display_name);
        } else {
            // else user `display_name (userid)`
            let users_with_same_name = users_with_same_name
                .into_iter()
                .filter_map(|id| {
                    self.members
                        .get(&id)
                        .map(|m| {
                            m.display_name
                                .as_ref()
                                .map(|d| format!("{} ({})", d, m.user_id))
                                .unwrap_or_else(|| format!("{}", m.user_id))
                        })
                        .map(|m| (id, m))
                })
                .collect::<Vec<_>>();

            // update all existing users with same name
            for (id, member) in users_with_same_name {
                self.display_names.insert(id, member);
            }

            // insert new member's display name
            self.display_names.insert(
                member.user_id.clone(),
                member
                    .display_name
                    .as_ref()
                    .map(|n| format!("{} ({})", n, member.user_id))
                    .unwrap_or_else(|| format!("{}", member.user_id)),
            );
        }

        self.members
            .insert(UserId::try_from(event.state_key.as_str()).unwrap(), member);

        true
    }

    /// Add to the list of `RoomAliasId`s.
    fn push_room_alias(&mut self, alias: &RoomAliasId) -> bool {
        self.room_name.push_alias(alias.clone());
        true
    }

    /// RoomAliasId is `#alias:hostname` and `port`
    fn canonical_alias(&mut self, alias: &RoomAliasId) -> bool {
        self.room_name.set_canonical(alias.clone());
        true
    }

    fn set_room_name(&mut self, name: &str) -> bool {
        self.room_name.set_name(name);
        true
    }

    fn set_room_power_level(&mut self, event: &PowerLevelsEvent) -> bool {
        let PowerLevelsEventContent {
            ban,
            events,
            events_default,
            invite,
            kick,
            redact,
            state_default,
            users_default,
            notifications: NotificationPowerLevels { room },
            ..
        } = &event.content;

        let power = PowerLevels {
            ban: *ban,
            events: events.clone(),
            events_default: *events_default,
            invite: *invite,
            kick: *kick,
            redact: *redact,
            state_default: *state_default,
            users_default: *users_default,
            notifications: *room,
        };
        self.power_levels = Some(power);
        true
    }

    pub(crate) fn set_room_summary(&mut self, summary: &RoomSummary) {
        let RoomSummary {
            heroes,
            joined_member_count,
            invited_member_count,
        } = summary;
        self.room_name.heroes = heroes.clone();
        self.room_name.invited_member_count = *invited_member_count;
        self.room_name.joined_member_count = *joined_member_count;
    }

    pub(crate) fn set_unread_notice_count(&mut self, notifications: &UnreadNotificationsCount) {
        self.unread_highlight = notifications.highlight_count;
        self.unread_notifications = notifications.notification_count;
    }

    /// Handle a room.member updating the room state if necessary.
    ///
    /// Returns true if the joined member list changed, false otherwise.
    pub fn handle_membership(&mut self, event: &MemberEvent) -> bool {
        match event.membership_change() {
            MembershipChange::Invited | MembershipChange::Joined => self.add_member(event),
            _ => {
                let user = if let Ok(id) = UserId::try_from(event.state_key.as_str()) {
                    id
                } else {
                    return false;
                };
                if let Some(member) = self.members.get_mut(&user) {
                    member.update_member(event)
                } else {
                    false
                }
            }
        }
    }

    /// Handle a room.message event and update the `MessageQueue` if necessary.
    ///
    /// Returns true if `MessageQueue` was added to.
    #[cfg(feature = "messages")]
    #[cfg_attr(docsrs, doc(cfg(feature = "messages")))]
    pub fn handle_message(&mut self, event: &MessageEvent) -> bool {
        self.messages.push(event.clone())
    }

    /// Handle a room.aliases event, updating the room state if necessary.
    ///
    /// Returns true if the room name changed, false otherwise.
    pub fn handle_room_aliases(&mut self, event: &AliasesEvent) -> bool {
        match event.content.aliases.as_slice() {
            [alias] => self.push_room_alias(alias),
            [alias, ..] => self.push_room_alias(alias),
            _ => false,
        }
    }

    /// Handle a room.canonical_alias event, updating the room state if necessary.
    ///
    /// Returns true if the room name changed, false otherwise.
    pub fn handle_canonical(&mut self, event: &CanonicalAliasEvent) -> bool {
        match &event.content.alias {
            Some(name) => self.canonical_alias(&name),
            _ => false,
        }
    }

    /// Handle a room.name event, updating the room state if necessary.
    ///
    /// Returns true if the room name changed, false otherwise.
    pub fn handle_room_name(&mut self, event: &NameEvent) -> bool {
        match event.content.name() {
            Some(name) => self.set_room_name(name),
            _ => false,
        }
    }

    /// Handle a room.name event, updating the room state if necessary.
    ///
    /// Returns true if the room name changed, false otherwise.
    pub fn handle_stripped_room_name(&mut self, event: &StrippedRoomName) -> bool {
        match event.content.name() {
            Some(name) => self.set_room_name(name),
            _ => false,
        }
    }

    /// Handle a room.power_levels event, updating the room state if necessary.
    ///
    /// Returns true if the room name changed, false otherwise.
    pub fn handle_power_level(&mut self, event: &PowerLevelsEvent) -> bool {
        // NOTE: this is always true, we assume that if we get an event their is an update.
        let mut updated = self.set_room_power_level(event);

        let mut max_power = event.content.users_default;
        for power in event.content.users.values() {
            max_power = *power.max(&max_power);
        }

        for user in event.content.users.keys() {
            if let Some(member) = self.members.get_mut(user) {
                if member.update_power(event, max_power) {
                    updated = true;
                }
            }
        }
        updated
    }

    fn handle_tombstone(&mut self, event: &TombstoneEvent) -> bool {
        self.tombstone = Some(Tombstone {
            body: event.content.body.clone(),
            replacement: event.content.replacement_room.clone(),
        });
        true
    }

    fn handle_encryption_event(&mut self, _event: &EncryptionEvent) -> bool {
        self.encrypted = true;
        true
    }

    /// Receive a timeline event for this room and update the room state.
    ///
    /// Returns true if the joined member list changed, false otherwise.
    ///
    /// # Arguments
    ///
    /// * `event` - The event of the room.
    pub fn receive_timeline_event(&mut self, event: &RoomEvent) -> bool {
        match event {
            // update to the current members of the room
            RoomEvent::RoomMember(member) => self.handle_membership(member),
            // finds all events related to the name of the room for later use
            RoomEvent::RoomName(name) => self.handle_room_name(name),
            RoomEvent::RoomCanonicalAlias(c_alias) => self.handle_canonical(c_alias),
            RoomEvent::RoomAliases(alias) => self.handle_room_aliases(alias),
            // power levels of the room members
            RoomEvent::RoomPowerLevels(power) => self.handle_power_level(power),
            RoomEvent::RoomTombstone(tomb) => self.handle_tombstone(tomb),
            RoomEvent::RoomEncryption(encrypt) => self.handle_encryption_event(encrypt),
            #[cfg(feature = "messages")]
            RoomEvent::RoomMessage(msg) => self.handle_message(msg),
            _ => false,
        }
    }

    /// Receive a state event for this room and update the room state.
    ///
    /// Returns true if the state of the `Room` has changed, false otherwise.
    ///
    /// # Arguments
    ///
    /// * `event` - The event of the room.
    pub fn receive_state_event(&mut self, event: &StateEvent) -> bool {
        match event {
            // update to the current members of the room
            StateEvent::RoomMember(member) => self.handle_membership(member),
            // finds all events related to the name of the room for later use
            StateEvent::RoomName(name) => self.handle_room_name(name),
            StateEvent::RoomCanonicalAlias(c_alias) => self.handle_canonical(c_alias),
            StateEvent::RoomAliases(alias) => self.handle_room_aliases(alias),
            // power levels of the room members
            StateEvent::RoomPowerLevels(power) => self.handle_power_level(power),
            StateEvent::RoomTombstone(tomb) => self.handle_tombstone(tomb),
            StateEvent::RoomEncryption(encrypt) => self.handle_encryption_event(encrypt),
            _ => false,
        }
    }

    /// Receive a stripped state event for this room and update the room state.
    ///
    /// Returns true if the state of the `Room` has changed, false otherwise.
    ///
    /// # Arguments
    ///
    /// * `event` - The `AnyStrippedStateEvent` sent by the server for invited but not
    /// joined rooms.
    pub fn receive_stripped_state_event(&mut self, event: &AnyStrippedStateEvent) -> bool {
        match event {
            AnyStrippedStateEvent::RoomName(n) => self.handle_stripped_room_name(n),
            _ => false,
        }
    }

    /// Receive a presence event from an `IncomingResponse` and updates the client state.
    ///
    /// This will only update the user if found in the current room looped through
    /// by `Client::sync`.
    /// Returns true if the specific users presence has changed, false otherwise.
    ///
    /// # Arguments
    ///
    /// * `event` - The presence event for a specified room member.
    pub fn receive_presence_event(&mut self, event: &PresenceEvent) -> bool {
        if let Some(member) = self.members.get_mut(&event.sender) {
            if member.did_update_presence(event) {
                false
            } else {
                member.update_presence(event);
                true
            }
        } else {
            // this is probably an error as we have a `PresenceEvent` for a user
            // we don't know about
            false
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::events::room::member::MembershipState;
    use crate::identifiers::UserId;
    use crate::{BaseClient, Session};
    use matrix_sdk_test::{async_test, sync_response, EventBuilder, EventsFile, SyncResponseFile};

    #[cfg(target_arch = "wasm32")]
    use wasm_bindgen_test::*;

    use std::convert::TryFrom;
    use std::ops::Deref;

    fn get_client() -> BaseClient {
        let session = Session {
            access_token: "1234".to_owned(),
            user_id: UserId::try_from("@example:localhost").unwrap(),
            device_id: "DEVICEID".to_owned(),
        };
        BaseClient::new(Some(session)).unwrap()
    }

    fn get_room_id() -> RoomId {
        RoomId::try_from("!SVkFJHzfwvuaIEawgC:localhost").unwrap()
    }

    #[async_test]
    async fn user_presence() {
        let client = get_client();

        let mut response = sync_response(SyncResponseFile::Default);

        client.receive_sync_response(&mut response).await.unwrap();

        let rooms_lock = &client.joined_rooms();
        let rooms = rooms_lock.read().await;
        let room = &rooms
            .get(&RoomId::try_from("!SVkFJHzfwvuaIEawgC:localhost").unwrap())
            .unwrap()
            .read()
            .await;

        assert_eq!(2, room.members.len());
        for member in room.members.values() {
            assert_eq!(MembershipState::Join, member.membership);
        }

        assert!(room.deref().power_levels.is_some())
    }

    #[async_test]
    async fn room_events() {
        let client = get_client();
        let room_id = get_room_id();
        let user_id = UserId::try_from("@example:localhost").unwrap();

        let mut response = EventBuilder::default()
            .add_room_event(EventsFile::Member, RoomEvent::RoomMember)
            .add_room_event(EventsFile::PowerLevels, RoomEvent::RoomPowerLevels)
            .build_sync_response();

        client.receive_sync_response(&mut response).await.unwrap();

        let room = client.get_joined_room(&room_id).await.unwrap();
        let room = room.read().await;

        assert_eq!(room.members.len(), 1);
        assert!(room.power_levels.is_some());
        assert_eq!(
            room.power_levels.as_ref().unwrap().kick,
            crate::js_int::Int::new(50).unwrap()
        );
        let admin = room.members.get(&user_id).unwrap();
        assert_eq!(
            admin.power_level.unwrap(),
            crate::js_int::Int::new(100).unwrap()
        );
    }

    #[async_test]
    async fn calculate_aliases() {
        let client = get_client();

        let room_id = get_room_id();

        let mut response = EventBuilder::default()
            .add_state_event(EventsFile::Aliases, StateEvent::RoomAliases)
            .build_sync_response();

        client.receive_sync_response(&mut response).await.unwrap();

        let room = client.get_joined_room(&room_id).await.unwrap();
        let room = room.read().await;

        assert_eq!("tutorial", room.display_name());
    }

    #[async_test]
    async fn calculate_alias() {
        let client = get_client();

        let room_id = get_room_id();

        let mut response = EventBuilder::default()
            .add_state_event(EventsFile::Alias, StateEvent::RoomCanonicalAlias)
            .build_sync_response();

        client.receive_sync_response(&mut response).await.unwrap();

        let room = client.get_joined_room(&room_id).await.unwrap();
        let room = room.read().await;

        assert_eq!("tutorial", room.display_name());
    }

    #[async_test]
    async fn calculate_name() {
        let client = get_client();

        let room_id = get_room_id();

        let mut response = EventBuilder::default()
            .add_state_event(EventsFile::Name, StateEvent::RoomName)
            .build_sync_response();

        client.receive_sync_response(&mut response).await.unwrap();

        let room = client.get_joined_room(&room_id).await.unwrap();
        let room = room.read().await;

        assert_eq!("room name", room.display_name());
    }

    #[async_test]
    async fn calculate_room_names_from_summary() {
        let mut response = sync_response(SyncResponseFile::DefaultWithSummary);

        let session = Session {
            access_token: "1234".to_owned(),
            user_id: UserId::try_from("@example:localhost").unwrap(),
            device_id: "DEVICEID".to_owned(),
        };
        let client = BaseClient::new(Some(session)).unwrap();
        client.receive_sync_response(&mut response).await.unwrap();

        let mut room_names = vec![];
        for room in client.joined_rooms().read().await.values() {
            room_names.push(room.read().await.display_name())
        }

        assert_eq!(vec!["example, example2"], room_names);
    }
}
