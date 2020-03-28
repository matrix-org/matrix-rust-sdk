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

#[cfg(feature = "encryption")]
use std::result::Result as StdResult;

use crate::api::r0 as api;
use crate::error::Result;
use crate::events::collections::all::{RoomEvent, StateEvent};
use crate::events::room::{
    aliases::AliasesEvent,
    canonical_alias::CanonicalAliasEvent,
    member::{MemberEvent, MembershipState},
    name::NameEvent,
};
use crate::events::EventResult;
use crate::identifiers::RoomAliasId;
use crate::session::Session;
use std::sync::{Arc, RwLock};

#[cfg(feature = "encryption")]
use tokio::sync::Mutex;

#[cfg(feature = "encryption")]
use crate::crypto::{OlmMachine, OneTimeKeys};
#[cfg(feature = "encryption")]
use ruma_client_api::r0::keys::{upload_keys::Response as KeysUploadResponse, DeviceKeys};
use ruma_identifiers::RoomId;

pub type Token = String;
pub type UserId = String;

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
/// A Matrix room member.
pub struct RoomMember {
    /// The unique mxid of the user.
    pub user_id: UserId,
    /// The human readable name of the user.
    pub display_name: Option<String>,
    /// The matrix url of the users avatar.
    pub avatar_url: Option<String>,
    /// The users power level.
    pub power_level: u8,
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
                .flat_map(|m| m.display_name.clone())
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

        let member = RoomMember {
            user_id: event.state_key.clone(),
            display_name: event.content.displayname.clone(),
            avatar_url: event.content.avatar_url.clone(),
            power_level: 0,
        };

        self.members.insert(event.state_key.clone(), member);

        true
    }

    fn remove_member(&mut self, event: &MemberEvent) -> bool {
        if !self.members.contains_key(&event.state_key) {
            return false;
        }

        true
    }

    fn update_joined_member(&mut self, event: &MemberEvent) -> bool {
        if let Some(member) = self.members.get_mut(&event.state_key) {
            member.display_name = event.content.displayname.clone();
            member.avatar_url = event.content.avatar_url.clone();
        }

        false
    }

    fn handle_join(&mut self, event: &MemberEvent) -> bool {
        match &event.prev_content {
            Some(c) => match c.membership {
                MembershipState::Join => self.update_joined_member(event),
                MembershipState::Invite => self.add_member(event),
                MembershipState::Leave => self.remove_member(event),
                _ => false,
            },
            None => self.add_member(event),
        }
    }

    fn handle_leave(&mut self, _event: &MemberEvent) -> bool {
        false
    }

    /// Handle a room.member updating the room state if necessary.
    ///
    /// Returns true if the joined member list changed, false otherwise.
    pub fn handle_membership(&mut self, event: &MemberEvent) -> bool {
        match event.content.membership {
            MembershipState::Join => self.handle_join(event),
            MembershipState::Leave => self.handle_leave(event),
            MembershipState::Ban => self.handle_leave(event),
            MembershipState::Invite => false,
            MembershipState::Knock => false,
            _ => false,
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

    /// Receive a timeline event for this room and update the room state.
    ///
    /// Returns true if the joined member list changed, false otherwise.
    ///
    /// # Arguments
    ///
    /// * `event` - The event of the room.
    pub fn receive_timeline_event(&mut self, event: &RoomEvent) -> bool {
        match event {
            RoomEvent::RoomMember(m) => self.handle_membership(m),
            RoomEvent::RoomName(n) => self.handle_room_name(n),
            RoomEvent::RoomCanonicalAlias(ca) => self.handle_canonical(ca),
            RoomEvent::RoomAliases(a) => self.handle_room_aliases(a),
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
}

#[derive(Debug)]
/// A no IO Client implementation.
///
/// This Client is a state machine that receives responses and events and
/// accordingly updates it's state.
pub struct Client {
    /// The current client session containing our user id, device id and access
    /// token.
    pub session: Option<Session>,
    /// The current sync token that should be used for the next sync call.
    pub sync_token: Option<Token>,
    /// A map of the rooms our user is joined in.
    pub joined_rooms: HashMap<String, Arc<RwLock<Room>>>,
    #[cfg(feature = "encryption")]
    olm: Arc<Mutex<Option<OlmMachine>>>,
}

impl Client {
    /// Create a new client.
    ///
    /// # Arguments
    ///
    /// * `session` - An optional session if the user already has one from a
    /// previous login call.
    pub fn new(session: Option<Session>) -> Result<Self> {
        #[cfg(feature = "encryption")]
        let olm = match &session {
            Some(s) => Some(OlmMachine::new(&s.user_id, &s.device_id)?),
            None => None,
        };

        Ok(Client {
            session,
            sync_token: None,
            joined_rooms: HashMap::new(),
            #[cfg(feature = "encryption")]
            olm: Arc::new(Mutex::new(olm)),
        })
    }

    /// Is the client logged in.
    pub fn logged_in(&self) -> bool {
        self.session.is_some()
    }

    /// Receive a login response and update the session of the client.
    ///
    /// # Arguments
    ///
    /// * `response` - A successful login response that contains our access token
    /// and device id.
    pub async fn receive_login_response(
        &mut self,
        response: &api::session::login::Response,
    ) -> Result<()> {
        let session = Session {
            access_token: response.access_token.clone(),
            device_id: response.device_id.clone(),
            user_id: response.user_id.clone(),
        };
        self.session = Some(session);

        #[cfg(feature = "encryption")]
        {
            let mut olm = self.olm.lock().await;
            *olm = Some(OlmMachine::new(&response.user_id, &response.device_id)?);
        }

        Ok(())
    }

    pub(crate) fn calculate_room_name(&self, room_id: &str) -> Option<String> {
        self.joined_rooms.get(room_id).and_then(|r| {
            r.read()
                .map(|r| r.room_name.calculate_name(room_id, &r.members))
                .ok()
        })
    }

    pub(crate) fn calculate_room_names(&self) -> Vec<String> {
        self.joined_rooms
            .iter()
            .flat_map(|(id, room)| {
                room.read()
                    .map(|r| r.room_name.calculate_name(id, &r.members))
                    .ok()
            })
            .collect()
    }

    pub(crate) fn get_or_create_room(&mut self, room_id: &str) -> &mut Arc<RwLock<Room>> {
        #[allow(clippy::or_fun_call)]
        self.joined_rooms
            .entry(room_id.to_string())
            .or_insert(Arc::new(RwLock::new(Room::new(
                room_id,
                &self
                    .session
                    .as_ref()
                    .expect("Receiving events while not being logged in")
                    .user_id
                    .to_string(),
            ))))
    }

    /// Receive a timeline event for a joined room and update the client state.
    ///
    /// If the event was a encrypted room event and decryption was successful
    /// the decrypted event will be returned, otherwise None.
    ///
    /// # Arguments
    ///
    /// * `room_id` - The unique id of the room the event belongs to.
    ///
    /// * `event` - The event that should be handled by the client.
    pub async fn receive_joined_timeline_event(
        &mut self,
        room_id: &RoomId,
        event: &mut EventResult<RoomEvent>,
    ) -> Option<EventResult<RoomEvent>> {
        match event {
            EventResult::Ok(e) => {
                #[cfg(feature = "encryption")]
                let mut decrypted_event = None;
                #[cfg(not(feature = "encryption"))]
                let decrypted_event = None;

                #[cfg(feature = "encryption")]
                {
                    match e {
                        RoomEvent::RoomEncrypted(e) => {
                            e.room_id = Some(room_id.to_owned());
                            let mut olm = self.olm.lock().await;

                            if let Some(o) = &mut *olm {
                                decrypted_event = o.decrypt_room_event(e).await.ok();
                            }
                        }
                        _ => (),
                    }
                }

                let mut room = self
                    .get_or_create_room(&room_id.to_string())
                    .write()
                    .unwrap();
                room.receive_timeline_event(e);
                decrypted_event
            }
            _ => None,
        }
    }

    /// Receive a state event for a joined room and update the client state.
    ///
    /// Returns true if the membership list of the room changed, false
    /// otherwise.
    ///
    /// # Arguments
    ///
    /// * `room_id` - The unique id of the room the event belongs to.
    ///
    /// * `event` - The event that should be handled by the client.
    pub fn receive_joined_state_event(&mut self, room_id: &str, event: &StateEvent) -> bool {
        let mut room = self.get_or_create_room(room_id).write().unwrap();
        room.receive_state_event(event)
    }

    /// Receive a response from a sync call.
    ///
    /// # Arguments
    ///
    /// * `response` - The response that we received after a successful sync.
    pub async fn receive_sync_response(
        &mut self,
        response: &mut api::sync::sync_events::IncomingResponse,
    ) {
        self.sync_token = Some(response.next_batch.clone());

        #[cfg(feature = "encryption")]
        {
            let mut olm = self.olm.lock().await;

            if let Some(o) = &mut *olm {
                o.receive_sync_response(response).await;
            }
        }
    }

    /// Should account or one-time keys be uploaded to the server.
    #[cfg(feature = "encryption")]
    #[cfg_attr(docsrs, doc(cfg(feature = "encryption")))]
    pub async fn should_upload_keys(&self) -> bool {
        let olm = self.olm.lock().await;

        match &*olm {
            Some(o) => o.should_upload_keys().await,
            None => false,
        }
    }

    /// Get a tuple of device and one-time keys that need to be uploaded.
    ///
    /// Returns an empty error if no keys need to be uploaded.
    #[cfg(feature = "encryption")]
    #[cfg_attr(docsrs, doc(cfg(feature = "encryption")))]
    pub async fn keys_for_upload(
        &self,
    ) -> StdResult<(Option<DeviceKeys>, Option<OneTimeKeys>), ()> {
        let olm = self.olm.lock().await;

        match &*olm {
            Some(o) => o.keys_for_upload().await,
            None => Err(()),
        }
    }

    /// Receive a successful keys upload response.
    ///
    /// # Arguments
    ///
    /// * `response` - The keys upload response of the request that the client
    ///     performed.
    ///
    /// # Panics
    /// Panics if the client hasn't been logged in.
    #[cfg(feature = "encryption")]
    #[cfg_attr(docsrs, doc(cfg(feature = "encryption")))]
    pub async fn receive_keys_upload_response(&self, response: &KeysUploadResponse) -> Result<()> {
        let mut olm = self.olm.lock().await;

        let o = olm.as_mut().expect("Client isn't logged in.");
        o.receive_keys_upload_response(response).await?;
        Ok(())
    }
}
