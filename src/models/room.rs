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
use crate::api::r0 as api;
use crate::events::collections::all::{RoomEvent, StateEvent};
use crate::events::room::{
    aliases::AliasesEvent,
    canonical_alias::CanonicalAliasEvent,
    member::{MemberEvent, MembershipState},
    name::NameEvent,
    power_levels::PowerLevelsEvent,
};
use crate::events::{
    presence::{PresenceEvent, PresenceEventContent},
    EventResult,
};
use crate::identifiers::RoomAliasId;
use crate::session::Session;

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
    /// A flag indicating if the room is encrypted.
    pub encrypted: bool,
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
        // https://github.com/matrix-org/matrix-js-sdk/blob/33941eb37bffe41958ba9887fc8070dfb1a0ee76/src/models/room.js#L1823
        // the order in which we check for a name ^^
        if let Some(name) = &self.name {
            name.clone()
        } else if let Some(alias) = &self.canonical_alias {
            alias.alias().to_string()
        } else if !self.aliases.is_empty() {
            self.aliases[0].alias().to_string()
        } else {
            // TODO
            let mut names = members
                .values()
                .flat_map(|m| m.user.display_name.clone())
                .take(3)
                .collect::<Vec<_>>();

            if names.is_empty() {
                // TODO implement the rest of matrix-js-sdk handling of room names
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

    // fn remove_member(&mut self, event: &MemberEvent) -> bool {
    //     if let Some(member) = self.members.get_mut(&event.sender.to_string()) {
    //         let changed = member.membership == event.content.membership;
    //         member.membership = event.content.membership;
    //         changed
    //     } else {
    //         false
    //     }
    // }

    // fn update_joined_member(&mut self, event: &MemberEvent) -> bool {
    //     if let Some(member) = self.members.get_mut(&event.state_key) {
    //         member.update(event);
    //     }

    //     false
    // }

    // fn handle_join(&mut self, event: &MemberEvent) -> bool {
    //     match &event.prev_content {
    //         Some(c) => match c.membership {
    //             MembershipState::Join => self.update_joined_member(event),
    //             MembershipState::Invite => self.add_member(event),
    //             MembershipState::Leave => self.remove_member(event),
    //             _ => false,
    //         },
    //         None => self.add_member(event),
    //     }
    // }

    // fn handle_leave(&mut self, event: &MemberEvent) -> bool {

    // }

    /// Handle a room.member updating the room state if necessary.
    ///
    /// Returns true if the joined member list changed, false otherwise.
    pub fn handle_membership(&mut self, event: &MemberEvent) -> bool {
        match &event.content.membership {
            MembershipState::Invite | MembershipState::Join => self.add_member(event),
            _ => {
                if let Some(member) = self.members.get_mut(&event.state_key) {
                    member.update_member(event)
                } else {
                    false
                }
            }
        }
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
    /// Returns true if the joined member list changed, false otherwise.
    ///
    /// # Arguments
    ///
    /// * `event` - The event of the room.
    pub fn receive_presence_event(&mut self, event: &PresenceEvent) -> bool {
        let PresenceEvent {
            content:
                PresenceEventContent {
                    avatar_url,
                    currently_active,
                    displayname,
                    last_active_ago,
                    presence,
                    status_msg,
                },
            sender,
        } = event;

        if let Some(user) = self
            .members
            .get_mut(&sender.to_string())
            .map(|m| &mut m.user)
        {
            if user.display_name == *displayname
                && user.avatar_url == *avatar_url
                && user.presence.as_ref() == Some(presence)
                && user.status_msg == *status_msg
                && user.last_active_ago == *last_active_ago
                && user.currently_active == *currently_active
            {
                false
            } else {
                user.presence_events.push(event.clone());
                *user = User {
                    display_name: displayname.clone(),
                    avatar_url: avatar_url.clone(),
                    presence: Some(presence.clone()),
                    status_msg: status_msg.clone(),
                    last_active_ago: *last_active_ago,
                    currently_active: *currently_active,
                    // TODO better way of moving vec over
                    events: user.events.clone(),
                    presence_events: user.presence_events.clone(),
                };
                true
            }
        } else {
            // this is probably an error as we have a `PresenceEvent` for a user
            // we dont know about
            false
        }
    }
}

// pub struct User {
//     /// The human readable name of the user.
//     pub display_name: Option<String>,
//     /// The matrix url of the users avatar.
//     pub avatar_url: Option<String>,
//     /// The presence of the user, if found.
//     pub presence: Option<PresenceState>,
//     /// The presence status message, if found.
//     pub status_msg: Option<String>,
//     /// The time, in ms, since the user interacted with the server.
//     pub last_active_ago: Option<UInt>,
//     /// If the user should be considered active.
//     pub currently_active: Option<bool>,
//     /// The events that created the state of the current user.
//     // TODO do we want to hold the whole state or just update our structures.
//     pub events: Vec<Event>,
//     /// The `PresenceEvent`s connected to this user.
//     pub presence_events: Vec<PresenceEvent>,
// }

// pub struct RoomMember {
//     /// The unique mxid of the user.
//     pub user_id: UserId,
//     /// The unique id of the room.
//     pub room_id: Option<RoomId>,
//     /// If the member is typing.
//     pub typing: Option<bool>,
//     /// The user data for this room member.
//     pub user: User,
//     /// The users power level.
//     pub power_level: Option<Int>,
//     /// The normalized power level of this `RoomMember` (0-100).
//     pub power_level_norm: Option<Int>,
//     /// The `MembershipState` of this `RoomMember`.
//     pub membership: MembershipState,
//     /// The human readable name of this room member.
//     pub name: String,
//     /// The events that created the state of this room member.
//     pub events: Vec<Event>
// }
