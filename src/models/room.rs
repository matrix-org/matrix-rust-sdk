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
use std::sync::{Arc, RwLock};

use super::{RoomMember, User, UserId};

use crate::events::collections::all::{RoomEvent, StateEvent};
use crate::events::room::{
    aliases::AliasesEvent,
    canonical_alias::CanonicalAliasEvent,
    member::{MemberEvent, MembershipChange},
    name::NameEvent,
    power_levels::PowerLevelsEvent,
};
use crate::events::{
    presence::{PresenceEvent, PresenceEventContent},
    EventResult,
};
use crate::identifiers::RoomAliasId;
use crate::session::Session;

use js_int::UInt;

#[cfg(feature = "encryption")]
use tokio::sync::Mutex;

#[cfg(feature = "encryption")]
use crate::crypto::{OlmMachine, OneTimeKeys};
#[cfg(feature = "encryption")]
use ruma_client_api::r0::keys::{upload_keys::Response as KeysUploadResponse, DeviceKeys};

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

#[derive(Debug)]
/// A Matrix rooom.
pub struct Room {
    /// The unique id of the room.
    pub room_id: String,
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

    pub fn calculate_name(&self, room_id: &str, members: &HashMap<UserId, RoomMember>) -> String {
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
                .flat_map(|m| m.user.display_name.clone())
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
    pub fn new(room_id: &str, own_user_id: &str) -> Self {
        Room {
            room_id: room_id.to_string(),
            room_name: RoomName::default(),
            own_user_id: own_user_id.to_owned(),
            creator: None,
            members: HashMap::new(),
            typing_users: Vec::new(),
            encrypted: false,
            unread_highlight: None,
            unread_notifications: None,
        }
    }

    fn add_member(&mut self, event: &MemberEvent) -> bool {
        if self.members.contains_key(&event.state_key) {
            return false;
        }

        let member = RoomMember::new(event);

        self.members.insert(event.state_key.clone(), member);

        true
    }

    /// Add to the list of `RoomAliasId`s.
    fn room_aliases(&mut self, alias: &RoomAliasId) -> bool {
        self.room_name.push_alias(alias.clone());
        true
    }

    /// RoomAliasId is `#alias:hostname` and `port`
    fn canonical_alias(&mut self, alias: &RoomAliasId) -> bool {
        self.room_name.set_canonical(alias.clone());
        true
    }

    fn name_room(&mut self, name: &str) -> bool {
        self.room_name.set_name(name);
        true
    }

    /// Handle a room.member updating the room state if necessary.
    ///
    /// Returns true if the joined member list changed, false otherwise.
    pub fn handle_membership(&mut self, event: &MemberEvent) -> bool {
        match event.membership_change() {
            MembershipChange::Invited | MembershipChange::Joined => self.add_member(event),
            _ => {
                if let Some(member) = self.members.get_mut(&event.state_key) {
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
            [alias] => self.room_aliases(alias),
            [alias, ..] => self.room_aliases(alias),
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
            Some(name) => self.name_room(name),
            _ => false,
        }
    }

    /// Handle a room.power_levels event, updating the room state if necessary.
    ///
    /// Returns true if the room name changed, false otherwise.
    pub fn handle_power_level(&mut self, event: &PowerLevelsEvent) -> bool {
        if let Some(member) = self.members.get_mut(&event.state_key) {
            member.update_power(event)
        } else {
            false
        }
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
            // finds all events related to the name of the room for later calculation
            RoomEvent::RoomName(n) => self.handle_room_name(n),
            RoomEvent::RoomCanonicalAlias(ca) => self.handle_canonical(ca),
            RoomEvent::RoomAliases(a) => self.handle_room_aliases(a),
            // power levels of the room members
            RoomEvent::RoomPowerLevels(p) => self.handle_power_level(p),
            _ => false,
        }
    }

    /// Receive a state event for this room and update the room state.
    ///
    /// Returns true if the joined member list changed, false otherwise.
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
        if let Some(user) = self
            .members
            .get_mut(&event.sender.to_string())
            .map(|m| &mut m.user)
        {
            if user.did_update_presence(event) {
                false
            } else {
                user.update_presence(event);
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
    use crate::{AsyncClient, Session, SyncSettings};

    use mockito::{mock, Matcher};
    use tokio::runtime::Runtime;
    use url::Url;

    use std::convert::TryFrom;
    use std::str::FromStr;
    use std::time::Duration;

    #[tokio::test]
    async fn user_presence() {
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

        let mut client = AsyncClient::new(homeserver, Some(session)).unwrap();

        let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

        let _response = client.sync(sync_settings).await.unwrap();

        let rooms = &client.base_client.read().await.joined_rooms;
        let room = &rooms
            .get("!SVkFJHzfwvuaIEawgC:localhost")
            .unwrap()
            .read()
            .unwrap();

        assert_eq!(2, room.members.len());
        for (id, member) in &room.members {
            assert_eq!(MembershipState::Join, member.membership);
        }
    }
}
