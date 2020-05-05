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
use std::collections::{BTreeMap, HashSet};
use std::fmt;
use std::sync::Arc;

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
use crate::events::EventJson;
use crate::identifiers::{RoomId, UserId};
use crate::models::Room;
use crate::session::Session;
use crate::state::{ClientState, StateStore};
use crate::EventEmitter;

#[cfg(feature = "encryption")]
use tokio::sync::Mutex;
use tokio::sync::RwLock;

#[cfg(feature = "encryption")]
use crate::api::r0::keys::{
    claim_keys::Response as KeysClaimResponse, get_keys::Response as KeysQueryResponse,
    upload_keys::Response as KeysUploadResponse, DeviceKeys, KeyAlgorithm,
};
#[cfg(feature = "encryption")]
use crate::api::r0::to_device::send_event_to_device;
#[cfg(feature = "encryption")]
use crate::events::room::message::MessageEventContent;
#[cfg(feature = "encryption")]
use crate::identifiers::DeviceId;
#[cfg(feature = "encryption")]
use matrix_sdk_crypto::{OlmMachine, OneTimeKeys};

pub type Token = String;

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
    pub joined_rooms: HashMap<RoomId, Arc<RwLock<Room>>>,
    /// A list of ignored users.
    pub ignored_users: Vec<UserId>,
    /// The push ruleset for the logged in user.
    pub push_ruleset: Option<Ruleset>,
    /// Any implementor of EventEmitter will act as the callbacks for various
    /// events.
    pub event_emitter: Option<Box<dyn EventEmitter>>,
    /// Any implementor of `StateStore` will be called to save `Room` and
    /// some `BaseClient` state during `AsyncClient::sync` calls.
    ///
    /// There is a default implementation `JsonStore` that saves JSON to disk.
    pub state_store: Option<Box<dyn StateStore>>,
    /// Does the `Client` need to sync with the state store.
    needs_state_store_sync: bool,

    #[cfg(feature = "encryption")]
    olm: Arc<Mutex<Option<OlmMachine>>>,
}

impl fmt::Debug for Client {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Client")
            .field("session", &self.session)
            .field("sync_token", &self.sync_token)
            .field("joined_rooms", &self.joined_rooms)
            .field("ignored_users", &self.ignored_users)
            .field("push_ruleset", &self.push_ruleset)
            .field("event_emitter", &"EventEmitter<...>")
            .finish()
    }
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
            Some(s) => Some(OlmMachine::new(&s.user_id, &s.device_id)),
            None => None,
        };

        Ok(Client {
            session,
            sync_token: None,
            joined_rooms: HashMap::new(),
            ignored_users: Vec::new(),
            push_ruleset: None,
            event_emitter: None,
            state_store: None,
            needs_state_store_sync: true,
            #[cfg(feature = "encryption")]
            olm: Arc::new(Mutex::new(olm)),
        })
    }

    /// Is the client logged in.
    pub fn logged_in(&self) -> bool {
        self.session.is_some()
    }

    /// Add `EventEmitter` to `Client`.
    ///
    /// The methods of `EventEmitter` are called when the respective `RoomEvents` occur.
    pub async fn add_event_emitter(&mut self, emitter: Box<dyn EventEmitter>) {
        self.event_emitter = Some(emitter);
    }

    /// Returns true if the state store has been loaded into the client.
    pub fn is_state_store_synced(&self) -> bool {
        !self.needs_state_store_sync
    }

    /// When a client is provided the state store will load state from the `StateStore`.
    ///
    /// Returns `true` when a state store sync has successfully completed.
    pub(crate) async fn sync_with_state_store(&mut self) -> Result<bool> {
        if let Some(store) = self.state_store.as_ref() {
            if let Some(sess) = self.session.as_ref() {
                if let Some(client_state) = store.load_client_state(sess).await? {
                    let ClientState {
                        sync_token,
                        ignored_users,
                        push_ruleset,
                    } = client_state;
                    self.sync_token = sync_token;
                    self.ignored_users = ignored_users;
                    self.push_ruleset = push_ruleset;
                } else {
                    // return false and continues with a sync request then save the state and create
                    // and populate the files during the sync
                    return Ok(false);
                }

                let mut rooms = store.load_all_rooms().await?;
                self.joined_rooms = rooms
                    .drain()
                    .map(|(k, room)| (k, Arc::new(RwLock::new(room))))
                    .collect();

                self.needs_state_store_sync = false;
            }
        }
        Ok(!self.needs_state_store_sync)
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
            *olm = Some(OlmMachine::new(&response.user_id, &response.device_id));
        }

        Ok(())
    }

    pub(crate) async fn calculate_room_name(&self, room_id: &RoomId) -> Option<String> {
        if let Some(room) = self.joined_rooms.get(room_id) {
            let room = room.read().await;
            Some(room.room_name.calculate_name(&room.members))
        } else {
            None
        }
    }

    pub(crate) async fn calculate_room_names(&self) -> Vec<String> {
        let mut res = Vec::new();
        for room in self.joined_rooms.values() {
            let room = room.read().await;
            res.push(room.room_name.calculate_name(&room.members))
        }
        res
    }

    pub(crate) fn get_or_create_room(&mut self, room_id: &RoomId) -> &mut Arc<RwLock<Room>> {
        #[allow(clippy::or_fun_call)]
        self.joined_rooms
            .entry(room_id.clone())
            .or_insert(Arc::new(RwLock::new(Room::new(
                room_id,
                &self
                    .session
                    .as_ref()
                    .expect("Receiving events while not being logged in")
                    .user_id,
            ))))
    }

    pub(crate) fn get_room(&self, room_id: &RoomId) -> Option<&Arc<RwLock<Room>>> {
        self.joined_rooms.get(room_id)
    }

    /// Handle a m.ignored_user_list event, updating the room state if necessary.
    ///
    /// Returns true if the room name changed, false otherwise.
    pub(crate) fn handle_ignored_users(&mut self, event: &IgnoredUserListEvent) -> bool {
        // this avoids cloning every UserId for the eq check
        if self.ignored_users.iter().collect::<Vec<_>>()
            == event.content.ignored_users.iter().collect::<Vec<_>>()
        {
            false
        } else {
            self.ignored_users = event.content.ignored_users.to_vec();
            true
        }
    }

    /// Handle a m.ignored_user_list event, updating the room state if necessary.
    ///
    /// Returns true if the room name changed, false otherwise.
    pub(crate) fn handle_push_rules(&mut self, event: &PushRulesEvent) -> bool {
        // TODO this is basically a stub
        // TODO ruma removed PartialEq for evens, so this doesn't work anymore.
        // Returning always true for now should be ok here since those don't
        // change often.
        // if self.push_ruleset.as_ref() == Some(&event.content.global) {
        //     false
        // } else {
        self.push_ruleset = Some(event.content.global.clone());
        true
        // }
    }

    /// Receive a timeline event for a joined room and update the client state.
    ///
    /// Returns a tuple of the successfully decrypted event, or None on failure and
    /// a bool, true when the `Room` state has been updated.
    ///
    /// # Arguments
    ///
    /// * `room_id` - The unique id of the room the event belongs to.
    ///
    /// * `event` - The event that should be handled by the client.
    pub async fn receive_joined_timeline_event(
        &mut self,
        room_id: &RoomId,
        event: &mut EventJson<RoomEvent>,
    ) -> (Option<EventJson<RoomEvent>>, bool) {
        match event.deserialize() {
            #[allow(unused_mut)]
            Ok(mut e) => {
                #[cfg(feature = "encryption")]
                let mut decrypted_event = None;
                #[cfg(not(feature = "encryption"))]
                let decrypted_event = None;

                #[cfg(feature = "encryption")]
                {
                    if let RoomEvent::RoomEncrypted(ref mut e) = e {
                        e.room_id = Some(room_id.to_owned());
                        let mut olm = self.olm.lock().await;

                        if let Some(o) = &mut *olm {
                            decrypted_event = o.decrypt_room_event(&e).await.ok();
                        }
                    }
                }

                let mut room = self.get_or_create_room(&room_id).write().await;
                (decrypted_event, room.receive_timeline_event(&e))
            }
            _ => (None, false),
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
    pub async fn receive_joined_state_event(
        &mut self,
        room_id: &RoomId,
        event: &StateEvent,
    ) -> bool {
        let mut room = self.get_or_create_room(room_id).write().await;
        room.receive_state_event(event)
    }

    /// Receive a presence event from a sync response and updates the client state.
    ///
    /// Returns true if the membership list of the room changed, false
    /// otherwise.
    ///
    /// # Arguments
    ///
    /// * `room_id` - The unique id of the room the event belongs to.
    ///
    /// * `event` - The event that should be handled by the client.
    pub async fn receive_presence_event(
        &mut self,
        room_id: &RoomId,
        event: &PresenceEvent,
    ) -> bool {
        // this should be the room that was just created in the `Client::sync` loop.
        if let Some(room) = self.get_room(room_id) {
            let mut room = room.write().await;
            room.receive_presence_event(event)
        } else {
            false
        }
    }

    /// Receive an account data event from a sync response and updates the client state.
    ///
    /// Returns true if the state of the `Room` has changed, false otherwise.
    ///
    /// # Arguments
    ///
    /// * `room_id` - The unique id of the room the event belongs to.
    ///
    /// * `event` - The presence event for a specified room member.
    pub async fn receive_account_data_event(
        &mut self,
        room_id: &RoomId,
        event: &NonRoomEvent,
    ) -> bool {
        match event {
            NonRoomEvent::IgnoredUserList(iu) => self.handle_ignored_users(iu),
            NonRoomEvent::Presence(p) => self.receive_presence_event(room_id, p).await,
            NonRoomEvent::PushRules(pr) => self.handle_push_rules(pr),
            _ => false,
        }
    }

    /// Receive an ephemeral event from a sync response and updates the client state.
    ///
    /// Returns true if the state of the `Room` has changed, false otherwise.
    ///
    /// # Arguments
    ///
    /// * `room_id` - The unique id of the room the event belongs to.
    ///
    /// * `event` - The presence event for a specified room member.
    pub async fn receive_ephemeral_event(
        &mut self,
        room_id: &RoomId,
        event: &NonRoomEvent,
    ) -> bool {
        match event {
            NonRoomEvent::IgnoredUserList(iu) => self.handle_ignored_users(iu),
            NonRoomEvent::Presence(p) => self.receive_presence_event(room_id, p).await,
            NonRoomEvent::PushRules(pr) => self.handle_push_rules(pr),
            _ => false,
        }
    }

    /// Receive a response from a sync call.
    ///
    /// # Arguments
    ///
    /// * `response` - The response that we received after a successful sync.
    ///
    /// * `did_update` - Signals to the `StateStore` if the client state needs updating.
    pub async fn receive_sync_response(
        &mut self,
        response: &mut api::sync::sync_events::Response,
        did_update: bool,
    ) -> Result<()> {
        self.sync_token = Some(response.next_batch.clone());

        #[cfg(feature = "encryption")]
        {
            let mut olm = self.olm.lock().await;

            if let Some(o) = &mut *olm {
                o.receive_sync_response(response).await;

                // TODO once the base client deals with callbacks move this into the
                // part where we already iterate through the rooms to avoid yet
                // another room loop.
                for room in self.joined_rooms.values() {
                    let room = room.write().await;
                    if !room.is_encrypted() {
                        continue;
                    }

                    o.update_tracked_users(room.members.keys()).await;
                }
            }
        }

        if did_update {
            if let Some(store) = self.state_store.as_ref() {
                let state = ClientState::from_base_client(&self);
                store.store_client_state(state).await?;
            }
        }
        Ok(())
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

    /// Should the client share a group session for the given room.
    ///
    /// Returns true if a session needs to be shared before room messages can be
    /// encrypted, false if one is already shared and ready to encrypt room
    /// messages.
    ///
    /// This should be called every time a new room message wants to be sent out
    /// since group sessions can expire at any time.
    #[cfg(feature = "encryption")]
    #[cfg_attr(docsrs, doc(cfg(feature = "encryption")))]
    pub async fn should_share_group_session(&self, room_id: &RoomId) -> bool {
        let olm = self.olm.lock().await;

        match &*olm {
            Some(o) => o.should_share_group_session(room_id),
            None => false,
        }
    }

    /// Should users be queried for their device keys.
    #[cfg(feature = "encryption")]
    #[cfg_attr(docsrs, doc(cfg(feature = "encryption")))]
    pub async fn should_query_keys(&self) -> bool {
        let olm = self.olm.lock().await;

        match &*olm {
            Some(o) => o.should_query_keys(),
            None => false,
        }
    }

    /// Get a tuple of device and one-time keys that need to be uploaded.
    ///
    /// Returns an empty error if no keys need to be uploaded.
    #[cfg(feature = "encryption")]
    #[cfg_attr(docsrs, doc(cfg(feature = "encryption")))]
    pub async fn get_missing_sessions(
        &self,
        users: impl Iterator<Item = &UserId>,
    ) -> Result<BTreeMap<UserId, BTreeMap<DeviceId, KeyAlgorithm>>> {
        let mut olm = self.olm.lock().await;

        match &mut *olm {
            Some(o) => Ok(o.get_missing_sessions(users).await?),
            None => Ok(BTreeMap::new()),
        }
    }

    /// Get a to-device request that will share a group session for a room.
    #[cfg(feature = "encryption")]
    #[cfg_attr(docsrs, doc(cfg(feature = "encryption")))]
    pub async fn share_group_session(
        &self,
        room_id: &RoomId,
    ) -> Result<Vec<send_event_to_device::Request>> {
        let room = self.get_room(room_id).expect("No room found");
        let mut olm = self.olm.lock().await;

        match &mut *olm {
            Some(o) => {
                let room = room.write().await;
                let members = room.members.keys();
                Ok(o.share_group_session(room_id, members).await?)
            }
            None => panic!("Olm machine wasn't started"),
        }
    }

    /// Encrypt a message event content.
    #[cfg(feature = "encryption")]
    #[cfg_attr(docsrs, doc(cfg(feature = "encryption")))]
    pub async fn encrypt(
        &self,
        room_id: &RoomId,
        content: MessageEventContent,
    ) -> Result<MessageEventContent> {
        let mut olm = self.olm.lock().await;

        // TODO enable this again once we can send encrypted event
        // contents with ruma.
        // match &mut *olm {
        //     Some(o) => Ok(MessageEventContent::Encrypted(
        //         o.encrypt(room_id, content).await?,
        //     )),
        //     None => panic!("Olm machine wasn't started"),
        // }
        Ok(content)
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

    /// Get the users that we need to query keys for.
    ///
    /// Returns an empty error if no keys need to be queried.
    #[cfg(feature = "encryption")]
    #[cfg_attr(docsrs, doc(cfg(feature = "encryption")))]
    pub async fn users_for_key_query(&self) -> StdResult<HashSet<UserId>, ()> {
        let olm = self.olm.lock().await;

        match &*olm {
            Some(o) => Ok(o.users_for_key_query()),
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

    /// Receive a successful keys claim response.
    ///
    /// # Arguments
    ///
    /// * `response` - The keys claim response of the request that the client
    /// performed.
    ///
    /// # Panics
    /// Panics if the client hasn't been logged in.
    #[cfg(feature = "encryption")]
    #[cfg_attr(docsrs, doc(cfg(feature = "encryption")))]
    pub async fn receive_keys_claim_response(&self, response: &KeysClaimResponse) -> Result<()> {
        let mut olm = self.olm.lock().await;

        let o = olm.as_mut().expect("Client isn't logged in.");
        o.receive_keys_claim_response(response).await?;
        Ok(())
    }

    /// Receive a successful keys query response.
    ///
    /// # Arguments
    ///
    /// * `response` - The keys query response of the request that the client
    /// performed.
    ///
    /// # Panics
    /// Panics if the client hasn't been logged in.
    #[cfg(feature = "encryption")]
    #[cfg_attr(docsrs, doc(cfg(feature = "encryption")))]
    pub async fn receive_keys_query_response(&self, response: &KeysQueryResponse) -> Result<()> {
        let mut olm = self.olm.lock().await;

        let o = olm.as_mut().expect("Client isn't logged in.");
        o.receive_keys_query_response(response).await?;
        // TODO notify our callers of new devices via some callback.
        Ok(())
    }

    pub(crate) async fn emit_timeline_event(&self, room_id: &RoomId, event: &RoomEvent) {
        match event {
            RoomEvent::RoomMember(mem) => {
                if let Some(ee) = &self.event_emitter {
                    if let Some(room) = self.get_room(&room_id) {
                        ee.on_room_member(Arc::clone(&room), &mem).await;
                    }
                }
            }
            RoomEvent::RoomName(name) => {
                if let Some(ee) = &self.event_emitter {
                    if let Some(room) = self.get_room(&room_id) {
                        ee.on_room_name(Arc::clone(&room), &name).await;
                    }
                }
            }
            RoomEvent::RoomCanonicalAlias(canonical) => {
                if let Some(ee) = &self.event_emitter {
                    if let Some(room) = self.get_room(&room_id) {
                        ee.on_room_canonical_alias(Arc::clone(&room), &canonical)
                            .await;
                    }
                }
            }
            RoomEvent::RoomAliases(aliases) => {
                if let Some(ee) = &self.event_emitter {
                    if let Some(room) = self.get_room(&room_id) {
                        ee.on_room_aliases(Arc::clone(&room), &aliases).await;
                    }
                }
            }
            RoomEvent::RoomAvatar(avatar) => {
                if let Some(ee) = &self.event_emitter {
                    if let Some(room) = self.get_room(&room_id) {
                        ee.on_room_avatar(Arc::clone(&room), &avatar).await;
                    }
                }
            }
            RoomEvent::RoomMessage(msg) => {
                if let Some(ee) = &self.event_emitter {
                    if let Some(room) = self.get_room(&room_id) {
                        ee.on_room_message(Arc::clone(&room), &msg).await;
                    }
                }
            }
            RoomEvent::RoomMessageFeedback(msg_feedback) => {
                if let Some(ee) = &self.event_emitter {
                    if let Some(room) = self.get_room(&room_id) {
                        ee.on_room_message_feedback(Arc::clone(&room), &msg_feedback)
                            .await;
                    }
                }
            }
            RoomEvent::RoomRedaction(redaction) => {
                if let Some(ee) = &self.event_emitter {
                    if let Some(room) = self.get_room(&room_id) {
                        ee.on_room_redaction(Arc::clone(&room), &redaction).await;
                    }
                }
            }
            RoomEvent::RoomPowerLevels(power) => {
                if let Some(ee) = &self.event_emitter {
                    if let Some(room) = self.get_room(&room_id) {
                        ee.on_room_power_levels(Arc::clone(&room), &power).await;
                    }
                }
            }
            RoomEvent::RoomTombstone(tomb) => {
                if let Some(ee) = &self.event_emitter {
                    if let Some(room) = self.get_room(&room_id) {
                        ee.on_room_tombstone(Arc::clone(&room), &tomb).await;
                    }
                }
            }
            _ => {}
        }
    }

    pub(crate) async fn emit_state_event(&self, room_id: &RoomId, event: &StateEvent) {
        match event {
            StateEvent::RoomMember(member) => {
                if let Some(ee) = &self.event_emitter {
                    if let Some(room) = self.get_room(&room_id) {
                        ee.on_state_member(Arc::clone(&room), &member).await;
                    }
                }
            }
            StateEvent::RoomName(name) => {
                if let Some(ee) = &self.event_emitter {
                    if let Some(room) = self.get_room(&room_id) {
                        ee.on_state_name(Arc::clone(&room), &name).await;
                    }
                }
            }
            StateEvent::RoomCanonicalAlias(canonical) => {
                if let Some(ee) = &self.event_emitter {
                    if let Some(room) = self.get_room(&room_id) {
                        ee.on_state_canonical_alias(Arc::clone(&room), &canonical)
                            .await;
                    }
                }
            }
            StateEvent::RoomAliases(aliases) => {
                if let Some(ee) = &self.event_emitter {
                    if let Some(room) = self.get_room(&room_id) {
                        ee.on_state_aliases(Arc::clone(&room), &aliases).await;
                    }
                }
            }
            StateEvent::RoomAvatar(avatar) => {
                if let Some(ee) = &self.event_emitter {
                    if let Some(room) = self.get_room(&room_id) {
                        ee.on_state_avatar(Arc::clone(&room), &avatar).await;
                    }
                }
            }
            StateEvent::RoomPowerLevels(power) => {
                if let Some(ee) = &self.event_emitter {
                    if let Some(room) = self.get_room(&room_id) {
                        ee.on_state_power_levels(Arc::clone(&room), &power).await;
                    }
                }
            }
            StateEvent::RoomJoinRules(rules) => {
                if let Some(ee) = &self.event_emitter {
                    if let Some(room) = self.get_room(&room_id) {
                        ee.on_state_join_rules(Arc::clone(&room), &rules).await;
                    }
                }
            }
            StateEvent::RoomTombstone(tomb) => {
                if let Some(ee) = &self.event_emitter {
                    if let Some(room) = self.get_room(&room_id) {
                        ee.on_room_tombstone(Arc::clone(&room), &tomb).await;
                    }
                }
            }
            _ => {}
        }
    }

    pub(crate) async fn emit_account_data_event(&self, room_id: &RoomId, event: &NonRoomEvent) {
        match event {
            NonRoomEvent::Presence(presence) => {
                if let Some(ee) = &self.event_emitter {
                    if let Some(room) = self.get_room(&room_id) {
                        ee.on_account_presence(Arc::clone(&room), &presence).await;
                    }
                }
            }
            NonRoomEvent::IgnoredUserList(ignored) => {
                if let Some(ee) = &self.event_emitter {
                    if let Some(room) = self.get_room(&room_id) {
                        ee.on_account_ignored_users(Arc::clone(&room), &ignored)
                            .await;
                    }
                }
            }
            NonRoomEvent::PushRules(rules) => {
                if let Some(ee) = &self.event_emitter {
                    if let Some(room) = self.get_room(&room_id) {
                        ee.on_account_push_rules(Arc::clone(&room), &rules).await;
                    }
                }
            }
            NonRoomEvent::FullyRead(full_read) => {
                if let Some(ee) = &self.event_emitter {
                    if let Some(room) = self.get_room(&room_id) {
                        ee.on_account_data_fully_read(Arc::clone(&room), &full_read)
                            .await;
                    }
                }
            }
            _ => {}
        }
    }

    pub(crate) async fn emit_ephemeral_event(&self, room_id: &RoomId, event: &NonRoomEvent) {
        match event {
            NonRoomEvent::Presence(presence) => {
                if let Some(ee) = &self.event_emitter {
                    if let Some(room) = self.get_room(&room_id) {
                        ee.on_account_presence(Arc::clone(&room), &presence).await;
                    }
                }
            }
            NonRoomEvent::IgnoredUserList(ignored) => {
                if let Some(ee) = &self.event_emitter {
                    if let Some(room) = self.get_room(&room_id) {
                        ee.on_account_ignored_users(Arc::clone(&room), &ignored)
                            .await;
                    }
                }
            }
            NonRoomEvent::PushRules(rules) => {
                if let Some(ee) = &self.event_emitter {
                    if let Some(room) = self.get_room(&room_id) {
                        ee.on_account_push_rules(Arc::clone(&room), &rules).await;
                    }
                }
            }
            NonRoomEvent::FullyRead(full_read) => {
                if let Some(ee) = &self.event_emitter {
                    if let Some(room) = self.get_room(&room_id) {
                        ee.on_account_data_fully_read(Arc::clone(&room), &full_read)
                            .await;
                    }
                }
            }
            _ => {}
        }
    }

    pub(crate) async fn emit_presence_event(&self, room_id: &RoomId, event: &PresenceEvent) {
        if let Some(ee) = &self.event_emitter {
            if let Some(room) = self.get_room(&room_id) {
                ee.on_presence_event(Arc::clone(&room), &event).await;
            }
        }
    }
}

#[cfg(test)]
mod test {

    use crate::identifiers::UserId;
    use crate::{AsyncClient, Session, SyncSettings};

    use mockito::{mock, Matcher};
    use url::Url;

    use std::convert::TryFrom;
    use std::str::FromStr;
    use std::time::Duration;

    #[tokio::test]
    async fn account_data() {
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
        .with_body_from_file("../test_data/sync.json")
        .create();

        let client = AsyncClient::new(homeserver, Some(session)).unwrap();

        let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

        let _response = client.sync(sync_settings).await.unwrap();

        let bc = &client.base_client.read().await;
        assert_eq!(1, bc.ignored_users.len())
    }
}
