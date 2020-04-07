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

use std::collections::HashMap;
use std::convert::TryFrom;

use super::RoomMember;

use crate::events::collections::all::{RoomEvent, StateEvent};
use crate::events::presence::PresenceEvent;
use crate::events::room::{
    aliases::AliasesEvent,
    canonical_alias::CanonicalAliasEvent,
    encryption::EncryptionEvent,
    member::{MemberEvent, MembershipChange},
    name::NameEvent,
    power_levels::{NotificationPowerLevels, PowerLevelsEvent, PowerLevelsEventContent},
};
use crate::events::EventType;
use crate::identifiers::{RoomAliasId, RoomId, UserId};

use js_int::{Int, UInt};

#[derive(Debug, Default)]
/// `RoomName` allows the calculation of a text room name.
pub struct RoomName {
    /// The displayed name of the room.
    name: Option<String>,
    /// The canonical alias of the room ex. `#room-name:example.com` and port number.
    canonical_alias: Option<RoomAliasId>,
    /// List of `RoomAliasId`s the room has been given.
    aliases: Vec<RoomAliasId>,
}

#[derive(Debug, PartialEq, Eq)]
pub struct PowerLevels {
    /// The level required to ban a user.
    pub ban: Int,
    /// The level required to send specific event types.
    ///
    /// This is a mapping from event type to power level required.
    pub events: HashMap<EventType, Int>,
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

#[derive(Debug)]
/// A Matrix rooom.
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

    pub fn calculate_name(
        &self,
        room_id: &RoomId,
        members: &HashMap<UserId, RoomMember>,
    ) -> String {
        // https://matrix.org/docs/spec/client_server/latest#calculating-the-display-name-for-a-room.
        // the order in which we check for a name ^^
        if let Some(name) = &self.name {
            name.clone()
        } else if let Some(alias) = &self.canonical_alias {
            alias.alias().to_string()
        } else if !self.aliases.is_empty() {
            self.aliases[0].alias().to_string()
        } else {
            let mut names = members
                .values()
                .flat_map(|m| m.display_name.clone())
                .take(3)
                .collect::<Vec<_>>();

            if names.is_empty() {
                // TODO implement the rest of display name for room spec
                format!("Room {}", room_id)
            } else {
                // stabilize order
                names.sort();
                names.join(", ")
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
            typing_users: Vec::new(),
            power_levels: None,
            encrypted: false,
            unread_highlight: None,
            unread_notifications: None,
        }
    }

    /// Return the display name of the room.
    pub fn calculate_name(&self) -> String {
        self.room_name.calculate_name(&self.room_id, &self.members)
    }

    /// Is the room a encrypted room.
    pub fn is_encrypted(&self) -> bool {
        self.encrypted
    }

    fn add_member(&mut self, event: &MemberEvent) -> bool {
        if self
            .members
            .contains_key(&UserId::try_from(event.state_key.as_str()).unwrap())
        {
            return false;
        }

        let member = RoomMember::new(event);

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

    fn set_name_room(&mut self, name: &str) -> bool {
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
            Some(name) => self.set_name_room(name),
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
            RoomEvent::RoomMember(m) => self.handle_membership(m),
            // finds all events related to the name of the room for later use
            RoomEvent::RoomName(n) => self.handle_room_name(n),
            RoomEvent::RoomCanonicalAlias(ca) => self.handle_canonical(ca),
            RoomEvent::RoomAliases(a) => self.handle_room_aliases(a),
            // power levels of the room members
            RoomEvent::RoomPowerLevels(p) => self.handle_power_level(p),
            RoomEvent::RoomEncryption(e) => self.handle_encryption_event(e),
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
            StateEvent::RoomMember(m) => self.handle_membership(m),
            StateEvent::RoomName(n) => self.handle_room_name(n),
            StateEvent::RoomCanonicalAlias(ca) => self.handle_canonical(ca),
            StateEvent::RoomAliases(a) => self.handle_room_aliases(a),
            StateEvent::RoomPowerLevels(p) => self.handle_power_level(p),
            StateEvent::RoomEncryption(e) => self.handle_encryption_event(e),
            _ => false,
        }
    }

    /// Receive a presence event from an `IncomingResponse` and updates the client state.
    ///
    /// This will only update the user if found in the current room looped through by `AsyncClient::sync`.
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
    use crate::test_builder::EventBuilder;
    use crate::{assert_, assert_eq_};
    use crate::{AsyncClient, Session, SyncSettings};

    use mockito::{mock, Matcher};
    use url::Url;

    use std::convert::TryFrom;
    use std::ops::Deref;
    use std::str::FromStr;
    use std::time::Duration;

    #[tokio::test]
    async fn user_presence() {
        let homeserver = Url::from_str(&mockito::server_url()).unwrap();

        let session = Session {
            access_token: "1234".to_owned(),
            user_id: UserId::try_from("@example:localhost").unwrap(),
            device_id: "DEVICEID".to_owned(),
        };

        let _m = mock(
            "GET",
            Matcher::Regex(r"^/_matrix/client/r0/sync\?.*$".to_string()),
        )
        .with_status(200)
        .with_body_from_file("tests/data/sync.json")
        .create();

        let mut client = AsyncClient::new(homeserver, Some(session)).unwrap();

        let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

        let _response = client.sync(sync_settings).await.unwrap();

        let rooms = &client.base_client.read().await.joined_rooms;
        let room = &rooms
            .get(&RoomId::try_from("!SVkFJHzfwvuaIEawgC:localhost").unwrap())
            .unwrap()
            .lock()
            .await;

        assert_eq!(2, room.members.len());
        for (_id, member) in &room.members {
            assert_eq!(MembershipState::Join, member.membership);
        }

        assert!(room.deref().power_levels.is_some())
    }

    #[tokio::test]
    async fn room_events() {
        fn test_room_users(room: &Room) -> Result<(), String> {
            assert_eq_!(room.members.len(), 1);
            Ok(())
        }

        fn test_room_power(room: &Room) -> Result<(), String> {
            assert_!(room.power_levels.is_some());
            assert_eq_!(
                room.power_levels.as_ref().unwrap().kick,
                js_int::Int::new(50).unwrap()
            );
            let admin = room
                .members
                .get(&UserId::try_from("@example:localhost").unwrap())
                .unwrap();
            assert_eq_!(admin.power_level.unwrap(), js_int::Int::new(100).unwrap());
            Ok(())
        }

        let rid = RoomId::try_from("!roomid:room.com").unwrap();
        let uid = UserId::try_from("@example:localhost").unwrap();
        let bld = EventBuilder::default();
        let runner = bld
            .add_room_event_from_file("./tests/data/events/member.json", RoomEvent::RoomMember)
            .add_room_event_from_file(
                "./tests/data/events/power_levels.json",
                RoomEvent::RoomPowerLevels,
            )
            .build_room_runner(&rid, &uid)
            .add_room_assert(test_room_power)
            .add_room_assert(test_room_users);

        runner.run_test().await;
    }
}
