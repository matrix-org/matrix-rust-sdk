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
    deserialized_responses::{
        AmbiguityChanges, JoinedRoom, LeftRoom, MemberEvent, MembersResponse, Rooms,
        StrippedMemberEvent, SyncResponse, SyncRoomEvent, Timeline, TimelineSlice,
    },
    instant::Instant,
    locks::RwLock,
};
#[cfg(feature = "encryption")]
use matrix_sdk_common::{locks::Mutex, uuid::Uuid};
#[cfg(feature = "encryption")]
use matrix_sdk_crypto::{
    store::{CryptoStore, CryptoStoreError},
    Device, EncryptionSettings, IncomingResponse, MegolmError, OlmError, OlmMachine,
    OutgoingRequest, Sas, ToDeviceRequest, UserDevices,
};
#[cfg(feature = "encryption")]
use ruma::{
    api::client::r0::keys::claim_keys::Request as KeysClaimRequest,
    events::{
        room::{encrypted::EncryptedEventContent, history_visibility::HistoryVisibility},
        AnyMessageEventContent, AnySyncMessageEvent,
    },
    DeviceId,
};
use ruma::{
    api::client::r0::{
        self as api,
        message::get_message_events::{Direction, Response as GetMessageEventsResponse},
        push::get_notifications::Notification,
    },
    events::{
        room::member::{MemberEventContent, MembershipState},
        AnyGlobalAccountDataEvent, AnyRoomAccountDataEvent, AnyStrippedStateEvent,
        AnySyncEphemeralRoomEvent, AnySyncRoomEvent, AnySyncStateEvent, EventContent, EventType,
        StateEvent,
    },
    push::{Action, PushConditionRoomCtx, Ruleset},
    serde::Raw,
    MilliSecondsSinceUnixEpoch, RoomId, UInt, UserId,
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

/// Transform state event by hoisting `prev_content` field from `unsigned` to
/// the top level.
///
/// Due to a [bug in synapse][synapse-bug], `prev_content` often ends up in
/// `unsigned` contrary to the C2S spec. Some more discussion can be found
/// [here][discussion]. Until this is fixed in synapse or handled in Ruma, we
/// use this to hoist up `prev_content` to the top level.
///
/// [synapse-bug]: <https://github.com/matrix-org/matrix-doc/issues/684#issuecomment-641182668>
/// [discussion]: <https://github.com/matrix-org/matrix-doc/issues/684#issuecomment-641182668>
pub fn hoist_and_deserialize_state_event(
    event: &Raw<AnySyncStateEvent>,
) -> StdResult<AnySyncStateEvent, serde_json::Error> {
    let prev_content = event.deserialize_as::<AdditionalEventData>()?.unsigned.prev_content;

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
    let prev_content = event.deserialize_as::<AdditionalEventData>()?.unsigned.prev_content;

    let mut e = event.deserialize()?;

    if e.prev_content.is_none() {
        e.prev_content = prev_content.and_then(|e| e.deserialize().ok());
    }

    Ok(e)
}

fn hoist_room_event_prev_content(
    event: &Raw<AnySyncRoomEvent>,
) -> StdResult<AnySyncRoomEvent, serde_json::Error> {
    let prev_content = event
        .deserialize_as::<AdditionalEventData>()
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
    /// * `response` - A successful login response that contains our access
    ///   token
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

    #[allow(clippy::too_many_arguments)]
    async fn handle_timeline(
        &self,
        room: &Room,
        ruma_timeline: api::sync::sync_events::Timeline,
        push_rules: &Ruleset,
        room_info: &mut RoomInfo,
        changes: &mut StateChanges,
        ambiguity_cache: &mut AmbiguityCache,
        user_ids: &mut BTreeSet<UserId>,
    ) -> Result<Timeline> {
        let room_id = room.room_id();
        let user_id = room.own_user_id();
        let mut timeline = Timeline::new(ruma_timeline.limited, ruma_timeline.prev_batch.clone());
        let mut push_context = self.get_push_room_context(room, room_info, changes).await?;

        for event in ruma_timeline.events {
            #[allow(unused_mut)]
            let mut event: SyncRoomEvent = event.into();

            match hoist_room_event_prev_content(&event.event) {
                Ok(e) => {
                    #[allow(clippy::single_match)]
                    match &e {
                        AnySyncRoomEvent::State(s) => match s {
                            AnySyncStateEvent::RoomMember(member) => {
                                if let Ok(member) = MemberEvent::try_from(member.clone()) {
                                    ambiguity_cache.handle_event(changes, room_id, &member).await?;

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
                                let raw_event: Raw<AnySyncStateEvent> =
                                    Raw::from_json(event.event.clone().into_json());
                                changes.add_state_event(room_id, s.clone(), raw_event);
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
                                    event = decrypted;
                                }
                            }
                        }
                        // TODO if there is redacted state save the room id,
                        // event type and state key, add a method to get the
                        // requests that are needed to be called to heal this
                        // redacted state.
                        _ => (),
                    }

                    if let Some(context) = &mut push_context {
                        self.update_push_room_context(context, user_id, room_info, changes).await;
                    } else {
                        push_context = self.get_push_room_context(room, room_info, changes).await?;
                    }

                    if let Some(context) = &push_context {
                        let actions = push_rules.get_actions(&event.event, context).to_vec();

                        if actions.iter().any(|a| matches!(a, Action::Notify)) {
                            changes.add_notification(
                                room_id,
                                Notification::new(
                                    actions,
                                    event.event.clone(),
                                    false,
                                    room_id.clone(),
                                    MilliSecondsSinceUnixEpoch::now(),
                                ),
                            );
                        }
                        // TODO if there is an
                        // Action::SetTweak(Tweak::Highlight) we need to store
                        // its value with the event so a client can show if the
                        // event is highlighted
                        // in the UI.
                        // Requires the possibility to associate custom data
                        // with events and to
                        // store them.
                    }
                }
                Err(e) => {
                    warn!("Error deserializing event {:?}", e);
                }
            }

            timeline.events.push(event);
        }

        Ok(timeline)
    }

    #[allow(clippy::type_complexity)]
    fn handle_invited_state(
        &self,
        events: &[Raw<AnyStrippedStateEvent>],
        room_info: &mut RoomInfo,
    ) -> (
        BTreeMap<UserId, StrippedMemberEvent>,
        BTreeMap<String, BTreeMap<String, Raw<AnyStrippedStateEvent>>>,
    ) {
        events.iter().fold(
            (BTreeMap::new(), BTreeMap::new()),
            |(mut members, mut state_events), raw_event| {
                match raw_event.deserialize() {
                    Ok(e) => {

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
                                .insert(e.state_key().to_owned(), raw_event.clone());
                        }
                    }
                    Err(err) => {
                        warn!(
                            "Couldn't deserialize stripped state event for room {}: {:?}",
                            room_info.room_id, err
                        );
                    }
                }
                (members, state_events)
            },
        )
    }

    async fn handle_state(
        &self,
        changes: &mut StateChanges,
        ambiguity_cache: &mut AmbiguityCache,
        events: &[Raw<AnySyncStateEvent>],
        room_info: &mut RoomInfo,
    ) -> StoreResult<BTreeSet<UserId>> {
        let mut members = BTreeMap::new();
        let mut state_events = BTreeMap::new();
        let mut user_ids = BTreeSet::new();
        let mut profiles = BTreeMap::new();

        let room_id = room_info.room_id.clone();

        for raw_event in events {
            let event = match hoist_and_deserialize_state_event(raw_event) {
                Ok(e) => e,
                Err(e) => {
                    warn!(
                        "Couldn't deserialize state event for room {}: {:?} {:#?}",
                        room_id, e, raw_event
                    );
                    continue;
                }
            };

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
                    .insert(event.state_key().to_owned(), raw_event.clone());
            }
        }

        changes.members.insert(room_id.as_ref().clone(), members);
        changes.profiles.insert(room_id.as_ref().clone(), profiles);
        changes.state.insert(room_id.as_ref().clone(), state_events);

        Ok(user_ids)
    }

    async fn handle_room_account_data(
        &self,
        room_id: &RoomId,
        events: &[Raw<AnyRoomAccountDataEvent>],
        changes: &mut StateChanges,
    ) {
        for raw_event in events {
            if let Ok(event) = raw_event.deserialize() {
                changes.add_room_account_data(room_id, event, raw_event.clone());
            }
        }
    }

    async fn handle_account_data(
        &self,
        events: &[Raw<AnyGlobalAccountDataEvent>],
        changes: &mut StateChanges,
    ) {
        let mut account_data = BTreeMap::new();

        for raw_event in events {
            let event = if let Ok(e) = raw_event.deserialize() {
                e
            } else {
                continue;
            };

            if let AnyGlobalAccountDataEvent::Direct(e) = &event {
                for (user_id, rooms) in e.content.iter() {
                    for room_id in rooms {
                        if let Some(room) = changes.room_infos.get_mut(room_id) {
                            room.base_info.dm_target = Some(user_id.clone());
                        } else if let Some(room) = self.store.get_room(room_id) {
                            let mut info = room.clone_info();
                            info.base_info.dm_target = Some(user_id.clone());
                            changes.add_room(info);
                        }
                    }
                }
            }

            account_data.insert(event.content().event_type().to_owned(), raw_event.clone());
        }

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
        #[cfg(test)]
        let api::sync::sync_events::Response {
            next_batch,
            rooms,
            presence,
            account_data,
            to_device,
            device_lists,
            device_one_time_keys_count,
            __test_exhaustive: _,
        } = response;

        #[cfg(not(test))]
        let api::sync::sync_events::Response {
            next_batch,
            rooms,
            presence,
            account_data,
            to_device,
            device_lists,
            device_one_time_keys_count,
            ..
        } = response;

        // The server might respond multiple times with the same sync token, in
        // that case we already received this response and there's nothing to
        // do.
        if self.sync_token.read().await.as_ref() == Some(&next_batch) {
            return Ok(SyncResponse::new(next_batch));
        }

        let now = Instant::now();

        #[cfg(feature = "encryption")]
        let to_device = {
            let olm = self.olm.lock().await;

            if let Some(o) = &*olm {
                // Let the crypto machine handle the sync response, this
                // decrypts to-device events, but leaves room events alone.
                // This makes sure that we have the decryption keys for the room
                // events at hand.
                o.receive_sync_changes(to_device, &device_lists, &device_one_time_keys_count)
                    .await?
            } else {
                to_device
            }
        };

        let mut changes = StateChanges::new(next_batch.clone());
        let mut ambiguity_cache = AmbiguityCache::new(self.store.clone());

        self.handle_account_data(&account_data.events, &mut changes).await;

        let push_rules = self.get_push_rules(&changes).await?;

        let mut new_rooms = Rooms::default();

        for (room_id, new_info) in rooms.join {
            let room = self.store.get_or_create_room(&room_id, RoomType::Joined).await;
            let mut room_info = room.clone_info();
            room_info.mark_as_joined();

            room_info.update_summary(&new_info.summary);
            room_info.set_prev_batch(new_info.timeline.prev_batch.as_deref());

            let mut user_ids = self
                .handle_state(
                    &mut changes,
                    &mut ambiguity_cache,
                    &new_info.state.events,
                    &mut room_info,
                )
                .await?;

            if let Some(event) =
                new_info.ephemeral.events.iter().find_map(|e| match e.deserialize() {
                    Ok(AnySyncEphemeralRoomEvent::Receipt(event)) => Some(event.content),
                    _ => None,
                })
            {
                changes.add_receipts(&room_id, event);
            }

            if new_info.timeline.limited {
                room_info.mark_members_missing();
            }

            let timeline = self
                .handle_timeline(
                    &room,
                    new_info.timeline,
                    &push_rules,
                    &mut room_info,
                    &mut changes,
                    &mut ambiguity_cache,
                    &mut user_ids,
                )
                .await?;

            self.handle_room_account_data(&room_id, &new_info.account_data.events, &mut changes)
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

            changes.add_timeline(
                &room_id,
                TimelineSlice::new(
                    timeline.events.iter().cloned().rev().collect(),
                    next_batch.clone(),
                    timeline.prev_batch.clone(),
                ),
            );

            new_rooms.join.insert(
                room_id,
                JoinedRoom::new(
                    timeline,
                    new_info.state,
                    new_info.account_data,
                    new_info.ephemeral,
                    notification_count,
                ),
            );

            changes.add_room(room_info);
        }

        for (room_id, new_info) in rooms.leave {
            let room = self.store.get_or_create_room(&room_id, RoomType::Left).await;
            let mut room_info = room.clone_info();
            room_info.mark_as_left();

            let mut user_ids = self
                .handle_state(
                    &mut changes,
                    &mut ambiguity_cache,
                    &new_info.state.events,
                    &mut room_info,
                )
                .await?;

            let timeline = self
                .handle_timeline(
                    &room,
                    new_info.timeline,
                    &push_rules,
                    &mut room_info,
                    &mut changes,
                    &mut ambiguity_cache,
                    &mut user_ids,
                )
                .await?;

            self.handle_room_account_data(&room_id, &new_info.account_data.events, &mut changes)
                .await;

            changes.add_timeline(
                &room_id,
                TimelineSlice::new(
                    timeline.events.iter().cloned().rev().collect(),
                    next_batch.clone(),
                    timeline.prev_batch.clone(),
                ),
            );
            changes.add_room(room_info);
            new_rooms
                .leave
                .insert(room_id, LeftRoom::new(timeline, new_info.state, new_info.account_data));
        }

        for (room_id, new_info) in rooms.invite {
            {
                let room = self.store.get_or_create_room(&room_id, RoomType::Invited).await;
                let mut room_info = room.clone_info();
                room_info.mark_as_invited();
                changes.add_room(room_info);
            }

            let room = self.store.get_or_create_stripped_room(&room_id).await;
            let mut room_info = room.clone_info();

            let (members, state_events) =
                self.handle_invited_state(&new_info.invite_state.events, &mut room_info);

            changes.stripped_members.insert(room_id.clone(), members);
            changes.stripped_state.insert(room_id.clone(), state_events);
            changes.add_stripped_room(room_info);

            new_rooms.invite.insert(room_id, new_info);
        }

        changes.presence = presence
            .events
            .iter()
            .filter_map(|e| {
                let event = e.deserialize().ok()?;
                Some((event.sender, e.clone()))
            })
            .collect();

        changes.ambiguity_maps = ambiguity_cache.cache;

        self.store.save_changes(&changes).await?;
        *self.sync_token.write().await = Some(next_batch.clone());
        self.apply_changes(&changes).await;

        info!("Processed a sync response in {:?}", now.elapsed());

        let response = SyncResponse {
            next_batch,
            rooms: new_rooms,
            presence,
            account_data,
            to_device,
            device_lists,
            device_one_time_keys_count: device_one_time_keys_count
                .into_iter()
                .map(|(k, v)| (k, v.into()))
                .collect(),
            ambiguity_changes: AmbiguityChanges { changes: ambiguity_cache.changes },
            notifications: changes.notifications,
        };

        Ok(response)
    }

    async fn apply_changes(&self, changes: &StateChanges) {
        for (room_id, room_info) in &changes.room_infos {
            if let Some(room) = self.store.get_room(room_id) {
                room.update_summary(room_info.clone())
            }
        }
    }

    /// Receive a successful /messages response.
    ///
    /// * `response` - The successful response from /messages.
    pub async fn receive_messages(
        &self,
        room_id: &RoomId,
        direction: &Direction,
        response: &GetMessageEventsResponse,
    ) -> Result<Vec<SyncRoomEvent>> {
        let mut changes = StateChanges::default();

        let mut events: Vec<SyncRoomEvent> = vec![];
        for event in &response.chunk {
            #[allow(unused_mut)]
            let mut event: SyncRoomEvent = event.clone().into();

            #[cfg(feature = "encryption")]
            match hoist_room_event_prev_content(&event.event) {
                Ok(e) => {
                    if let AnySyncRoomEvent::Message(AnySyncMessageEvent::RoomEncrypted(
                        encrypted,
                    )) = e
                    {
                        if let Some(olm) = self.olm_machine().await {
                            if let Ok(decrypted) = olm.decrypt_room_event(&encrypted, room_id).await
                            {
                                event = decrypted;
                            }
                        }
                    }
                }
                Err(error) => {
                    warn!("Error deserializing event {:?}", error);
                }
            }

            events.push(event);
        }

        let (chunk, start, end) = match direction {
            Direction::Backward => {
                (events.clone(), response.start.clone().unwrap(), response.end.clone())
            }
            Direction::Forward => (
                events.iter().rev().cloned().collect(),
                response.end.clone().unwrap(),
                response.start.clone(),
            ),
        };

        let timeline = TimelineSlice::new(chunk, start, end);
        changes.add_timeline(room_id, timeline);

        self.store().save_changes(&changes).await?;

        Ok(events)
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
            .filter_map(|e| hoist_member_event(e).ok().and_then(|e| MemberEvent::try_from(e).ok()))
            .collect();
        let mut ambiguity_cache = AmbiguityCache::new(self.store.clone());

        if let Some(room) = self.store.get_room(room_id) {
            let mut room_info = room.clone_info();
            room_info.mark_members_synced();

            let mut changes = StateChanges::default();

            #[cfg(feature = "encryption")]
            let mut user_ids = BTreeSet::new();

            for member in &members {
                if self.store.get_member_event(room_id, &member.state_key).await?.is_none() {
                    #[cfg(feature = "encryption")]
                    match member.content.membership {
                        MembershipState::Join | MembershipState::Invite => {
                            user_ids.insert(member.state_key.clone());
                        }
                        _ => (),
                    }

                    ambiguity_cache.handle_event(&changes, room_id, member).await?;

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
            ambiguity_changes: AmbiguityChanges { changes: ambiguity_cache.changes },
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
    /// * `filter_name` - The name that should be used to persist the filter id
    ///   in
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
        Ok(self.store.save_filter(filter_name, &response.filter_id).await?)
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
    ///   flow. For in-room verifications this will be the event id of the
    ///   *m.key.verification.request* event that started the flow, for the
    ///   to-device verification flows this will be the transaction id of the
    ///   *m.key.verification.start* event.
    #[cfg(feature = "encryption")]
    #[cfg_attr(feature = "docs", doc(cfg(encryption)))]
    pub async fn get_verification(&self, flow_id: &str) -> Option<Sas> {
        self.olm.lock().await.as_ref().and_then(|o| o.get_verification(flow_id))
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
    /// # use ruma::UserId;
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
    /// If the client is currently logged in, this will return a
    /// `matrix_sdk::Session` object which can later be given to
    /// `restore_login`.
    ///
    /// Returns a session object if the client is logged in. Otherwise returns
    /// `None`.
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
    /// # use ruma::UserId;
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

    /// Get the push rules.
    ///
    /// Gets the push rules from `changes` if they have been updated, otherwise
    /// get them from the store. As a fallback, uses
    /// `Ruleset::server_default` if the user is logged in.
    pub async fn get_push_rules(&self, changes: &StateChanges) -> Result<Ruleset> {
        if let Some(AnyGlobalAccountDataEvent::PushRules(event)) = changes
            .account_data
            .get(EventType::PushRules.as_str())
            .and_then(|e| e.deserialize().ok())
        {
            Ok(event.content.global)
        } else if let Some(AnyGlobalAccountDataEvent::PushRules(event)) = self
            .store
            .get_account_data_event(EventType::PushRules)
            .await?
            .and_then(|e| e.deserialize().ok())
        {
            Ok(event.content.global)
        } else if let Some(session) = self.get_session().await {
            Ok(Ruleset::server_default(&session.user_id))
        } else {
            Ok(Ruleset::new())
        }
    }

    /// Get the push context for the given room.
    ///
    /// Tries to get the data from `changes` or the up to date `room_info`.
    /// Loads the data from the store otherwise.
    ///
    /// Returns `None` if some data couldn't be found. This should only happen
    /// in brand new rooms, while we process its state.
    pub async fn get_push_room_context(
        &self,
        room: &Room,
        room_info: &RoomInfo,
        changes: &StateChanges,
    ) -> Result<Option<PushConditionRoomCtx>> {
        let room_id = room.room_id();
        let user_id = room.own_user_id();

        let member_count = room_info.active_members_count();

        let user_display_name = if let Some(member) =
            changes.members.get(room_id).and_then(|members| members.get(user_id))
        {
            member.content.displayname.clone().unwrap_or_else(|| user_id.localpart().to_owned())
        } else if let Some(member) = room.get_member(user_id).await? {
            member.name().to_owned()
        } else {
            return Ok(None);
        };

        let room_power_levels = if let Some(AnySyncStateEvent::RoomPowerLevels(event)) = changes
            .state
            .get(room_id)
            .and_then(|types| types.get(EventType::RoomPowerLevels.as_str()))
            .and_then(|events| events.get(""))
            .and_then(|e| e.deserialize().ok())
        {
            event.content
        } else if let Some(AnySyncStateEvent::RoomPowerLevels(event)) = self
            .store
            .get_state_event(room_id, EventType::RoomPowerLevels, "")
            .await?
            .and_then(|e| e.deserialize().ok())
        {
            event.content
        } else {
            return Ok(None);
        };

        Ok(Some(PushConditionRoomCtx {
            room_id: room_id.clone(),
            member_count: UInt::new(member_count).unwrap_or(UInt::MAX),
            user_display_name,
            users_power_levels: room_power_levels.users,
            default_power_level: room_power_levels.users_default,
            notification_power_levels: room_power_levels.notifications,
        }))
    }

    /// Update the push context for the given room.
    ///
    /// Updates the context data from `changes` or `room_info`.
    pub async fn update_push_room_context(
        &self,
        push_rules: &mut PushConditionRoomCtx,
        user_id: &UserId,
        room_info: &RoomInfo,
        changes: &StateChanges,
    ) {
        let room_id = &room_info.room_id;

        push_rules.member_count = UInt::new(room_info.active_members_count()).unwrap_or(UInt::MAX);

        if let Some(member) = changes.members.get(room_id).and_then(|members| members.get(user_id))
        {
            push_rules.user_display_name =
                member.content.displayname.clone().unwrap_or_else(|| user_id.localpart().to_owned())
        }

        if let Some(AnySyncStateEvent::RoomPowerLevels(event)) = changes
            .state
            .get(room_id)
            .and_then(|types| types.get(EventType::RoomPowerLevels.as_str()))
            .and_then(|events| events.get(""))
            .and_then(|e| e.deserialize().ok())
        {
            let room_power_levels = event.content;

            push_rules.users_power_levels = room_power_levels.users;
            push_rules.default_power_level = room_power_levels.users_default;
            push_rules.notification_power_levels = room_power_levels.notifications;
        }
    }
}

#[cfg(test)]
mod test {}
