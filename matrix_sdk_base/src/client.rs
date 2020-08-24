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

use std::{
    collections::HashMap,
    fmt,
    ops::Deref,
    path::{Path, PathBuf},
    result::Result as StdResult,
    sync::Arc,
};

#[cfg(feature = "encryption")]
use matrix_sdk_common::locks::Mutex;
use matrix_sdk_common::{
    api::r0 as api,
    events::{
        ignored_user_list::IgnoredUserListEvent, push_rules::PushRulesEvent,
        room::member::MemberEventContent, AnyBasicEvent, AnyStrippedStateEvent,
        AnySyncEphemeralRoomEvent, AnySyncMessageEvent, AnySyncRoomEvent, AnySyncStateEvent,
    },
    identifiers::{RoomId, UserId},
    locks::RwLock,
    push::Ruleset,
    uuid::Uuid,
    Raw,
};
#[cfg(feature = "encryption")]
use matrix_sdk_common::{
    api::r0::keys::claim_keys::Request as KeysClaimRequest,
    api::r0::to_device::send_event_to_device::IncomingRequest as OwnedToDeviceRequest,
    events::room::{
        encrypted::EncryptedEventContent, message::MessageEventContent as MsgEventContent,
    },
    identifiers::DeviceId,
};
#[cfg(feature = "encryption")]
use matrix_sdk_crypto::{
    CryptoStore, CryptoStoreError, Device, IncomingResponse, OlmError, OlmMachine, OutgoingRequest,
    Sas, UserDevices,
};
use zeroize::Zeroizing;

#[cfg(not(target_arch = "wasm32"))]
use crate::JsonStore;

use crate::{
    error::Result,
    event_emitter::CustomEvent,
    events::presence::PresenceEvent,
    models::Room,
    session::Session,
    state::{AllRooms, ClientState, StateStore},
    EventEmitter,
};

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
    pub prev_content: Option<Raw<MemberEventContent>>,
}

/// Transform room event by hoisting `prev_content` field from `unsigned` to the top level.
///
/// Due to a [bug in synapse][synapse-bug], `prev_content` often ends up in `unsigned` contrary to
/// the C2S spec. Some more discussion can be found [here][discussion]. Until this is fixed in
/// synapse or handled in Ruma, we use this to hoist up `prev_content` to the top level.
///
/// [synapse-bug]: <https://github.com/matrix-org/matrix-doc/issues/684#issuecomment-641182668>
/// [discussion]: <https://github.com/matrix-org/matrix-doc/issues/684#issuecomment-641182668>
fn hoist_room_event_prev_content(event: &Raw<AnySyncRoomEvent>) -> Option<Raw<AnySyncRoomEvent>> {
    let prev_content = serde_json::from_str::<AdditionalEventData>(event.json().get())
        .map(|more_unsigned| more_unsigned.unsigned)
        .map(|additional| additional.prev_content)
        .ok()
        .flatten()?;

    let mut ev = event.deserialize().ok()?;

    match &mut ev {
        AnySyncRoomEvent::State(AnySyncStateEvent::RoomMember(ref mut member))
            if member.prev_content.is_none() =>
        {
            if let Ok(prev) = prev_content.deserialize() {
                member.prev_content = Some(prev)
            }

            Some(Raw::from(ev))
        }
        _ => None,
    }
}

/// Transform state event by hoisting `prev_content` field from `unsigned` to the top level.
///
/// See comment of `hoist_room_event_prev_content`.
fn hoist_state_event_prev_content(
    event: &Raw<AnySyncStateEvent>,
) -> Option<Raw<AnySyncStateEvent>> {
    let prev_content = serde_json::from_str::<AdditionalEventData>(event.json().get())
        .map(|more_unsigned| more_unsigned.unsigned)
        .map(|additional| additional.prev_content)
        .ok()
        .flatten()?;

    let mut ev = event.deserialize().ok()?;
    match &mut ev {
        AnySyncStateEvent::RoomMember(ref mut member) if member.prev_content.is_none() => {
            member.prev_content = Some(prev_content.deserialize().ok()?);
            Some(Raw::from(ev))
        }
        _ => None,
    }
}

fn stripped_deserialize_prev_content(
    event: &Raw<AnyStrippedStateEvent>,
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
/// accordingly updates its state.
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

#[cfg(not(tarpaulin_include))]
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

#[cfg(not(tarpaulin_include))]
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
        // If there wasn't a state store opened, try to open the default one if
        // a store path was provided.
        if self.state_store.read().await.is_none() {
            #[cfg(not(target_arch = "wasm32"))]
            if let Some(path) = &*self.store_path {
                let store = JsonStore::open(path)?;
                *self.state_store.write().await = Some(Box::new(store));
            }
        }

        self.sync_with_state_store(&session).await?;

        #[cfg(feature = "encryption")]
        {
            let mut olm = self.olm.lock().await;
            let store = self.cryptostore.lock().await.take();

            if let Some(store) = store {
                *olm = Some(
                    OlmMachine::new_with_store(
                        session.user_id.to_owned(),
                        session.device_id.as_str().into(),
                        store,
                    )
                    .await
                    .map_err(OlmError::from)?,
                );
            } else if let Some(path) = self.store_path.as_ref() {
                #[cfg(feature = "sqlite_cryptostore")]
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
                #[cfg(not(feature = "sqlite_cryptostore"))]
                {
                    *olm = Some(OlmMachine::new(&session.user_id, &session.device_id));
                }
            } else {
                *olm = Some(OlmMachine::new(&session.user_id, &session.device_id));
            }
        }

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
    /// Returns a bool, true when the `Room` state has been updated.
    ///
    /// This will in-place replace the event with a decrypted one if the
    /// encryption feature is turned on, the event is encrypted and if we
    /// successfully decrypted the event.
    ///
    /// # Arguments
    ///
    /// * `room_id` - The unique id of the room the event belongs to.
    ///
    /// * `event` - The event that should be handled by the client.
    pub async fn receive_joined_timeline_event(
        &self,
        room_id: &RoomId,
        event: &mut Raw<AnySyncRoomEvent>,
    ) -> Result<bool> {
        match event.deserialize() {
            #[allow(unused_mut)]
            Ok(mut e) => {
                #[cfg(feature = "encryption")]
                if let AnySyncRoomEvent::Message(AnySyncMessageEvent::RoomEncrypted(
                    ref mut encrypted_event,
                )) = e
                {
                    let olm = self.olm.lock().await;

                    if let Some(o) = &*olm {
                        if let Ok(decrypted) = o.decrypt_room_event(&encrypted_event, room_id).await
                        {
                            if let Ok(d) = decrypted.deserialize() {
                                e = d
                            }
                            *event = decrypted;
                        }
                    }
                }

                let room_lock = self.get_or_create_joined_room(&room_id).await?;
                let mut room = room_lock.write().await;

                if let AnySyncRoomEvent::State(AnySyncStateEvent::RoomMember(mem_event)) = &mut e {
                    let (changed, _) = room.handle_membership(mem_event, false);

                    // The memberlist of the room changed, invalidate the group session
                    // of the room.
                    if changed {
                        #[cfg(feature = "encryption")]
                        self.invalidate_group_session(room_id).await;
                    }

                    Ok(changed)
                } else {
                    Ok(room.receive_timeline_event(&e))
                }
            }
            _ => Ok(false),
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
        event: &AnySyncStateEvent,
    ) -> Result<bool> {
        let room_lock = self.get_or_create_joined_room(room_id).await?;
        let mut room = room_lock.write().await;

        if let AnySyncStateEvent::RoomMember(e) = event {
            let (changed, _) = room.handle_membership(e, true);

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
        event: &Raw<AnySyncRoomEvent>,
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
        event: &AnySyncStateEvent,
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
    pub async fn receive_account_data_event(&self, _: &RoomId, event: &AnyBasicEvent) -> bool {
        match event {
            AnyBasicEvent::IgnoredUserList(event) => self.handle_ignored_users(event).await,
            AnyBasicEvent::PushRules(event) => self.handle_push_rules(event).await,
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
    pub async fn receive_ephemeral_event(&self, event: &AnySyncEphemeralRoomEvent) -> bool {
        match event {
            AnySyncEphemeralRoomEvent::FullyRead(_) => {}
            AnySyncEphemeralRoomEvent::Receipt(_) => {}
            AnySyncEphemeralRoomEvent::Typing(_) => {}
            _ => {}
        };
        false
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
            let olm = self.olm.lock().await;

            if let Some(o) = &*olm {
                // Let the crypto machine handle the sync response, this
                // decryptes to-device events, but leaves room events alone.
                // This makes sure that we have the deryption keys for the room
                // events at hand.
                o.receive_sync_response(response).await;
            }
        }

        // when events change state, updated_* signals to StateStore to update database
        self.iter_joined_rooms(response).await?;
        self.iter_invited_rooms(response).await?;
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
                for event in &mut joined_room.state.events {
                    // XXX: Related to `prev_content` and `unsigned`; see the doc comment of
                    // `hoist_room_event_prev_content`
                    if let Some(e) = hoist_state_event_prev_content(event) {
                        *event = e;
                    }

                    if let Ok(e) = event.deserialize() {
                        // FIXME: receive_* and emit_* methods shouldn't be called in parallel. We
                        // should only pass events to receive_* methods and then let *them* emit.
                        if self.receive_joined_state_event(&room_id, &e).await? {
                            updated = true;
                        }
                        self.emit_state_event(&room_id, &e, RoomStateType::Joined)
                            .await;
                    }
                }

                self.get_or_create_joined_room(&room_id).await?.clone()
            };

            // RoomSummary contains information for calculating room name.
            matrix_room
                .write()
                .await
                .set_room_summary(&joined_room.summary);

            // Set unread notification count.
            matrix_room
                .write()
                .await
                .set_unread_notice_count(&joined_room.unread_notifications);

            for mut event in &mut joined_room.timeline.events {
                // XXX: Related to `prev_content` and `unsigned`; see the doc comment of
                // `hoist_room_event_prev_content`
                if let Some(e) = hoist_room_event_prev_content(event) {
                    *event = e;
                }

                // FIXME: receive_* and emit_* methods shouldn't be called in parallel. We
                // should only pass events to receive_* methods and then let *them* emit.
                let timeline_update = self
                    .receive_joined_timeline_event(room_id, &mut event)
                    .await?;
                if timeline_update {
                    updated = true;
                };

                if let Ok(e) = event.deserialize() {
                    self.emit_timeline_event(&room_id, &e, RoomStateType::Joined)
                        .await;
                } else {
                    self.emit_unrecognized_event(&room_id, &event, RoomStateType::Joined)
                        .await;
                }
            }

            #[cfg(feature = "encryption")]
            {
                let olm = self.olm.lock().await;

                if let Some(o) = &*olm {
                    let room = matrix_room.read().await;

                    // If the room is encrypted, update the tracked users.
                    if room.is_encrypted() {
                        o.update_tracked_users(room.joined_members.keys()).await;
                        o.update_tracked_users(room.invited_members.keys()).await;
                    }
                }
            }

            // look at AccountData to further cut down users by collecting ignored users
            for account_data in &joined_room.account_data.events {
                {
                    // FIXME: receive_* and emit_* methods shouldn't be called in parallel. We
                    // should only pass events to receive_* methods and then let *them* emit.
                    if let Ok(e) = account_data.deserialize() {
                        if self.receive_account_data_event(&room_id, &e).await {
                            updated = true;
                        }
                        self.emit_account_data_event(room_id, &e, RoomStateType::Joined)
                            .await;
                    }
                }
            }

            // After the room has been created and state/timeline events accounted for we use the room_id of the newly created
            // room to add any presence events that relate to a user in the current room. This is not super
            // efficient but we need a room_id so we would loop through now or later.
            for presence in &mut response.presence.events {
                {
                    // FIXME: receive_* and emit_* methods shouldn't be called in parallel. We
                    // should only pass events to receive_* methods and then let *them* emit.
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
                        // FIXME: receive_* and emit_* methods shouldn't be called in parallel. We
                        // should only pass events to receive_* methods and then let *them* emit.
                        if self.receive_ephemeral_event(&e).await {
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
                for event in &mut left_room.state.events {
                    // XXX: Related to `prev_content` and `unsigned`; see the doc comment of
                    // `hoist_room_event_prev_content`
                    if let Some(e) = hoist_state_event_prev_content(event) {
                        *event = e;
                    }

                    // FIXME: receive_* and emit_* methods shouldn't be called in parallel. We
                    // should only pass events to receive_* methods and then let *them* emit.
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
                // XXX: Related to `prev_content` and `unsigned`; see the doc comment of
                // `hoist_room_event_prev_content`
                if let Some(e) = hoist_room_event_prev_content(event) {
                    *event = e;
                }

                // FIXME: receive_* and emit_* methods shouldn't be called in parallel. We
                // should only pass events to receive_* methods and then let *them* emit.
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
                        // FIXME: receive_* and emit_* methods shouldn't be called in parallel. We
                        // should only pass events to receive_* methods and then let *them* emit.
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
                            let prev_content = raw_content
                                .prev_content
                                .and_then(|json| json.deserialize().ok());
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

    /// Should the client share a group session for the given room.
    ///
    /// Returns true if a session needs to be shared before room messages can be
    /// encrypted, false if one is already shared and ready to encrypt room
    /// messages.
    ///
    /// This should be called every time a new room message wants to be sent out
    /// since group sessions can expire at any time.
    #[cfg(feature = "encryption")]
    #[cfg_attr(feature = "docs", doc(cfg(encryption)))]
    pub async fn should_share_group_session(&self, room_id: &RoomId) -> bool {
        let olm = self.olm.lock().await;

        match &*olm {
            Some(o) => o.should_share_group_session(room_id),
            None => false,
        }
    }

    /// Get the outgoing requests that need to be sent out.
    ///
    /// This returns a list of `OutGoingRequest`, those requests need to be sent
    /// out to the server and the responses need to be passed back to the state
    /// machine using [`mark_request_as_sent`].
    ///
    /// [`mark_request_as_sent`]: #method.mark_request_as_sent
    #[cfg(feature = "encryption")]
    #[cfg_attr(feature = "docs", doc(cfg(encryption)))]
    pub async fn outgoing_requests(&self) -> Vec<OutgoingRequest> {
        let olm = self.olm.lock().await;

        match &*olm {
            Some(o) => o.outgoing_requests().await,
            None => vec![],
        }
    }

    /// Mark the request with the given request id as sent.
    ///
    /// # Arguments
    ///
    /// * `request_id` - The unique id of the request that was sent out. This is
    /// needed to couple the response with the now sent out request.
    ///
    /// * `response` - The response that was received from the server after the
    /// outgoing request was sent out.
    #[cfg(feature = "encryption")]
    #[cfg_attr(feature = "docs", doc(cfg(encryption)))]
    pub async fn mark_request_as_sent<'a>(
        &self,
        request_id: &Uuid,
        response: impl Into<IncomingResponse<'a>>,
    ) -> Result<()> {
        let olm = self.olm.lock().await;

        match &*olm {
            Some(o) => Ok(o.mark_request_as_sent(request_id, response).await?),
            None => Ok(()),
        }
    }

    /// Get a tuple of device and one-time keys that need to be uploaded.
    ///
    /// Returns an empty error if no keys need to be uploaded.
    #[cfg(feature = "encryption")]
    #[cfg_attr(feature = "docs", doc(cfg(encryption)))]
    pub async fn get_missing_sessions(
        &self,
        users: impl Iterator<Item = &UserId>,
    ) -> Result<Option<(Uuid, KeysClaimRequest)>> {
        let olm = self.olm.lock().await;

        match &*olm {
            Some(o) => Ok(o.get_missing_sessions(users).await?),
            None => Ok(None),
        }
    }

    /// Get a to-device request that will share a group session for a room.
    #[cfg(feature = "encryption")]
    #[cfg_attr(feature = "docs", doc(cfg(encryption)))]
    pub async fn share_group_session(&self, room_id: &RoomId) -> Result<Vec<OwnedToDeviceRequest>> {
        let room = self.get_joined_room(room_id).await.expect("No room found");
        let olm = self.olm.lock().await;

        match &*olm {
            Some(o) => {
                let room = room.write().await;

                // XXX: We construct members in a slightly roundabout way instead of chaining the
                // iterators directly because of https://github.com/rust-lang/rust/issues/64552
                let joined_members = room.joined_members.keys();
                let invited_members = room.joined_members.keys();
                let members: Vec<&UserId> = joined_members.chain(invited_members).collect();
                Ok(o.share_group_session(
                    room_id,
                    members.into_iter(),
                    room.encrypted.clone().unwrap_or_default(),
                )
                .await?)
            }
            None => panic!("Olm machine wasn't started"),
        }
    }

    /// Encrypt a message event content.
    #[cfg(feature = "encryption")]
    #[cfg_attr(feature = "docs", doc(cfg(encryption)))]
    pub async fn encrypt(
        &self,
        room_id: &RoomId,
        content: MsgEventContent,
    ) -> Result<EncryptedEventContent> {
        let olm = self.olm.lock().await;

        match &*olm {
            Some(o) => Ok(o.encrypt(room_id, content).await?),
            None => panic!("Olm machine wasn't started"),
        }
    }

    /// Invalidate the currently active outbound group session for the given
    /// room.
    ///
    /// Returns true if a session was invalidated, false if there was no session
    /// to invalidate.
    #[cfg(feature = "encryption")]
    #[cfg_attr(feature = "docs", doc(cfg(encryption)))]
    pub async fn invalidate_group_session(&self, room_id: &RoomId) -> bool {
        let olm = self.olm.lock().await;

        match &*olm {
            Some(o) => o.invalidate_group_session(room_id),
            None => false,
        }
    }

    pub(crate) async fn emit_timeline_event(
        &self,
        room_id: &RoomId,
        event: &AnySyncRoomEvent,
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
            AnySyncRoomEvent::State(event) => match event {
                AnySyncStateEvent::RoomMember(e) => event_emitter.on_room_member(room, e).await,
                AnySyncStateEvent::RoomName(e) => event_emitter.on_room_name(room, e).await,
                AnySyncStateEvent::RoomCanonicalAlias(e) => {
                    event_emitter.on_room_canonical_alias(room, e).await
                }
                AnySyncStateEvent::RoomAliases(e) => event_emitter.on_room_aliases(room, e).await,
                AnySyncStateEvent::RoomAvatar(e) => event_emitter.on_room_avatar(room, e).await,
                AnySyncStateEvent::RoomPowerLevels(e) => {
                    event_emitter.on_room_power_levels(room, e).await
                }
                AnySyncStateEvent::RoomTombstone(e) => {
                    event_emitter.on_room_tombstone(room, e).await
                }
                AnySyncStateEvent::RoomJoinRules(e) => {
                    event_emitter.on_room_join_rules(room, e).await
                }
                AnySyncStateEvent::Custom(e) => {
                    event_emitter
                        .on_custom_event(room, &CustomEvent::State(e))
                        .await
                }
                _ => {}
            },
            AnySyncRoomEvent::Message(event) => match event {
                AnySyncMessageEvent::RoomMessage(e) => event_emitter.on_room_message(room, e).await,
                AnySyncMessageEvent::RoomMessageFeedback(e) => {
                    event_emitter.on_room_message_feedback(room, e).await
                }
                AnySyncMessageEvent::RoomRedaction(e) => {
                    event_emitter.on_room_redaction(room, e).await
                }
                AnySyncMessageEvent::Custom(e) => {
                    event_emitter
                        .on_custom_event(room, &CustomEvent::Message(e))
                        .await
                }
                _ => {}
            },
            AnySyncRoomEvent::RedactedState(_event) => {}
            AnySyncRoomEvent::RedactedMessage(_event) => {}
        }
    }

    pub(crate) async fn emit_state_event(
        &self,
        room_id: &RoomId,
        event: &AnySyncStateEvent,
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
            AnySyncStateEvent::RoomMember(member) => {
                event_emitter.on_state_member(room, &member).await
            }
            AnySyncStateEvent::RoomName(name) => event_emitter.on_state_name(room, &name).await,
            AnySyncStateEvent::RoomCanonicalAlias(canonical) => {
                event_emitter
                    .on_state_canonical_alias(room, &canonical)
                    .await
            }
            AnySyncStateEvent::RoomAliases(aliases) => {
                event_emitter.on_state_aliases(room, &aliases).await
            }
            AnySyncStateEvent::RoomAvatar(avatar) => {
                event_emitter.on_state_avatar(room, &avatar).await
            }
            AnySyncStateEvent::RoomPowerLevels(power) => {
                event_emitter.on_state_power_levels(room, &power).await
            }
            AnySyncStateEvent::RoomJoinRules(rules) => {
                event_emitter.on_state_join_rules(room, &rules).await
            }
            AnySyncStateEvent::RoomTombstone(tomb) => {
                // TODO make `on_state_tombstone` method
                event_emitter.on_room_tombstone(room, &tomb).await
            }
            AnySyncStateEvent::Custom(custom) => {
                event_emitter
                    .on_custom_event(room, &CustomEvent::State(custom))
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
        event: &AnyBasicEvent,
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
            AnyBasicEvent::Presence(presence) => {
                event_emitter.on_non_room_presence(room, &presence).await
            }
            AnyBasicEvent::IgnoredUserList(ignored) => {
                event_emitter
                    .on_non_room_ignored_users(room, &ignored)
                    .await
            }
            AnyBasicEvent::PushRules(rules) => {
                event_emitter.on_non_room_push_rules(room, &rules).await
            }
            _ => {}
        }
    }

    pub(crate) async fn emit_ephemeral_event(
        &self,
        room_id: &RoomId,
        event: &AnySyncEphemeralRoomEvent,
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
            AnySyncEphemeralRoomEvent::FullyRead(full_read) => {
                event_emitter.on_non_room_fully_read(room, full_read).await
            }
            AnySyncEphemeralRoomEvent::Typing(typing) => {
                event_emitter.on_non_room_typing(room, typing).await
            }
            AnySyncEphemeralRoomEvent::Receipt(receipt) => {
                event_emitter.on_non_room_receipt(room, receipt).await
            }
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
        event: &Raw<T>,
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
            ee.on_unrecognized_event(room, event.json()).await;
        }
    }

    /// Get a `Sas` verification object with the given flow id.
    ///
    /// # Arguments
    ///
    /// * `flow_id` - The unique id that identifies a interactive verification
    ///     flow. For in-room verifications this will be the event id of the
    ///     *m.key.verification.request* event that started the flow, for the
    ///     to-device verification flows this will be the transaction id of the
    ///     *m.key.verification.start* event.
    #[cfg(feature = "encryption")]
    #[cfg_attr(feature = "docs", doc(cfg(encryption)))]
    pub async fn get_verification(&self, flow_id: &str) -> Option<Sas> {
        self.olm
            .lock()
            .await
            .as_ref()
            .and_then(|o| o.get_verification(flow_id))
    }

    /// Get a specific device of a user.
    ///
    /// # Arguments
    ///
    /// * `user_id` - The unique id of the user that the device belongs to.
    ///
    /// * `device_id` - The unique id of the device.
    ///
    /// Returns a `Device` if one is found and the crypto store didn't throw an
    /// error.
    ///
    /// This will always return None if the client hasn't been logged in.
    ///
    /// # Example
    ///
    /// ```
    /// # use std::convert::TryFrom;
    /// # use matrix_sdk_base::BaseClient;
    /// # use matrix_sdk_common::identifiers::UserId;
    /// # use futures::executor::block_on;
    /// # let alice = UserId::try_from("@alice:example.org").unwrap();
    /// # let client = BaseClient::new().unwrap();
    /// # block_on(async {
    /// let device = client.get_device(&alice, "DEVICEID".into()).await;
    ///
    /// println!("{:?}", device);
    /// # });
    /// ```
    #[cfg(feature = "encryption")]
    #[cfg_attr(feature = "docs", doc(cfg(encryption)))]
    pub async fn get_device(&self, user_id: &UserId, device_id: &DeviceId) -> Option<Device> {
        let olm = self.olm.lock().await;
        olm.as_ref()?.get_device(user_id, device_id).await
    }

    /// Get a map holding all the devices of an user.
    ///
    /// This will always return an empty map if the client hasn't been logged
    /// in.
    ///
    /// # Arguments
    ///
    /// * `user_id` - The unique id of the user that the devices belong to.
    ///
    /// # Panics
    ///
    /// Panics if the client hasn't been logged in and the crypto layer thus
    /// hasn't been initialized.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use std::convert::TryFrom;
    /// # use matrix_sdk_base::BaseClient;
    /// # use matrix_sdk_common::identifiers::UserId;
    /// # use futures::executor::block_on;
    /// # let alice = UserId::try_from("@alice:example.org").unwrap();
    /// # let client = BaseClient::new().unwrap();
    /// # block_on(async {
    /// let devices = client.get_user_devices(&alice).await.unwrap();
    ///
    /// for device in devices.devices() {
    ///     println!("{:?}", device);
    /// }
    /// # });
    /// ```
    #[cfg(feature = "encryption")]
    #[cfg_attr(feature = "docs", doc(cfg(encryption)))]
    pub async fn get_user_devices(
        &self,
        user_id: &UserId,
    ) -> StdResult<UserDevices, CryptoStoreError> {
        let olm = self.olm.lock().await;

        if let Some(olm) = olm.as_ref() {
            Ok(olm.get_user_devices(user_id).await?)
        } else {
            // TODO remove this panic.
            panic!("The client hasn't been logged in")
        }
    }
}

#[cfg(test)]
mod test {
    #[cfg(feature = "messages")]
    use crate::{
        events::AnySyncRoomEvent, identifiers::event_id, BaseClientConfig, JsonStore, Raw,
    };
    use crate::{
        identifiers::{room_id, user_id, RoomId},
        BaseClient, Session,
    };
    use matrix_sdk_common_macros::async_trait;
    use matrix_sdk_test::{async_test, test_json, EventBuilder, EventsJson};
    use serde_json::json;
    use std::convert::TryFrom;
    #[cfg(not(target_arch = "wasm32"))]
    use tempfile::tempdir;

    #[cfg(target_arch = "wasm32")]
    use wasm_bindgen_test::*;

    async fn get_client() -> BaseClient {
        let session = Session {
            access_token: "1234".to_owned(),
            user_id: user_id!("@example:localhost"),
            device_id: "DEVICEID".into(),
        };
        let client = BaseClient::new().unwrap();
        client.restore_login(session).await.unwrap();
        client
    }

    fn get_room_id() -> RoomId {
        room_id!("!SVkFJHzfwvuaIEawgC:localhost")
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
            "type": "m.room.member"
        })
    }

    #[async_test]
    async fn test_joined_room_creation() {
        let mut sync_response = EventBuilder::default()
            .add_state_event(EventsJson::Member)
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
            .add_custom_left_event(&room_id, member_event())
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
        let room_id = room_id!("!left_room:localhost");
        let mut sync_response = EventBuilder::default()
            .add_custom_left_event(&room_id, member_event())
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

        let mem = member_event();

        let mut sync_response = EventBuilder::default()
            .add_custom_joined_event(&room_id, mem)
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
        let room_id = room_id!("!invited_room:localhost");
        let mut sync_response = EventBuilder::default()
            .add_custom_invited_event(&room_id, member_event())
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
            .add_custom_joined_event(&room_id, member_event())
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
        use matrix_sdk_common::{
            events::{
                room::member::{MemberEventContent, MembershipChange},
                SyncStateEvent,
            },
            locks::RwLock,
        };
        use std::sync::{
            atomic::{AtomicBool, Ordering},
            Arc,
        };

        struct EE(Arc<AtomicBool>);
        #[async_trait]
        impl EventEmitter for EE {
            async fn on_room_member(
                &self,
                room: SyncRoom,
                event: &SyncStateEvent<MemberEventContent>,
            ) {
                if let SyncRoom::Joined(_) = room {
                    if let MembershipChange::Joined = event.membership_change() {
                        self.0.swap(true, Ordering::SeqCst);
                    }
                }
                if event.prev_content.is_none() {
                    self.0.swap(false, Ordering::SeqCst);
                }
            }
        }

        let room_id = get_room_id();
        let user_id = user_id!("@example:localhost");

        let passed = Arc::new(AtomicBool::default());
        let emitter = EE(Arc::clone(&passed));
        let mut client = get_client().await;

        client.event_emitter = Arc::new(RwLock::new(Some(Box::new(emitter))));

        // We can't do this through `EventBuilder` since it goes through a de/ser cycle and the
        // `prev_content` is lost. Luckily, this test won't be needed once ruma fixes
        // `prev_content` parsing.
        let join_event: serde_json::Value = serde_json::json!({
            "content": {
                "avatar_url": null,
                "displayname": "example",
                "membership": "join"
            },
            "event_id": "$151800140517rfvjc:localhost",
            "membership": "join",
            "origin_server_ts": 151800140,
            "sender": user_id.as_ref(),
            "state_key": user_id.as_ref(),
            "type": "m.room.member",
            "unsigned": {
                "age": 297036,
                "replaces_state": "$151800111315tsynI:localhost",
                "prev_content": {
                    "avatar_url": null,
                    "displayname": "example",
                    "membership": "invite"
                }
            }
        });

        let display_name_change_event: serde_json::Value = serde_json::json!({
            "content": {
                "avatar_url": null,
                "displayname": "changed",
                "membership": "join"
            },
            "event_id": "$191804320221Tallh:localhost",
            "membership": "join",
            "origin_server_ts": 151800140,
            "sender": user_id.as_ref(),
            "state_key": user_id.as_ref(),
            "type": "m.room.member",
            "unsigned": {
                "age": 297036,
                "replaces_state": "$151800140517rfvjc:localhost",
                "prev_content": {
                    "avatar_url": null,
                    "displayname": "example",
                    "membership": "join"
                }
            }
        });

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
                "events": vec![ join_event, display_name_change_event ],
                "limited": true,
                "prev_batch": "t392-516_47314_0_7_1_1_1_11444_1"
            },
            "unread_notifications": {
                "highlight_count": 0,
                "notification_count": 11
            }
        });
        joined_rooms.insert(room_id.clone(), joined_room);

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

        // This is a tricky test. Since we receive and emit the event separately, we have to test
        // both paths.

        // This first part tests that the event was received correctly (with
        // `prev_content` hoisted).
        //
        // However, we can't simply test that the member is joined since a missing `prev_content`
        // is considered to be `"membership": "invite"` by default, which would still work out
        // correctly. Hence we test that his display name was changed.
        let room = client.get_joined_room(&room_id).await.unwrap();
        let room = room.read().await;
        let member = room.joined_members.get(&user_id).unwrap();
        assert_eq!(*member.display_name.as_ref().unwrap(), "changed");

        // The second part tests that the event is emitted correctly. If `prev_content` were
        // missing, this bool would had been flipped.
        assert!(passed.load(Ordering::SeqCst))
    }

    #[async_test]
    async fn test_unrecognized_events() {
        use super::*;

        use crate::{EventEmitter, SyncRoom};
        use matrix_sdk_common::{events::EventContent, locks::RwLock};
        use std::sync::{
            atomic::{AtomicBool, Ordering},
            Arc,
        };

        struct EE(Arc<AtomicBool>);
        #[async_trait]
        impl EventEmitter for EE {
            async fn on_custom_event(&self, room: SyncRoom, event: &CustomEvent<'_>) {
                if let SyncRoom::Joined(_) = room {
                    if let CustomEvent::Message(event) = event {
                        if event.content.event_type() == "m.room.not_real" {
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
        let event = json!({
            "content": {
                "whatever": "you want"
            },
            "event_id": "$eventid:foo",
            "origin_server_ts": 159026265,
            "sender": "@alice:matrix.org",
            "type": "m.room.not_real",
            "unsigned": {
                "age": 85
            }
        });

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
        use matrix_sdk_common::{api::r0::sync::sync_events, locks::RwLock};
        use std::sync::{
            atomic::{AtomicBool, Ordering},
            Arc,
        };

        struct EE(Arc<AtomicBool>);
        #[async_trait]
        impl EventEmitter for EE {
            async fn on_custom_event(&self, room: SyncRoom, event: &CustomEvent<'_>) {
                if let SyncRoom::Joined(_) = room {
                    if let CustomEvent::Message(custom) = event {
                        if custom.content.event_type == "m.reaction"
                            && custom.content.json.get("m.relates_to").is_some()
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
        let mut sync = sync_events::Response::try_from(response).unwrap();

        client.receive_sync_response(&mut sync).await.unwrap();

        assert!(passed.load(Ordering::SeqCst))
    }

    #[cfg(feature = "messages")]
    #[async_test]
    async fn message_queue_redaction_event_store_deser() {
        use std::ops::Deref;

        let room_id = room_id!("!SVkFJHzfwvuaIEawgC:localhost");

        let session = Session {
            access_token: "1234".to_owned(),
            user_id: user_id!("@cheeky_monkey:matrix.org"),
            device_id: "DEVICEID".into(),
        };

        let _m = mockito::mock(
            "GET",
            mockito::Matcher::Regex(r"^/_matrix/client/r0/sync\?.*$".to_string()),
        )
        .with_status(200)
        .with_body(test_json::SYNC.to_string())
        .create();

        let dir = tempdir().unwrap();
        // a sync response to populate our JSON store
        let config =
            BaseClientConfig::default().state_store(Box::new(JsonStore::open(dir.path()).unwrap()));
        let client = BaseClient::new_with_config(config).unwrap();
        client.restore_login(session.clone()).await.unwrap();

        let response = http::Response::builder()
            .body(serde_json::to_vec(test_json::SYNC.deref()).unwrap())
            .unwrap();
        let mut sync =
            matrix_sdk_common::api::r0::sync::sync_events::Response::try_from(response).unwrap();

        client.receive_sync_response(&mut sync).await.unwrap();

        let json = serde_json::json!({
            "content": {
                "reason": "😀"
            },
            "event_id": "$XXXX:localhost",
            "origin_server_ts": 151957878,
            "sender": "@example:localhost",
            "type": "m.room.redaction",
            "redacts": "$152037280074GZeOm:localhost"
        });
        let mut event: Raw<AnySyncRoomEvent> = serde_json::from_value(json).unwrap();
        client
            .receive_joined_timeline_event(&room_id, &mut event)
            .await
            .unwrap();

        // check that the message has actually been redacted
        for room in client.joined_rooms().read().await.values() {
            let queue = &room.read().await.messages;
            if let crate::events::AnyPossiblyRedactedSyncMessageEvent::Redacted(
                crate::events::AnyRedactedSyncMessageEvent::RoomMessage(event),
            ) = &queue.msgs[0]
            {
                // this is the id from the message event in the sync response
                assert_eq!(event.event_id, event_id!("$152037280074GZeOm:localhost"))
            } else {
                panic!("message event in message queue should be redacted")
            }
        }

        // `receive_joined_timeline_event` does not save the state to the store
        // so we must do it ourselves
        client.store_room_state(&room_id).await.unwrap();

        // we load state from the store only
        let config =
            BaseClientConfig::default().state_store(Box::new(JsonStore::open(dir.path()).unwrap()));
        let client = BaseClient::new_with_config(config).unwrap();
        client.restore_login(session).await.unwrap();

        // make sure that our redacted message event is redacted and that ser/de works
        // properly
        for room in client.joined_rooms().read().await.values() {
            let queue = &room.read().await.messages;
            if let crate::events::AnyPossiblyRedactedSyncMessageEvent::Redacted(
                crate::events::AnyRedactedSyncMessageEvent::RoomMessage(event),
            ) = &queue.msgs[0]
            {
                // this is the id from the message event in the sync response
                assert_eq!(event.event_id, event_id!("$152037280074GZeOm:localhost"))
            } else {
                panic!("[post store sync] message event in message queue should be redacted")
            }
        }
    }

    #[async_test]
    #[cfg(feature = "encryption")]
    async fn test_group_session_invalidation() {
        let client = get_client().await;
        let room_id = get_room_id();

        let mut sync_response = EventBuilder::default()
            .add_state_event(EventsJson::Member)
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
