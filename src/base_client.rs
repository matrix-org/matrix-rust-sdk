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

#[cfg(feature = "encryption")]
use std::result::Result as StdResult;

use crate::api::r0 as api;
use crate::error::Result;
use crate::events::collections::all::{RoomEvent, StateEvent};
use crate::events::presence::PresenceEvent;
// `NonRoomEvent` is what it is aliased as
use crate::events::collections::only::Event as NonRoomEvent;
use crate::events::ignored_user_list::IgnoredUserListEvent;
use crate::events::push_rules::{PushRulesEvent, Ruleset};
use crate::events::room::{
    aliases::AliasesEvent,
    canonical_alias::CanonicalAliasEvent,
    member::{MemberEvent, MembershipState},
    name::NameEvent,
};
use crate::events::EventResult;
use crate::identifiers::{RoomAliasId, UserId as Uid};
use crate::models::Room;
use crate::session::Session;
use std::sync::{Arc, RwLock};

use js_int::UInt;

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

#[derive(Clone, Debug, Default)]
pub struct CurrentRoom {
    last_active: Option<UInt>,
    current_room_id: Option<RoomId>,
}

impl CurrentRoom {
    pub(crate) fn comes_after(&self, user: &Uid, event: &PresenceEvent) -> bool {
        if user == &event.sender {
            event.content.last_active_ago < self.last_active
        } else {
            false
        }
    }

    pub(crate) fn update(&mut self, room_id: &str, event: &PresenceEvent) {
        self.last_active = event.content.last_active_ago;
        self.current_room_id =
            Some(RoomId::try_from(room_id).expect("room id failed CurrentRoom::update"));
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
    /// The most recent room the logged in user used by `RoomId`.
    pub current_room_id: CurrentRoom,
    /// A list of ignored users.
    pub ignored_users: Vec<UserId>,
    /// The push ruleset for the logged in user.
    pub push_ruleset: Option<Ruleset>,

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
            current_room_id: CurrentRoom::default(),
            ignored_users: Vec::new(),
            push_ruleset: None,
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

    pub(crate) fn current_room_id(&self) -> Option<RoomId> {
        self.current_room_id.current_room_id.clone()
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

    /// Handle a m.ignored_user_list event, updating the room state if necessary.
    ///
    /// Returns true if the room name changed, false otherwise.
    pub(crate) fn handle_ignored_users(&mut self, event: &IgnoredUserListEvent) -> bool {
        // TODO use actual UserId instead of string?
        if self.ignored_users
            == event
                .content
                .ignored_users
                .iter()
                .map(|u| u.to_string())
                .collect::<Vec<String>>()
        {
            false
        } else {
            self.ignored_users = event
                .content
                .ignored_users
                .iter()
                .map(|u| u.to_string())
                .collect();
            true
        }
    }

    /// Handle a m.ignored_user_list event, updating the room state if necessary.
    ///
    /// Returns true if the room name changed, false otherwise.
    pub(crate) fn handle_push_rules(&mut self, event: &PushRulesEvent) -> bool {
        // TODO this is basically a stub
        if self.push_ruleset.as_ref() == Some(&event.content.global) {
            false
        } else {
            self.push_ruleset = Some(event.content.global.clone());
            true
        }
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

    /// Receive a presence event from an `IncomingResponse` and updates the client state.
    ///
    /// Returns true if the membership list of the room changed, false
    /// otherwise.
    ///
    /// # Arguments
    ///
    /// * `room_id` - The unique id of the room the event belongs to.
    ///
    /// * `event` - The event that should be handled by the client.
    pub fn receive_presence_event(&mut self, room_id: &str, event: &PresenceEvent) -> bool {
        let user_id = &self
            .session
            .as_ref()
            .expect("to receive events you must be logged in")
            .user_id;
        if self.current_room_id.comes_after(user_id, event) {
            self.current_room_id.update(room_id, event);
        }
        // this should be guaranteed to find the room that was just created in the `Client::sync` loop.
        let mut room = self.get_or_create_room(room_id).write().unwrap();
        room.receive_presence_event(event)
    }

    /// Receive a presence event from an `IncomingResponse` and updates the client state.
    ///
    /// This will only update the user if found in the current room looped through by `AsyncClient::sync`.
    /// Returns true if the specific users presence has changed, false otherwise.
    ///
    /// # Arguments
    ///
    /// * `event` - The presence event for a specified room member.
    pub fn receive_account_data(&mut self, room_id: &str, event: &NonRoomEvent) -> bool {
        match event {
            NonRoomEvent::IgnoredUserList(iu) => self.handle_ignored_users(iu),
            NonRoomEvent::Presence(p) => self.receive_presence_event(room_id, p),
            NonRoomEvent::PushRules(pr) => self.handle_push_rules(pr),
            _ => false,
        }
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
