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
    fmt, iter,
    sync::Arc,
};

use eyeball::{SharedObservable, Subscriber};
use eyeball_im::{Vector, VectorDiff};
use futures_util::Stream;
#[cfg(feature = "e2e-encryption")]
use matrix_sdk_crypto::{
    store::DynCryptoStore, CollectStrategy, DecryptionSettings, EncryptionSettings,
    EncryptionSyncChanges, OlmError, OlmMachine, ToDeviceRequest, TrustRequirement,
};
#[cfg(feature = "e2e-encryption")]
use ruma::events::{
    room::{history_visibility::HistoryVisibility, message::MessageType},
    SyncMessageLikeEvent,
};
#[cfg(doc)]
use ruma::DeviceId;
use ruma::{
    api::client as api,
    events::{
        ignored_user_list::IgnoredUserListEvent,
        push_rules::{PushRulesEvent, PushRulesEventContent},
        room::{
            member::{MembershipState, RoomMemberEventContent, SyncRoomMemberEvent},
            power_levels::{
                RoomPowerLevelsEvent, RoomPowerLevelsEventContent, StrippedRoomPowerLevelsEvent,
            },
        },
        AnyRoomAccountDataEvent, AnyStrippedStateEvent, AnySyncEphemeralRoomEvent,
        AnySyncMessageLikeEvent, AnySyncStateEvent, AnySyncTimelineEvent,
        GlobalAccountDataEventType, StateEvent, StateEventType, SyncStateEvent,
    },
    push::{Action, PushConditionRoomCtx, Ruleset},
    serde::Raw,
    time::Instant,
    OwnedRoomId, OwnedUserId, RoomId, RoomVersionId, UInt, UserId,
};
use tokio::sync::{broadcast, Mutex};
#[cfg(feature = "e2e-encryption")]
use tokio::sync::{RwLock, RwLockReadGuard};
use tracing::{debug, error, info, instrument, trace, warn};

#[cfg(all(feature = "e2e-encryption", feature = "experimental-sliding-sync"))]
use crate::latest_event::{is_suitable_for_latest_event, LatestEvent, PossibleLatestEvent};
#[cfg(feature = "e2e-encryption")]
use crate::RoomMemberships;
use crate::{
    deserialized_responses::{RawAnySyncOrStrippedTimelineEvent, SyncTimelineEvent},
    error::{Error, Result},
    event_cache_store::DynEventCacheStore,
    response_processors::AccountDataProcessor,
    rooms::{
        normal::{RoomInfoNotableUpdate, RoomInfoNotableUpdateReasons},
        Room, RoomInfo, RoomState,
    },
    store::{
        ambiguity_map::AmbiguityCache, DynStateStore, MemoryStore, Result as StoreResult,
        StateChanges, StateStoreDataKey, StateStoreDataValue, StateStoreExt, Store, StoreConfig,
    },
    sync::{JoinedRoomUpdate, LeftRoomUpdate, Notification, RoomUpdates, SyncResponse, Timeline},
    RoomStateFilter, SessionMeta,
};

/// A no IO Client implementation.
///
/// This Client is a state machine that receives responses and events and
/// accordingly updates its state.
#[derive(Clone)]
pub struct BaseClient {
    /// Database
    pub(crate) store: Store,
    /// The store used by the event cache.
    event_cache_store: Arc<DynEventCacheStore>,
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
    olm_machine: Arc<RwLock<Option<OlmMachine>>>,
    /// Observable of when a user is ignored/unignored.
    pub(crate) ignore_user_list_changes: SharedObservable<Vec<String>>,

    /// A sender that is used to communicate changes to room information. Each
    /// event contains the room and a boolean whether this event should
    /// trigger a room list update.
    pub(crate) room_info_notable_update_sender: broadcast::Sender<RoomInfoNotableUpdate>,

    /// The strategy to use for picking recipient devices, when sending an
    /// encrypted message.
    #[cfg(feature = "e2e-encryption")]
    pub room_key_recipient_strategy: CollectStrategy,

    /// The trust requirement to use for decrypting events.
    #[cfg(feature = "e2e-encryption")]
    pub decryption_trust_requirement: TrustRequirement,
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
    ///   previous login call.
    pub fn with_store_config(config: StoreConfig) -> Self {
        let (room_info_notable_update_sender, _room_info_notable_update_receiver) =
            broadcast::channel(u16::MAX as usize);

        BaseClient {
            store: Store::new(config.state_store),
            event_cache_store: config.event_cache_store,
            #[cfg(feature = "e2e-encryption")]
            crypto_store: config.crypto_store,
            #[cfg(feature = "e2e-encryption")]
            olm_machine: Default::default(),
            ignore_user_list_changes: Default::default(),
            room_info_notable_update_sender,
            #[cfg(feature = "e2e-encryption")]
            room_key_recipient_strategy: Default::default(),
            #[cfg(feature = "e2e-encryption")]
            decryption_trust_requirement: TrustRequirement::Untrusted,
        }
    }

    /// Clones the current base client to use the same crypto store but a
    /// different, in-memory store config, and resets transient state.
    #[cfg(feature = "e2e-encryption")]
    pub async fn clone_with_in_memory_state_store(&self) -> Result<Self> {
        let config = StoreConfig::new().state_store(MemoryStore::new());
        let config = config.crypto_store(self.crypto_store.clone());

        let copy = Self {
            store: Store::new(config.state_store),
            event_cache_store: config.event_cache_store,
            // We copy the crypto store as well as the `OlmMachine` for two reasons:
            // 1. The `self.crypto_store` is the same as the one used inside the `OlmMachine`.
            // 2. We need to ensure that the parent and child use the same data and caches inside
            //    the `OlmMachine` so the various ratchets and places where new randomness gets
            //    introduced don't diverge, i.e. one-time keys that get generated by the Olm Account
            //    or Olm sessions when they encrypt or decrypt messages.
            crypto_store: self.crypto_store.clone(),
            olm_machine: self.olm_machine.clone(),
            ignore_user_list_changes: Default::default(),
            room_info_notable_update_sender: self.room_info_notable_update_sender.clone(),
            room_key_recipient_strategy: self.room_key_recipient_strategy.clone(),
            decryption_trust_requirement: self.decryption_trust_requirement,
        };

        if let Some(session_meta) = self.session_meta().cloned() {
            copy.store
                .set_session_meta(session_meta, &copy.room_info_notable_update_sender)
                .await?;
        }

        Ok(copy)
    }

    /// Clones the current base client to use the same crypto store but a
    /// different, in-memory store config, and resets transient state.
    #[cfg(not(feature = "e2e-encryption"))]
    #[allow(clippy::unused_async)]
    pub async fn clone_with_in_memory_state_store(&self) -> Result<Self> {
        let config = StoreConfig::new().state_store(MemoryStore::new());
        Ok(Self::with_store_config(config))
    }

    /// Get the session meta information.
    ///
    /// If the client is currently logged in, this will return a
    /// [`SessionMeta`] object which contains the user ID and device ID.
    /// Otherwise it returns `None`.
    pub fn session_meta(&self) -> Option<&SessionMeta> {
        self.store.session_meta()
    }

    /// Get all the rooms this client knows about.
    pub fn rooms(&self) -> Vec<Room> {
        self.store.rooms()
    }

    /// Get all the rooms this client knows about, filtered by room state.
    pub fn rooms_filtered(&self, filter: RoomStateFilter) -> Vec<Room> {
        self.store.rooms_filtered(filter)
    }

    /// Get a stream of all the rooms changes, in addition to the existing
    /// rooms.
    pub fn rooms_stream(&self) -> (Vector<Room>, impl Stream<Item = Vec<VectorDiff<Room>>>) {
        self.store.rooms_stream()
    }

    /// Lookup the Room for the given RoomId, or create one, if it didn't exist
    /// yet in the store
    pub fn get_or_create_room(&self, room_id: &RoomId, room_state: RoomState) -> Room {
        self.store.get_or_create_room(
            room_id,
            room_state,
            self.room_info_notable_update_sender.clone(),
        )
    }

    /// Get a reference to the store.
    #[allow(unknown_lints, clippy::explicit_auto_deref)]
    pub fn store(&self) -> &DynStateStore {
        &*self.store
    }

    /// Get a reference to the event cache store.
    pub fn event_cache_store(&self) -> &DynEventCacheStore {
        &*self.event_cache_store
    }

    /// Is the client logged in.
    pub fn logged_in(&self) -> bool {
        self.store.session_meta().is_some()
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
    /// * `custom_account` - A custom
    ///   [`matrix_sdk_crypto::vodozemac::olm::Account`] to be used for the
    ///   identity and one-time keys of this [`BaseClient`]. If no account is
    ///   provided, a new default one or one from the store will be used. If an
    ///   account is provided and one already exists in the store for this
    ///   [`UserId`]/[`DeviceId`] combination, an error will be raised. This is
    ///   useful if one wishes to create identity keys before knowing the
    ///   user/device IDs, e.g., to use the identity key as the device ID.
    ///
    /// This method panics if it is called twice.
    pub async fn set_session_meta(
        &self,
        session_meta: SessionMeta,
        #[cfg(feature = "e2e-encryption")] custom_account: Option<
            crate::crypto::vodozemac::olm::Account,
        >,
    ) -> Result<()> {
        debug!(user_id = ?session_meta.user_id, device_id = ?session_meta.device_id, "Restoring login");
        self.store
            .set_session_meta(session_meta.clone(), &self.room_info_notable_update_sender)
            .await?;

        #[cfg(feature = "e2e-encryption")]
        self.regenerate_olm(custom_account).await?;

        Ok(())
    }

    /// Recreate an `OlmMachine` from scratch.
    ///
    /// In particular, this will clear all its caches.
    #[cfg(feature = "e2e-encryption")]
    pub async fn regenerate_olm(
        &self,
        custom_account: Option<crate::crypto::vodozemac::olm::Account>,
    ) -> Result<()> {
        tracing::debug!("regenerating OlmMachine");
        let session_meta = self.session_meta().ok_or(Error::OlmError(OlmError::MissingSession))?;

        // Recreate the `OlmMachine` and wipe the in-memory cache in the store
        // because we suspect it has stale data.
        let olm_machine = OlmMachine::with_store(
            &session_meta.user_id,
            &session_meta.device_id,
            self.crypto_store.clone(),
            custom_account,
        )
        .await
        .map_err(OlmError::from)?;

        *self.olm_machine.write().await = Some(olm_machine);
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
        if let Some(olm) = self.olm_machine().await.as_ref() {
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
        let olm = self.olm_machine().await;
        let Some(olm) = olm.as_ref() else { return Ok(None) };

        let decryption_settings = DecryptionSettings {
            sender_device_trust_requirement: self.decryption_trust_requirement,
        };
        let event: SyncTimelineEvent =
            olm.decrypt_room_event(event.cast_ref(), room_id, &decryption_settings).await?.into();

        if let Ok(AnySyncTimelineEvent::MessageLike(e)) = event.raw().deserialize() {
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
        ignore_state_events: bool,
        prev_batch: Option<String>,
        push_rules: &Ruleset,
        user_ids: &mut BTreeSet<OwnedUserId>,
        room_info: &mut RoomInfo,
        changes: &mut StateChanges,
        notifications: &mut BTreeMap<OwnedRoomId, Vec<Notification>>,
        ambiguity_cache: &mut AmbiguityCache,
    ) -> Result<Timeline> {
        let mut timeline = Timeline::new(limited, prev_batch);
        let mut push_context = self.get_push_room_context(room, room_info, changes).await?;

        for event in events {
            let mut event: SyncTimelineEvent = event.into();

            match event.raw().deserialize() {
                Ok(e) => {
                    #[allow(clippy::single_match)]
                    match &e {
                        AnySyncTimelineEvent::State(s) if !ignore_state_events => {
                            match s {
                                AnySyncStateEvent::RoomMember(member) => {
                                    Box::pin(ambiguity_cache.handle_event(
                                        changes,
                                        room.room_id(),
                                        member,
                                    ))
                                    .await?;

                                    match member.membership() {
                                        MembershipState::Join | MembershipState::Invite => {
                                            user_ids.insert(member.state_key().to_owned());
                                        }
                                        _ => {
                                            user_ids.remove(member.state_key());
                                        }
                                    }

                                    handle_room_member_event_for_profiles(
                                        room.room_id(),
                                        member,
                                        changes,
                                    );
                                }
                                _ => {
                                    room_info.handle_state_event(s);
                                }
                            }

                            let raw_event: Raw<AnySyncStateEvent> = event.raw().clone().cast();
                            changes.add_state_event(room.room_id(), s.clone(), raw_event);
                        }

                        AnySyncTimelineEvent::State(_) => { /* do nothing */ }

                        AnySyncTimelineEvent::MessageLike(
                            AnySyncMessageLikeEvent::RoomRedaction(r),
                        ) => {
                            let room_version =
                                room_info.room_version().unwrap_or(&RoomVersionId::V1);

                            if let Some(redacts) = r.redacts(room_version) {
                                room_info.handle_redaction(r, event.raw().cast_ref());
                                let raw_event = event.raw().clone().cast();

                                changes.add_redaction(room.room_id(), redacts, raw_event);
                            }
                        }

                        #[cfg(feature = "e2e-encryption")]
                        AnySyncTimelineEvent::MessageLike(e) => match e {
                            AnySyncMessageLikeEvent::RoomEncrypted(
                                SyncMessageLikeEvent::Original(_),
                            ) => {
                                if let Ok(Some(e)) = Box::pin(
                                    self.decrypt_sync_room_event(event.raw(), room.room_id()),
                                )
                                .await
                                {
                                    event = e;
                                }
                            }
                            AnySyncMessageLikeEvent::RoomMessage(
                                SyncMessageLikeEvent::Original(original_event),
                            ) => match &original_event.content.msgtype {
                                MessageType::VerificationRequest(_) => {
                                    Box::pin(self.handle_verification_event(e, room.room_id()))
                                        .await?;
                                }
                                _ => (),
                            },
                            _ if e.event_type().to_string().starts_with("m.key.verification") => {
                                Box::pin(self.handle_verification_event(e, room.room_id())).await?;
                            }
                            _ => (),
                        },

                        #[cfg(not(feature = "e2e-encryption"))]
                        AnySyncTimelineEvent::MessageLike(_) => (),
                    }

                    if let Some(context) = &mut push_context {
                        self.update_push_room_context(
                            context,
                            room.own_user_id(),
                            room_info,
                            changes,
                        )
                    } else {
                        push_context = self.get_push_room_context(room, room_info, changes).await?;
                    }

                    if let Some(context) = &push_context {
                        let actions = push_rules.get_actions(event.raw(), context);

                        if actions.iter().any(Action::should_notify) {
                            notifications.entry(room.room_id().to_owned()).or_default().push(
                                Notification {
                                    actions: actions.to_owned(),
                                    event: RawAnySyncOrStrippedTimelineEvent::Sync(
                                        event.raw().clone(),
                                    ),
                                },
                            );
                        }
                        event.push_actions = actions.to_owned();
                    }
                }
                Err(e) => {
                    warn!("Error deserializing event: {e}");
                }
            }

            timeline.events.push(event);
        }

        Ok(timeline)
    }

    #[instrument(skip_all, fields(room_id = ?room_info.room_id))]
    pub(crate) async fn handle_invited_state(
        &self,
        room: &Room,
        events: &[Raw<AnyStrippedStateEvent>],
        push_rules: &Ruleset,
        room_info: &mut RoomInfo,
        changes: &mut StateChanges,
        notifications: &mut BTreeMap<OwnedRoomId, Vec<Notification>>,
    ) -> Result<()> {
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

        changes.stripped_state.insert(room_info.room_id().to_owned(), state_events.clone());

        // We need to check for notifications after we have handled all state
        // events, to make sure we have the full push context.
        if let Some(push_context) = self.get_push_room_context(room, room_info, changes).await? {
            // Check every event again for notification.
            for event in state_events.values().flat_map(|map| map.values()) {
                let actions = push_rules.get_actions(event, &push_context);
                if actions.iter().any(Action::should_notify) {
                    notifications.entry(room.room_id().to_owned()).or_default().push(
                        Notification {
                            actions: actions.to_owned(),
                            event: RawAnySyncOrStrippedTimelineEvent::Stripped(event.clone()),
                        },
                    );
                }
            }
        }

        Ok(())
    }

    /// Process the events provided during a sync.
    ///
    /// events must be exactly the same list of events that are in raw_events,
    /// but deserialised. We demand them here to avoid deserialising
    /// multiple times.
    #[instrument(skip_all, fields(room_id = ?room_info.room_id))]
    pub(crate) async fn handle_state(
        &self,
        raw_events: &[Raw<AnySyncStateEvent>],
        events: &[AnySyncStateEvent],
        room_info: &mut RoomInfo,
        changes: &mut StateChanges,
        ambiguity_cache: &mut AmbiguityCache,
    ) -> StoreResult<BTreeSet<OwnedUserId>> {
        let mut state_events = BTreeMap::new();
        let mut user_ids = BTreeSet::new();

        assert_eq!(raw_events.len(), events.len());

        for (raw_event, event) in iter::zip(raw_events, events) {
            room_info.handle_state_event(event);

            if let AnySyncStateEvent::RoomMember(member) = &event {
                ambiguity_cache.handle_event(changes, &room_info.room_id, member).await?;

                match member.membership() {
                    MembershipState::Join | MembershipState::Invite => {
                        user_ids.insert(member.state_key().to_owned());
                    }
                    _ => (),
                }

                handle_room_member_event_for_profiles(&room_info.room_id, member, changes);
            }

            state_events
                .entry(event.event_type())
                .or_insert_with(BTreeMap::new)
                .insert(event.state_key().to_owned(), raw_event.clone());
        }

        changes.state.insert((*room_info.room_id).to_owned(), state_events);

        Ok(user_ids)
    }

    #[instrument(skip_all, fields(?room_id))]
    pub(crate) async fn handle_room_account_data(
        &self,
        room_id: &RoomId,
        events: &[Raw<AnyRoomAccountDataEvent>],
        changes: &mut StateChanges,
        room_info_notable_updates: &mut BTreeMap<OwnedRoomId, RoomInfoNotableUpdateReasons>,
    ) {
        // Small helper to make the code easier to read.
        //
        // It finds the appropriate `RoomInfo`, allowing the caller to modify it, and
        // save it in the correct place.
        fn on_room_info<F>(
            room_id: &RoomId,
            changes: &mut StateChanges,
            client: &BaseClient,
            mut on_room_info: F,
        ) where
            F: FnMut(&mut RoomInfo),
        {
            // `StateChanges` has the `RoomInfo`.
            if let Some(room_info) = changes.room_infos.get_mut(room_id) {
                // Show time.
                on_room_info(room_info);
            }
            // The `BaseClient` has the `Room`, which has the `RoomInfo`.
            else if let Some(room) = client.store.room(room_id) {
                // Clone the `RoomInfo`.
                let mut room_info = room.clone_info();

                // Show time.
                on_room_info(&mut room_info);

                // Update the `RoomInfo` via `StateChanges`.
                changes.add_room(room_info);
            }
        }

        // Handle new events.
        for raw_event in events {
            match raw_event.deserialize() {
                Ok(event) => {
                    changes.add_room_account_data(room_id, event.clone(), raw_event.clone());

                    match event {
                        AnyRoomAccountDataEvent::MarkedUnread(event) => {
                            on_room_info(room_id, changes, self, |room_info| {
                                if room_info.base_info.is_marked_unread != event.content.unread {
                                    // Notify the room list about a manual read marker change if the
                                    // value's changed.
                                    room_info_notable_updates
                                        .entry(room_id.to_owned())
                                        .or_default()
                                        .insert(RoomInfoNotableUpdateReasons::UNREAD_MARKER);
                                }

                                room_info.base_info.is_marked_unread = event.content.unread;
                            });
                        }

                        AnyRoomAccountDataEvent::Tag(event) => {
                            on_room_info(room_id, changes, self, |room_info| {
                                room_info.base_info.handle_notable_tags(&event.content.tags);
                            });
                        }

                        // Nothing.
                        _ => {}
                    }
                }

                Err(err) => {
                    warn!("unable to deserialize account data event: {err}");
                }
            }
        }
    }

    #[cfg(feature = "e2e-encryption")]
    #[instrument(skip_all)]
    pub(crate) async fn preprocess_to_device_events(
        &self,
        encryption_sync_changes: EncryptionSyncChanges<'_>,
        changes: &mut StateChanges,
        room_info_notable_updates: &mut BTreeMap<OwnedRoomId, RoomInfoNotableUpdateReasons>,
    ) -> Result<Vec<Raw<ruma::events::AnyToDeviceEvent>>> {
        if let Some(o) = self.olm_machine().await.as_ref() {
            // Let the crypto machine handle the sync response, this
            // decrypts to-device events, but leaves room events alone.
            // This makes sure that we have the decryption keys for the room
            // events at hand.
            let (events, room_key_updates) =
                o.receive_sync_changes(encryption_sync_changes).await?;

            #[cfg(feature = "experimental-sliding-sync")]
            for room_key_update in room_key_updates {
                if let Some(room) = self.get_room(&room_key_update.room_id) {
                    self.decrypt_latest_events(&room, changes, room_info_notable_updates).await;
                }
            }

            #[cfg(not(feature = "experimental-sliding-sync"))] // Silence unused variable warnings.
            let _ = (room_key_updates, changes, room_info_notable_updates);

            Ok(events)
        } else {
            // If we have no OlmMachine, just return the events that were passed in.
            // This should not happen unless we forget to set things up by calling
            // set_session_meta().
            Ok(encryption_sync_changes.to_device_events)
        }
    }

    /// Decrypt any of this room's latest_encrypted_events
    /// that we can and if we can, change latest_event to reflect what we
    /// found, and remove any older encrypted events from
    /// latest_encrypted_events.
    #[cfg(all(feature = "e2e-encryption", feature = "experimental-sliding-sync"))]
    async fn decrypt_latest_events(
        &self,
        room: &Room,
        changes: &mut StateChanges,
        room_info_notable_updates: &mut BTreeMap<OwnedRoomId, RoomInfoNotableUpdateReasons>,
    ) {
        // Try to find a message we can decrypt and is suitable for using as the latest
        // event. If we found one, set it as the latest and delete any older
        // encrypted events
        if let Some((found, found_index)) = self.decrypt_latest_suitable_event(room).await {
            room.on_latest_event_decrypted(found, found_index, changes, room_info_notable_updates);
        }
    }

    /// Attempt to decrypt a latest event, trying the latest stored encrypted
    /// one first, and walking backwards, stopping when we find an event
    /// that we can decrypt, and that is suitable to be the latest event
    /// (i.e. we can usefully display it as a message preview). Returns the
    /// decrypted event if we found one, along with its index in the
    /// latest_encrypted_events list, or None if we didn't find one.
    #[cfg(all(feature = "e2e-encryption", feature = "experimental-sliding-sync"))]
    async fn decrypt_latest_suitable_event(
        &self,
        room: &Room,
    ) -> Option<(Box<LatestEvent>, usize)> {
        let enc_events = room.latest_encrypted_events();

        // Walk backwards through the encrypted events, looking for one we can decrypt
        for (i, event) in enc_events.iter().enumerate().rev() {
            // Size of the decrypt_sync_room_event future should not impact this
            // async fn since it is likely that there aren't even any encrypted
            // events when calling it.
            let decrypt_sync_room_event =
                Box::pin(self.decrypt_sync_room_event(event, room.room_id()));

            if let Ok(Some(decrypted)) = decrypt_sync_room_event.await {
                // We found an event we can decrypt
                if let Ok(any_sync_event) = decrypted.raw().deserialize() {
                    // We can deserialize it to find its type
                    match is_suitable_for_latest_event(&any_sync_event) {
                        PossibleLatestEvent::YesRoomMessage(_)
                        | PossibleLatestEvent::YesPoll(_)
                        | PossibleLatestEvent::YesCallInvite(_)
                        | PossibleLatestEvent::YesCallNotify(_)
                        | PossibleLatestEvent::YesSticker(_) => {
                            // The event is the right type for us to use as latest_event
                            return Some((Box::new(LatestEvent::new(decrypted)), i));
                        }
                        _ => (),
                    }
                }
            }
        }
        None
    }

    /// User has knocked on a room.
    ///
    /// Update the internal and cached state accordingly. Return the final Room.
    pub async fn room_knocked(&self, room_id: &RoomId) -> Result<Room> {
        let room = self.store.get_or_create_room(
            room_id,
            RoomState::Knocked,
            self.room_info_notable_update_sender.clone(),
        );

        if room.state() != RoomState::Knocked {
            let _sync_lock = self.sync_lock().lock().await;

            let mut room_info = room.clone_info();
            room_info.mark_as_knocked();
            room_info.mark_state_partially_synced();
            room_info.mark_members_missing(); // the own member event changed
            let mut changes = StateChanges::default();
            changes.add_room(room_info.clone());
            self.store.save_changes(&changes).await?; // Update the store
            room.set_room_info(room_info, RoomInfoNotableUpdateReasons::MEMBERSHIP);
        }

        Ok(room)
    }

    /// User has joined a room.
    ///
    /// Update the internal and cached state accordingly. Return the final Room.
    pub async fn room_joined(&self, room_id: &RoomId) -> Result<Room> {
        let room = self.store.get_or_create_room(
            room_id,
            RoomState::Joined,
            self.room_info_notable_update_sender.clone(),
        );

        if room.state() != RoomState::Joined {
            let _sync_lock = self.sync_lock().lock().await;

            let mut room_info = room.clone_info();
            room_info.mark_as_joined();
            room_info.mark_state_partially_synced();
            room_info.mark_members_missing(); // the own member event changed
            let mut changes = StateChanges::default();
            changes.add_room(room_info.clone());
            self.store.save_changes(&changes).await?; // Update the store
            room.set_room_info(room_info, RoomInfoNotableUpdateReasons::MEMBERSHIP);
        }

        Ok(room)
    }

    /// User has left a room.
    ///
    /// Update the internal and cached state accordingly.
    pub async fn room_left(&self, room_id: &RoomId) -> Result<()> {
        let room = self.store.get_or_create_room(
            room_id,
            RoomState::Left,
            self.room_info_notable_update_sender.clone(),
        );

        if room.state() != RoomState::Left {
            let _sync_lock = self.sync_lock().lock().await;

            let mut room_info = room.clone_info();
            room_info.mark_as_left();
            room_info.mark_state_partially_synced();
            room_info.mark_members_missing(); // the own member event changed
            let mut changes = StateChanges::default();
            changes.add_room(room_info.clone());
            self.store.save_changes(&changes).await?; // Update the store
            room.set_room_info(room_info, RoomInfoNotableUpdateReasons::MEMBERSHIP);
        }

        Ok(())
    }

    /// Get access to the store's sync lock.
    pub fn sync_lock(&self) -> &Mutex<()> {
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
        // The server might respond multiple times with the same sync token, in
        // that case we already received this response and there's nothing to
        // do.
        if self.store.sync_token.read().await.as_ref() == Some(&response.next_batch) {
            info!("Got the same sync response twice");
            return Ok(SyncResponse::default());
        }

        let now = Instant::now();
        let mut changes = Box::new(StateChanges::new(response.next_batch.clone()));

        #[cfg_attr(not(feature = "e2e-encryption"), allow(unused_mut))]
        let mut room_info_notable_updates =
            BTreeMap::<OwnedRoomId, RoomInfoNotableUpdateReasons>::new();

        #[cfg(feature = "e2e-encryption")]
        let to_device = self
            .preprocess_to_device_events(
                EncryptionSyncChanges {
                    to_device_events: response.to_device.events,
                    changed_devices: &response.device_lists,
                    one_time_keys_counts: &response.device_one_time_keys_count,
                    unused_fallback_keys: response.device_unused_fallback_key_types.as_deref(),
                    next_batch_token: Some(response.next_batch.clone()),
                },
                &mut changes,
                &mut room_info_notable_updates,
            )
            .await?;

        #[cfg(not(feature = "e2e-encryption"))]
        let to_device = response.to_device.events;

        let mut ambiguity_cache = AmbiguityCache::new(self.store.inner.clone());

        let account_data_processor = AccountDataProcessor::process(&response.account_data.events);

        let push_rules = self.get_push_rules(&account_data_processor).await?;

        let mut new_rooms = RoomUpdates::default();
        let mut notifications = Default::default();

        for (room_id, new_info) in response.rooms.join {
            let room = self.store.get_or_create_room(
                &room_id,
                RoomState::Joined,
                self.room_info_notable_update_sender.clone(),
            );

            let mut room_info = room.clone_info();

            room_info.mark_as_joined();
            room_info.update_from_ruma_summary(&new_info.summary);
            room_info.set_prev_batch(new_info.timeline.prev_batch.as_deref());
            room_info.mark_state_fully_synced();

            let state_events = Self::deserialize_state_events(&new_info.state.events);
            let (raw_state_events, state_events): (Vec<_>, Vec<_>) =
                state_events.into_iter().unzip();

            let mut user_ids = self
                .handle_state(
                    &raw_state_events,
                    &state_events,
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
                    false,
                    new_info.timeline.prev_batch,
                    &push_rules,
                    &mut user_ids,
                    &mut room_info,
                    &mut changes,
                    &mut notifications,
                    &mut ambiguity_cache,
                )
                .await?;

            // Save the new `RoomInfo`.
            changes.add_room(room_info);

            self.handle_room_account_data(
                &room_id,
                &new_info.account_data.events,
                &mut changes,
                &mut Default::default(),
            )
            .await;

            // `Self::handle_room_account_data` might have updated the `RoomInfo`. Let's
            // fetch it again.
            //
            // SAFETY: `unwrap` is safe because the `RoomInfo` has been inserted 2 lines
            // above.
            let mut room_info = changes.room_infos.get(&room_id).unwrap().clone();

            #[cfg(feature = "e2e-encryption")]
            if room_info.is_encrypted() {
                if let Some(o) = self.olm_machine().await.as_ref() {
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

            let ambiguity_changes = ambiguity_cache.changes.remove(&room_id).unwrap_or_default();

            new_rooms.join.insert(
                room_id,
                JoinedRoomUpdate::new(
                    timeline,
                    new_info.state.events,
                    new_info.account_data.events,
                    new_info.ephemeral.events,
                    notification_count,
                    ambiguity_changes,
                ),
            );

            changes.add_room(room_info);
        }

        for (room_id, new_info) in response.rooms.leave {
            let room = self.store.get_or_create_room(
                &room_id,
                RoomState::Left,
                self.room_info_notable_update_sender.clone(),
            );

            let mut room_info = room.clone_info();
            room_info.mark_as_left();
            room_info.mark_state_partially_synced();

            let state_events = Self::deserialize_state_events(&new_info.state.events);
            let (raw_state_events, state_events): (Vec<_>, Vec<_>) =
                state_events.into_iter().unzip();

            let mut user_ids = self
                .handle_state(
                    &raw_state_events,
                    &state_events,
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
                    false,
                    new_info.timeline.prev_batch,
                    &push_rules,
                    &mut user_ids,
                    &mut room_info,
                    &mut changes,
                    &mut notifications,
                    &mut ambiguity_cache,
                )
                .await?;

            // Save the new `RoomInfo`.
            changes.add_room(room_info);

            self.handle_room_account_data(
                &room_id,
                &new_info.account_data.events,
                &mut changes,
                &mut Default::default(),
            )
            .await;

            let ambiguity_changes = ambiguity_cache.changes.remove(&room_id).unwrap_or_default();

            new_rooms.leave.insert(
                room_id,
                LeftRoomUpdate::new(
                    timeline,
                    new_info.state.events,
                    new_info.account_data.events,
                    ambiguity_changes,
                ),
            );
        }

        for (room_id, new_info) in response.rooms.invite {
            let room = self.store.get_or_create_room(
                &room_id,
                RoomState::Invited,
                self.room_info_notable_update_sender.clone(),
            );

            let mut room_info = room.clone_info();
            room_info.mark_as_invited();
            room_info.mark_state_fully_synced();

            self.handle_invited_state(
                &room,
                &new_info.invite_state.events,
                &push_rules,
                &mut room_info,
                &mut changes,
                &mut notifications,
            )
            .await?;

            changes.add_room(room_info);

            new_rooms.invite.insert(room_id, new_info);
        }

        account_data_processor.apply(&mut changes, &self.store).await;

        changes.presence = response
            .presence
            .events
            .iter()
            .filter_map(|e| {
                let event = e.deserialize().ok()?;
                Some((event.sender, e.clone()))
            })
            .collect();

        changes.ambiguity_maps = ambiguity_cache.cache;

        {
            let _sync_lock = self.sync_lock().lock().await;
            self.store.save_changes(&changes).await?;
            *self.store.sync_token.write().await = Some(response.next_batch.clone());
            self.apply_changes(&changes, room_info_notable_updates);
        }

        // Now that all the rooms information have been saved, update the display name
        // cache (which relies on information stored in the database). This will
        // live in memory, until the next sync which will saves the room info to
        // disk; we do this to avoid saving that would be redundant with the
        // above. Oh well.
        new_rooms.update_in_memory_caches(&self.store).await;

        info!("Processed a sync response in {:?}", now.elapsed());

        let response = SyncResponse {
            rooms: new_rooms,
            presence: response.presence.events,
            account_data: response.account_data.events,
            to_device,
            notifications,
        };

        Ok(response)
    }

    pub(crate) fn apply_changes(
        &self,
        changes: &StateChanges,
        room_info_notable_updates: BTreeMap<OwnedRoomId, RoomInfoNotableUpdateReasons>,
    ) {
        if let Some(event) = changes.account_data.get(&GlobalAccountDataEventType::IgnoredUserList)
        {
            match event.deserialize_as::<IgnoredUserListEvent>() {
                Ok(event) => {
                    let user_ids: Vec<String> =
                        event.content.ignored_users.keys().map(|id| id.to_string()).collect();

                    self.ignore_user_list_changes.set(user_ids);
                }
                Err(error) => {
                    error!("Failed to deserialize ignored user list event: {error}")
                }
            }
        }

        for (room_id, room_info) in &changes.room_infos {
            if let Some(room) = self.store.room(room_id) {
                let room_info_notable_update_reasons =
                    room_info_notable_updates.get(room_id).copied().unwrap_or_default();

                room.set_room_info(room_info.clone(), room_info_notable_update_reasons)
            }
        }
    }

    /// Receive a get member events response and convert it to a deserialized
    /// `MembersResponse`
    ///
    /// This client-server request must be made without filters to make sure all
    /// members are received. Otherwise, an error is returned.
    ///
    /// # Arguments
    ///
    /// * `room_id` - The room id this response belongs to.
    ///
    /// * `response` - The raw response that was received from the server.
    #[instrument(skip_all, fields(?room_id))]
    pub async fn receive_all_members(
        &self,
        room_id: &RoomId,
        request: &api::membership::get_member_events::v3::Request,
        response: &api::membership::get_member_events::v3::Response,
    ) -> Result<()> {
        if request.membership.is_some() || request.not_membership.is_some() || request.at.is_some()
        {
            // This function assumes all members are loaded at once to optimise how display
            // name disambiguation works. Using it with partial member list results
            // would produce incorrect disambiguated display name entries
            return Err(Error::InvalidReceiveMembersParameters);
        }

        let Some(room) = self.store.room(room_id) else {
            // The room is unknown to us: leave early.
            return Ok(());
        };

        let mut chunk = Vec::with_capacity(response.chunk.len());
        let mut changes = StateChanges::default();

        #[cfg(feature = "e2e-encryption")]
        let mut user_ids = BTreeSet::new();

        let mut ambiguity_map: BTreeMap<String, BTreeSet<OwnedUserId>> = BTreeMap::new();

        for raw_event in &response.chunk {
            let member = match raw_event.deserialize() {
                Ok(ev) => ev,
                Err(e) => {
                    let event_id: Option<String> = raw_event.get_field("event_id").ok().flatten();
                    debug!(event_id, "Failed to deserialize member event: {e}");
                    continue;
                }
            };

            // TODO: All the actions in this loop used to be done only when the membership
            // event was not in the store before. This was changed with the new room API,
            // because e.g. leaving a room makes members events outdated and they need to be
            // fetched by `members`. Therefore, they need to be overwritten here, even
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

            if let StateEvent::Original(e) = &member {
                if let Some(d) = &e.content.displayname {
                    ambiguity_map.entry(d.clone()).or_default().insert(member.state_key().clone());
                }
            }

            let sync_member: SyncRoomMemberEvent = member.clone().into();
            handle_room_member_event_for_profiles(room_id, &sync_member, &mut changes);

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
        if room.is_encrypted() {
            if let Some(o) = self.olm_machine().await.as_ref() {
                o.update_tracked_users(user_ids.iter().map(Deref::deref)).await?
            }
        }

        changes.ambiguity_maps.insert(room_id.to_owned(), ambiguity_map);

        let _sync_lock = self.sync_lock().lock().await;
        let mut room_info = room.clone_info();
        room_info.mark_members_synced();
        changes.add_room(room_info);

        self.store.save_changes(&changes).await?;
        self.apply_changes(&changes, Default::default());

        Ok(())
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
    ///   in the store.
    ///
    /// * `response` - The successful filter upload response containing the
    ///   filter id.
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
    ///   persist the filter.
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
        match self.olm_machine().await.as_ref() {
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
                let settings = EncryptionSettings::new(
                    settings,
                    history_visibility,
                    self.room_key_recipient_strategy.clone(),
                );

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
        self.store.room(room_id)
    }

    /// Get the olm machine.
    #[cfg(feature = "e2e-encryption")]
    pub async fn olm_machine(&self) -> RwLockReadGuard<'_, Option<OlmMachine>> {
        self.olm_machine.read().await
    }

    /// Get the push rules.
    ///
    /// Gets the push rules previously processed, otherwise get them from the
    /// store. As a fallback, uses [`Ruleset::server_default`] if the user
    /// is logged in.
    pub(crate) async fn get_push_rules(
        &self,
        account_data_processor: &AccountDataProcessor,
    ) -> Result<Ruleset> {
        if let Some(event) = account_data_processor
            .push_rules()
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
        let user_display_name = if let Some(AnySyncStateEvent::RoomMember(member)) =
            changes.state.get(room_id).and_then(|events| {
                events.get(&StateEventType::RoomMember)?.get(user_id.as_str())?.deserialize().ok()
            }) {
            member
                .as_original()
                .and_then(|ev| ev.content.displayname.clone())
                .unwrap_or_else(|| user_id.localpart().to_owned())
        } else if let Some(AnyStrippedStateEvent::RoomMember(member)) =
            changes.stripped_state.get(room_id).and_then(|events| {
                events.get(&StateEventType::RoomMember)?.get(user_id.as_str())?.deserialize().ok()
            })
        {
            member.content.displayname.unwrap_or_else(|| user_id.localpart().to_owned())
        } else if let Some(member) = Box::pin(room.get_member(user_id)).await? {
            member.name().to_owned()
        } else {
            trace!("Couldn't get push context because of missing own member information");
            return Ok(None);
        };

        let power_levels = if let Some(event) = changes.state.get(room_id).and_then(|types| {
            types
                .get(&StateEventType::RoomPowerLevels)?
                .get("")?
                .deserialize_as::<RoomPowerLevelsEvent>()
                .ok()
        }) {
            Some(event.power_levels().into())
        } else if let Some(event) = changes.stripped_state.get(room_id).and_then(|types| {
            types
                .get(&StateEventType::RoomPowerLevels)?
                .get("")?
                .deserialize_as::<StrippedRoomPowerLevelsEvent>()
                .ok()
        }) {
            Some(event.power_levels().into())
        } else {
            self.store
                .get_state_event_static::<RoomPowerLevelsEventContent>(room_id)
                .await?
                .and_then(|e| e.deserialize().ok())
                .map(|event| event.power_levels().into())
        };

        Ok(Some(PushConditionRoomCtx {
            user_id: user_id.to_owned(),
            room_id: room_id.to_owned(),
            member_count: UInt::new(member_count).unwrap_or(UInt::MAX),
            user_display_name,
            power_levels,
        }))
    }

    /// Update the push context for the given room.
    ///
    /// Updates the context data from `changes` or `room_info`.
    pub fn update_push_room_context(
        &self,
        push_rules: &mut PushConditionRoomCtx,
        user_id: &UserId,
        room_info: &RoomInfo,
        changes: &StateChanges,
    ) {
        let room_id = &*room_info.room_id;

        push_rules.member_count = UInt::new(room_info.active_members_count()).unwrap_or(UInt::MAX);

        // TODO: Use if let chain once stable
        if let Some(AnySyncStateEvent::RoomMember(member)) =
            changes.state.get(room_id).and_then(|events| {
                events.get(&StateEventType::RoomMember)?.get(user_id.as_str())?.deserialize().ok()
            })
        {
            push_rules.user_display_name = member
                .as_original()
                .and_then(|ev| ev.content.displayname.clone())
                .unwrap_or_else(|| user_id.localpart().to_owned())
        }

        if let Some(AnySyncStateEvent::RoomPowerLevels(event)) =
            changes.state.get(room_id).and_then(|types| {
                types.get(&StateEventType::RoomPowerLevels)?.get("")?.deserialize().ok()
            })
        {
            push_rules.power_levels = Some(event.power_levels().into());
        }
    }

    /// Returns a subscriber that publishes an event every time the ignore user
    /// list changes
    pub fn subscribe_to_ignore_user_list_changes(&self) -> Subscriber<Vec<String>> {
        self.ignore_user_list_changes.subscribe()
    }

    pub(crate) fn deserialize_state_events(
        raw_events: &[Raw<AnySyncStateEvent>],
    ) -> Vec<(Raw<AnySyncStateEvent>, AnySyncStateEvent)> {
        raw_events
            .iter()
            .filter_map(|raw_event| match raw_event.deserialize() {
                Ok(event) => Some((raw_event.clone(), event)),
                Err(e) => {
                    warn!("Couldn't deserialize state event: {e}");
                    None
                }
            })
            .collect()
    }

    /// Returns a new receiver that gets future room info notable updates.
    ///
    /// Learn more by reading the [`RoomInfoNotableUpdate`] type.
    pub fn room_info_notable_update_receiver(&self) -> broadcast::Receiver<RoomInfoNotableUpdate> {
        self.room_info_notable_update_sender.subscribe()
    }
}

impl Default for BaseClient {
    fn default() -> Self {
        Self::new()
    }
}

fn handle_room_member_event_for_profiles(
    room_id: &RoomId,
    event: &SyncStateEvent<RoomMemberEventContent>,
    changes: &mut StateChanges,
) {
    // Senders can fake the profile easily so we keep track of profiles that the
    // member set themselves to avoid having confusing profile changes when a
    // member gets kicked/banned.
    if event.state_key() == event.sender() {
        changes
            .profiles
            .entry(room_id.to_owned())
            .or_default()
            .insert(event.sender().to_owned(), event.into());
    }

    if *event.membership() == MembershipState::Invite {
        // Remove any profile previously stored for the invited user.
        //
        // A room member could have joined the room and left it later; in that case, the
        // server may return a dummy, empty profile along the `leave` event. We
        // don't want to reuse that empty profile when the member has been
        // re-invited, so we remove it from the database.
        changes
            .profiles_to_delete
            .entry(room_id.to_owned())
            .or_default()
            .push(event.state_key().clone());
    }
}

#[cfg(test)]
mod tests {
    use matrix_sdk_test::{
        async_test, ruma_response_from_json, sync_timeline_event, InvitedRoomBuilder,
        LeftRoomBuilder, StateTestEvent, StrippedStateTestEvent, SyncResponseBuilder,
    };
    use ruma::{api::client as api, room_id, serde::Raw, user_id, UserId};
    use serde_json::{json, value::to_raw_value};

    use super::BaseClient;
    use crate::{
        store::StateStoreExt, test_utils::logged_in_base_client, DisplayName, RoomState,
        SessionMeta,
    };

    #[async_test]
    async fn test_invite_after_leaving() {
        let user_id = user_id!("@alice:example.org");
        let room_id = room_id!("!test:example.org");

        let client = logged_in_base_client(Some(user_id)).await;

        let mut sync_builder = SyncResponseBuilder::new();

        let response = sync_builder
            .add_left_room(LeftRoomBuilder::new(room_id).add_timeline_event(sync_timeline_event!({
                "content": {
                    "displayname": "Alice",
                    "membership": "left",
                },
                "event_id": "$994173582443PhrSn:example.org",
                "origin_server_ts": 1432135524678u64,
                "sender": user_id,
                "state_key": user_id,
                "type": "m.room.member",
            })))
            .build_sync_response();
        client.receive_sync_response(response).await.unwrap();
        assert_eq!(client.get_room(room_id).unwrap().state(), RoomState::Left);

        let response = sync_builder
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
    async fn test_invite_displayname() {
        let user_id = user_id!("@alice:example.org");
        let room_id = room_id!("!ithpyNKDtmhneaTQja:example.org");

        let client = logged_in_base_client(Some(user_id)).await;

        let response = ruma_response_from_json(&json!({
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
        }));

        client.receive_sync_response(response).await.unwrap();

        let room = client.get_room(room_id).expect("Room not found");
        assert_eq!(room.state(), RoomState::Invited);
        assert_eq!(
            room.compute_display_name().await.expect("fetching display name failed"),
            DisplayName::Calculated("Kyra".to_owned())
        );
    }

    #[cfg(all(feature = "e2e-encryption", feature = "experimental-sliding-sync"))]
    #[async_test]
    async fn test_when_there_are_no_latest_encrypted_events_decrypting_them_does_nothing() {
        use std::collections::BTreeMap;

        use crate::{rooms::normal::RoomInfoNotableUpdateReasons, StateChanges};

        // Given a room
        let user_id = user_id!("@u:u.to");
        let room_id = room_id!("!r:u.to");
        let client = logged_in_base_client(Some(user_id)).await;
        let room = process_room_join_test_helper(&client, room_id, "$1", user_id).await;

        // Sanity: it has no latest_encrypted_events or latest_event
        assert!(room.latest_encrypted_events().is_empty());
        assert!(room.latest_event().is_none());

        // When I tell it to do some decryption
        let mut changes = StateChanges::default();
        let mut room_info_notable_updates = BTreeMap::new();
        client.decrypt_latest_events(&room, &mut changes, &mut room_info_notable_updates).await;

        // Then nothing changed
        assert!(room.latest_encrypted_events().is_empty());
        assert!(room.latest_event().is_none());
        assert!(changes.room_infos.is_empty());
        assert!(!room_info_notable_updates
            .get(room_id)
            .copied()
            .unwrap_or_default()
            .contains(RoomInfoNotableUpdateReasons::LATEST_EVENT));
    }

    // TODO: I wanted to write more tests here for decrypt_latest_events but I got
    // lost trying to set up my OlmMachine to be able to encrypt and decrypt
    // events. In the meantime, there are tests for the most difficult logic
    // inside Room.  --andyb

    #[cfg(feature = "e2e-encryption")]
    async fn process_room_join_test_helper(
        client: &BaseClient,
        room_id: &ruma::RoomId,
        event_id: &str,
        user_id: &UserId,
    ) -> crate::Room {
        let mut sync_builder = SyncResponseBuilder::new();
        let response = sync_builder
            .add_joined_room(matrix_sdk_test::JoinedRoomBuilder::new(room_id).add_timeline_event(
                sync_timeline_event!({
                    "content": {
                        "displayname": "Alice",
                        "membership": "join",
                    },
                    "event_id": event_id,
                    "origin_server_ts": 1432135524678u64,
                    "sender": user_id,
                    "state_key": user_id,
                    "type": "m.room.member",
                }),
            ))
            .build_sync_response();

        client.receive_sync_response(response).await.unwrap();

        client.get_room(room_id).expect("Just-created room not found!")
    }

    #[async_test]
    async fn test_deserialization_failure() {
        let user_id = user_id!("@alice:example.org");
        let room_id = room_id!("!ithpyNKDtmhneaTQja:example.org");

        let client = BaseClient::new();
        client
            .set_session_meta(
                SessionMeta { user_id: user_id.to_owned(), device_id: "FOOBAR".into() },
                #[cfg(feature = "e2e-encryption")]
                None,
            )
            .await
            .unwrap();

        let response = ruma_response_from_json(&json!({
            "next_batch": "asdkl;fjasdkl;fj;asdkl;f",
            "rooms": {
                "join": {
                    "!ithpyNKDtmhneaTQja:example.org": {
                        "state": {
                            "events": [
                                {
                                    "invalid": "invalid",
                                },
                                {
                                    "content": {
                                        "name": "The room name"
                                    },
                                    "event_id": "$143273582443PhrSn:example.org",
                                    "origin_server_ts": 1432735824653u64,
                                    "room_id": "!jEsUZKDJdhlrceRyVU:example.org",
                                    "sender": "@example:example.org",
                                    "state_key": "",
                                    "type": "m.room.name",
                                    "unsigned": {
                                        "age": 1234
                                    }
                                },
                            ]
                        }
                    }
                }
            }
        }));

        client.receive_sync_response(response).await.unwrap();
        client
            .store()
            .get_state_event_static::<ruma::events::room::name::RoomNameEventContent>(room_id)
            .await
            .expect("Failed to fetch state event")
            .expect("State event not found")
            .deserialize()
            .expect("Failed to deserialize state event");
    }

    #[async_test]
    async fn test_invited_members_arent_ignored() {
        let user_id = user_id!("@alice:example.org");
        let inviter_user_id = user_id!("@bob:example.org");
        let room_id = room_id!("!ithpyNKDtmhneaTQja:example.org");

        let client = BaseClient::new();
        client
            .set_session_meta(
                SessionMeta { user_id: user_id.to_owned(), device_id: "FOOBAR".into() },
                #[cfg(feature = "e2e-encryption")]
                None,
            )
            .await
            .unwrap();

        // Preamble: let the SDK know about the room.
        let mut sync_builder = SyncResponseBuilder::new();
        let response = sync_builder
            .add_joined_room(matrix_sdk_test::JoinedRoomBuilder::new(room_id))
            .build_sync_response();
        client.receive_sync_response(response).await.unwrap();

        // When I process the result of a /members request that only contains an invited
        // member,
        let request = api::membership::get_member_events::v3::Request::new(room_id.to_owned());

        let raw_member_event = json!({
            "content": {
                "avatar_url": "mxc://localhost/fewjilfewjil42",
                "displayname": "Invited Alice",
                "membership": "invite"
            },
            "event_id": "$151800140517rfvjc:localhost",
            "origin_server_ts": 151800140,
            "room_id": room_id,
            "sender": inviter_user_id,
            "state_key": user_id,
            "type": "m.room.member",
            "unsigned": {
                "age": 13374242,
            }
        });
        let response = api::membership::get_member_events::v3::Response::new(vec![Raw::from_json(
            to_raw_value(&raw_member_event).unwrap(),
        )]);

        // It's correctly processed,
        client.receive_all_members(room_id, &request, &response).await.unwrap();

        let room = client.get_room(room_id).unwrap();

        // And I can get the invited member display name and avatar.
        let member = room.get_member(user_id).await.expect("ok").expect("exists");

        assert_eq!(member.user_id(), user_id);
        assert_eq!(member.display_name().unwrap(), "Invited Alice");
        assert_eq!(member.avatar_url().unwrap().to_string(), "mxc://localhost/fewjilfewjil42");
    }

    #[async_test]
    async fn test_reinvited_members_get_a_display_name() {
        let user_id = user_id!("@alice:example.org");
        let inviter_user_id = user_id!("@bob:example.org");
        let room_id = room_id!("!ithpyNKDtmhneaTQja:example.org");

        let client = BaseClient::new();
        client
            .set_session_meta(
                SessionMeta { user_id: user_id.to_owned(), device_id: "FOOBAR".into() },
                #[cfg(feature = "e2e-encryption")]
                None,
            )
            .await
            .unwrap();

        // Preamble: let the SDK know about the room, and that the invited user left it.
        let mut sync_builder = SyncResponseBuilder::new();
        let response = sync_builder
            .add_joined_room(matrix_sdk_test::JoinedRoomBuilder::new(room_id).add_state_event(
                StateTestEvent::Custom(json!({
                    "content": {
                        "avatar_url": null,
                        "displayname": null,
                        "membership": "leave"
                    },
                    "event_id": "$151803140217rkvjc:localhost",
                    "origin_server_ts": 151800139,
                    "room_id": room_id,
                    "sender": user_id,
                    "state_key": user_id,
                    "type": "m.room.member",
                })),
            ))
            .build_sync_response();
        client.receive_sync_response(response).await.unwrap();

        // Now, say that the user has been re-invited.
        let request = api::membership::get_member_events::v3::Request::new(room_id.to_owned());

        let raw_member_event = json!({
            "content": {
                "avatar_url": "mxc://localhost/fewjilfewjil42",
                "displayname": "Invited Alice",
                "membership": "invite"
            },
            "event_id": "$151800140517rfvjc:localhost",
            "origin_server_ts": 151800140,
            "room_id": room_id,
            "sender": inviter_user_id,
            "state_key": user_id,
            "type": "m.room.member",
            "unsigned": {
                "age": 13374242,
            }
        });
        let response = api::membership::get_member_events::v3::Response::new(vec![Raw::from_json(
            to_raw_value(&raw_member_event).unwrap(),
        )]);

        // It's correctly processed,
        client.receive_all_members(room_id, &request, &response).await.unwrap();

        let room = client.get_room(room_id).unwrap();

        // And I can get the invited member display name and avatar.
        let member = room.get_member(user_id).await.expect("ok").expect("exists");

        assert_eq!(member.user_id(), user_id);
        assert_eq!(member.display_name().unwrap(), "Invited Alice");
        assert_eq!(member.avatar_url().unwrap().to_string(), "mxc://localhost/fewjilfewjil42");
    }
}
