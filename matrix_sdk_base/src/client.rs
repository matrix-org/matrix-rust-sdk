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
use std::path::{Path, PathBuf};
use std::sync::Arc;
use zeroize::Zeroizing;

use std::result::Result as StdResult;

use crate::api::r0 as api;
use crate::error::Result;
use crate::events::collections::all::{RoomEvent, StateEvent};
use crate::events::presence::PresenceEvent;
// `NonRoomEvent` is what it is aliased as
use crate::event_emitter::CustomOrRawEvent;
use crate::events::collections::only::Event as NonRoomEvent;
use crate::events::ignored_user_list::IgnoredUserListEvent;
use crate::events::push_rules::{PushRulesEvent, Ruleset};
use crate::events::room::member::MemberEventContent;
use crate::events::stripped::AnyStrippedStateEvent;
use crate::events::EventJson;
use crate::identifiers::{RoomId, UserId};
use crate::models::Room;
use crate::session::Session;
use crate::state::{AllRooms, ClientState, StateStore};
use crate::EventEmitter;

#[cfg(feature = "encryption")]
use matrix_sdk_common::locks::Mutex;
use matrix_sdk_common::locks::RwLock;
use std::ops::Deref;

#[cfg(feature = "encryption")]
use crate::api::r0::keys::{
    claim_keys::Response as KeysClaimResponse, get_keys::Response as KeysQueryResponse,
    upload_keys::Response as KeysUploadResponse, DeviceKeys, KeyAlgorithm,
};
#[cfg(feature = "encryption")]
use crate::api::r0::to_device::send_event_to_device;
#[cfg(feature = "encryption")]
use crate::events::room::{encrypted::EncryptedEventContent, message::MessageEventContent};
#[cfg(feature = "encryption")]
use crate::identifiers::DeviceId;
#[cfg(not(target_arch = "wasm32"))]
use crate::JsonStore;
#[cfg(feature = "encryption")]
use matrix_sdk_crypto::{CryptoStore, OlmError, OlmMachine, OneTimeKeys};

pub type Token = String;

/// A deserialization wrapper for extracting the prev_content field when
/// found in an `unsigned` field.
///
/// Represents the outer `unsigned` field
#[derive(serde::Deserialize)]
pub struct AdditionalEventData {
    unsigned: AdditionalUnsignedData,
}

/// A deserialization wrapper for extracting the prev_content field when
/// found in an `unsigned` field.
///
/// Represents the inner `prev_content` field
#[derive(serde::Deserialize)]
pub struct AdditionalUnsignedData {
    pub prev_content: Option<EventJson<MemberEventContent>>,
}

/// If a `prev_content` field is found inside of `unsigned` we move it up to the events `prev_content` field.
fn deserialize_prev_content(event: &EventJson<RoomEvent>) -> Option<EventJson<RoomEvent>> {
    let prev_content = serde_json::from_str::<AdditionalEventData>(event.json().get())
        .map(|more_unsigned| more_unsigned.unsigned)
        .map(|additional| additional.prev_content)
        .ok()
        .flatten()?;

    let mut ev = event.deserialize().ok()?;
    match &mut ev {
        RoomEvent::RoomMember(ref mut member) if member.prev_content.is_none() => {
            member.prev_content = prev_content.deserialize().ok();
            Some(EventJson::from(ev))
        }
        _ => None,
    }
}

fn stripped_deserialize_prev_content(
    event: &EventJson<AnyStrippedStateEvent>,
) -> Option<AdditionalUnsignedData> {
    serde_json::from_str::<AdditionalEventData>(event.json().get())
        .map(|more_unsigned| more_unsigned.unsigned)
        .ok()
}

/// Signals to the `BaseClient` which `RoomState` to send to `EventEmitter`.
#[derive(Debug)]
pub enum RoomStateType {
    /// Represents a joined room, the `joined_rooms` HashMap will be used.
    Joined,
    /// Represents a left room, the `left_rooms` HashMap will be used.
    Left,
    /// Represents an invited room, the `invited_rooms` HashMap will be used.
    Invited,
}

/// An enum that represents the state of the given `Room`.
///
/// If the event came from the `join`, `invite` or `leave` rooms map from the server
/// the variant that holds the corresponding room is used. `RoomState` is generic
/// so it can be used to represent a `Room` or an `Arc<RwLock<Room>>`
#[derive(Debug)]
pub enum RoomState<R> {
    /// A room from the `join` section of a sync response.
    Joined(R),
    /// A room from the `leave` section of a sync response.
    Left(R),
    /// A room from the `invite` section of a sync response.
    Invited(R),
}

/// A no IO Client implementation.
///
/// This Client is a state machine that receives responses and events and
/// accordingly updates it's state.
#[derive(Clone)]
pub struct BaseClient {
    /// The current client session containing our user id, device id and access
    /// token.
    session: Arc<RwLock<Option<Session>>>,
    /// The current sync token that should be used for the next sync call.
    pub(crate) sync_token: Arc<RwLock<Option<Token>>>,
    /// A map of the rooms our user is joined in.
    joined_rooms: Arc<RwLock<HashMap<RoomId, Arc<RwLock<Room>>>>>,
    /// A map of the rooms our user is invited to.
    invited_rooms: Arc<RwLock<HashMap<RoomId, Arc<RwLock<Room>>>>>,
    /// A map of the rooms our user has left.
    left_rooms: Arc<RwLock<HashMap<RoomId, Arc<RwLock<Room>>>>>,
    /// A list of ignored users.
    pub(crate) ignored_users: Arc<RwLock<Vec<UserId>>>,
    /// The push ruleset for the logged in user.
    pub(crate) push_ruleset: Arc<RwLock<Option<Ruleset>>>,
    /// Any implementor of EventEmitter will act as the callbacks for various
    /// events.
    event_emitter: Arc<RwLock<Option<Box<dyn EventEmitter>>>>,
    /// Any implementor of `StateStore` will be called to save `Room` and
    /// some `BaseClient` state after receiving a sync response.
    ///
    /// There is a default implementation `JsonStore` that saves JSON to disk.
    state_store: Arc<RwLock<Option<Box<dyn StateStore>>>>,

    #[cfg(feature = "encryption")]
    olm: Arc<Mutex<Option<OlmMachine>>>,
    #[cfg(feature = "encryption")]
    cryptostore: Arc<Mutex<Option<Box<dyn CryptoStore>>>>,
    store_path: Arc<Option<PathBuf>>,
    store_passphrase: Arc<Zeroizing<String>>,
}

#[cfg_attr(tarpaulin, skip)]
impl fmt::Debug for BaseClient {
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

/// Configuration for the creation of the `BaseClient`.
///
/// # Example
///
/// ```
/// # use matrix_sdk_base::BaseClientConfig;
///
/// let client_config = BaseClientConfig::new()
///     .store_path("/home/example/matrix-sdk-client")
///     .passphrase("test-passphrase".to_owned());
/// ```
#[derive(Default)]
pub struct BaseClientConfig {
    state_store: Option<Box<dyn StateStore>>,
    #[cfg(feature = "encryption")]
    crypto_store: Option<Box<dyn CryptoStore>>,
    store_path: Option<PathBuf>,
    passphrase: Option<Zeroizing<String>>,
}

#[cfg_attr(tarpaulin, skip)]
impl std::fmt::Debug for BaseClientConfig {
    fn fmt(&self, fmt: &mut std::fmt::Formatter<'_>) -> StdResult<(), std::fmt::Error> {
        fmt.debug_struct("BaseClientConfig").finish()
    }
}

impl BaseClientConfig {
    /// Create a new default `BaseClientConfig`.
    pub fn new() -> Self {
        Default::default()
    }

    /// Set a custom implementation of a `StateStore`.
    ///
    /// The state store should be opened before being set.
    pub fn state_store(mut self, store: Box<dyn StateStore>) -> Self {
        self.state_store = Some(store);
        self
    }

    /// Set a custom implementation of a `CryptoStore`.
    ///
    /// The crypto store should be opened before being set.
    #[cfg(feature = "encryption")]
    pub fn crypto_store(mut self, store: Box<dyn CryptoStore>) -> Self {
        self.crypto_store = Some(store);
        self
    }

    /// Set the path for storage.
    ///
    /// # Arguments
    ///
    /// * `path` - The path where the stores should save data in. It is the
    /// callers responsibility to make sure that the path exists.
    ///
    /// In the default configuration the client will open default
    /// implementations for the crypto store and the state store. It will use
    /// the given path to open the stores. If no path is provided no store will
    /// be opened
    pub fn store_path<P: AsRef<Path>>(mut self, path: P) -> Self {
        self.store_path = Some(path.as_ref().into());
        self
    }

    /// Set the passphrase to encrypt the crypto store.
    ///
    /// # Argument
    ///
    /// * `passphrase` - The passphrase that will be used to encrypt the data in
    /// the cryptostore.
    ///
    /// This is only used if no custom cryptostore is set.
    pub fn passphrase(mut self, passphrase: String) -> Self {
        self.passphrase = Some(Zeroizing::new(passphrase));
        self
    }
}

impl BaseClient {
    /// Create a new default client.
    pub fn new() -> Result<Self> {
        BaseClient::new_with_config(BaseClientConfig::default())
    }

    /// Create a new client.
    ///
    /// # Arguments
    ///
    /// * `config` - An optional session if the user already has one from a
    /// previous login call.
    pub fn new_with_config(config: BaseClientConfig) -> Result<Self> {
        Ok(BaseClient {
            session: Arc::new(RwLock::new(None)),
            sync_token: Arc::new(RwLock::new(None)),
            joined_rooms: Arc::new(RwLock::new(HashMap::new())),
            invited_rooms: Arc::new(RwLock::new(HashMap::new())),
            left_rooms: Arc::new(RwLock::new(HashMap::new())),
            ignored_users: Arc::new(RwLock::new(Vec::new())),
            push_ruleset: Arc::new(RwLock::new(None)),
            event_emitter: Arc::new(RwLock::new(None)),
            state_store: Arc::new(RwLock::new(config.state_store)),
            #[cfg(feature = "encryption")]
            olm: Arc::new(Mutex::new(None)),
            #[cfg(feature = "encryption")]
            cryptostore: Arc::new(Mutex::new(config.crypto_store)),
            store_path: Arc::new(config.store_path),
            store_passphrase: Arc::new(
                config
                    .passphrase
                    .unwrap_or_else(|| Zeroizing::new("DEFAULT_PASSPHRASE".to_owned())),
            ),
        })
    }

    /// The current client session containing our user id, device id and access
    /// token.
    pub fn session(&self) -> &Arc<RwLock<Option<Session>>> {
        &self.session
    }

    /// Is the client logged in.
    pub async fn logged_in(&self) -> bool {
        // TODO turn this into a atomic bool so this method doesn't need to be
        // async.
        self.session.read().await.is_some()
    }

    /// Add `EventEmitter` to `Client`.
    ///
    /// The methods of `EventEmitter` are called when the respective `RoomEvents` occur.
    pub async fn add_event_emitter(&self, emitter: Box<dyn EventEmitter>) {
        *self.event_emitter.write().await = Some(emitter);
    }

    /// When a client is provided the state store will load state from the `StateStore`.
    ///
    /// Returns `true` when a state store sync has successfully completed.
    async fn sync_with_state_store(&self, session: &Session) -> Result<bool> {
        let store = self.state_store.read().await;

        let loaded = if let Some(store) = store.as_ref() {
            if let Some(client_state) = store.load_client_state(session).await? {
                let ClientState {
                    sync_token,
                    ignored_users,
                    push_ruleset,
                } = client_state;
                *self.sync_token.write().await = sync_token;
                *self.ignored_users.write().await = ignored_users;
                *self.push_ruleset.write().await = push_ruleset;
            } else {
                // return false and continues with a sync request then save the state and create
                // and populate the files during the sync
                return Ok(false);
            }

            let AllRooms {
                mut joined,
                mut invited,
                mut left,
            } = store.load_all_rooms().await?;

            *self.joined_rooms.write().await = joined
                .drain()
                .map(|(k, room)| (k, Arc::new(RwLock::new(room))))
                .collect();

            *self.invited_rooms.write().await = invited
                .drain()
                .map(|(k, room)| (k, Arc::new(RwLock::new(room))))
                .collect();

            *self.left_rooms.write().await = left
                .drain()
                .map(|(k, room)| (k, Arc::new(RwLock::new(room))))
                .collect();

            true
        } else {
            false
        };

        Ok(loaded)
    }

    /// When a client is provided the state store will load state from the `StateStore`.
    ///
    /// Returns `true` when a state store sync has successfully completed.
    pub async fn store_room_state(&self, room_id: &RoomId) -> Result<()> {
        if let Some(store) = self.state_store.read().await.as_ref() {
            if let Some(room) = self.get_joined_room(room_id).await {
                let room = room.read().await;
                store
                    .store_room_state(RoomState::Joined(room.deref()))
                    .await?;
            }
            if let Some(room) = self.get_invited_room(room_id).await {
                let room = room.read().await;
                store
                    .store_room_state(RoomState::Invited(room.deref()))
                    .await?;
            }
            if let Some(room) = self.get_left_room(room_id).await {
                let room = room.read().await;
                store
                    .store_room_state(RoomState::Left(room.deref()))
                    .await?;
            }
        }
        Ok(())
    }

    /// Receive a login response and update the session of the client.
    ///
    /// # Arguments
    ///
    /// * `response` - A successful login response that contains our access token
    /// and device id.
    pub async fn receive_login_response(
        &self,
        response: &api::session::login::Response,
    ) -> Result<()> {
        let session = Session {
            access_token: response.access_token.clone(),
            device_id: response.device_id.clone(),
            user_id: response.user_id.clone(),
        };
        self.restore_login(session).await
    }

    /// Restore a previously logged in session.
    ///
    /// # Arguments
    ///
    /// * `session` - An session that the user already has from a
    /// previous login call.
    pub async fn restore_login(&self, session: Session) -> Result<()> {
        #[cfg(feature = "encryption")]
        {
            let mut olm = self.olm.lock().await;
            let store = self.cryptostore.lock().await.take();

            if let Some(store) = store {
                *olm = Some(
                    OlmMachine::new_with_store(
                        session.user_id.to_owned(),
                        session.device_id.to_owned(),
                        store,
                    )
                    .await
                    .map_err(OlmError::from)?,
                );
            } else if let Some(path) = self.store_path.as_ref() {
                #[cfg(feature = "sqlite-cryptostore")]
                {
                    *olm = Some(
                        OlmMachine::new_with_default_store(
                            &session.user_id,
                            &session.device_id,
                            path,
                            &self.store_passphrase,
                        )
                        .await
                        .map_err(OlmError::from)?,
                    );
                }
                #[cfg(not(feature = "sqlite-cryptostore"))]
                {
                    *olm = Some(OlmMachine::new(&session.user_id, &session.device_id));
                }
            } else {
                *olm = Some(OlmMachine::new(&session.user_id, &session.device_id));
            }
        }

        // If there wasn't a state store opened, try to open the default one if
        // a store path was provided.
        if self.state_store.read().await.is_none() {
            #[cfg(not(target_arch = "wasm32"))]
            {
                if let Some(path) = &*self.store_path {
                    let store = JsonStore::open(path)?;
                    *self.state_store.write().await = Some(Box::new(store));
                }
            }
        }

        self.sync_with_state_store(&session).await?;

        *self.session.write().await = Some(session);

        Ok(())
    }

    pub(crate) async fn get_or_create_joined_room(
        &self,
        room_id: &RoomId,
    ) -> Result<Arc<RwLock<Room>>> {
        // If this used to be an invited or left room remove them from our other
        // hashmaps.
        if self.invited_rooms.write().await.remove(room_id).is_some() {
            if let Some(store) = self.state_store.read().await.as_ref() {
                store.delete_room_state(RoomState::Invited(room_id)).await?;
            }
        }

        if self.left_rooms.write().await.remove(room_id).is_some() {
            if let Some(store) = self.state_store.read().await.as_ref() {
                store.delete_room_state(RoomState::Left(room_id)).await?;
            }
        }

        let mut rooms = self.joined_rooms.write().await;
        #[allow(clippy::or_fun_call)]
        Ok(rooms
            .entry(room_id.clone())
            .or_insert(Arc::new(RwLock::new(Room::new(
                room_id,
                &self
                    .session
                    .read()
                    .await
                    .as_ref()
                    .expect("Receiving events while not being logged in")
                    .user_id,
            ))))
            .clone())
    }

    /// Get a joined room with the given room id.
    ///
    /// # Arguments
    ///
    /// `room_id` - The unique id of the room that should be fetched.
    pub async fn get_joined_room(&self, room_id: &RoomId) -> Option<Arc<RwLock<Room>>> {
        self.joined_rooms.read().await.get(room_id).cloned()
    }

    /// Returns the joined rooms this client knows about.
    ///
    /// A `HashMap` of room id to `matrix::models::Room`
    pub fn joined_rooms(&self) -> Arc<RwLock<HashMap<RoomId, Arc<RwLock<Room>>>>> {
        self.joined_rooms.clone()
    }

    pub(crate) async fn get_or_create_invited_room(
        &self,
        room_id: &RoomId,
    ) -> Result<Arc<RwLock<Room>>> {
        // Remove the left rooms only here, since a join -> invite action per
        // spec can't happen.
        if self.left_rooms.write().await.remove(room_id).is_some() {
            if let Some(store) = self.state_store.read().await.as_ref() {
                store.delete_room_state(RoomState::Left(room_id)).await?;
            }
        }

        let mut rooms = self.invited_rooms.write().await;
        #[allow(clippy::or_fun_call)]
        Ok(rooms
            .entry(room_id.clone())
            .or_insert(Arc::new(RwLock::new(Room::new(
                room_id,
                &self
                    .session
                    .read()
                    .await
                    .as_ref()
                    .expect("Receiving events while not being logged in")
                    .user_id,
            ))))
            .clone())
    }

    /// Get an invited room with the given room id.
    ///
    /// # Arguments
    ///
    /// `room_id` - The unique id of the room that should be fetched.
    pub async fn get_invited_room(&self, room_id: &RoomId) -> Option<Arc<RwLock<Room>>> {
        self.invited_rooms.read().await.get(room_id).cloned()
    }

    /// Returns the invited rooms this client knows about.
    ///
    /// A `HashMap` of room id to `matrix::models::Room`
    pub fn invited_rooms(&self) -> Arc<RwLock<HashMap<RoomId, Arc<RwLock<Room>>>>> {
        self.invited_rooms.clone()
    }

    pub(crate) async fn get_or_create_left_room(
        &self,
        room_id: &RoomId,
    ) -> Result<Arc<RwLock<Room>>> {
        // If this used to be an invited or joined room remove them from our other
        // hashmaps.
        if self.invited_rooms.write().await.remove(room_id).is_some() {
            if let Some(store) = self.state_store.read().await.as_ref() {
                store.delete_room_state(RoomState::Invited(room_id)).await?;
            }
        }

        if self.joined_rooms.write().await.remove(room_id).is_some() {
            if let Some(store) = self.state_store.read().await.as_ref() {
                store.delete_room_state(RoomState::Joined(room_id)).await?;
            }
        }

        let mut rooms = self.left_rooms.write().await;
        #[allow(clippy::or_fun_call)]
        Ok(rooms
            .entry(room_id.clone())
            .or_insert(Arc::new(RwLock::new(Room::new(
                room_id,
                &self
                    .session
                    .read()
                    .await
                    .as_ref()
                    .expect("Receiving events while not being logged in")
                    .user_id,
            ))))
            .clone())
    }

    /// Get an left room with the given room id.
    ///
    /// # Arguments
    ///
    /// `room_id` - The unique id of the room that should be fetched.
    pub async fn get_left_room(&self, room_id: &RoomId) -> Option<Arc<RwLock<Room>>> {
        self.left_rooms.read().await.get(room_id).cloned()
    }

    /// Returns the left rooms this client knows about.
    ///
    /// A `HashMap` of room id to `matrix::models::Room`
    pub fn left_rooms(&self) -> Arc<RwLock<HashMap<RoomId, Arc<RwLock<Room>>>>> {
        self.left_rooms.clone()
    }

    /// Handle a m.ignored_user_list event, updating the room state if necessary.
    ///
    /// Returns true if the room name changed, false otherwise.
    pub(crate) async fn handle_ignored_users(&self, event: &IgnoredUserListEvent) -> bool {
        // this avoids cloning every UserId for the eq check
        if self.ignored_users.read().await.iter().collect::<Vec<_>>()
            == event.content.ignored_users.iter().collect::<Vec<_>>()
        {
            false
        } else {
            *self.ignored_users.write().await = event.content.ignored_users.to_vec();
            true
        }
    }

    /// Handle a m.ignored_user_list event, updating the room state if necessary.
    ///
    /// Returns true if the room name changed, false otherwise.
    pub(crate) async fn handle_push_rules(&self, event: &PushRulesEvent) -> bool {
        // TODO this is basically a stub
        // TODO ruma removed PartialEq for evens, so this doesn't work anymore.
        // Returning always true for now should be ok here since those don't
        // change often.
        // if self.push_ruleset.as_ref() == Some(&event.content.global) {
        //     false
        // } else {
        *self.push_ruleset.write().await = Some(event.content.global.clone());
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
        &self,
        room_id: &RoomId,
        event: &mut EventJson<RoomEvent>,
    ) -> Result<(Option<EventJson<RoomEvent>>, bool)> {
        // if the event is a m.room.member event the server will sometimes
        // send the `prev_content` field as part of the unsigned field this extracts and
        // places it where everything else expects it.
        if let Some(e) = deserialize_prev_content(event) {
            *event = e;
        }

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

                let room_lock = self.get_or_create_joined_room(&room_id).await?;
                let mut room = room_lock.write().await;

                if let RoomEvent::RoomMember(mem_event) = &mut e {
                    let changed = room.handle_membership(mem_event);

                    // The memberlist of the room changed, invalidate the group session
                    // of the room.
                    if changed {
                        #[cfg(feature = "encryption")]
                        self.invalidate_group_session(room_id).await;
                    }

                    Ok((decrypted_event, changed))
                } else {
                    Ok((decrypted_event, room.receive_timeline_event(&e)))
                }
            }
            _ => Ok((None, false)),
        }
    }

    /// Receive a state event for a joined room and update the client state.
    ///
    /// Returns true if the state of the room changed, false
    /// otherwise.
    ///
    /// # Arguments
    ///
    /// * `room_id` - The unique id of the room the event belongs to.
    ///
    /// * `event` - The event that should be handled by the client.
    pub async fn receive_joined_state_event(
        &self,
        room_id: &RoomId,
        event: &StateEvent,
    ) -> Result<bool> {
        let room_lock = self.get_or_create_joined_room(room_id).await?;
        let mut room = room_lock.write().await;

        if let StateEvent::RoomMember(e) = event {
            let changed = room.handle_membership(e);

            // The memberlist of the room changed, invalidate the group session
            // of the room.
            if changed {
                #[cfg(feature = "encryption")]
                self.invalidate_group_session(room_id).await;
            }

            Ok(changed)
        } else {
            Ok(room.receive_state_event(event))
        }
    }

    /// Receive a state event for a room the user has been invited to.
    ///
    /// Returns true if the state of the room changed, false
    /// otherwise.
    ///
    /// # Arguments
    ///
    /// * `room_id` - The unique id of the room the event belongs to.
    ///
    /// * `event` - A `AnyStrippedStateEvent` that should be handled by the client.
    pub async fn receive_invite_state_event(
        &self,
        room_id: &RoomId,
        event: &AnyStrippedStateEvent,
    ) -> Result<bool> {
        let room_lock = self.get_or_create_invited_room(room_id).await?;
        let mut room = room_lock.write().await;
        Ok(room.receive_stripped_state_event(event))
    }

    /// Receive a timeline event for a room the user has left and update the client state.
    ///
    /// Returns a tuple of the successfully decrypted event, or None on failure and
    /// a bool, true when the `Room` state has been updated.
    ///
    /// # Arguments
    ///
    /// * `room_id` - The unique id of the room the event belongs to.
    ///
    /// * `event` - The event that should be handled by the client.
    pub async fn receive_left_timeline_event(
        &self,
        room_id: &RoomId,
        event: &EventJson<RoomEvent>,
    ) -> Result<bool> {
        match event.deserialize() {
            Ok(e) => {
                let room_lock = self.get_or_create_left_room(room_id).await?;
                let mut room = room_lock.write().await;
                Ok(room.receive_timeline_event(&e))
            }
            _ => Ok(false),
        }
    }

    /// Receive a state event for a room the user has left and update the client state.
    ///
    /// Returns true if the state of the room changed, false
    /// otherwise.
    ///
    /// # Arguments
    ///
    /// * `room_id` - The unique id of the room the event belongs to.
    ///
    /// * `event` - The event that should be handled by the client.
    pub async fn receive_left_state_event(
        &self,
        room_id: &RoomId,
        event: &StateEvent,
    ) -> Result<bool> {
        let room_lock = self.get_or_create_left_room(room_id).await?;
        let mut room = room_lock.write().await;
        Ok(room.receive_state_event(event))
    }

    /// Receive a presence event from a sync response and updates the client state.
    ///
    /// Returns true if the state of the room changed, false
    /// otherwise.
    ///
    /// # Arguments
    ///
    /// * `room_id` - The unique id of the room the event belongs to.
    ///
    /// * `event` - The event that should be handled by the client.
    pub async fn receive_presence_event(&self, room_id: &RoomId, event: &PresenceEvent) -> bool {
        // this should be the room that was just created in the `Client::sync` loop.
        if let Some(room) = self.get_joined_room(room_id).await {
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
    pub async fn receive_account_data_event(&self, room_id: &RoomId, event: &NonRoomEvent) -> bool {
        match event {
            NonRoomEvent::IgnoredUserList(iu) => self.handle_ignored_users(iu).await,
            NonRoomEvent::Presence(p) => self.receive_presence_event(room_id, p).await,
            NonRoomEvent::PushRules(pr) => self.handle_push_rules(pr).await,
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
    pub async fn receive_ephemeral_event(&self, room_id: &RoomId, event: &NonRoomEvent) -> bool {
        match event {
            NonRoomEvent::IgnoredUserList(iu) => self.handle_ignored_users(iu).await,
            NonRoomEvent::Presence(p) => self.receive_presence_event(room_id, p).await,
            NonRoomEvent::PushRules(pr) => self.handle_push_rules(pr).await,
            _ => false,
        }
    }

    /// Get the current, if any, sync token of the client.
    /// This will be None if the client didn't sync at least once.
    pub async fn sync_token(&self) -> Option<String> {
        self.sync_token.read().await.clone()
    }

    /// Receive a response from a sync call.
    ///
    /// # Arguments
    ///
    /// * `response` - The response that we received after a successful sync.
    pub async fn receive_sync_response(
        &self,
        response: &mut api::sync::sync_events::Response,
    ) -> Result<()> {
        // The server might respond multiple times with the same sync token, in
        // that case we already received this response and there's nothing to
        // do.
        if self.sync_token.read().await.as_ref() == Some(&response.next_batch) {
            return Ok(());
        }

        *self.sync_token.write().await = Some(response.next_batch.clone());

        #[cfg(feature = "encryption")]
        {
            let mut olm = self.olm.lock().await;

            if let Some(o) = &mut *olm {
                // Let the crypto machine handle the sync response, this
                // decryptes to-device events, but leaves room events alone.
                // This makes sure that we have the deryption keys for the room
                // events at hand.
                o.receive_sync_response(response).await;
            }
        }

        // when events change state, updated_* signals to StateStore to update database
        self.iter_joined_rooms(response).await?;
        self.iter_invited_rooms(&response).await?;
        self.iter_left_rooms(response).await?;

        let store = self.state_store.read().await;

        // Store now the new sync token an other client specific state. Since we
        // know the sync token changed we can assume that this needs to be done
        // always.
        if let Some(store) = store.as_ref() {
            let state = ClientState::from_base_client(&self).await;
            store.store_client_state(state).await?;
        }

        Ok(())
    }

    async fn iter_joined_rooms(
        &self,
        response: &mut api::sync::sync_events::Response,
    ) -> Result<bool> {
        let mut updated = false;
        for (room_id, joined_room) in &mut response.rooms.join {
            let matrix_room = {
                for event in &joined_room.state.events {
                    if let Ok(e) = event.deserialize() {
                        if self.receive_joined_state_event(&room_id, &e).await? {
                            updated = true;
                        }
                        self.emit_state_event(&room_id, &e, RoomStateType::Joined)
                            .await;
                    }
                }

                self.get_or_create_joined_room(&room_id).await?.clone()
            };

            #[cfg(feature = "encryption")]
            {
                let mut olm = self.olm.lock().await;

                if let Some(o) = &mut *olm {
                    let room = matrix_room.read().await;

                    // If the room is encrypted, update the tracked users.
                    if room.is_encrypted() {
                        o.update_tracked_users(room.members.keys()).await;
                    }
                }
            }

            // RoomSummary contains information for calculating room name
            matrix_room
                .write()
                .await
                .set_room_summary(&joined_room.summary);

            // set unread notification count
            matrix_room
                .write()
                .await
                .set_unread_notice_count(&joined_room.unread_notifications);

            for mut event in &mut joined_room.timeline.events {
                let decrypted_event = {
                    let (decrypt_ev, timeline_update) = self
                        .receive_joined_timeline_event(room_id, &mut event)
                        .await?;
                    if timeline_update {
                        updated = true;
                    };
                    decrypt_ev
                };

                if let Some(e) = decrypted_event {
                    *event = e;
                }

                if let Some(e) = deserialize_prev_content(&event) {
                    *event = e;
                }

                if let Ok(e) = event.deserialize() {
                    self.emit_timeline_event(&room_id, &e, RoomStateType::Joined)
                        .await;
                } else {
                    self.emit_unrecognized_event(&room_id, &event, RoomStateType::Joined)
                        .await;
                }
            }

            // look at AccountData to further cut down users by collecting ignored users
            if let Some(account_data) = &joined_room.account_data {
                for account_data in &account_data.events {
                    {
                        if let Ok(e) = account_data.deserialize() {
                            if self.receive_account_data_event(&room_id, &e).await {
                                updated = true;
                            }
                            self.emit_account_data_event(room_id, &e, RoomStateType::Joined)
                                .await;
                        }
                    }
                }
            }

            // After the room has been created and state/timeline events accounted for we use the room_id of the newly created
            // room to add any presence events that relate to a user in the current room. This is not super
            // efficient but we need a room_id so we would loop through now or later.
            for presence in &mut response.presence.events {
                {
                    if let Ok(e) = presence.deserialize() {
                        if self.receive_presence_event(&room_id, &e).await {
                            updated = true;
                        }

                        self.emit_presence_event(&room_id, &e, RoomStateType::Joined)
                            .await;
                    }
                }
            }

            for ephemeral in &mut joined_room.ephemeral.events {
                {
                    if let Ok(e) = ephemeral.deserialize() {
                        if self.receive_ephemeral_event(&room_id, &e).await {
                            updated = true;
                        }

                        self.emit_ephemeral_event(&room_id, &e, RoomStateType::Joined)
                            .await;
                    }
                }
            }

            if updated {
                if let Some(store) = self.state_store.read().await.as_ref() {
                    store
                        .store_room_state(RoomState::Joined(matrix_room.read().await.deref()))
                        .await?;
                }
            }
        }
        Ok(updated)
    }

    async fn iter_left_rooms(
        &self,
        response: &mut api::sync::sync_events::Response,
    ) -> Result<bool> {
        let mut updated = false;
        for (room_id, left_room) in &mut response.rooms.leave {
            let matrix_room = {
                for event in &left_room.state.events {
                    if let Ok(e) = event.deserialize() {
                        if self.receive_left_state_event(&room_id, &e).await? {
                            updated = true;
                        }
                    }
                }

                self.get_or_create_left_room(&room_id).await?.clone()
            };

            for event in &mut left_room.state.events {
                if let Ok(e) = event.deserialize() {
                    self.emit_state_event(&room_id, &e, RoomStateType::Left)
                        .await;
                }
            }

            for event in &mut left_room.timeline.events {
                if let Some(e) = deserialize_prev_content(&event) {
                    *event = e;
                }

                if self.receive_left_timeline_event(room_id, &event).await? {
                    updated = true;
                };

                if let Ok(e) = event.deserialize() {
                    self.emit_timeline_event(&room_id, &e, RoomStateType::Left)
                        .await;
                }
            }

            if updated {
                if let Some(store) = self.state_store.read().await.as_ref() {
                    store
                        .store_room_state(RoomState::Left(matrix_room.read().await.deref()))
                        .await?;
                }
            }
        }
        Ok(updated)
    }

    async fn iter_invited_rooms(
        &self,
        response: &api::sync::sync_events::Response,
    ) -> Result<bool> {
        let mut updated = false;
        for (room_id, invited_room) in &response.rooms.invite {
            let matrix_room = {
                for event in &invited_room.invite_state.events {
                    if let Ok(e) = event.deserialize() {
                        if self.receive_invite_state_event(&room_id, &e).await? {
                            updated = true;
                        }
                    }
                }

                self.get_or_create_invited_room(&room_id).await?.clone()
            };

            for event in &invited_room.invite_state.events {
                if let Ok(mut e) = event.deserialize() {
                    // if the event is a m.room.member event the server will sometimes
                    // send the `prev_content` field as part of the unsigned field.
                    if let AnyStrippedStateEvent::RoomMember(_) = &mut e {
                        if let Some(raw_content) = stripped_deserialize_prev_content(event) {
                            let prev_content = match raw_content.prev_content {
                                Some(json) => json.deserialize().ok(),
                                None => None,
                            };
                            self.emit_stripped_state_event(
                                &room_id,
                                &e,
                                prev_content,
                                RoomStateType::Invited,
                            )
                            .await;
                            continue;
                        }
                    }
                    self.emit_stripped_state_event(&room_id, &e, None, RoomStateType::Invited)
                        .await;
                }
            }

            if updated {
                if let Some(store) = self.state_store.read().await.as_ref() {
                    store
                        .store_room_state(RoomState::Invited(matrix_room.read().await.deref()))
                        .await?;
                }
            }
        }
        Ok(updated)
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
        let room = self.get_joined_room(room_id).await.expect("No room found");
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
    ) -> Result<EncryptedEventContent> {
        let mut olm = self.olm.lock().await;

        match &mut *olm {
            Some(o) => Ok(o.encrypt(room_id, content).await?),
            None => panic!("Olm machine wasn't started"),
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

    /// Invalidate the currently active outbound group session for the given
    /// room.
    ///
    /// Returns true if a session was invalidated, false if there was no session
    /// to invalidate.
    #[cfg(feature = "encryption")]
    #[cfg_attr(docsrs, doc(cfg(feature = "encryption")))]
    pub async fn invalidate_group_session(&self, room_id: &RoomId) -> bool {
        let mut olm = self.olm.lock().await;

        match &mut *olm {
            Some(o) => o.invalidate_group_session(room_id),
            None => false,
        }
    }

    pub(crate) async fn emit_timeline_event(
        &self,
        room_id: &RoomId,
        event: &RoomEvent,
        room_state: RoomStateType,
    ) {
        let lock = self.event_emitter.read().await;
        let event_emitter = if let Some(ee) = lock.as_ref() {
            ee
        } else {
            return;
        };

        let room = match room_state {
            RoomStateType::Invited => {
                if let Some(room) = self.get_invited_room(&room_id).await {
                    RoomState::Invited(Arc::clone(&room))
                } else {
                    return;
                }
            }
            RoomStateType::Joined => {
                if let Some(room) = self.get_joined_room(&room_id).await {
                    RoomState::Joined(Arc::clone(&room))
                } else {
                    return;
                }
            }
            RoomStateType::Left => {
                if let Some(room) = self.get_left_room(&room_id).await {
                    RoomState::Left(Arc::clone(&room))
                } else {
                    return;
                }
            }
        };

        match event {
            RoomEvent::RoomMember(mem) => event_emitter.on_room_member(room, &mem).await,
            RoomEvent::RoomName(name) => event_emitter.on_room_name(room, &name).await,
            RoomEvent::RoomCanonicalAlias(canonical) => {
                event_emitter
                    .on_room_canonical_alias(room, &canonical)
                    .await
            }
            RoomEvent::RoomAliases(aliases) => event_emitter.on_room_aliases(room, &aliases).await,
            RoomEvent::RoomAvatar(avatar) => event_emitter.on_room_avatar(room, &avatar).await,
            RoomEvent::RoomMessage(msg) => event_emitter.on_room_message(room, &msg).await,
            RoomEvent::RoomMessageFeedback(msg_feedback) => {
                event_emitter
                    .on_room_message_feedback(room, &msg_feedback)
                    .await
            }
            RoomEvent::RoomRedaction(redaction) => {
                event_emitter.on_room_redaction(room, &redaction).await
            }
            RoomEvent::RoomPowerLevels(power) => {
                event_emitter.on_room_power_levels(room, &power).await
            }
            RoomEvent::RoomTombstone(tomb) => event_emitter.on_room_tombstone(room, &tomb).await,
            RoomEvent::CustomRoom(custom) => {
                event_emitter
                    .on_unrecognized_event(room, &CustomOrRawEvent::CustomRoom(custom))
                    .await
            }
            _ => {}
        }
    }

    pub(crate) async fn emit_state_event(
        &self,
        room_id: &RoomId,
        event: &StateEvent,
        room_state: RoomStateType,
    ) {
        let lock = self.event_emitter.read().await;
        let event_emitter = if let Some(ee) = lock.as_ref() {
            ee
        } else {
            return;
        };

        let room = match room_state {
            RoomStateType::Invited => {
                if let Some(room) = self.get_invited_room(&room_id).await {
                    RoomState::Invited(Arc::clone(&room))
                } else {
                    return;
                }
            }
            RoomStateType::Joined => {
                if let Some(room) = self.get_joined_room(&room_id).await {
                    RoomState::Joined(Arc::clone(&room))
                } else {
                    return;
                }
            }
            RoomStateType::Left => {
                if let Some(room) = self.get_left_room(&room_id).await {
                    RoomState::Left(Arc::clone(&room))
                } else {
                    return;
                }
            }
        };

        match event {
            StateEvent::RoomMember(member) => event_emitter.on_state_member(room, &member).await,
            StateEvent::RoomName(name) => event_emitter.on_state_name(room, &name).await,
            StateEvent::RoomCanonicalAlias(canonical) => {
                event_emitter
                    .on_state_canonical_alias(room, &canonical)
                    .await
            }
            StateEvent::RoomAliases(aliases) => {
                event_emitter.on_state_aliases(room, &aliases).await
            }
            StateEvent::RoomAvatar(avatar) => event_emitter.on_state_avatar(room, &avatar).await,
            StateEvent::RoomPowerLevels(power) => {
                event_emitter.on_state_power_levels(room, &power).await
            }
            StateEvent::RoomJoinRules(rules) => {
                event_emitter.on_state_join_rules(room, &rules).await
            }
            StateEvent::RoomTombstone(tomb) => event_emitter.on_room_tombstone(room, &tomb).await,
            StateEvent::CustomState(custom) => {
                event_emitter
                    .on_unrecognized_event(room, &CustomOrRawEvent::CustomState(custom))
                    .await
            }
            _ => {}
        }
    }

    pub(crate) async fn emit_stripped_state_event(
        &self,
        room_id: &RoomId,
        event: &AnyStrippedStateEvent,
        prev_content: Option<MemberEventContent>,
        room_state: RoomStateType,
    ) {
        let lock = self.event_emitter.read().await;
        let event_emitter = if let Some(ee) = lock.as_ref() {
            ee
        } else {
            return;
        };

        let room = match room_state {
            RoomStateType::Invited => {
                if let Some(room) = self.get_invited_room(&room_id).await {
                    RoomState::Invited(Arc::clone(&room))
                } else {
                    return;
                }
            }
            RoomStateType::Joined => {
                if let Some(room) = self.get_joined_room(&room_id).await {
                    RoomState::Joined(Arc::clone(&room))
                } else {
                    return;
                }
            }
            RoomStateType::Left => {
                if let Some(room) = self.get_left_room(&room_id).await {
                    RoomState::Left(Arc::clone(&room))
                } else {
                    return;
                }
            }
        };

        match event {
            AnyStrippedStateEvent::RoomMember(member) => {
                event_emitter
                    .on_stripped_state_member(room, &member, prev_content)
                    .await
            }
            AnyStrippedStateEvent::RoomName(name) => {
                event_emitter.on_stripped_state_name(room, &name).await
            }
            AnyStrippedStateEvent::RoomCanonicalAlias(canonical) => {
                event_emitter
                    .on_stripped_state_canonical_alias(room, &canonical)
                    .await
            }
            AnyStrippedStateEvent::RoomAliases(aliases) => {
                event_emitter
                    .on_stripped_state_aliases(room, &aliases)
                    .await
            }
            AnyStrippedStateEvent::RoomAvatar(avatar) => {
                event_emitter.on_stripped_state_avatar(room, &avatar).await
            }
            AnyStrippedStateEvent::RoomPowerLevels(power) => {
                event_emitter
                    .on_stripped_state_power_levels(room, &power)
                    .await
            }
            AnyStrippedStateEvent::RoomJoinRules(rules) => {
                event_emitter
                    .on_stripped_state_join_rules(room, &rules)
                    .await
            }
            _ => {}
        }
    }

    pub(crate) async fn emit_account_data_event(
        &self,
        room_id: &RoomId,
        event: &NonRoomEvent,
        room_state: RoomStateType,
    ) {
        let lock = self.event_emitter.read().await;
        let event_emitter = if let Some(ee) = lock.as_ref() {
            ee
        } else {
            return;
        };

        let room = match room_state {
            RoomStateType::Invited => {
                if let Some(room) = self.get_invited_room(&room_id).await {
                    RoomState::Invited(Arc::clone(&room))
                } else {
                    return;
                }
            }
            RoomStateType::Joined => {
                if let Some(room) = self.get_joined_room(&room_id).await {
                    RoomState::Joined(Arc::clone(&room))
                } else {
                    return;
                }
            }
            RoomStateType::Left => {
                if let Some(room) = self.get_left_room(&room_id).await {
                    RoomState::Left(Arc::clone(&room))
                } else {
                    return;
                }
            }
        };

        match event {
            NonRoomEvent::Presence(presence) => {
                event_emitter.on_non_room_presence(room, &presence).await
            }
            NonRoomEvent::IgnoredUserList(ignored) => {
                event_emitter
                    .on_non_room_ignored_users(room, &ignored)
                    .await
            }
            NonRoomEvent::PushRules(rules) => {
                event_emitter.on_non_room_push_rules(room, &rules).await
            }
            NonRoomEvent::FullyRead(full_read) => {
                event_emitter.on_non_room_fully_read(room, &full_read).await
            }
            NonRoomEvent::Typing(typing) => event_emitter.on_non_room_typing(room, &typing).await,
            _ => {}
        }
    }

    pub(crate) async fn emit_ephemeral_event(
        &self,
        room_id: &RoomId,
        event: &NonRoomEvent,
        room_state: RoomStateType,
    ) {
        let lock = self.event_emitter.read().await;
        let event_emitter = if let Some(ee) = lock.as_ref() {
            ee
        } else {
            return;
        };

        let room = match room_state {
            RoomStateType::Invited => {
                if let Some(room) = self.get_invited_room(&room_id).await {
                    RoomState::Invited(Arc::clone(&room))
                } else {
                    return;
                }
            }
            RoomStateType::Joined => {
                if let Some(room) = self.get_joined_room(&room_id).await {
                    RoomState::Joined(Arc::clone(&room))
                } else {
                    return;
                }
            }
            RoomStateType::Left => {
                if let Some(room) = self.get_left_room(&room_id).await {
                    RoomState::Left(Arc::clone(&room))
                } else {
                    return;
                }
            }
        };

        match event {
            NonRoomEvent::Presence(presence) => {
                event_emitter.on_non_room_presence(room, &presence).await
            }
            NonRoomEvent::IgnoredUserList(ignored) => {
                event_emitter
                    .on_non_room_ignored_users(room, &ignored)
                    .await
            }
            NonRoomEvent::PushRules(rules) => {
                event_emitter.on_non_room_push_rules(room, &rules).await
            }
            NonRoomEvent::FullyRead(full_read) => {
                event_emitter.on_non_room_fully_read(room, &full_read).await
            }
            NonRoomEvent::Typing(typing) => event_emitter.on_non_room_typing(room, &typing).await,
            _ => {}
        }
    }

    pub(crate) async fn emit_presence_event(
        &self,
        room_id: &RoomId,
        event: &PresenceEvent,
        room_state: RoomStateType,
    ) {
        let room = match room_state {
            RoomStateType::Invited => {
                if let Some(room) = self.get_invited_room(&room_id).await {
                    RoomState::Invited(Arc::clone(&room))
                } else {
                    return;
                }
            }
            RoomStateType::Joined => {
                if let Some(room) = self.get_joined_room(&room_id).await {
                    RoomState::Joined(Arc::clone(&room))
                } else {
                    return;
                }
            }
            RoomStateType::Left => {
                if let Some(room) = self.get_left_room(&room_id).await {
                    RoomState::Left(Arc::clone(&room))
                } else {
                    return;
                }
            }
        };
        if let Some(ee) = &self.event_emitter.read().await.as_ref() {
            ee.on_presence_event(room, &event).await;
        }
    }

    pub(crate) async fn emit_unrecognized_event<T>(
        &self,
        room_id: &RoomId,
        event: &EventJson<T>,
        room_state: RoomStateType,
    ) {
        let room = match room_state {
            RoomStateType::Invited => {
                if let Some(room) = self.get_invited_room(&room_id).await {
                    RoomState::Invited(Arc::clone(&room))
                } else {
                    return;
                }
            }
            RoomStateType::Joined => {
                if let Some(room) = self.get_joined_room(&room_id).await {
                    RoomState::Joined(Arc::clone(&room))
                } else {
                    return;
                }
            }
            RoomStateType::Left => {
                if let Some(room) = self.get_left_room(&room_id).await {
                    RoomState::Left(Arc::clone(&room))
                } else {
                    return;
                }
            }
        };
        if let Some(ee) = &self.event_emitter.read().await.as_ref() {
            ee.on_unrecognized_event(room, &CustomOrRawEvent::RawJson(event.json()))
                .await;
        }
    }
}

#[cfg(test)]
mod test {
    use crate::identifiers::{RoomId, UserId};
    use crate::{
        events::{collections::all::RoomEvent, stripped::AnyStrippedStateEvent},
        BaseClient, Session,
    };
    use matrix_sdk_common_macros::async_trait;
    use matrix_sdk_test::{async_test, test_json, EventBuilder, EventsJson};
    use serde_json::json;
    use std::convert::TryFrom;

    #[cfg(target_arch = "wasm32")]
    use wasm_bindgen_test::*;

    async fn get_client() -> BaseClient {
        let session = Session {
            access_token: "1234".to_owned(),
            user_id: UserId::try_from("@example:localhost").unwrap(),
            device_id: "DEVICEID".to_owned(),
        };
        let client = BaseClient::new().unwrap();
        client.restore_login(session).await.unwrap();
        client
    }

    fn get_room_id() -> RoomId {
        RoomId::try_from("!SVkFJHzfwvuaIEawgC:localhost").unwrap()
    }

    fn member_event() -> serde_json::Value {
        json!({
            "content": {
                "displayname": "example",
                "membership": "join"
            },
            "event_id": "$151800140517rfvjc:localhost",
            "membership": "join",
            "origin_server_ts": 0,
            "sender": "@example:localhost",
            "state_key": "@example:localhost",
            "type": "m.room.member",
        })
    }

    #[async_test]
    async fn test_joined_room_creation() {
        let mut sync_response = EventBuilder::default()
            .add_room_event(EventsJson::Member, RoomEvent::RoomMember)
            .build_sync_response();
        let client = get_client().await;
        let room_id = get_room_id();

        let room = client.get_joined_room(&room_id).await;
        assert!(room.is_none());

        client
            .receive_sync_response(&mut sync_response)
            .await
            .unwrap();

        let room = client.get_left_room(&room_id).await;
        assert!(room.is_none());

        let room = client.get_joined_room(&room_id).await;
        assert!(room.is_some());

        let mut sync_response = EventBuilder::default()
            .add_custom_left_event(&room_id, member_event(), RoomEvent::RoomMember)
            .build_sync_response();

        sync_response.next_batch = "Hello".to_owned();

        client
            .receive_sync_response(&mut sync_response)
            .await
            .unwrap();

        let room = client.get_joined_room(&room_id).await;
        assert!(room.is_none());

        let room = client.get_left_room(&room_id).await;
        assert!(room.is_some());
    }

    #[async_test]
    async fn test_left_room_creation() {
        let room_id = RoomId::try_from("!left_room:localhost").unwrap();
        let mut sync_response = EventBuilder::default()
            .add_custom_left_event(&room_id, member_event(), RoomEvent::RoomMember)
            .build_sync_response();

        let client = get_client().await;

        let room = client.get_left_room(&room_id).await;
        assert!(room.is_none());

        client
            .receive_sync_response(&mut sync_response)
            .await
            .unwrap();

        let room = client.get_left_room(&room_id).await;
        assert!(room.is_some());

        let mut sync_response = EventBuilder::default()
            .add_custom_joined_event(&room_id, member_event(), RoomEvent::RoomMember)
            .build_sync_response();

        sync_response.next_batch = "Hello".to_owned();

        client
            .receive_sync_response(&mut sync_response)
            .await
            .unwrap();

        let room = client.get_left_room(&room_id).await;
        assert!(room.is_none());

        let room = client.get_joined_room(&room_id).await;
        assert!(room.is_some());
    }

    #[async_test]
    async fn test_invited_room_creation() {
        let room_id = RoomId::try_from("!invited_room:localhost").unwrap();
        let mut sync_response = EventBuilder::default()
            .add_custom_invited_event(&room_id, member_event(), AnyStrippedStateEvent::RoomMember)
            .build_sync_response();

        let client = get_client().await;

        let room = client.get_invited_room(&room_id).await;
        assert!(room.is_none());

        client
            .receive_sync_response(&mut sync_response)
            .await
            .unwrap();

        let room = client.get_invited_room(&room_id).await;
        assert!(room.is_some());

        let mut sync_response = EventBuilder::default()
            .add_custom_joined_event(&room_id, member_event(), RoomEvent::RoomMember)
            .build_sync_response();

        sync_response.next_batch = "Hello".to_owned();

        client
            .receive_sync_response(&mut sync_response)
            .await
            .unwrap();

        let room = client.get_invited_room(&room_id).await;
        assert!(room.is_none());

        let room = client.get_joined_room(&room_id).await;
        assert!(room.is_some());
    }

    #[async_test]
    async fn test_prev_content_from_unsigned() {
        use super::*;

        use crate::{EventEmitter, SyncRoom};
        use matrix_sdk_common::events::room::member::{MemberEvent, MembershipChange};
        use matrix_sdk_common::locks::RwLock;
        use std::sync::{
            atomic::{AtomicBool, Ordering},
            Arc,
        };

        struct EE(Arc<AtomicBool>);
        #[async_trait]
        impl EventEmitter for EE {
            async fn on_room_member(&self, room: SyncRoom, event: &MemberEvent) {
                if let SyncRoom::Joined(_) = room {
                    match event.membership_change() {
                        MembershipChange::Joined => {
                            self.0.swap(true, Ordering::SeqCst);
                        }
                        _ => {}
                    };
                }
                if event.prev_content.is_none() {
                    self.0.swap(false, Ordering::SeqCst);
                }
            }
        }

        let room_id = get_room_id();
        let passed = Arc::new(AtomicBool::default());
        let emitter = EE(Arc::clone(&passed));
        let mut client = get_client().await;

        client.event_emitter = Arc::new(RwLock::new(Some(Box::new(emitter))));

        // This is needed other wise the `EventBuilder` goes through a de/ser cycle and the `prev_content` is lost.
        let event: &serde_json::Value = &test_json::MEMBER;

        let mut joined_rooms: HashMap<RoomId, serde_json::Value> = HashMap::new();
        let joined_room = serde_json::json!({
            "summary": {},
            "account_data": {
                "events": [],
            },
            "ephemeral": {
                "events": [],
            },
            "state": {
                "events": [],
            },
            "timeline": {
                "events": vec![ event ],
                "limited": true,
                "prev_batch": "t392-516_47314_0_7_1_1_1_11444_1"
            },
            "unread_notifications": {
                "highlight_count": 0,
                "notification_count": 11
            }
        });
        joined_rooms.insert(room_id, joined_room);

        let empty_room: HashMap<RoomId, serde_json::Value> = HashMap::new();
        let body = serde_json::json!({
            "device_one_time_keys_count": {},
            "next_batch": "s526_47314_0_7_1_1_1_11444_1",
            "device_lists": {
                "changed": [],
                "left": []
            },
            "rooms": {
                "invite": empty_room,
                "join": joined_rooms,
                "leave": empty_room,
            },
            "to_device": {
                "events": []
            },
            "presence": {
                "events": []
            }
        });
        let response = http::Response::builder()
            .body(serde_json::to_vec(&body).unwrap())
            .unwrap();
        let mut sync =
            matrix_sdk_common::api::r0::sync::sync_events::Response::try_from(response).unwrap();

        client.receive_sync_response(&mut sync).await.unwrap();

        assert!(passed.load(Ordering::SeqCst))
    }

    #[async_test]
    async fn test_unrecognized_events() {
        use super::*;

        use crate::{EventEmitter, SyncRoom};
        use matrix_sdk_common::locks::RwLock;
        use std::sync::{
            atomic::{AtomicBool, Ordering},
            Arc,
        };

        struct EE(Arc<AtomicBool>);
        #[async_trait]
        impl EventEmitter for EE {
            async fn on_unrecognized_event(&self, room: SyncRoom, event: &CustomOrRawEvent<'_>) {
                if let SyncRoom::Joined(_) = room {
                    if let CustomOrRawEvent::RawJson(raw) = event {
                        let val = serde_json::to_value(raw).unwrap();
                        if val.get("type").unwrap() == &json! { "m.room.message" }
                            && val.get("content").unwrap().get("m.relates_to").is_some()
                        {
                            self.0.swap(true, Ordering::SeqCst);
                        }
                    }
                }
            }
        }

        let room_id = get_room_id();
        let passed = Arc::new(AtomicBool::default());
        let emitter = EE(Arc::clone(&passed));
        let mut client = get_client().await;

        client.event_emitter = Arc::new(RwLock::new(Some(Box::new(emitter))));

        // This is needed other wise the `EventBuilder` goes through a de/ser cycle and the `prev_content` is lost.
        let event: &serde_json::Value = &test_json::MESSAGE_EDIT;

        let mut joined_rooms: HashMap<RoomId, serde_json::Value> = HashMap::new();
        let joined_room = serde_json::json!({
            "summary": {},
            "account_data": {
                "events": [],
            },
            "ephemeral": {
                "events": [],
            },
            "state": {
                "events": [],
            },
            "timeline": {
                "events": vec![ event ],
                "limited": true,
                "prev_batch": "t392-516_47314_0_7_1_1_1_11444_1"
            },
            "unread_notifications": {
                "highlight_count": 0,
                "notification_count": 11
            }
        });
        joined_rooms.insert(room_id, joined_room);

        let empty_room: HashMap<RoomId, serde_json::Value> = HashMap::new();
        let body = serde_json::json!({
            "device_one_time_keys_count": {},
            "next_batch": "s526_47314_0_7_1_1_1_11444_1",
            "device_lists": {
                "changed": [],
                "left": []
            },
            "rooms": {
                "invite": empty_room,
                "join": joined_rooms,
                "leave": empty_room,
            },
            "to_device": {
                "events": []
            },
            "presence": {
                "events": []
            }
        });
        let response = http::Response::builder()
            .body(serde_json::to_vec(&body).unwrap())
            .unwrap();
        let mut sync =
            matrix_sdk_common::api::r0::sync::sync_events::Response::try_from(response).unwrap();

        client.receive_sync_response(&mut sync).await.unwrap();

        assert!(passed.load(Ordering::SeqCst))
    }

    #[async_test]
    async fn test_unrecognized_custom_event() {
        use super::*;

        use crate::{EventEmitter, SyncRoom};
        use matrix_sdk_common::locks::RwLock;
        use std::sync::{
            atomic::{AtomicBool, Ordering},
            Arc,
        };

        struct EE(Arc<AtomicBool>);
        #[async_trait]
        impl EventEmitter for EE {
            async fn on_unrecognized_event(&self, room: SyncRoom, event: &CustomOrRawEvent<'_>) {
                if let SyncRoom::Joined(_) = room {
                    if let CustomOrRawEvent::CustomRoom(custom) = event {
                        if custom.event_type == "m.reaction"
                            && custom.content.get("m.relates_to").is_some()
                        {
                            self.0.swap(true, Ordering::SeqCst);
                        }
                    }
                }
            }
        }

        let room_id = get_room_id();
        let passed = Arc::new(AtomicBool::default());
        let emitter = EE(Arc::clone(&passed));
        let mut client = get_client().await;

        client.event_emitter = Arc::new(RwLock::new(Some(Box::new(emitter))));

        // This is needed other wise the `EventBuilder` goes through a de/ser cycle and the `prev_content` is lost.
        let event: &serde_json::Value = &test_json::REACTION;

        let mut joined_rooms: HashMap<RoomId, serde_json::Value> = HashMap::new();
        let joined_room = serde_json::json!({
            "summary": {},
            "account_data": {
                "events": [],
            },
            "ephemeral": {
                "events": [],
            },
            "state": {
                "events": [],
            },
            "timeline": {
                "events": vec![ event ],
                "limited": true,
                "prev_batch": "t392-516_47314_0_7_1_1_1_11444_1"
            },
            "unread_notifications": {
                "highlight_count": 0,
                "notification_count": 11
            }
        });
        joined_rooms.insert(room_id, joined_room);

        let empty_room: HashMap<RoomId, serde_json::Value> = HashMap::new();
        let body = serde_json::json!({
            "device_one_time_keys_count": {},
            "next_batch": "s526_47314_0_7_1_1_1_11444_1",
            "device_lists": {
                "changed": [],
                "left": []
            },
            "rooms": {
                "invite": empty_room,
                "join": joined_rooms,
                "leave": empty_room,
            },
            "to_device": {
                "events": []
            },
            "presence": {
                "events": []
            }
        });
        let response = http::Response::builder()
            .body(serde_json::to_vec(&body).unwrap())
            .unwrap();
        let mut sync =
            matrix_sdk_common::api::r0::sync::sync_events::Response::try_from(response).unwrap();

        client.receive_sync_response(&mut sync).await.unwrap();

        assert!(passed.load(Ordering::SeqCst))
    }

    #[async_test]
    #[cfg(feature = "encryption")]
    async fn test_group_session_invalidation() {
        let client = get_client().await;
        let room_id = get_room_id();

        let mut sync_response = EventBuilder::default()
            .add_room_event(EventsJson::Member, RoomEvent::RoomMember)
            .build_sync_response();

        client
            .receive_sync_response(&mut sync_response)
            .await
            .unwrap();

        assert!(client.should_share_group_session(&room_id).await);
        let _ = client.share_group_session(&room_id).await.unwrap();
        assert!(!client.should_share_group_session(&room_id).await);
        client.invalidate_group_session(&room_id).await;
    }
}
