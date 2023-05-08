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

#[cfg(feature = "e2e-encryption")]
use std::ops::Deref;
use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    sync::Arc,
};

use eyeball::{shared::Observable as SharedObservable, Subscriber};
use matrix_sdk_common::instant::Instant;
#[cfg(feature = "e2e-encryption")]
use matrix_sdk_crypto::{
    store::DynCryptoStore, EncryptionSettings, OlmError, OlmMachine, ToDeviceRequest,
};
#[cfg(feature = "e2e-encryption")]
use once_cell::sync::OnceCell;
#[cfg(feature = "e2e-encryption")]
use ruma::events::{
    room::{
        history_visibility::HistoryVisibility, message::MessageType,
        redaction::SyncRoomRedactionEvent,
    },
    AnySyncMessageLikeEvent, SyncMessageLikeEvent,
};
use ruma::{
    api::client::{self as api, push::get_notifications::v3::Notification},
    events::{
        push_rules::{PushRulesEvent, PushRulesEventContent},
        room::{
            member::{MembershipState, SyncRoomMemberEvent},
            power_levels::{RoomPowerLevelsEvent, RoomPowerLevelsEventContent},
        },
        AnyGlobalAccountDataEvent, AnyRoomAccountDataEvent, AnyStrippedStateEvent,
        AnySyncEphemeralRoomEvent, AnySyncStateEvent, AnySyncTimelineEvent,
        GlobalAccountDataEventType, StateEventType,
    },
    push::{Action, PushConditionRoomCtx, Ruleset},
    serde::Raw,
    MilliSecondsSinceUnixEpoch, OwnedUserId, RoomId, UInt, UserId,
};
use tokio::sync::RwLock;
use tracing::{debug, info, instrument, trace, warn};

use crate::{
    deserialized_responses::{AmbiguityChanges, MembersResponse, SyncTimelineEvent},
    error::Result,
    rooms::{Room, RoomInfo, RoomState},
    store::{
        ambiguity_map::AmbiguityCache, DynStateStore, Result as StoreResult, StateChanges,
        StateStoreDataKey, StateStoreDataValue, StateStoreExt, Store, StoreConfig,
    },
    sync::{JoinedRoom, LeftRoom, Rooms, SyncResponse, Timeline},
    Session, SessionMeta, SessionTokens,
};
#[cfg(feature = "e2e-encryption")]
use crate::{error::Error, RoomMemberships};

/// A no IO Client implementation.
///
/// This Client is a state machine that receives responses and events and
/// accordingly updates its state.
#[derive(Clone)]
pub struct BaseClient {
    /// Database
    pub(crate) store: Store,
    /// The store used for encryption.
    ///
    /// This field is only meant to be used for `OlmMachine` initialization.
    /// All operations on it happen inside the `OlmMachine`.
    #[cfg(feature = "e2e-encryption")]
    crypto_store: Arc<DynCryptoStore>,
    /// The olm-machine that is created once the
    /// [`SessionMeta`][crate::session::SessionMeta] is set via
    /// [`BaseClient::set_session_meta`]
    #[cfg(feature = "e2e-encryption")]
    olm_machine: OnceCell<OlmMachine>,
    pub(crate) ignore_user_list_changes_tx: Arc<SharedObservable<()>>,
}

#[cfg(not(tarpaulin_include))]
impl fmt::Debug for BaseClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Client")
            .field("session_meta", &self.store.session_meta())
            .field("sync_token", &self.store.sync_token)
            .finish_non_exhaustive()
    }
}

impl BaseClient {
    /// Create a new default client.
    pub fn new() -> Self {
        BaseClient::with_store_config(StoreConfig::default())
    }

    /// Create a new client.
    ///
    /// # Arguments
    ///
    /// * `config` - An optional session if the user already has one from a
    /// previous login call.
    pub fn with_store_config(config: StoreConfig) -> Self {
        BaseClient {
            store: Store::new(config.state_store),
            #[cfg(feature = "e2e-encryption")]
            crypto_store: config.crypto_store,
            #[cfg(feature = "e2e-encryption")]
            olm_machine: Default::default(),
            ignore_user_list_changes_tx: Default::default(),
        }
    }

    /// Get the session meta information.
    ///
    /// If the client is currently logged in, this will return a
    /// [`SessionMeta`] object which contains the user ID and device ID.
    /// Otherwise it returns `None`.
    pub fn session_meta(&self) -> Option<&SessionMeta> {
        self.store.session_meta()
    }

    /// Get the session tokens.
    ///
    /// This returns a subscriber object that you can use both to
    /// [`get`](Subscriber::get) the current value as well as to react to
    /// changes to the tokens.
    ///
    /// If the client is currently logged in, the inner value is a
    /// [`SessionTokens`] object which contains the access token and optional
    /// refresh token. Otherwise it is `None`.
    pub fn session_tokens(&self) -> Subscriber<Option<SessionTokens>> {
        self.store.session_tokens()
    }

    /// Set the session tokens.
    pub fn set_session_tokens(&self, tokens: SessionTokens) {
        self.store.set_session_tokens(tokens)
    }

    /// Get the user login session.
    ///
    /// If the client is currently logged in, this will return a
    /// [`Session`][crate::session::Session] object.
    ///
    /// Returns a session object if the client is logged in. Otherwise returns
    /// `None`.
    pub fn session(&self) -> Option<Session> {
        self.store.session()
    }

    /// Get all the rooms this client knows about.
    pub fn get_rooms(&self) -> Vec<Room> {
        self.store.get_rooms()
    }

    /// Lookup the Room for the given RoomId, or create one, if it didn't exist
    /// yet in the store
    pub async fn get_or_create_room(&self, room_id: &RoomId, room_state: RoomState) -> Room {
        self.store.get_or_create_room(room_id, room_state).await
    }

    /// Get all the rooms this client knows about.
    pub fn get_stripped_rooms(&self) -> Vec<Room> {
        self.store.get_stripped_rooms()
    }

    /// Get a reference to the store.
    #[allow(unknown_lints, clippy::explicit_auto_deref)]
    pub fn store(&self) -> &DynStateStore {
        &*self.store
    }

    /// Is the client logged in.
    pub fn logged_in(&self) -> bool {
        self.store.session_meta().is_some()
    }

    /// Receive a login response and update the session of the client.
    ///
    /// # Arguments
    ///
    /// * `response` - A successful login response that contains our access
    ///   token and device ID.
    pub async fn receive_login_response(
        &self,
        response: &api::session::login::v3::Response,
    ) -> Result<()> {
        let tokens = SessionTokens {
            access_token: response.access_token.clone(),
            refresh_token: response.refresh_token.clone(),
        };
        self.set_session_tokens(tokens);

        let meta = SessionMeta {
            device_id: response.device_id.clone(),
            user_id: response.user_id.clone(),
        };
        self.set_session_meta(meta).await
    }

    /// Set the meta of the session.
    ///
    /// If encryption is enabled, this also initializes or restores the
    /// `OlmMachine`.
    ///
    /// # Arguments
    ///
    /// * `session_meta` - The meta of a session that the user already has from
    ///   a previous login call.
    ///
    /// This method panics if it is called twice.
    pub async fn set_session_meta(&self, session_meta: SessionMeta) -> Result<()> {
        debug!(user_id = ?session_meta.user_id, device_id = ?session_meta.device_id, "Restoring login");
        self.store.set_session_meta(session_meta.clone()).await?;

        #[cfg(feature = "e2e-encryption")]
        {
            let olm_machine = OlmMachine::with_store(
                &session_meta.user_id,
                &session_meta.device_id,
                self.crypto_store.clone(),
            )
            .await
            .map_err(OlmError::from)?;

            if self.olm_machine.set(olm_machine).is_err() {
                return Err(Error::BadCryptoStoreState);
            }
        }

        Ok(())
    }

    /// Get the current, if any, sync token of the client.
    /// This will be None if the client didn't sync at least once.
    pub async fn sync_token(&self) -> Option<String> {
        self.store.sync_token.read().await.clone()
    }

    #[cfg(feature = "e2e-encryption")]
    async fn handle_verification_event(
        &self,
        event: &AnySyncMessageLikeEvent,
        room_id: &RoomId,
    ) -> Result<()> {
        if let Some(olm) = self.olm_machine() {
            olm.receive_verification_event(&event.clone().into_full_event(room_id.to_owned()))
                .await?;
        }

        Ok(())
    }

    #[cfg(feature = "e2e-encryption")]
    async fn decrypt_sync_room_event(
        &self,
        event: &Raw<AnySyncTimelineEvent>,
        room_id: &RoomId,
    ) -> Result<Option<SyncTimelineEvent>> {
        let Some(olm) = self.olm_machine() else { return Ok(None) };

        let event = olm.decrypt_room_event(event.cast_ref(), room_id).await?;
        let event: SyncTimelineEvent = event.into();

        if let Ok(AnySyncTimelineEvent::MessageLike(e)) = event.event.deserialize() {
            match &e {
                AnySyncMessageLikeEvent::RoomMessage(SyncMessageLikeEvent::Original(
                    original_event,
                )) => {
                    if let MessageType::VerificationRequest(_) = &original_event.content.msgtype {
                        self.handle_verification_event(&e, room_id).await?;
                    }
                }
                _ if e.event_type().to_string().starts_with("m.key.verification") => {
                    self.handle_verification_event(&e, room_id).await?;
                }
                _ => (),
            }
        }

        Ok(Some(event))
    }

    #[allow(clippy::too_many_arguments)]
    #[instrument(skip_all, fields(room_id = ?room_info.room_id))]
    pub(crate) async fn handle_timeline(
        &self,
        room: &Room,
        limited: bool,
        events: Vec<Raw<AnySyncTimelineEvent>>,
        prev_batch: Option<String>,
        push_rules: &Ruleset,
        user_ids: &mut BTreeSet<OwnedUserId>,
        room_info: &mut RoomInfo,
        changes: &mut StateChanges,
        ambiguity_cache: &mut AmbiguityCache,
    ) -> Result<Timeline> {
        let room_id = room.room_id();
        let user_id = room.own_user_id();
        let mut timeline = Timeline::new(limited, prev_batch);
        let mut push_context = self.get_push_room_context(room, room_info, changes).await?;

        for event in events {
            #[allow(unused_mut)]
            let mut event: SyncTimelineEvent = event.into();

            match event.event.deserialize() {
                Ok(e) => {
                    #[allow(clippy::single_match)]
                    match &e {
                        AnySyncTimelineEvent::State(s) => {
                            match s {
                                AnySyncStateEvent::RoomMember(member) => {
                                    ambiguity_cache.handle_event(changes, room_id, member).await?;

                                    match member.membership() {
                                        MembershipState::Join | MembershipState::Invite => {
                                            user_ids.insert(member.state_key().to_owned());
                                        }
                                        _ => {
                                            user_ids.remove(member.state_key());
                                        }
                                    }

                                    // Senders can fake the profile easily so we keep track
                                    // of profiles that the member set themselves to avoid
                                    // having confusing profile changes when a member gets
                                    // kicked/banned.
                                    if member.state_key() == member.sender() {
                                        changes
                                            .profiles
                                            .entry(room_id.to_owned())
                                            .or_default()
                                            .insert(member.sender().to_owned(), member.into());
                                    }
                                }
                                _ => {
                                    room_info.handle_state_event(s);
                                }
                            }

                            let raw_event: Raw<AnySyncStateEvent> = event.event.clone().cast();
                            changes.add_state_event(room_id, s.clone(), raw_event);
                        }

                        #[cfg(feature = "e2e-encryption")]
                        AnySyncTimelineEvent::MessageLike(
                            AnySyncMessageLikeEvent::RoomRedaction(
                                // Redacted redactions don't have the `redacts` key, so we can't
                                // know what they were meant to redact. A future room version might
                                // move the redacts key, replace the current redaction event
                                // altogether, or have the redacts key survive redaction.
                                SyncRoomRedactionEvent::Original(r),
                            ),
                        ) => {
                            room_info.handle_redaction(r);
                            let raw_event = event.event.clone().cast();
                            changes.add_redaction(room_id, &r.redacts, raw_event);
                        }

                        #[cfg(feature = "e2e-encryption")]
                        AnySyncTimelineEvent::MessageLike(e) => match e {
                            AnySyncMessageLikeEvent::RoomEncrypted(
                                SyncMessageLikeEvent::Original(_),
                            ) => {
                                if let Ok(Some(e)) =
                                    self.decrypt_sync_room_event(&event.event, room_id).await
                                {
                                    event = e;
                                }
                            }
                            AnySyncMessageLikeEvent::RoomMessage(
                                SyncMessageLikeEvent::Original(original_event),
                            ) => match &original_event.content.msgtype {
                                MessageType::VerificationRequest(_) => {
                                    self.handle_verification_event(e, room_id).await?;
                                }
                                _ => (),
                            },
                            _ if e.event_type().to_string().starts_with("m.key.verification") => {
                                self.handle_verification_event(e, room_id).await?;
                            }
                            _ => (),
                        },

                        #[cfg(not(feature = "e2e-encryption"))]
                        _ => (),
                    }

                    if let Some(context) = &mut push_context {
                        self.update_push_room_context(context, user_id, room_info, changes).await;
                    } else {
                        push_context = self.get_push_room_context(room, room_info, changes).await?;
                    }

                    if let Some(context) = &push_context {
                        let actions = push_rules.get_actions(&event.event, context);

                        if actions.iter().any(Action::should_notify) {
                            changes.add_notification(
                                room_id,
                                Notification::new(
                                    actions.to_owned(),
                                    event.event.clone(),
                                    false,
                                    room_id.to_owned(),
                                    MilliSecondsSinceUnixEpoch::now(),
                                ),
                            );
                        }
                        event.push_actions = actions.to_owned();
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

    #[instrument(skip_all, fields(room_id = ?room_info.room_id))]
    pub(crate) fn handle_invited_state(
        &self,
        events: &[Raw<AnyStrippedStateEvent>],
        room_info: &mut RoomInfo,
        changes: &mut StateChanges,
    ) {
        let mut state_events = BTreeMap::new();

        for raw_event in events {
            match raw_event.deserialize() {
                Ok(e) => {
                    room_info.handle_stripped_state_event(&e);
                    state_events
                        .entry(e.event_type())
                        .or_insert_with(BTreeMap::new)
                        .insert(e.state_key().to_owned(), raw_event.clone());
                }
                Err(err) => {
                    warn!(
                        room_id = ?room_info.room_id,
                        "Couldn't deserialize stripped state event: {err:?}",
                    );
                }
            }
        }

        changes.stripped_state.insert(room_info.room_id().to_owned(), state_events);
    }

    #[instrument(skip_all, fields(room_id = ?room_info.room_id))]
    pub(crate) async fn handle_state(
        &self,
        events: &[Raw<AnySyncStateEvent>],
        room_info: &mut RoomInfo,
        changes: &mut StateChanges,
        ambiguity_cache: &mut AmbiguityCache,
    ) -> StoreResult<BTreeSet<OwnedUserId>> {
        let mut state_events = BTreeMap::new();
        let mut user_ids = BTreeSet::new();
        let mut profiles = BTreeMap::new();

        for raw_event in events {
            let event = match raw_event.deserialize() {
                Ok(ev) => ev,
                Err(e) => {
                    warn!("Couldn't deserialize state event: {e}");
                    continue;
                }
            };

            room_info.handle_state_event(&event);

            if let AnySyncStateEvent::RoomMember(member) = &event {
                ambiguity_cache.handle_event(changes, &room_info.room_id, member).await?;

                match member.membership() {
                    MembershipState::Join | MembershipState::Invite => {
                        user_ids.insert(member.state_key().to_owned());
                    }
                    _ => (),
                }

                // Senders can fake the profile easily so we keep track
                // of profiles that the member set themselves to avoid
                // having confusing profile changes when a member gets
                // kicked/banned.
                if member.state_key() == member.sender() {
                    profiles.insert(member.sender().to_owned(), member.into());
                }
            }

            state_events
                .entry(event.event_type())
                .or_insert_with(BTreeMap::new)
                .insert(event.state_key().to_owned(), raw_event.clone());
        }

        changes.profiles.insert((*room_info.room_id).to_owned(), profiles);
        changes.state.insert((*room_info.room_id).to_owned(), state_events);

        Ok(user_ids)
    }

    #[instrument(skip_all, fields(?room_id))]
    pub(crate) async fn handle_room_account_data(
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

    #[instrument(skip_all)]
    pub(crate) async fn handle_account_data(
        &self,
        events: &[Raw<AnyGlobalAccountDataEvent>],
        changes: &mut StateChanges,
    ) {
        let mut account_data = BTreeMap::new();

        for raw_event in events {
            let event = match raw_event.deserialize() {
                Ok(e) => e,
                Err(e) => {
                    let event_type: Option<String> = raw_event.get_field("type").ok().flatten();
                    warn!(event_type, "Failed to deserialize a global account data event: {e}");
                    continue;
                }
            };

            if let AnyGlobalAccountDataEvent::Direct(e) = &event {
                for (user_id, rooms) in e.content.iter() {
                    for room_id in rooms {
                        trace!(
                            room_id = room_id.as_str(),
                            target = user_id.as_str(),
                            "Marking room as direct room"
                        );

                        if let Some(room) = changes.room_infos.get_mut(room_id) {
                            room.base_info.dm_targets.insert(user_id.clone());
                        } else if let Some(room) = self.store.get_room(room_id) {
                            let mut info = room.clone_info();
                            if info.base_info.dm_targets.insert(user_id.clone()) {
                                changes.add_room(info);
                            }
                        }
                    }
                }
            }

            account_data.insert(event.event_type(), raw_event.clone());
        }

        changes.account_data = account_data;
    }

    #[cfg(feature = "e2e-encryption")]
    #[instrument(skip_all)]
    pub(crate) async fn preprocess_to_device_events(
        &self,
        to_device_events: Vec<Raw<ruma::events::AnyToDeviceEvent>>,
        changed_devices: &api::sync::sync_events::DeviceLists,
        one_time_keys_counts: &BTreeMap<ruma::DeviceKeyAlgorithm, UInt>,
        unused_fallback_keys: Option<&[ruma::DeviceKeyAlgorithm]>,
    ) -> Result<Vec<Raw<ruma::events::AnyToDeviceEvent>>> {
        if let Some(o) = self.olm_machine() {
            // Let the crypto machine handle the sync response, this
            // decrypts to-device events, but leaves room events alone.
            // This makes sure that we have the decryption keys for the room
            // events at hand.
            Ok(o.receive_sync_changes(
                to_device_events,
                changed_devices,
                one_time_keys_counts,
                unused_fallback_keys,
            )
            .await?)
        } else {
            Ok(to_device_events)
        }
    }

    /// User has joined a room.
    ///
    /// Update the internal and cached state accordingly. Return the final Room.
    pub async fn room_joined(&self, room_id: &RoomId) -> Result<Room> {
        let room = self.store.get_or_create_room(room_id, RoomState::Joined).await;
        if room.state() != RoomState::Joined {
            let _sync_lock = self.sync_lock().read().await;

            let mut room_info = room.clone_info();
            room_info.mark_as_joined();
            room_info.mark_state_partially_synced();
            room_info.mark_members_missing(); // the own member event changed
            let mut changes = StateChanges::default();
            changes.add_room(room_info.clone());
            self.store.save_changes(&changes).await?; // Update the store
            room.update_summary(room_info); // Update the cached room handle
        }

        Ok(room)
    }

    /// User has left a room.
    ///
    /// Update the internal and cached state accordingly. Return the final Room.
    pub async fn room_left(&self, room_id: &RoomId) -> Result<Room> {
        let room = self.store.get_or_create_room(room_id, RoomState::Left).await;
        if room.state() != RoomState::Left {
            let _sync_lock = self.sync_lock().read().await;

            let mut room_info = room.clone_info();
            room_info.mark_as_left();
            room_info.mark_state_partially_synced();
            room_info.mark_members_missing(); // the own member event changed
            let mut changes = StateChanges::default();
            changes.add_room(room_info.clone());
            self.store.save_changes(&changes).await?; // Update the store
            room.update_summary(room_info); // Update the cached room handle
        }

        Ok(room)
    }

    /// Get access to the store's sync lock.
    pub fn sync_lock(&self) -> &RwLock<()> {
        self.store.sync_lock()
    }

    /// Receive a response from a sync call.
    ///
    /// # Arguments
    ///
    /// * `response` - The response that we received after a successful sync.
    #[instrument(skip_all)]
    pub async fn receive_sync_response(
        &self,
        response: api::sync::sync_events::v3::Response,
    ) -> Result<SyncResponse> {
        #[allow(unused_variables)]
        let api::sync::sync_events::v3::Response {
            next_batch,
            rooms,
            presence,
            account_data,
            to_device,
            device_lists,
            device_one_time_keys_count,
            device_unused_fallback_key_types,
            ..
        } = response;

        // The server might respond multiple times with the same sync token, in
        // that case we already received this response and there's nothing to
        // do.
        if self.store.sync_token.read().await.as_ref() == Some(&next_batch) {
            return Ok(SyncResponse::default());
        }

        let now = Instant::now();
        let to_device_events = to_device.events;

        #[cfg(feature = "e2e-encryption")]
        let to_device_events = self
            .preprocess_to_device_events(
                to_device_events,
                &device_lists,
                &device_one_time_keys_count,
                device_unused_fallback_key_types.as_deref(),
            )
            .await?;

        let mut changes = StateChanges::new(next_batch.clone());
        let mut ambiguity_cache = AmbiguityCache::new(self.store.inner.clone());

        self.handle_account_data(&account_data.events, &mut changes).await;

        let push_rules = self.get_push_rules(&changes).await?;

        let mut new_rooms = Rooms::default();

        for (room_id, new_info) in rooms.join {
            let room = self.store.get_or_create_room(&room_id, RoomState::Joined).await;
            let mut room_info = room.clone_info();
            room_info.mark_as_joined();

            room_info.update_summary(&new_info.summary);
            room_info.set_prev_batch(new_info.timeline.prev_batch.as_deref());
            room_info.mark_state_fully_synced();

            let mut user_ids = self
                .handle_state(
                    &new_info.state.events,
                    &mut room_info,
                    &mut changes,
                    &mut ambiguity_cache,
                )
                .await?;

            for raw in &new_info.ephemeral.events {
                match raw.deserialize() {
                    Ok(AnySyncEphemeralRoomEvent::Receipt(event)) => {
                        changes.add_receipts(&room_id, event.content);
                    }
                    Ok(_) => {}
                    Err(e) => {
                        let event_id: Option<String> = raw.get_field("event_id").ok().flatten();
                        #[rustfmt::skip]
                        info!(
                            ?room_id, event_id,
                            "Failed to deserialize ephemeral room event: {e}"
                        );
                    }
                }
            }

            if new_info.timeline.limited {
                room_info.mark_members_missing();
            }

            let timeline = self
                .handle_timeline(
                    &room,
                    new_info.timeline.limited,
                    new_info.timeline.events,
                    new_info.timeline.prev_batch,
                    &push_rules,
                    &mut user_ids,
                    &mut room_info,
                    &mut changes,
                    &mut ambiguity_cache,
                )
                .await?;

            self.handle_room_account_data(&room_id, &new_info.account_data.events, &mut changes)
                .await;

            #[cfg(feature = "e2e-encryption")]
            if room_info.is_encrypted() {
                if let Some(o) = self.olm_machine() {
                    if !room.is_encrypted() {
                        // The room turned on encryption in this sync, we need
                        // to also get all the existing users and mark them for
                        // tracking.
                        let user_ids =
                            self.store.get_user_ids(&room_id, RoomMemberships::ACTIVE).await?;
                        o.update_tracked_users(user_ids.iter().map(Deref::deref)).await?
                    }

                    o.update_tracked_users(user_ids.iter().map(Deref::deref)).await?;
                }
            }

            let notification_count = new_info.unread_notifications.into();
            room_info.update_notification_count(notification_count);

            new_rooms.join.insert(
                room_id,
                JoinedRoom::new(
                    timeline,
                    new_info.state,
                    new_info.account_data.events,
                    new_info.ephemeral,
                    notification_count,
                ),
            );

            changes.add_room(room_info);
        }

        for (room_id, new_info) in rooms.leave {
            let room = self.store.get_or_create_room(&room_id, RoomState::Left).await;
            let mut room_info = room.clone_info();
            room_info.mark_as_left();
            room_info.mark_state_partially_synced();

            let mut user_ids = self
                .handle_state(
                    &new_info.state.events,
                    &mut room_info,
                    &mut changes,
                    &mut ambiguity_cache,
                )
                .await?;

            let timeline = self
                .handle_timeline(
                    &room,
                    new_info.timeline.limited,
                    new_info.timeline.events,
                    new_info.timeline.prev_batch,
                    &push_rules,
                    &mut user_ids,
                    &mut room_info,
                    &mut changes,
                    &mut ambiguity_cache,
                )
                .await?;

            self.handle_room_account_data(&room_id, &new_info.account_data.events, &mut changes)
                .await;

            changes.add_room(room_info);
            new_rooms
                .leave
                .insert(room_id, LeftRoom::new(timeline, new_info.state, new_info.account_data));
        }

        for (room_id, new_info) in rooms.invite {
            let room = self.store.get_or_create_stripped_room(&room_id).await;
            let mut room_info = room.clone_info();

            if let Some(r) = self.store.get_room(&room_id) {
                let mut room_info = r.clone_info();
                room_info.mark_as_invited();
                room_info.mark_state_fully_synced();
                changes.add_room(room_info);
            }

            self.handle_invited_state(&new_info.invite_state.events, &mut room_info, &mut changes);

            changes.add_stripped_room(room_info);

            new_rooms.invite.insert(room_id, new_info);
        }

        // TODO remove this, we're processing account data events here again
        // because we want to have the push rules in place before we process
        // rooms and their events, but we want to create the rooms before we
        // process the `m.direct` account data event.
        self.handle_account_data(&account_data.events, &mut changes).await;

        changes.presence = presence
            .events
            .iter()
            .filter_map(|e| {
                let event = e.deserialize().ok()?;
                Some((event.sender, e.clone()))
            })
            .collect();

        changes.ambiguity_maps = ambiguity_cache.cache;

        let sync_lock = self.sync_lock().write().await;
        self.store.save_changes(&changes).await?;
        *self.store.sync_token.write().await = Some(next_batch.clone());
        self.apply_changes(&changes).await;
        drop(sync_lock);

        info!("Processed a sync response in {:?}", now.elapsed());

        let response = SyncResponse {
            rooms: new_rooms,
            presence,
            account_data: account_data.events,
            to_device_events,
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

    pub(crate) async fn apply_changes(&self, changes: &StateChanges) {
        if changes.account_data.contains_key(&GlobalAccountDataEventType::IgnoredUserList) {
            self.ignore_user_list_changes_tx.set(());
        }
        for (room_id, room_info) in &changes.room_infos {
            if let Some(room) = self.store.get_room(room_id) {
                room.update_summary(room_info.clone())
            }
        }

        for (room_id, room_info) in &changes.stripped_room_infos {
            if let Some(room) = self.store.get_stripped_room(room_id) {
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
    #[instrument(skip_all, fields(?room_id))]
    pub async fn receive_members(
        &self,
        room_id: &RoomId,
        response: &api::membership::get_member_events::v3::Response,
    ) -> Result<MembersResponse> {
        let mut chunk = Vec::with_capacity(response.chunk.len());
        let mut ambiguity_cache = AmbiguityCache::new(self.store.inner.clone());

        if let Some(room) = self.store.get_room(room_id) {
            let mut room_info = room.clone_info();
            room_info.mark_members_synced();

            let mut changes = StateChanges::default();

            #[cfg(feature = "e2e-encryption")]
            let mut user_ids = BTreeSet::new();

            for raw_event in &response.chunk {
                let member = match raw_event.deserialize() {
                    Ok(ev) => ev,
                    Err(e) => {
                        let event_id: Option<String> =
                            raw_event.get_field("event_id").ok().flatten();
                        debug!(event_id, "Failed to deserialize member event: {e}");
                        continue;
                    }
                };

                // TODO: All the actions in this loop used to be done only when the membership
                // event was not in the store before. This was changed with the new room API,
                // because e.g. leaving a room makes members events outdated and they need to be
                // fetched by `get_members`. Therefore, they need to be overwritten here, even
                // if they exist.
                // However, this makes a new problem occur where setting the member events here
                // potentially races with the sync.
                // See <https://github.com/matrix-org/matrix-rust-sdk/issues/1205>.

                #[cfg(feature = "e2e-encryption")]
                match member.membership() {
                    MembershipState::Join | MembershipState::Invite => {
                        user_ids.insert(member.state_key().to_owned());
                    }
                    _ => (),
                }

                let sync_member: SyncRoomMemberEvent = member.clone().into();

                ambiguity_cache.handle_event(&changes, room_id, &sync_member).await?;

                if member.state_key() == member.sender() {
                    changes
                        .profiles
                        .entry(room_id.to_owned())
                        .or_default()
                        .insert(member.sender().to_owned(), sync_member.into());
                }

                changes
                    .state
                    .entry(room_id.to_owned())
                    .or_default()
                    .entry(member.event_type())
                    .or_default()
                    .insert(member.state_key().to_string(), raw_event.clone().cast());
                chunk.push(member);
            }

            #[cfg(feature = "e2e-encryption")]
            if room_info.is_encrypted() {
                if let Some(o) = self.olm_machine() {
                    o.update_tracked_users(user_ids.iter().map(Deref::deref)).await?
                }
            }

            changes.ambiguity_maps = ambiguity_cache.cache;
            changes.add_room(room_info);

            self.store.save_changes(&changes).await?;
            self.apply_changes(&changes).await;
        }

        Ok(MembersResponse {
            chunk,
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
        response: &api::filter::create_filter::v3::Response,
    ) -> Result<()> {
        Ok(self
            .store
            .set_kv_data(
                StateStoreDataKey::Filter(filter_name),
                StateStoreDataValue::Filter(response.filter_id.clone()),
            )
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
        let filter = self
            .store
            .get_kv_data(StateStoreDataKey::Filter(filter_name))
            .await?
            .map(|d| d.into_filter().expect("State store data not a filter"));

        Ok(filter)
    }

    /// Get a to-device request that will share a room key with users in a room.
    #[cfg(feature = "e2e-encryption")]
    pub async fn share_room_key(&self, room_id: &RoomId) -> Result<Vec<Arc<ToDeviceRequest>>> {
        match self.olm_machine() {
            Some(o) => {
                let (history_visibility, settings) = self
                    .get_room(room_id)
                    .map(|r| (r.history_visibility(), r.encryption_settings()))
                    .unwrap_or((HistoryVisibility::Joined, None));

                // Don't share the group session with members that are invited
                // if the history visibility is set to `Joined`
                let filter = if history_visibility == HistoryVisibility::Joined {
                    RoomMemberships::JOIN
                } else {
                    RoomMemberships::ACTIVE
                };

                let members = self.store.get_user_ids(room_id, filter).await?;

                let settings = settings.ok_or(Error::EncryptionNotEnabled)?;
                let settings = EncryptionSettings::new(settings, history_visibility, false);

                Ok(o.share_room_key(room_id, members.iter().map(Deref::deref), settings).await?)
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

    /// Get the olm machine.
    #[cfg(feature = "e2e-encryption")]
    pub fn olm_machine(&self) -> Option<&OlmMachine> {
        self.olm_machine.get()
    }

    /// Get the push rules.
    ///
    /// Gets the push rules from `changes` if they have been updated, otherwise
    /// get them from the store. As a fallback, uses
    /// `Ruleset::server_default` if the user is logged in.
    pub async fn get_push_rules(&self, changes: &StateChanges) -> Result<Ruleset> {
        if let Some(event) = changes
            .account_data
            .get(&GlobalAccountDataEventType::PushRules)
            .and_then(|ev| ev.deserialize_as::<PushRulesEvent>().ok())
        {
            Ok(event.content.global)
        } else if let Some(event) = self
            .store
            .get_account_data_event_static::<PushRulesEventContent>()
            .await?
            .and_then(|ev| ev.deserialize().ok())
        {
            Ok(event.content.global)
        } else if let Some(session_meta) = self.store.session_meta() {
            Ok(Ruleset::server_default(&session_meta.user_id))
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

        // TODO: Use if let chain once stable
        let user_display_name = if let Some(Ok(AnySyncStateEvent::RoomMember(member))) = changes
            .state
            .get(room_id)
            .and_then(|events| events.get(&StateEventType::RoomMember))
            .and_then(|members| members.get(user_id.as_str()))
            .map(Raw::deserialize)
        {
            member
                .as_original()
                .and_then(|ev| ev.content.displayname.clone())
                .unwrap_or_else(|| user_id.localpart().to_owned())
        } else if let Some(member) = room.get_member(user_id).await? {
            member.name().to_owned()
        } else {
            return Ok(None);
        };

        let room_power_levels = if let Some(event) = changes
            .state
            .get(room_id)
            .and_then(|types| types.get(&StateEventType::RoomPowerLevels)?.get(""))
            .and_then(|e| e.deserialize_as::<RoomPowerLevelsEvent>().ok())
        {
            event.power_levels()
        } else if let Some(event) = self
            .store
            .get_state_event_static::<RoomPowerLevelsEventContent>(room_id)
            .await?
            .and_then(|e| e.deserialize().ok())
        {
            event.power_levels()
        } else {
            return Ok(None);
        };

        Ok(Some(PushConditionRoomCtx {
            user_id: user_id.to_owned(),
            room_id: room_id.to_owned(),
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
        let room_id = &*room_info.room_id;

        push_rules.member_count = UInt::new(room_info.active_members_count()).unwrap_or(UInt::MAX);

        // TODO: Use if let chain once stable
        if let Some(Ok(AnySyncStateEvent::RoomMember(member))) = changes
            .state
            .get(room_id)
            .and_then(|events| events.get(&StateEventType::RoomMember))
            .and_then(|members| members.get(user_id.as_str()))
            .map(Raw::deserialize)
        {
            push_rules.user_display_name = member
                .as_original()
                .and_then(|ev| ev.content.displayname.clone())
                .unwrap_or_else(|| user_id.localpart().to_owned())
        }

        if let Some(AnySyncStateEvent::RoomPowerLevels(event)) = changes
            .state
            .get(room_id)
            .and_then(|types| types.get(&StateEventType::RoomPowerLevels)?.get(""))
            .and_then(|e| e.deserialize().ok())
        {
            let room_power_levels = event.power_levels();

            push_rules.users_power_levels = room_power_levels.users;
            push_rules.default_power_level = room_power_levels.users_default;
            push_rules.notification_power_levels = room_power_levels.notifications;
        }
    }

    /// Returns a subscriber that publishes an event every time the ignore user
    /// list changes
    pub fn subscribe_to_ignore_user_list_changes(&self) -> Subscriber<()> {
        self.ignore_user_list_changes_tx.subscribe()
    }
}

impl Default for BaseClient {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use matrix_sdk_test::{
        async_test, response_from_file, EventBuilder, InvitedRoomBuilder, LeftRoomBuilder,
        StrippedStateTestEvent, TimelineTestEvent,
    };
    use ruma::{
        api::{client as api, IncomingResponse},
        room_id, user_id,
    };
    use serde_json::json;

    use super::BaseClient;
    use crate::{DisplayName, RoomState, SessionMeta};

    #[async_test]
    async fn invite_after_leaving() {
        let user_id = user_id!("@alice:example.org");
        let room_id = room_id!("!test:example.org");

        let client = BaseClient::new();
        client
            .set_session_meta(SessionMeta {
                user_id: user_id.to_owned(),
                device_id: "FOOBAR".into(),
            })
            .await
            .unwrap();

        let mut ev_builder = EventBuilder::new();

        let response = ev_builder
            .add_left_room(LeftRoomBuilder::new(room_id).add_timeline_event(
                TimelineTestEvent::Custom(json!({
                    "content": {
                        "displayname": "Alice",
                        "membership": "left",
                    },
                    "event_id": "$994173582443PhrSn:example.org",
                    "origin_server_ts": 1432135524678u64,
                    "sender": user_id,
                    "state_key": user_id,
                    "type": "m.room.member",
                })),
            ))
            .build_sync_response();
        client.receive_sync_response(response).await.unwrap();
        assert_eq!(client.get_room(room_id).unwrap().state(), RoomState::Left);

        let response = ev_builder
            .add_invited_room(InvitedRoomBuilder::new(room_id).add_state_event(
                StrippedStateTestEvent::Custom(json!({
                    "content": {
                        "displayname": "Alice",
                        "membership": "invite",
                    },
                    "event_id": "$143273582443PhrSn:example.org",
                    "origin_server_ts": 1432735824653u64,
                    "sender": "@example:example.org",
                    "state_key": user_id,
                    "type": "m.room.member",
                })),
            ))
            .build_sync_response();
        client.receive_sync_response(response).await.unwrap();
        assert_eq!(client.get_room(room_id).unwrap().state(), RoomState::Invited);
    }

    #[async_test]
    async fn invite_displayname_integration_test() {
        let user_id = user_id!("@alice:example.org");
        let room_id = room_id!("!ithpyNKDtmhneaTQja:example.org");

        let client = BaseClient::new();
        client
            .set_session_meta(SessionMeta {
                user_id: user_id.to_owned(),
                device_id: "FOOBAR".into(),
            })
            .await
            .unwrap();

        let response = api::sync::sync_events::v3::Response::try_from_http_response(response_from_file(&json!({
            "next_batch": "asdkl;fjasdkl;fj;asdkl;f",
            "device_one_time_keys_count": {
                "signed_curve25519": 50u64
            },
            "device_unused_fallback_key_types": [
                "signed_curve25519"
            ],
            "rooms": {
                "invite": {
                    "!ithpyNKDtmhneaTQja:example.org": {
                        "invite_state": {
                            "events": [
                                {
                                    "content": {
                                        "creator": "@test:example.org",
                                        "room_version": "9"
                                    },
                                    "sender": "@test:example.org",
                                    "state_key": "",
                                    "type": "m.room.create"
                                },
                                {
                                    "content": {
                                        "join_rule": "invite"
                                    },
                                    "sender": "@test:example.org",
                                    "state_key": "",
                                    "type": "m.room.join_rules"
                                },
                                {
                                    "content": {
                                        "algorithm": "m.megolm.v1.aes-sha2"
                                    },
                                    "sender": "@test:example.org",
                                    "state_key": "",
                                    "type": "m.room.encryption"
                                },
                                {
                                    "content": {
                                        "avatar_url": "mxc://example.org/dcBBDwuWEUrjfrOchvkirUST",
                                        "displayname": "Kyra",
                                        "membership": "join"
                                    },
                                    "sender": "@test:example.org",
                                    "state_key": "@test:example.org",
                                    "type": "m.room.member"
                                },
                                {
                                    "content": {
                                        "avatar_url": "mxc://example.org/ABFEXSDrESxovWwEnCYdNcHT",
                                        "displayname": "alice",
                                        "is_direct": true,
                                        "membership": "invite"
                                    },
                                    "origin_server_ts": 1650878657984u64,
                                    "sender": "@test:example.org",
                                    "state_key": "@alice:example.org",
                                    "type": "m.room.member",
                                    "unsigned": {
                                        "age": 14u64
                                    },
                                    "event_id": "$fLDqltg9Puj-kWItLSFVHPGN4YkgpYQf2qImPzdmgrE"
                                }
                            ]
                        }
                    }
                }
            }
        }))).expect("static json doesn't fail to parse");

        client.receive_sync_response(response).await.unwrap();

        let room = client.get_room(room_id).expect("Room not found");
        assert_eq!(room.state(), RoomState::Invited);
        assert_eq!(
            room.display_name().await.expect("fetching display name failed"),
            DisplayName::Calculated("Kyra".to_owned())
        );
    }
}
