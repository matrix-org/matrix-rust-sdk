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
    borrow::Borrow,
    collections::{BTreeMap, BTreeSet},
    fmt,
};
#[cfg(feature = "e2e-encryption")]
use std::{ops::Deref, sync::Arc};

use futures_signals::signal::ReadOnlyMutable;
#[cfg(feature = "experimental-timeline")]
use matrix_sdk_common::deserialized_responses::TimelineSlice;
use matrix_sdk_common::{
    deserialized_responses::{
        AmbiguityChanges, JoinedRoom, LeftRoom, MembersResponse, Rooms, SyncResponse,
        SyncTimelineEvent, Timeline,
    },
    instant::Instant,
};
#[cfg(feature = "e2e-encryption")]
use matrix_sdk_crypto::{
    store::{CryptoStore, MemoryStore as MemoryCryptoStore},
    EncryptionSettings, MegolmError, OlmError, OlmMachine, ToDeviceRequest,
};
#[cfg(feature = "e2e-encryption")]
use once_cell::sync::OnceCell;
#[cfg(feature = "e2e-encryption")]
use ruma::events::{
    room::{history_visibility::HistoryVisibility, redaction::SyncRoomRedactionEvent},
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
use tracing::{debug, info, trace, warn};

#[cfg(feature = "e2e-encryption")]
use crate::error::Error;
use crate::{
    error::Result,
    rooms::{Room, RoomInfo, RoomType},
    store::{
        ambiguity_map::AmbiguityCache, Result as StoreResult, StateChanges, StateStoreExt, Store,
        StoreConfig,
    },
    Session, SessionMeta, SessionTokens, StateStore,
};

/// A no IO Client implementation.
///
/// This Client is a state machine that receives responses and events and
/// accordingly updates its state.
#[derive(Clone)]
pub struct BaseClient {
    /// Database
    pub(crate) store: Store,
    /// The store used for encryption
    #[cfg(feature = "e2e-encryption")]
    crypto_store: Arc<dyn CryptoStore>,
    /// The olm-machine that is created once the
    /// [`Session`][crate::session::Session] is set via
    /// [`BaseClient::restore_login`]
    #[cfg(feature = "e2e-encryption")]
    olm_machine: OnceCell<OlmMachine>,
}

#[cfg(not(tarpaulin_include))]
impl fmt::Debug for BaseClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Client")
            .field("session_meta", &self.store.session_meta())
            .field("session_tokens", &self.store.session_tokens)
            .field("sync_token", &self.store.sync_token)
            .finish()
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
        let store = config.state_store.map(Store::new).unwrap_or_else(Store::open_memory_store);
        #[cfg(feature = "e2e-encryption")]
        let crypto_store =
            config.crypto_store.unwrap_or_else(|| Arc::new(MemoryCryptoStore::default()));

        BaseClient {
            store,
            #[cfg(feature = "e2e-encryption")]
            crypto_store,
            #[cfg(feature = "e2e-encryption")]
            olm_machine: Default::default(),
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
    /// If the client is currently logged in, this will return a
    /// [`SessionTokens`] object which contains the access token and optional
    /// refresh token. Otherwise it returns `None`.
    pub fn session_tokens(&self) -> ReadOnlyMutable<Option<SessionTokens>> {
        self.store.session_tokens()
    }

    /// Set the session tokens.
    pub fn set_session_tokens(&self, tokens: SessionTokens) {
        self.store.set_session_tokens(tokens)
    }

    /// Get the user login session.
    ///
    /// If the client is currently logged in, this will return a
    /// [`Session`][crate::session::Session] object which can later be given to
    /// [`BaseClient::restore_login`].
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

    /// Get all the rooms this client knows about.
    pub fn get_stripped_rooms(&self) -> Vec<Room> {
        self.store.get_stripped_rooms()
    }

    /// Get a reference to the store.
    #[allow(unknown_lints, clippy::explicit_auto_deref)]
    pub fn store(&self) -> &dyn StateStore {
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
        let session = Session {
            access_token: response.access_token.clone(),
            refresh_token: response.refresh_token.clone(),
            device_id: response.device_id.clone(),
            user_id: response.user_id.clone(),
        };
        self.restore_login(session).await
    }

    /// Restore a previously logged in session.
    ///
    /// # Arguments
    ///
    /// * `session` - An session that the user already has from a previous login
    ///   call.
    pub async fn restore_login(&self, session: Session) -> Result<()> {
        debug!(user_id = %session.user_id, device_id = %session.device_id, "Restoring login");
        self.store.restore_session(session.clone()).await?;

        #[cfg(feature = "e2e-encryption")]
        {
            let olm_machine = OlmMachine::with_store(
                &session.user_id,
                &session.device_id,
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
    async fn handle_unencrypted_verification_event(
        &self,
        event: &AnySyncMessageLikeEvent,
        room_id: &RoomId,
    ) -> Result<()> {
        if let Some(olm) = self.olm_machine() {
            olm.receive_unencrypted_verification_event(
                &event.clone().into_full_event(room_id.to_owned()),
            )
            .await?;
        }

        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    async fn handle_timeline(
        &self,
        room: &Room,
        ruma_timeline: api::sync::sync_events::v3::Timeline,
        push_rules: &Ruleset,
        user_ids: &mut BTreeSet<OwnedUserId>,
        room_info: &mut RoomInfo,
        changes: &mut StateChanges,
        ambiguity_cache: &mut AmbiguityCache,
    ) -> Result<Timeline> {
        let room_id = room.room_id();
        let user_id = room.own_user_id();
        let mut timeline = Timeline::new(ruma_timeline.limited, ruma_timeline.prev_batch.clone());
        let mut push_context = self.get_push_room_context(room, room_info, changes).await?;

        for event in ruma_timeline.events {
            #[allow(unused_mut)]
            let mut event: SyncTimelineEvent = event.into();

            match event.event.deserialize() {
                Ok(e) => {
                    #[allow(clippy::single_match)]
                    match &e {
                        AnySyncTimelineEvent::State(s) => match s {
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

                                changes
                                    .members
                                    .entry(room_id.to_owned())
                                    .or_default()
                                    .insert(member.state_key().to_owned(), member.clone());
                            }
                            _ => {
                                room_info.handle_state_event(s);
                                let raw_event: Raw<AnySyncStateEvent> = event.event.clone().cast();
                                changes.add_state_event(room_id, s.clone(), raw_event);
                            }
                        },

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
                            // FIXME: Find the event in self.store (needs
                            // something like a get_event_by_id), redact it and
                            // put it back via StateChanges. See
                            // https://github.com/matrix-org/matrix-rust-sdk/issues/607
                        }

                        #[cfg(feature = "e2e-encryption")]
                        AnySyncTimelineEvent::MessageLike(e) => match e {
                            AnySyncMessageLikeEvent::RoomEncrypted(
                                SyncMessageLikeEvent::Original(_),
                            ) => {
                                if let Some(olm) = self.olm_machine() {
                                    if let Ok(decrypted) = olm
                                        .decrypt_room_event(event.event.cast_ref(), room_id)
                                        .await
                                    {
                                        event = decrypted.into();
                                    }
                                }
                            }
                            AnySyncMessageLikeEvent::RoomMessage(
                                SyncMessageLikeEvent::Original(original_event),
                            ) => match &original_event.content.msgtype {
                                ruma::events::room::message::MessageType::VerificationRequest(
                                    _,
                                ) => {
                                    self.handle_unencrypted_verification_event(e, room_id).await?;
                                }
                                _ => (),
                            },
                            _ if e.event_type().to_string().starts_with("m.key.verification") => {
                                self.handle_unencrypted_verification_event(e, room_id).await?;
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

                        if actions.iter().any(|a| matches!(a, Action::Notify)) {
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

    pub(crate) fn handle_invited_state(
        &self,
        events: &[Raw<AnyStrippedStateEvent>],
        room_info: &mut RoomInfo,
        changes: &mut StateChanges,
    ) {
        let mut members = BTreeMap::new();
        let mut state_events = BTreeMap::new();

        for raw_event in events {
            match raw_event.deserialize() {
                Ok(AnyStrippedStateEvent::RoomMember(member)) => {
                    members.insert(member.state_key.clone(), member);
                }
                Ok(e) => {
                    room_info.handle_stripped_state_event(&e);
                    state_events
                        .entry(e.event_type())
                        .or_insert_with(BTreeMap::new)
                        .insert(e.state_key().to_owned(), raw_event.clone());
                }
                Err(err) => {
                    warn!(
                        room_id = %room_info.room_id,
                        "Couldn't deserialize stripped state event: {err:?}",
                    );
                }
            }
        }

        changes.stripped_members.insert(room_info.room_id().to_owned(), members);
        changes.stripped_state.insert(room_info.room_id().to_owned(), state_events);
    }

    pub(crate) async fn handle_state(
        &self,
        events: &[Raw<AnySyncStateEvent>],
        room_info: &mut RoomInfo,
        changes: &mut StateChanges,
        ambiguity_cache: &mut AmbiguityCache,
    ) -> StoreResult<BTreeSet<OwnedUserId>> {
        let mut members = BTreeMap::new();
        let mut state_events = BTreeMap::new();
        let mut user_ids = BTreeSet::new();
        let mut profiles = BTreeMap::new();

        let room_id = room_info.room_id.clone();

        for raw_event in events {
            let event = match raw_event.deserialize() {
                Ok(e) => e,
                Err(e) => {
                    warn!(%room_id, "Couldn't deserialize state event: {e:?}");
                    continue;
                }
            };

            room_info.handle_state_event(&event);

            if let AnySyncStateEvent::RoomMember(member) = event {
                ambiguity_cache.handle_event(changes, &room_id, &member).await?;

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
                    profiles.insert(member.sender().to_owned(), member.borrow().into());
                }

                members.insert(member.state_key().to_owned(), member);
            } else {
                state_events
                    .entry(event.event_type())
                    .or_insert_with(BTreeMap::new)
                    .insert(event.state_key().to_owned(), raw_event.clone());
            }
        }

        changes.members.insert((*room_id).to_owned(), members);
        changes.profiles.insert((*room_id).to_owned(), profiles);
        changes.state.insert((*room_id).to_owned(), state_events);

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
            let event = match raw_event.deserialize() {
                Ok(e) => e,
                Err(e) => {
                    warn!(error = ?e, "Failed to deserialize a global account data event");
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

    /// Receive a response from a sync call.
    ///
    /// # Arguments
    ///
    /// * `response` - The response that we received after a successful sync.
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
            return Ok(SyncResponse::new(next_batch));
        }

        let now = Instant::now();

        #[cfg(feature = "e2e-encryption")]
        let to_device = {
            if let Some(o) = self.olm_machine() {
                // Let the crypto machine handle the sync response, this
                // decrypts to-device events, but leaves room events alone.
                // This makes sure that we have the decryption keys for the room
                // events at hand.
                o.receive_sync_changes(
                    to_device,
                    &device_lists,
                    &device_one_time_keys_count,
                    device_unused_fallback_key_types.as_deref(),
                )
                .await?
            } else {
                to_device
            }
        };

        let mut changes = StateChanges::new(next_batch.clone());
        let mut ambiguity_cache = AmbiguityCache::new(self.store.inner.clone());

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
                    &new_info.state.events,
                    &mut room_info,
                    &mut changes,
                    &mut ambiguity_cache,
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
                        let joined = self.store.get_joined_user_ids(&room_id).await?;
                        let invited = self.store.get_invited_user_ids(&room_id).await?;

                        let user_ids: Vec<&UserId> =
                            joined.iter().chain(&invited).map(Deref::deref).collect();
                        o.update_tracked_users(user_ids).await
                    }

                    o.update_tracked_users(user_ids.iter().map(Deref::deref)).await;
                }
            }

            let notification_count = new_info.unread_notifications.into();
            room_info.update_notification_count(notification_count);

            #[cfg(feature = "experimental-timeline")]
            let timeline_slice = TimelineSlice::new(
                timeline.events.clone(),
                next_batch.clone(),
                timeline.prev_batch.clone(),
                timeline.limited,
                true,
            );

            #[cfg(feature = "experimental-timeline")]
            changes.add_timeline(&room_id, timeline_slice);

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
                    &new_info.state.events,
                    &mut room_info,
                    &mut changes,
                    &mut ambiguity_cache,
                )
                .await?;

            let timeline = self
                .handle_timeline(
                    &room,
                    new_info.timeline,
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

        self.store.save_changes(&changes).await?;
        *self.store.sync_token.write().await = Some(next_batch.clone());
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

    pub(crate) async fn apply_changes(&self, changes: &StateChanges) {
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

        #[cfg(feature = "experimental-timeline")]
        for (room_id, timeline_slice) in &changes.timeline {
            if let Some(room) = self.store.get_room(room_id) {
                room.add_timeline_slice(timeline_slice).await;
            }
        }
    }

    /// Receive a timeline slice obtained from a messages request.
    ///
    /// You should pass only slices requested from the store to this function.
    ///
    /// * `timeline` - The `TimelineSlice`
    #[cfg(feature = "experimental-timeline")]
    pub async fn receive_messages(&self, room_id: &RoomId, timeline: TimelineSlice) -> Result<()> {
        let mut changes = StateChanges::default();

        changes.add_timeline(room_id, timeline);

        self.store().save_changes(&changes).await?;

        Ok(())
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
        response: &api::membership::get_member_events::v3::Response,
    ) -> Result<MembersResponse> {
        let members: Vec<_> = response
            .chunk
            .iter()
            .filter_map(|event| match event.deserialize() {
                Ok(ev) => Some(ev),
                Err(e) => {
                    debug!(?event, "Failed to deserialize m.room.member event: {e}");
                    None
                }
            })
            .collect();

        let mut ambiguity_cache = AmbiguityCache::new(self.store.inner.clone());

        if let Some(room) = self.store.get_room(room_id) {
            let mut room_info = room.clone_info();
            room_info.mark_members_synced();

            let mut changes = StateChanges::default();

            #[cfg(feature = "e2e-encryption")]
            let mut user_ids = BTreeSet::new();

            for member in &members {
                let member: SyncRoomMemberEvent = member.clone().into();

                if self.store.get_member_event(room_id, member.state_key()).await?.is_none() {
                    #[cfg(feature = "e2e-encryption")]
                    match member.membership() {
                        MembershipState::Join | MembershipState::Invite => {
                            user_ids.insert(member.state_key().to_owned());
                        }
                        _ => (),
                    }

                    ambiguity_cache.handle_event(&changes, room_id, &member).await?;

                    if member.state_key() == member.sender() {
                        changes
                            .profiles
                            .entry(room_id.to_owned())
                            .or_default()
                            .insert(member.sender().to_owned(), member.borrow().into());
                    }

                    changes
                        .members
                        .entry(room_id.to_owned())
                        .or_default()
                        .insert(member.state_key().to_owned(), member);
                }
            }

            #[cfg(feature = "e2e-encryption")]
            if room_info.is_encrypted() {
                if let Some(o) = self.olm_machine() {
                    o.update_tracked_users(user_ids.iter().map(Deref::deref)).await
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
        response: &api::filter::create_filter::v3::Response,
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

    /// Get a to-device request that will share a room key with users in a room.
    #[cfg(feature = "e2e-encryption")]
    pub async fn share_room_key(&self, room_id: &RoomId) -> Result<Vec<Arc<ToDeviceRequest>>> {
        match self.olm_machine() {
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
                let settings = EncryptionSettings::new(settings, history_visibility, false);

                Ok(o.share_room_key(room_id, members.map(Deref::deref), settings).await?)
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

        let user_display_name = if let Some(member) =
            changes.members.get(room_id).and_then(|members| members.get(user_id))
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
        let room_id = &room_info.room_id;

        push_rules.member_count = UInt::new(room_info.active_members_count()).unwrap_or(UInt::MAX);

        if let Some(member) =
            changes.members.get(&**room_id).and_then(|members| members.get(user_id))
        {
            push_rules.user_display_name = member
                .as_original()
                .and_then(|ev| ev.content.displayname.clone())
                .unwrap_or_else(|| user_id.localpart().to_owned())
        }

        if let Some(AnySyncStateEvent::RoomPowerLevels(event)) = changes
            .state
            .get(&**room_id)
            .and_then(|types| types.get(&StateEventType::RoomPowerLevels)?.get(""))
            .and_then(|e| e.deserialize().ok())
        {
            let room_power_levels = event.power_levels();

            push_rules.users_power_levels = room_power_levels.users;
            push_rules.default_power_level = room_power_levels.users_default;
            push_rules.notification_power_levels = room_power_levels.notifications;
        }
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
    use crate::{DisplayName, RoomType, Session};

    #[async_test]
    async fn invite_after_leaving() {
        let user_id = user_id!("@alice:example.org");
        let room_id = room_id!("!test:example.org");

        let client = BaseClient::new();
        client
            .restore_login(Session {
                access_token: "token".to_owned(),
                refresh_token: None,
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
        assert_eq!(client.get_room(room_id).unwrap().room_type(), RoomType::Left);

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
        assert_eq!(client.get_room(room_id).unwrap().room_type(), RoomType::Invited);
    }

    #[async_test]
    async fn invite_displayname_integration_test() {
        let user_id = user_id!("@alice:example.org");
        let room_id = room_id!("!ithpyNKDtmhneaTQja:example.org");

        let client = BaseClient::new();
        client
            .restore_login(Session {
                access_token: "token".to_owned(),
                refresh_token: None,
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
        assert_eq!(room.room_type(), RoomType::Invited);
        assert_eq!(
            room.display_name().await.expect("fetching display name failed"),
            DisplayName::Calculated("Kyra".to_owned())
        );
    }
}
