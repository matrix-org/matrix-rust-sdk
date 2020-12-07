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
    convert::TryFrom,
    fmt,
    path::{Path, PathBuf},
    result::Result as StdResult,
    sync::Arc,
};

use dashmap::DashMap;

use futures::StreamExt;
#[cfg(feature = "encryption")]
use matrix_sdk_common::locks::Mutex;
use matrix_sdk_common::{
    api::r0 as api,
    events::{
        room::member::MemberEventContent, AnyBasicEvent, AnyStrippedStateEvent, AnySyncRoomEvent,
        AnySyncStateEvent, StateEvent, SyncStateEvent,
    },
    identifiers::{RoomId, UserId},
    locks::RwLock,
    Raw,
};
#[cfg(feature = "encryption")]
use matrix_sdk_common::{
    api::r0::keys::claim_keys::Request as KeysClaimRequest,
    events::{room::encrypted::EncryptedEventContent, AnyMessageEventContent, AnySyncMessageEvent},
    identifiers::DeviceId,
    uuid::Uuid,
};
#[cfg(feature = "encryption")]
use matrix_sdk_crypto::{
    store::{CryptoStore, CryptoStoreError},
    Device, EncryptionSettings, IncomingResponse, OlmError, OlmMachine, OutgoingRequest, Sas,
    ToDeviceRequest, UserDevices,
};
use tracing::{info, warn};
use zeroize::Zeroizing;

use crate::{
    error::Result,
    responses::{
        AccountData, Ephemeral, JoinedRoom, LeftRoom, Presence, Rooms, State, SyncResponse,
        Timeline,
    },
    session::Session,
    store::{InnerSummary, Room, RoomType, StateChanges, Store},
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

/// Transform state event by hoisting `prev_content` field from `unsigned` to the top level.
///
/// Due to a [bug in synapse][synapse-bug], `prev_content` often ends up in `unsigned` contrary to
/// the C2S spec. Some more discussion can be found [here][discussion]. Until this is fixed in
/// synapse or handled in Ruma, we use this to hoist up `prev_content` to the top level.
///
/// [synapse-bug]: <https://github.com/matrix-org/matrix-doc/issues/684#issuecomment-641182668>
/// [discussion]: <https://github.com/matrix-org/matrix-doc/issues/684#issuecomment-641182668>
pub fn hoist_and_deserialize_state_event(
    event: &Raw<AnySyncStateEvent>,
) -> StdResult<AnySyncStateEvent, serde_json::Error> {
    let prev_content = serde_json::from_str::<AdditionalEventData>(event.json().get())?
        .unsigned
        .prev_content;

    let mut ev = event.deserialize()?;

    if let AnySyncStateEvent::RoomMember(ref mut member) = ev {
        if member.prev_content.is_none() {
            member.prev_content = prev_content.map(|e| e.deserialize().ok()).flatten();
        }
    }

    Ok(ev)
}

fn hoist_member_event(
    event: &Raw<StateEvent<MemberEventContent>>,
) -> StdResult<StateEvent<MemberEventContent>, serde_json::Error> {
    let prev_content = serde_json::from_str::<AdditionalEventData>(event.json().get())?
        .unsigned
        .prev_content;

    let mut e = event.deserialize()?;

    if e.prev_content.is_none() {
        e.prev_content = prev_content.map(|e| e.deserialize().ok()).flatten();
    }

    Ok(e)
}

fn hoist_room_event_prev_content(
    event: &Raw<AnySyncRoomEvent>,
) -> StdResult<AnySyncRoomEvent, serde_json::Error> {
    let prev_content = serde_json::from_str::<AdditionalEventData>(event.json().get())
        .map(|more_unsigned| more_unsigned.unsigned)
        .map(|additional| additional.prev_content)?
        .map(|p| p.deserialize().ok())
        .flatten();

    let mut ev = event.deserialize()?;

    match &mut ev {
        AnySyncRoomEvent::State(AnySyncStateEvent::RoomMember(ref mut member))
            if member.prev_content.is_none() =>
        {
            member.prev_content = prev_content;
        }
        _ => (),
    }

    Ok(ev)
}

fn stripped_deserialize_prev_content(
    event: &Raw<AnyStrippedStateEvent>,
) -> Option<AdditionalUnsignedData> {
    serde_json::from_str::<AdditionalEventData>(event.json().get())
        .map(|more_unsigned| more_unsigned.unsigned)
        .ok()
}

fn handle_membership(
    changes: &mut StateChanges,
    room_id: &RoomId,
    event: &SyncStateEvent<MemberEventContent>,
) {
    use matrix_sdk_common::events::room::member::MembershipState::*;
    match &event.content.membership {
        Join => {
            info!("ADDING MEMBER {} to {}", event.state_key, room_id);
            changes.add_joined_member(room_id, event.clone())
            // TODO check if the display name is
            // ambigous
        }
        Invite => {
            info!("ADDING INVITED MEMBER {} to {}", event.state_key, room_id);
            changes.add_invited_member(room_id, event.clone())
        }
        membership => info!("UNHANDLED MEMBERSHIP {} {:?}", event.state_key, membership),
    }
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
    /// Database
    store: Store,
    rooms: Arc<DashMap<RoomId, Room>>,
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
        let store = if let Some(path) = &config.store_path {
            info!("Opening store in path {}", path.display());
            Store::open_with_path(path)
        } else {
            Store::open()
        };

        Ok(BaseClient {
            session: Arc::new(RwLock::new(None)),
            sync_token: Arc::new(RwLock::new(None)),
            store,
            rooms: Arc::new(DashMap::new()),
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
        // if self.state_store.read().await.is_none() {
        //     #[cfg(not(target_arch = "wasm32"))]
        //     if let Some(path) = &*self.store_path {
        //         let store = JsonStore::open(path)?;
        //         *self.state_store.write().await = Some(Box::new(store));
        //     }
        // }

        // self.sync_with_state_store(&session).await?;

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

    /// Get the current, if any, sync token of the client.
    /// This will be None if the client didn't sync at least once.
    pub async fn sync_token(&self) -> Option<String> {
        self.sync_token.read().await.clone()
    }

    async fn get_or_create_room(&self, room_id: &RoomId, room_type: RoomType) -> Room {
        let session = self.session.read().await;
        let user_id = &session
            .as_ref()
            .expect("Creating room while not being logged in")
            .user_id;

        self.rooms
            .entry(room_id.clone())
            .or_insert_with(|| Room::new(user_id, self.store.clone(), room_id, room_type))
            .clone()
    }

    async fn handle_timeline(
        &self,
        room_id: &RoomId,
        ruma_timeline: &api::sync::sync_events::Timeline,
        summary: &mut InnerSummary,
        mut changes: &mut StateChanges,
    ) -> Timeline {
        let mut timeline = Timeline::new(ruma_timeline.limited, ruma_timeline.prev_batch.clone());

        for event in &ruma_timeline.events {
            if let Ok(mut e) = hoist_room_event_prev_content(event) {
                match &mut e {
                    AnySyncRoomEvent::State(s) => match s {
                        AnySyncStateEvent::RoomMember(member) => {
                            handle_membership(&mut changes, room_id, member);
                        }
                        _ => {
                            summary.handle_state_event(&s);
                            changes.add_state_event(room_id, s.clone());
                        }
                    },
                    AnySyncRoomEvent::Message(message) =>
                    {
                        #[cfg(feature = "encryption")]
                        if let AnySyncMessageEvent::RoomEncrypted(encrypted) = message {
                            if let Some(olm) = self.olm_machine().await {
                                if let Ok(decrypted) =
                                    olm.decrypt_room_event(encrypted, room_id).await
                                {
                                    match decrypted.deserialize() {
                                        Ok(decrypted) => e = decrypted,
                                        Err(e) => {
                                            warn!("Error deserializing a decrypted event {:?} ", e)
                                        }
                                    }
                                }
                            }
                        }
                    }
                    _ => (),
                }

                timeline.events.push(e);
            }
        }

        timeline
    }

    async fn handle_state(
        &self,
        room_id: &RoomId,
        events: &[Raw<AnySyncStateEvent>],
        summary: &mut InnerSummary,
        mut changes: &mut StateChanges,
    ) -> State {
        let mut state = State::default();

        for e in events {
            if let Ok(event) = hoist_and_deserialize_state_event(e) {
                match &event {
                    AnySyncStateEvent::RoomMember(member) => {
                        handle_membership(&mut changes, room_id, member);
                    }
                    e => {
                        summary.handle_state_event(&e);
                        changes.add_state_event(room_id, e.clone());
                    }
                }

                state.events.push(event);
            }
        }

        state
    }

    async fn handle_room_account_data(
        &self,
        room_id: &RoomId,
        events: &[Raw<AnyBasicEvent>],
        changes: &mut StateChanges,
    ) -> AccountData {
        let events: Vec<AnyBasicEvent> =
            events.iter().filter_map(|e| e.deserialize().ok()).collect();

        for event in &events {
            changes.add_room_account_data(room_id, event.clone());
        }

        AccountData { events }
    }

    /// Receive a response from a sync call.
    ///
    /// # Arguments
    ///
    /// * `response` - The response that we received after a successful sync.
    pub async fn receive_sync_response(
        &self,
        mut response: api::sync::sync_events::Response,
    ) -> Result<SyncResponse> {
        // The server might respond multiple times with the same sync token, in
        // that case we already received this response and there's nothing to
        // do.
        if self.sync_token.read().await.as_ref() == Some(&response.next_batch) {
            return Ok(SyncResponse::new(response.next_batch));
        }

        #[cfg(feature = "encryption")]
        {
            let olm = self.olm.lock().await;

            if let Some(o) = &*olm {
                // Let the crypto machine handle the sync response, this
                // decryptes to-device events, but leaves room events alone.
                // This makes sure that we have the deryption keys for the room
                // events at hand.
                o.receive_sync_response(&mut response).await?;
            }
        }

        let mut changes = StateChanges::default();
        let mut rooms = Rooms::default();

        for (room_id, room_info) in response.rooms.join {
            let room = self.get_or_create_room(&room_id, RoomType::Joined).await;
            let mut summary = room.clone_summary();

            summary.mark_as_joined();
            summary.update(&room_info.summary);
            summary.set_prev_batch(room_info.timeline.prev_batch.as_deref());

            let state = self
                .handle_state(
                    &room_id,
                    &room_info.state.events,
                    &mut summary,
                    &mut changes,
                )
                .await;

            let timeline = self
                .handle_timeline(&room_id, &room_info.timeline, &mut summary, &mut changes)
                .await;

            let account_data = self
                .handle_room_account_data(&room_id, &room_info.account_data.events, &mut changes)
                .await;

            #[cfg(feature = "encryption")]
            if summary.is_encrypted() {
                // TODO if the room isn't encrypted but the new summary is,
                // add all the room users.
                if let Some(o) = self.olm_machine().await {
                    if let Some(users) = changes.joined_user_ids.get(&room_id) {
                        o.update_tracked_users(users).await
                    }

                    if let Some(users) = changes.invited_user_ids.get(&room_id) {
                        o.update_tracked_users(users).await
                    }
                }
            }

            if room_info.timeline.limited {
                summary.mark_members_missing();
            }

            let notification_count = room_info.unread_notifications.into();
            summary.update_notification_count(notification_count);

            // TODO should we store this?
            let ephemeral = Ephemeral {
                events: room_info
                    .ephemeral
                    .events
                    .into_iter()
                    .filter_map(|e| e.deserialize().ok())
                    .collect(),
            };

            rooms.join.insert(
                room_id,
                JoinedRoom::new(
                    timeline,
                    state,
                    account_data,
                    ephemeral,
                    notification_count,
                ),
            );

            changes.add_room(summary);
        }

        for (room_id, room_info) in response.rooms.leave {
            let room = self.get_or_create_room(&room_id, RoomType::Left).await;
            let mut summary = room.clone_summary();
            summary.mark_as_left();

            let state = self
                .handle_state(
                    &room_id,
                    &room_info.state.events,
                    &mut summary,
                    &mut changes,
                )
                .await;

            let timeline = self
                .handle_timeline(&room_id, &room_info.timeline, &mut summary, &mut changes)
                .await;

            let account_data = self
                .handle_room_account_data(&room_id, &room_info.account_data.events, &mut changes)
                .await;

            rooms
                .leave
                .insert(room_id, LeftRoom::new(timeline, state, account_data));
        }

        for event in &response.presence.events {
            if let Ok(e) = event.deserialize() {
                changes.add_presence_event(e);
            }
        }

        self.store.save_changes(&changes).await;
        *self.sync_token.write().await = Some(response.next_batch.clone());
        self.apply_changes(&changes).await;

        Ok(SyncResponse {
            next_batch: response.next_batch,
            rooms,
            presence: Presence {
                events: changes.presence.into_iter().map(|(_, v)| v).collect(),
            },
            device_lists: response.device_lists,
            device_one_time_keys_count: response
                .device_one_time_keys_count
                .into_iter()
                .map(|(k, v)| (k, v.into()))
                .collect(),

            ..Default::default()
        })
    }

    async fn apply_changes(&self, changes: &StateChanges) {
        // TODO emit room changes here
        for (room_id, summary) in &changes.room_summaries {
            if let Some(room) = self.get_room(&room_id) {
                room.update_summary(summary.clone())
            }
        }
    }

    pub async fn receive_members(
        &self,
        room_id: &RoomId,
        response: &api::membership::get_member_events::Response,
    ) -> Result<()> {
        if let Some(room) = self.get_room(room_id) {
            let mut summary = room.clone_summary();
            summary.mark_members_synced();

            let mut changes = StateChanges::default();

            // TODO make sure we don't overwrite memership events from a sync.
            for e in &response.chunk {
                if let Ok(event) = hoist_member_event(e) {
                    if let Ok(user_id) = UserId::try_from(event.state_key.as_str()) {
                        if self
                            .store
                            .get_member_event(room_id, &user_id)
                            .await
                            .is_none()
                        {
                            handle_membership(&mut changes, room_id, &event.into());
                        }
                    }
                }
            }

            #[cfg(feature = "encryption")]
            if summary.is_encrypted() {
                if let Some(o) = self.olm_machine().await {
                    if let Some(users) = changes.joined_user_ids.get(room_id) {
                        o.update_tracked_users(users).await
                    }

                    if let Some(users) = changes.invited_user_ids.get(room_id) {
                        o.update_tracked_users(users).await
                    }
                }
            }

            changes.add_room(summary);

            self.store.save_changes(&changes).await;
            self.apply_changes(&changes).await;
        }

        Ok(())
    }

    pub async fn receive_filter_upload(
        &self,
        filter_name: &str,
        response: &api::filter::create_filter::Response,
    ) {
        self.store
            .save_filter(filter_name, &response.filter_id)
            .await;
    }

    pub async fn get_filter(&self, filter_name: &str) -> Option<String> {
        self.store.get_filter(filter_name).await
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
        users: &mut impl Iterator<Item = &UserId>,
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
    pub async fn share_group_session(&self, room_id: &RoomId) -> Result<Vec<Arc<ToDeviceRequest>>> {
        let olm = self.olm.lock().await;

        match &*olm {
            Some(o) => {
                let joined = self.store.get_joined_user_ids(room_id).await;
                let invited = self.store.get_invited_user_ids(room_id).await;
                // TODO don't use collect here.
                let members: Vec<UserId> = joined.chain(invited).collect().await;
                Ok(
                    o.share_group_session(room_id, members.iter(), EncryptionSettings::default())
                        .await?,
                )
            }
            None => panic!("Olm machine wasn't started"),
        }
    }

    pub fn get_room(&self, room_id: &RoomId) -> Option<Room> {
        self.rooms.get(room_id).map(|r| r.clone())
    }

    /// Encrypt a message event content.
    #[cfg(feature = "encryption")]
    #[cfg_attr(feature = "docs", doc(cfg(encryption)))]
    pub async fn encrypt(
        &self,
        room_id: &RoomId,
        content: impl Into<AnyMessageEventContent>,
    ) -> Result<EncryptedEventContent> {
        let olm = self.olm.lock().await;

        match &*olm {
            Some(o) => Ok(o.encrypt(room_id, content.into()).await?),
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
    pub async fn get_device(
        &self,
        user_id: &UserId,
        device_id: &DeviceId,
    ) -> StdResult<Option<Device>, CryptoStoreError> {
        let olm = self.olm.lock().await;

        if let Some(olm) = olm.as_ref() {
            olm.get_device(user_id, device_id).await
        } else {
            Ok(None)
        }
    }

    /// Get the user login session.
    ///
    /// If the client is currently logged in, this will return a `matrix_sdk::Session` object which
    /// can later be given to `restore_login`.
    ///
    /// Returns a session object if the client is logged in. Otherwise returns `None`.
    pub async fn get_session(&self) -> Option<Session> {
        self.session.read().await.clone()
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

    /// Get the olm machine.
    #[cfg(feature = "encryption")]
    #[cfg_attr(feature = "docs", doc(cfg(encryption)))]
    pub async fn olm_machine(&self) -> Option<OlmMachine> {
        let olm = self.olm.lock().await;
        olm.as_ref().cloned()
    }
}

#[cfg(test)]
mod test {}
