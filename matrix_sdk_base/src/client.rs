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
    collections::{BTreeMap, BTreeSet},
    convert::TryFrom,
    fmt,
    path::{Path, PathBuf},
    result::Result as StdResult,
    sync::Arc,
};

use matrix_sdk_common::{
    api::r0 as api,
    deserialized_responses::{
        AccountData, AmbiguityChanges, Ephemeral, InviteState, InvitedRoom, JoinedRoom, LeftRoom,
        MemberEvent, MembersResponse, Presence, Rooms, State, StrippedMemberEvent, SyncResponse,
        Timeline,
    },
    events::{
        presence::PresenceEvent,
        room::{
            history_visibility::HistoryVisibility,
            member::{MemberEventContent, MembershipState},
        },
        AnyBasicEvent, AnyStrippedStateEvent, AnySyncRoomEvent, AnySyncStateEvent,
        AnyToDeviceEvent, EventContent, StateEvent,
    },
    identifiers::{RoomId, UserId},
    instant::Instant,
    locks::RwLock,
    Raw,
};
#[cfg(feature = "encryption")]
use matrix_sdk_common::{
    api::r0::keys::claim_keys::Request as KeysClaimRequest,
    events::{room::encrypted::EncryptedEventContent, AnyMessageEventContent, AnySyncMessageEvent},
    identifiers::DeviceId,
    locks::Mutex,
    uuid::Uuid,
};
#[cfg(feature = "encryption")]
use matrix_sdk_crypto::{
    store::{CryptoStore, CryptoStoreError},
    Device, EncryptionSettings, IncomingResponse, MegolmError, OlmError, OlmMachine,
    OutgoingRequest, Sas, ToDeviceRequest, UserDevices,
};
use tracing::{info, warn};
use zeroize::Zeroizing;

use crate::{
    error::Result,
    rooms::{Room, RoomInfo, RoomType},
    session::Session,
    store::{ambiguity_map::AmbiguityCache, Result as StoreResult, StateChanges, Store},
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
            member.prev_content = prev_content.and_then(|e| e.deserialize().ok());
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
        e.prev_content = prev_content.and_then(|e| e.deserialize().ok());
    }

    Ok(e)
}

fn hoist_room_event_prev_content(
    event: &Raw<AnySyncRoomEvent>,
) -> StdResult<AnySyncRoomEvent, serde_json::Error> {
    let prev_content = serde_json::from_str::<AdditionalEventData>(event.json().get())
        .map(|more_unsigned| more_unsigned.unsigned)
        .map(|additional| additional.prev_content)?
        .and_then(|p| p.deserialize().ok());

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
    #[cfg(feature = "encryption")]
    olm: Arc<Mutex<Option<OlmMachine>>>,
    #[cfg(feature = "encryption")]
    cryptostore: Arc<Mutex<Option<Box<dyn CryptoStore>>>>,
    store_path: Arc<Option<PathBuf>>,
    store_passphrase: Arc<Option<Zeroizing<String>>>,
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
        #[cfg(feature = "sled_state_store")]
        let stores = if let Some(path) = &config.store_path {
            if config.passphrase.is_some() {
                info!("Opening an encrypted store in path {}", path.display());
            } else {
                info!("Opening store in path {}", path.display());
            }
            Store::open_default(path, config.passphrase.as_deref().map(|p| p.as_str()))?
        } else {
            Store::open_temporary()?
        };
        #[cfg(not(feature = "sled_state_store"))]
        let stores = Store::open_memory_store();

        #[cfg(all(feature = "encryption", feature = "sled_state_store"))]
        let crypto_store = if config.crypto_store.is_none() {
            #[cfg(feature = "sled_cryptostore")]
            let store: Option<Box<dyn CryptoStore>> = Some(Box::new(
                matrix_sdk_crypto::store::SledStore::open_with_database(
                    stores.1,
                    config.passphrase.as_deref().map(|p| p.as_str()),
                )
                .map_err(OlmError::Store)?,
            ));
            #[cfg(not(feature = "sled_cryptostore"))]
            let store = config.crypto_store;

            store
        } else {
            config.crypto_store
        };
        #[cfg(all(not(feature = "sled_state_store"), feature = "encryption"))]
        let crypto_store = config.crypto_store;

        #[cfg(feature = "sled_state_store")]
        let store = stores.0;
        #[cfg(not(feature = "sled_state_store"))]
        let store = stores;

        Ok(BaseClient {
            session: store.session.clone(),
            sync_token: store.sync_token.clone(),
            store,
            #[cfg(feature = "encryption")]
            olm: Mutex::new(None).into(),
            #[cfg(feature = "encryption")]
            cryptostore: Mutex::new(crypto_store).into(),
            store_path: config.store_path.into(),
            store_passphrase: config.passphrase.into(),
        })
    }

    /// The current client session containing our user id, device id and access
    /// token.
    pub fn session(&self) -> &Arc<RwLock<Option<Session>>> {
        &self.session
    }

    /// Get a reference to the store.
    pub fn store(&self) -> &Store {
        &self.store
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
        self.store.restore_session(session.clone()).await?;

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
                #[cfg(feature = "sled_cryptostore")]
                {
                    *olm = Some(
                        OlmMachine::new_with_default_store(
                            &session.user_id,
                            &session.device_id,
                            path,
                            self.store_passphrase.as_deref().map(|p| p.as_str()),
                        )
                        .await
                        .map_err(OlmError::from)?,
                    );
                }
                #[cfg(not(feature = "sled_cryptostore"))]
                {
                    let _ = path;
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

    async fn handle_timeline(
        &self,
        room_id: &RoomId,
        ruma_timeline: api::sync::sync_events::Timeline,
        room_info: &mut RoomInfo,
        changes: &mut StateChanges,
        ambiguity_cache: &mut AmbiguityCache,
        user_ids: &mut BTreeSet<UserId>,
    ) -> StoreResult<Timeline> {
        let mut timeline = Timeline::new(ruma_timeline.limited, ruma_timeline.prev_batch.clone());

        for event in ruma_timeline.events {
            match hoist_room_event_prev_content(&event) {
                Ok(mut e) => {
                    match &mut e {
                        AnySyncRoomEvent::State(s) => match s {
                            AnySyncStateEvent::RoomMember(member) => {
                                if let Ok(member) = MemberEvent::try_from(member.clone()) {
                                    ambiguity_cache
                                        .handle_event(changes, room_id, &member)
                                        .await?;

                                    match member.content.membership {
                                        MembershipState::Join | MembershipState::Invite => {
                                            user_ids.insert(member.state_key.clone());
                                        }
                                        _ => {
                                            user_ids.remove(&member.state_key);
                                        }
                                    }

                                    // Senders can fake the profile easily so we keep track
                                    // of profiles that the member set themselves to avoid
                                    // having confusing profile changes when a member gets
                                    // kicked/banned.
                                    if member.state_key == member.sender {
                                        changes
                                            .profiles
                                            .entry(room_id.clone())
                                            .or_insert_with(BTreeMap::new)
                                            .insert(member.sender.clone(), member.content.clone());
                                    }

                                    changes
                                        .members
                                        .entry(room_id.clone())
                                        .or_insert_with(BTreeMap::new)
                                        .insert(member.state_key.clone(), member);
                                }
                            }
                            _ => {
                                room_info.handle_state_event(&s.content());
                                changes.add_state_event(room_id, s.clone());
                            }
                        },

                        #[cfg(feature = "encryption")]
                        AnySyncRoomEvent::Message(AnySyncMessageEvent::RoomEncrypted(
                            encrypted,
                        )) => {
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
                        // TODO if there is redacted state save the room id,
                        // event type and state key, add a method to get the
                        // requests that are needed to be called to heal this
                        // redacted state.
                        _ => (),
                    }

                    timeline.events.push(e);
                }
                Err(e) => {
                    warn!("Error deserializing event {:?}", e);
                }
            }
        }

        Ok(timeline)
    }

    #[allow(clippy::type_complexity)]
    fn handle_invited_state(
        &self,
        events: Vec<Raw<AnyStrippedStateEvent>>,
        room_info: &mut RoomInfo,
    ) -> (
        InviteState,
        BTreeMap<UserId, StrippedMemberEvent>,
        BTreeMap<String, BTreeMap<String, AnyStrippedStateEvent>>,
    ) {
        events.into_iter().fold(
            (InviteState::default(), BTreeMap::new(), BTreeMap::new()),
            |(mut state, mut members, mut state_events), e| {
                match e.deserialize() {
                    Ok(e) => {
                        state.events.push(e.clone());

                        if let AnyStrippedStateEvent::RoomMember(member) = e {
                            match StrippedMemberEvent::try_from(member) {
                                Ok(m) => {
                                    members.insert(m.state_key.clone(), m);
                                }
                                Err(e) => warn!(
                                    "Stripped member event in room {} has an invalid state key {:?}",
                                    room_info.room_id, e
                                ),
                            }
                        } else {
                            room_info.handle_state_event(&e.content());
                            state_events
                                .entry(e.content().event_type().to_owned())
                                .or_insert_with(BTreeMap::new)
                                .insert(e.state_key().to_owned(), e);
                        }
                    }
                    Err(err) => {
                        warn!(
                            "Couldn't deserialize stripped state event for room {}: {:?}",
                            room_info.room_id, err
                        );
                    }
                }
                (state, members, state_events)
            },
        )
    }

    async fn handle_state(
        &self,
        changes: &mut StateChanges,
        ambiguity_cache: &mut AmbiguityCache,
        events: Vec<Raw<AnySyncStateEvent>>,
        room_info: &mut RoomInfo,
    ) -> StoreResult<(State, BTreeSet<UserId>)> {
        let mut state = State::default();
        let mut members = BTreeMap::new();
        let mut state_events = BTreeMap::new();
        let mut user_ids = BTreeSet::new();
        let mut profiles = BTreeMap::new();

        let room_id = room_info.room_id.clone();

        for event in
            events
                .into_iter()
                .filter_map(|e| match hoist_and_deserialize_state_event(&e) {
                    Ok(e) => Some(e),
                    Err(err) => {
                        warn!(
                            "Couldn't deserialize state event for room {}: {:?} {:#?}",
                            room_id, err, e
                        );
                        None
                    }
                })
        {
            state.events.push(event.clone());
            room_info.handle_state_event(&event.content());

            if let AnySyncStateEvent::RoomMember(member) = event {
                match MemberEvent::try_from(member) {
                    Ok(m) => {
                        ambiguity_cache.handle_event(changes, &room_id, &m).await?;

                        match m.content.membership {
                            MembershipState::Join | MembershipState::Invite => {
                                user_ids.insert(m.state_key.clone());
                            }
                            _ => (),
                        }

                        // Senders can fake the profile easily so we keep track
                        // of profiles that the member set themselves to avoid
                        // having confusing profile changes when a member gets
                        // kicked/banned.
                        if m.state_key == m.sender {
                            profiles.insert(m.sender.clone(), m.content.clone());
                        }

                        members.insert(m.state_key.clone(), m);
                    }
                    Err(e) => warn!(
                        "Member event in room {} has an invalid state key {:?}",
                        room_info.room_id, e
                    ),
                }
            } else {
                state_events
                    .entry(event.content().event_type().to_owned())
                    .or_insert_with(BTreeMap::new)
                    .insert(event.state_key().to_owned(), event);
            }
        }

        changes.members.insert(room_id.as_ref().clone(), members);
        changes.profiles.insert(room_id.as_ref().clone(), profiles);
        changes.state.insert(room_id.as_ref().clone(), state_events);

        Ok((state, user_ids))
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

    async fn handle_account_data(
        &self,
        events: Vec<Raw<AnyBasicEvent>>,
        changes: &mut StateChanges,
    ) {
        let events: Vec<AnyBasicEvent> =
            events.iter().filter_map(|e| e.deserialize().ok()).collect();

        for event in &events {
            if let AnyBasicEvent::Direct(e) = event {
                for (user_id, rooms) in e.content.iter() {
                    for room_id in rooms {
                        if let Some(room) = changes.room_infos.get_mut(room_id) {
                            room.base_info.dm_target = Some(user_id.clone());
                        } else if let Some(room) = self.store.get_bare_room(room_id) {
                            let mut info = room.clone_info();
                            info.base_info.dm_target = Some(user_id.clone());
                            changes.add_room(info);
                        }
                    }
                }
            }
        }

        let account_data: BTreeMap<String, AnyBasicEvent> = events
            .into_iter()
            .map(|e| (e.content().event_type().to_owned(), e))
            .collect();

        changes.account_data = account_data;
    }

    /// Receive a response from a sync call.
    ///
    /// # Arguments
    ///
    /// * `response` - The response that we received after a successful sync.
    pub async fn receive_sync_response(
        &self,
        response: api::sync::sync_events::Response,
    ) -> Result<SyncResponse> {
        // The server might respond multiple times with the same sync token, in
        // that case we already received this response and there's nothing to
        // do.
        if self.sync_token.read().await.as_ref() == Some(&response.next_batch) {
            return Ok(SyncResponse::new(response.next_batch));
        }

        let now = Instant::now();

        #[cfg(feature = "encryption")]
        let to_device = {
            let olm = self.olm.lock().await;

            if let Some(o) = &*olm {
                // Let the crypto machine handle the sync response, this
                // decryptes to-device events, but leaves room events alone.
                // This makes sure that we have the deryption keys for the room
                // events at hand.
                o.receive_sync_changes(
                    &response.to_device,
                    &response.device_lists,
                    &response.device_one_time_keys_count,
                )
                .await?
            } else {
                response
                    .to_device
                    .events
                    .into_iter()
                    .filter_map(|e| e.deserialize().ok())
                    .collect::<Vec<AnyToDeviceEvent>>()
                    .into()
            }
        };
        #[cfg(not(feature = "encryption"))]
        let to_device = response
            .to_device
            .events
            .into_iter()
            .filter_map(|e| e.deserialize().ok())
            .collect::<Vec<AnyToDeviceEvent>>()
            .into();

        let mut changes = StateChanges::new(response.next_batch.clone());
        let mut ambiguity_cache = AmbiguityCache::new(self.store.clone());

        let mut rooms = Rooms::default();

        for (room_id, new_info) in response.rooms.join {
            let room = self
                .store
                .get_or_create_room(&room_id, RoomType::Joined)
                .await;
            let mut room_info = room.clone_info();
            room_info.mark_as_joined();

            room_info.update_summary(&new_info.summary);
            room_info.set_prev_batch(new_info.timeline.prev_batch.as_deref());

            let (state, mut user_ids) = self
                .handle_state(
                    &mut changes,
                    &mut ambiguity_cache,
                    new_info.state.events,
                    &mut room_info,
                )
                .await?;

            if new_info.timeline.limited {
                room_info.mark_members_missing();
            }

            let timeline = self
                .handle_timeline(
                    &room_id,
                    new_info.timeline,
                    &mut room_info,
                    &mut changes,
                    &mut ambiguity_cache,
                    &mut user_ids,
                )
                .await?;

            let account_data = self
                .handle_room_account_data(&room_id, &new_info.account_data.events, &mut changes)
                .await;

            #[cfg(feature = "encryption")]
            if room_info.is_encrypted() {
                if let Some(o) = self.olm_machine().await {
                    if !room.is_encrypted() {
                        // The room turned on encryption in this sync, we need
                        // to also get all the existing users and mark them for
                        // tracking.
                        let joined = self.store.get_joined_user_ids(&room_id).await?;
                        let invited = self.store.get_invited_user_ids(&room_id).await?;

                        let user_ids: Vec<&UserId> = joined.iter().chain(&invited).collect();
                        o.update_tracked_users(user_ids).await
                    }

                    o.update_tracked_users(&user_ids).await
                }
            }

            let notification_count = new_info.unread_notifications.into();
            room_info.update_notification_count(notification_count);

            let ephemeral = Ephemeral {
                events: new_info
                    .ephemeral
                    .events
                    .into_iter()
                    .filter_map(|e| e.deserialize().ok())
                    .collect(),
            };

            rooms.join.insert(
                room_id,
                JoinedRoom::new(timeline, state, account_data, ephemeral, notification_count),
            );

            changes.add_room(room_info);
        }

        for (room_id, new_info) in response.rooms.leave {
            let room = self
                .store
                .get_or_create_room(&room_id, RoomType::Left)
                .await;
            let mut room_info = room.clone_info();
            room_info.mark_as_left();

            let (state, mut user_ids) = self
                .handle_state(
                    &mut changes,
                    &mut ambiguity_cache,
                    new_info.state.events,
                    &mut room_info,
                )
                .await?;

            let timeline = self
                .handle_timeline(
                    &room_id,
                    new_info.timeline,
                    &mut room_info,
                    &mut changes,
                    &mut ambiguity_cache,
                    &mut user_ids,
                )
                .await?;

            let account_data = self
                .handle_room_account_data(&room_id, &new_info.account_data.events, &mut changes)
                .await;

            changes.add_room(room_info);
            rooms
                .leave
                .insert(room_id, LeftRoom::new(timeline, state, account_data));
        }

        for (room_id, new_info) in response.rooms.invite {
            {
                let room = self
                    .store
                    .get_or_create_room(&room_id, RoomType::Invited)
                    .await;
                let mut room_info = room.clone_info();
                room_info.mark_as_invited();
                changes.add_room(room_info);
            }

            let room = self.store.get_or_create_stripped_room(&room_id).await;
            let mut room_info = room.clone_info();

            let (state, members, state_events) =
                self.handle_invited_state(new_info.invite_state.events, &mut room_info);

            changes.stripped_members.insert(room_id.clone(), members);
            changes.stripped_state.insert(room_id.clone(), state_events);
            changes.add_stripped_room(room_info);

            let room = InvitedRoom {
                invite_state: state,
            };

            rooms.invite.insert(room_id, room);
        }

        let presence: BTreeMap<UserId, PresenceEvent> = response
            .presence
            .events
            .into_iter()
            .filter_map(|e| {
                let event = e.deserialize().ok()?;
                Some((event.sender.clone(), event))
            })
            .collect();

        changes.presence = presence;

        self.handle_account_data(response.account_data.events, &mut changes)
            .await;

        changes.ambiguity_maps = ambiguity_cache.cache;

        self.store.save_changes(&changes).await?;
        *self.sync_token.write().await = Some(response.next_batch.clone());
        self.apply_changes(&changes).await;

        info!("Processed a sync response in {:?}", now.elapsed());

        let response = SyncResponse {
            next_batch: response.next_batch,
            rooms,
            presence: Presence {
                events: changes.presence.into_iter().map(|(_, v)| v).collect(),
            },
            account_data: AccountData {
                events: changes.account_data.into_iter().map(|(_, e)| e).collect(),
            },
            to_device,
            device_lists: response.device_lists,
            device_one_time_keys_count: response
                .device_one_time_keys_count
                .into_iter()
                .map(|(k, v)| (k, v.into()))
                .collect(),
            ambiguity_changes: AmbiguityChanges {
                changes: ambiguity_cache.changes,
            },
        };

        Ok(response)
    }

    async fn apply_changes(&self, changes: &StateChanges) {
        for (room_id, room_info) in &changes.room_infos {
            if let Some(room) = self.store.get_bare_room(&room_id) {
                room.update_summary(room_info.clone())
            }
        }
    }

    /// Receive a get member events response and convert it to a deserialized
    /// `MembersResponse`
    ///
    ///
    /// # Arguments
    ///
    /// * `room_id` - The room id this response belongs to.
    ///
    /// * `response` - The raw response that was received from the server.
    pub async fn receive_members(
        &self,
        room_id: &RoomId,
        response: &api::membership::get_member_events::Response,
    ) -> Result<MembersResponse> {
        let members: Vec<MemberEvent> = response
            .chunk
            .iter()
            .filter_map(|e| {
                hoist_member_event(e)
                    .ok()
                    .and_then(|e| MemberEvent::try_from(e).ok())
            })
            .collect();
        let mut ambiguity_cache = AmbiguityCache::new(self.store.clone());

        if let Some(room) = self.store.get_bare_room(room_id) {
            let mut room_info = room.clone_info();
            room_info.mark_members_synced();

            let mut changes = StateChanges::default();

            #[cfg(feature = "encryption")]
            let mut user_ids = BTreeSet::new();

            for member in &members {
                if self
                    .store
                    .get_member_event(&room_id, &member.state_key)
                    .await?
                    .is_none()
                {
                    #[cfg(feature = "encryption")]
                    match member.content.membership {
                        MembershipState::Join | MembershipState::Invite => {
                            user_ids.insert(member.state_key.clone());
                        }
                        _ => (),
                    }

                    ambiguity_cache
                        .handle_event(&changes, room_id, &member)
                        .await?;

                    if member.state_key == member.sender {
                        changes
                            .profiles
                            .entry(room_id.clone())
                            .or_insert_with(BTreeMap::new)
                            .insert(member.sender.clone(), member.content.clone());
                    }

                    changes
                        .members
                        .entry(room_id.clone())
                        .or_insert_with(BTreeMap::new)
                        .insert(member.state_key.clone(), member.clone());
                }
            }

            #[cfg(feature = "encryption")]
            if room_info.is_encrypted() {
                if let Some(o) = self.olm_machine().await {
                    o.update_tracked_users(&user_ids).await
                }
            }

            changes.ambiguity_maps = ambiguity_cache.cache;
            changes.add_room(room_info);

            self.store.save_changes(&changes).await?;
            self.apply_changes(&changes).await;
        }

        Ok(MembersResponse {
            chunk: members,
            ambiguity_changes: AmbiguityChanges {
                changes: ambiguity_cache.changes,
            },
        })
    }

    /// Receive a successful filter upload response, the filter id will be
    /// stored under the given name in the store.
    ///
    /// The filter id can later be retrieved with the [`get_filter`] method.
    ///
    ///
    /// # Arguments
    ///
    /// * `filter_name` - The name that should be used to persist the filter id in
    /// the store.
    ///
    /// * `response` - The successful filter upload response containing the
    /// filter id.
    ///
    /// [`get_filter`]: #method.get_filter
    pub async fn receive_filter_upload(
        &self,
        filter_name: &str,
        response: &api::filter::create_filter::Response,
    ) -> Result<()> {
        Ok(self
            .store
            .save_filter(filter_name, &response.filter_id)
            .await?)
    }

    /// Get the filter id of a previously uploaded filter.
    ///
    /// *Note*: A filter will first need to be uploaded and persisted using
    /// [`receive_filter_upload`].
    ///
    /// # Arguments
    ///
    /// * `filter_name` - The name of the filter that was previously used to
    /// persist the filter.
    ///
    /// [`receive_filter_upload`]: #method.receive_filter_upload
    pub async fn get_filter(&self, filter_name: &str) -> StoreResult<Option<String>> {
        self.store.get_filter(filter_name).await
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
    pub async fn outgoing_requests(&self) -> Result<Vec<OutgoingRequest>, CryptoStoreError> {
        let olm = self.olm.lock().await;

        match &*olm {
            Some(o) => o.outgoing_requests().await,
            None => Ok(vec![]),
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
    pub async fn share_group_session(&self, room_id: &RoomId) -> Result<Vec<Arc<ToDeviceRequest>>> {
        let olm = self.olm.lock().await;

        match &*olm {
            Some(o) => {
                let (history_visibility, settings) = self
                    .get_room(room_id)
                    .map(|r| (r.history_visibility(), r.encryption_settings()))
                    .unwrap_or((HistoryVisibility::Joined, None));

                let joined = self.store.get_joined_user_ids(room_id).await?;
                let invited = self.store.get_invited_user_ids(room_id).await?;

                // Don't share the group session with members that are invited
                // if the history visibility is set to `Joined`
                let members = if history_visibility == HistoryVisibility::Joined {
                    joined.iter().chain(&[])
                } else {
                    joined.iter().chain(&invited)
                };

                let settings = settings.ok_or(MegolmError::EncryptionNotEnabled)?;
                let settings = EncryptionSettings::new(settings, history_visibility);

                Ok(o.share_group_session(room_id, members, settings).await?)
            }
            None => panic!("Olm machine wasn't started"),
        }
    }

    /// Get the room with the given room id.
    ///
    /// # Arguments
    ///
    /// * `room_id` - The id of the room that should be fetched.
    pub fn get_room(&self, room_id: &RoomId) -> Option<Room> {
        self.store.get_room(room_id)
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
    pub async fn invalidate_group_session(
        &self,
        room_id: &RoomId,
    ) -> Result<bool, CryptoStoreError> {
        let olm = self.olm.lock().await;

        match &*olm {
            Some(o) => o.invalidate_group_session(room_id).await,
            None => Ok(false),
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
