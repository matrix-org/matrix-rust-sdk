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
use std::sync::Arc;
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    fmt,
    ops::Deref,
};

use eyeball::{SharedObservable, Subscriber};
use eyeball_im::{Vector, VectorDiff};
use futures_util::Stream;
use matrix_sdk_common::timer;
#[cfg(feature = "e2e-encryption")]
use matrix_sdk_crypto::{
    CollectStrategy, DecryptionSettings, EncryptionSettings, OlmError, OlmMachine,
    TrustRequirement, store::DynCryptoStore, types::requests::ToDeviceRequest,
};
#[cfg(doc)]
use ruma::DeviceId;
#[cfg(feature = "e2e-encryption")]
use ruma::events::room::{history_visibility::HistoryVisibility, member::MembershipState};
use ruma::{
    MilliSecondsSinceUnixEpoch, OwnedRoomId, OwnedUserId, RoomId, UserId,
    api::client::{self as api, sync::sync_events::v5},
    events::{
        StateEvent, StateEventType,
        ignored_user_list::IgnoredUserListEventContent,
        push_rules::{PushRulesEvent, PushRulesEventContent},
        room::member::SyncRoomMemberEvent,
    },
    push::Ruleset,
    time::Instant,
};
use tokio::sync::{Mutex, broadcast};
#[cfg(feature = "e2e-encryption")]
use tokio::sync::{RwLock, RwLockReadGuard};
use tracing::{Level, debug, enabled, info, instrument, warn};

#[cfg(feature = "e2e-encryption")]
use crate::RoomMemberships;
use crate::{
    InviteAcceptanceDetails, RoomStateFilter, SessionMeta,
    deserialized_responses::DisplayName,
    error::{Error, Result},
    event_cache::store::{EventCacheStoreLock, EventCacheStoreLockState},
    media::store::MediaStoreLock,
    response_processors::{self as processors, Context},
    room::{
        Room, RoomInfoNotableUpdate, RoomInfoNotableUpdateReasons, RoomMembersUpdate, RoomState,
    },
    store::{
        BaseStateStore, DynStateStore, MemoryStore, Result as StoreResult, RoomLoadSettings,
        StateChanges, StateStoreDataKey, StateStoreDataValue, StateStoreExt, StoreConfig,
        ambiguity_map::AmbiguityCache,
    },
    sync::{RoomUpdates, SyncResponse},
};

/// A no (network) IO client implementation.
///
/// This client is a state machine that receives responses and events and
/// accordingly updates its state. It is not designed to be used directly, but
/// rather through `matrix_sdk::Client`.
///
/// ```rust
/// use matrix_sdk_base::{BaseClient, ThreadingSupport, store::StoreConfig};
///
/// let client = BaseClient::new(
///     StoreConfig::new("cross-process-holder-name".to_owned()),
///     ThreadingSupport::Disabled,
/// );
/// ```
#[derive(Clone)]
pub struct BaseClient {
    /// The state store.
    pub(crate) state_store: BaseStateStore,

    /// The store used by the event cache.
    event_cache_store: EventCacheStoreLock,

    /// The store used by the media cache.
    media_store: MediaStoreLock,

    /// The store used for encryption.
    ///
    /// This field is only meant to be used for `OlmMachine` initialization.
    /// All operations on it happen inside the `OlmMachine`.
    #[cfg(feature = "e2e-encryption")]
    crypto_store: Arc<DynCryptoStore>,

    /// The olm-machine that is created once the
    /// [`SessionMeta`][crate::session::SessionMeta] is set via
    /// [`BaseClient::activate`]
    #[cfg(feature = "e2e-encryption")]
    olm_machine: Arc<RwLock<Option<OlmMachine>>>,

    /// Observable of when a user is ignored/unignored.
    pub(crate) ignore_user_list_changes: SharedObservable<Vec<String>>,

    /// A sender that is used to communicate changes to room information. Each
    /// tick contains the room ID and the reasons that have generated this tick.
    pub(crate) room_info_notable_update_sender: broadcast::Sender<RoomInfoNotableUpdate>,

    /// The strategy to use for picking recipient devices, when sending an
    /// encrypted message.
    #[cfg(feature = "e2e-encryption")]
    pub room_key_recipient_strategy: CollectStrategy,

    /// The settings to use for decrypting events.
    #[cfg(feature = "e2e-encryption")]
    pub decryption_settings: DecryptionSettings,

    /// If the client should handle verification events received when syncing.
    #[cfg(feature = "e2e-encryption")]
    pub handle_verification_events: bool,

    /// Whether the client supports threads or not.
    pub threading_support: ThreadingSupport,
}

#[cfg(not(tarpaulin_include))]
impl fmt::Debug for BaseClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BaseClient")
            .field("session_meta", &self.state_store.session_meta())
            .field("sync_token", &self.state_store.sync_token)
            .finish_non_exhaustive()
    }
}

/// Whether this client instance supports threading or not. Currently used to
/// determine how the client handles read receipts and unread count computations
/// on the base SDK level.
///
/// Timelines on the other hand have a separate `TimelineFocus`
/// `hide_threaded_events` associated value that can be used to hide threaded
/// events but also to enable threaded read receipt sending. This is because
/// certain timeline instances should ignore threading no matter what's defined
/// at the client level. One such example are media filtered timelines which
/// should contain all the room's media no matter what thread its in (unless
/// explicitly opted into).
#[derive(Clone, Copy, Debug)]
pub enum ThreadingSupport {
    /// Threading enabled.
    Enabled {
        /// Enable client-wide thread subscriptions support (MSC4306 / MSC4308).
        ///
        /// This may cause filtering out of thread subscriptions, and loading
        /// the thread subscriptions via the sliding sync extension,
        /// when the room list service is being used.
        with_subscriptions: bool,
    },
    /// Threading disabled.
    Disabled,
}

impl BaseClient {
    /// Create a new client.
    ///
    /// # Arguments
    ///
    /// * `config` - the configuration for the stores (state store, event cache
    ///   store and crypto store).
    pub fn new(config: StoreConfig, threading_support: ThreadingSupport) -> Self {
        let store = BaseStateStore::new(config.state_store);

        // Create the channel to receive `RoomInfoNotableUpdate`.
        //
        // Let's consider the channel will receive 5 updates for 100 rooms maximum. This
        // is unrealistic in practise, as the sync mechanism is pretty unlikely to
        // trigger such amount of updates, it's a safe value.
        //
        // Also, note that it must not be
        // zero, because (i) it will panic, (ii) a new user has no room, but can create
        // rooms; remember that the channel's capacity is immutable.
        let (room_info_notable_update_sender, _room_info_notable_update_receiver) =
            broadcast::channel(500);

        BaseClient {
            state_store: store,
            event_cache_store: config.event_cache_store,
            media_store: config.media_store,
            #[cfg(feature = "e2e-encryption")]
            crypto_store: config.crypto_store,
            #[cfg(feature = "e2e-encryption")]
            olm_machine: Default::default(),
            ignore_user_list_changes: Default::default(),
            room_info_notable_update_sender,
            #[cfg(feature = "e2e-encryption")]
            room_key_recipient_strategy: Default::default(),
            #[cfg(feature = "e2e-encryption")]
            decryption_settings: DecryptionSettings {
                sender_device_trust_requirement: TrustRequirement::Untrusted,
            },
            #[cfg(feature = "e2e-encryption")]
            handle_verification_events: true,
            threading_support,
        }
    }

    /// Clones the current base client to use the same crypto store but a
    /// different, in-memory store config, and resets transient state.
    #[cfg(feature = "e2e-encryption")]
    pub async fn clone_with_in_memory_state_store(
        &self,
        cross_process_store_locks_holder_name: &str,
        handle_verification_events: bool,
    ) -> Result<Self> {
        let config = StoreConfig::new(cross_process_store_locks_holder_name.to_owned())
            .state_store(MemoryStore::new());
        let config = config.crypto_store(self.crypto_store.clone());

        let copy = Self {
            state_store: BaseStateStore::new(config.state_store),
            event_cache_store: config.event_cache_store,
            media_store: config.media_store,
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
            decryption_settings: self.decryption_settings.clone(),
            handle_verification_events,
            threading_support: self.threading_support,
        };

        copy.state_store
            .derive_from_other(&self.state_store, &copy.room_info_notable_update_sender)
            .await?;

        Ok(copy)
    }

    /// Clones the current base client to use the same crypto store but a
    /// different, in-memory store config, and resets transient state.
    #[cfg(not(feature = "e2e-encryption"))]
    #[allow(clippy::unused_async)]
    pub async fn clone_with_in_memory_state_store(
        &self,
        cross_process_store_locks_holder: &str,
        _handle_verification_events: bool,
    ) -> Result<Self> {
        let config = StoreConfig::new(cross_process_store_locks_holder.to_owned())
            .state_store(MemoryStore::new());
        Ok(Self::new(config, ThreadingSupport::Disabled))
    }

    /// Get the session meta information.
    ///
    /// If the client is currently logged in, this will return a
    /// [`SessionMeta`] object which contains the user ID and device ID.
    /// Otherwise it returns `None`.
    pub fn session_meta(&self) -> Option<&SessionMeta> {
        self.state_store.session_meta()
    }

    /// Get all the rooms this client knows about.
    pub fn rooms(&self) -> Vec<Room> {
        self.state_store.rooms()
    }

    /// Get all the rooms this client knows about, filtered by room state.
    pub fn rooms_filtered(&self, filter: RoomStateFilter) -> Vec<Room> {
        self.state_store.rooms_filtered(filter)
    }

    /// Get a stream of all the rooms changes, in addition to the existing
    /// rooms.
    pub fn rooms_stream(
        &self,
    ) -> (Vector<Room>, impl Stream<Item = Vec<VectorDiff<Room>>> + use<>) {
        self.state_store.rooms_stream()
    }

    /// Lookup the Room for the given RoomId, or create one, if it didn't exist
    /// yet in the store
    pub fn get_or_create_room(&self, room_id: &RoomId, room_state: RoomState) -> Room {
        self.state_store.get_or_create_room(
            room_id,
            room_state,
            self.room_info_notable_update_sender.clone(),
        )
    }

    /// Get a reference to the state store.
    pub fn state_store(&self) -> &DynStateStore {
        self.state_store.deref()
    }

    /// Get a reference to the event cache store.
    pub fn event_cache_store(&self) -> &EventCacheStoreLock {
        &self.event_cache_store
    }

    /// Get a reference to the media store.
    pub fn media_store(&self) -> &MediaStoreLock {
        &self.media_store
    }

    /// Check whether the client has been activated.
    ///
    /// See [`BaseClient::activate`] to know what it means.
    pub fn is_active(&self) -> bool {
        self.state_store.session_meta().is_some()
    }

    /// Activate the client.
    ///
    /// A client is considered active when:
    ///
    /// 1. It has a `SessionMeta` (user ID, device ID and access token),
    /// 2. Has loaded cached data from storage,
    /// 3. If encryption is enabled, it also initialized or restored its
    ///    `OlmMachine`.
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
    /// * `room_load_settings` — Specify how many rooms must be restored; use
    ///   `::default()` if you don't know which value to pick.
    ///
    /// # Panics
    ///
    /// This method panics if it is called twice.
    ///
    /// [`UserId`]: ruma::UserId
    pub async fn activate(
        &self,
        session_meta: SessionMeta,
        room_load_settings: RoomLoadSettings,
        #[cfg(feature = "e2e-encryption")] custom_account: Option<
            crate::crypto::vodozemac::olm::Account,
        >,
    ) -> Result<()> {
        debug!(user_id = ?session_meta.user_id, device_id = ?session_meta.device_id, "Activating the client");

        self.state_store
            .load_rooms(
                &session_meta.user_id,
                room_load_settings,
                &self.room_info_notable_update_sender,
            )
            .await?;
        self.state_store.load_sync_token().await?;
        self.state_store.set_session_meta(session_meta);

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
        self.state_store.sync_token.read().await.clone()
    }

    /// User has knocked on a room.
    ///
    /// Update the internal and cached state accordingly. Return the final Room.
    pub async fn room_knocked(&self, room_id: &RoomId) -> Result<Room> {
        let room = self.state_store.get_or_create_room(
            room_id,
            RoomState::Knocked,
            self.room_info_notable_update_sender.clone(),
        );

        if room.state() != RoomState::Knocked {
            let _state_store_lock = self.state_store_lock().lock().await;

            let mut room_info = room.clone_info();
            room_info.mark_as_knocked();
            room_info.mark_state_partially_synced();
            room_info.mark_members_missing(); // the own member event changed
            let mut changes = StateChanges::default();
            changes.add_room(room_info.clone());
            self.state_store.save_changes(&changes).await?; // Update the store
            room.set_room_info(room_info, RoomInfoNotableUpdateReasons::MEMBERSHIP);
        }

        Ok(room)
    }

    /// The user has joined a room using this specific client.
    ///
    /// This method should be called if the user accepts an invite or if they
    /// join a public room.
    ///
    /// The method will create a [`Room`] object if one does not exist yet and
    /// set the state of the [`Room`] to [`RoomState::Joined`]. The [`Room`]
    /// object will be persisted in the cache. Please note that the [`Room`]
    /// will be a stub until a sync has been received with the full room
    /// state using [`BaseClient::receive_sync_response`].
    ///
    /// Update the internal and cached state accordingly. Return the final Room.
    ///
    /// # Arguments
    ///
    /// * `room_id` - The unique ID identifying the joined room.
    /// * `inviter` - When joining this room in response to an invitation, the
    ///   inviter should be recorded before sending the join request to the
    ///   server. Providing the inviter here ensures that the
    ///   [`InviteAcceptanceDetails`] are stored for this room.
    ///
    /// # Examples
    ///
    /// ```rust
    /// # use matrix_sdk_base::{BaseClient, store::StoreConfig, RoomState, ThreadingSupport};
    /// # use ruma::{OwnedRoomId, OwnedUserId, RoomId};
    /// # async {
    /// # let client = BaseClient::new(StoreConfig::new("example".to_owned()), ThreadingSupport::Disabled);
    /// # async fn send_join_request() -> anyhow::Result<OwnedRoomId> { todo!() }
    /// # async fn maybe_get_inviter(room_id: &RoomId) -> anyhow::Result<Option<OwnedUserId>> { todo!() }
    /// # let room_id: &RoomId = todo!();
    /// let maybe_inviter = maybe_get_inviter(room_id).await?;
    /// let room_id = send_join_request().await?;
    /// let room = client.room_joined(&room_id, maybe_inviter).await?;
    ///
    /// assert_eq!(room.state(), RoomState::Joined);
    /// # matrix_sdk_test::TestResult::Ok(()) };
    /// ```
    pub async fn room_joined(
        &self,
        room_id: &RoomId,
        inviter: Option<OwnedUserId>,
    ) -> Result<Room> {
        let room = self.state_store.get_or_create_room(
            room_id,
            RoomState::Joined,
            self.room_info_notable_update_sender.clone(),
        );

        // If the state isn't `RoomState::Joined` then this means that we knew about
        // this room before. Let's modify the existing state now.
        if room.state() != RoomState::Joined {
            let _state_store_lock = self.state_store_lock().lock().await;

            let mut room_info = room.clone_info();
            let previous_state = room.state();

            room_info.mark_as_joined();
            room_info.mark_state_partially_synced();
            room_info.mark_members_missing(); // the own member event changed

            // If our previous state was an invite and we're now in the joined state, this
            // means that the user has explicitly accepted an invite. Let's
            // remember some details about the invite.
            //
            // This is somewhat of a workaround for our lack of cryptographic membership.
            // Later on we will decide if historic room keys should be accepted
            // based on this info. If a user has accepted an invite and we receive a room
            // key bundle shortly after, we might accept it. If we don't do
            // this, the homeserver could trick us into accepting any historic room key
            // bundle.
            if previous_state == RoomState::Invited
                && let Some(inviter) = inviter
            {
                let details = InviteAcceptanceDetails {
                    invite_accepted_at: MilliSecondsSinceUnixEpoch::now(),
                    inviter,
                };
                room_info.set_invite_acceptance_details(details);
            }

            let mut changes = StateChanges::default();
            changes.add_room(room_info.clone());

            self.state_store.save_changes(&changes).await?; // Update the store

            room.set_room_info(room_info, RoomInfoNotableUpdateReasons::MEMBERSHIP);
        }

        Ok(room)
    }

    /// User has left a room.
    ///
    /// Update the internal and cached state accordingly.
    pub async fn room_left(&self, room_id: &RoomId) -> Result<()> {
        let room = self.state_store.get_or_create_room(
            room_id,
            RoomState::Left,
            self.room_info_notable_update_sender.clone(),
        );

        if room.state() != RoomState::Left {
            let _state_store_lock = self.state_store_lock().lock().await;

            let mut room_info = room.clone_info();
            room_info.mark_as_left();
            room_info.mark_state_partially_synced();
            room_info.mark_members_missing(); // the own member event changed
            let mut changes = StateChanges::default();
            changes.add_room(room_info.clone());
            self.state_store.save_changes(&changes).await?; // Update the store
            room.set_room_info(room_info, RoomInfoNotableUpdateReasons::MEMBERSHIP);
        }

        Ok(())
    }

    /// Get a lock to the state store, with an exclusive access.
    ///
    /// It doesn't give an access to the state store itself. It's rather a lock
    /// to synchronise all accesses to the state store.
    pub fn state_store_lock(&self) -> &Mutex<()> {
        self.state_store.lock()
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
        self.receive_sync_response_with_requested_required_states(
            response,
            &RequestedRequiredStates::default(),
        )
        .await
    }

    /// Receive a response from a sync call, with the requested required state
    /// events.
    ///
    /// # Arguments
    ///
    /// * `response` - The response that we received after a successful sync.
    /// * `requested_required_states` - The requested required state events.
    pub async fn receive_sync_response_with_requested_required_states(
        &self,
        response: api::sync::sync_events::v3::Response,
        requested_required_states: &RequestedRequiredStates,
    ) -> Result<SyncResponse> {
        // The server might respond multiple times with the same sync token, in
        // that case we already received this response and there's nothing to
        // do.
        if self.state_store.sync_token.read().await.as_ref() == Some(&response.next_batch) {
            info!("Got the same sync response twice");
            return Ok(SyncResponse::default());
        }

        let now = if enabled!(Level::INFO) { Some(Instant::now()) } else { None };

        #[cfg(feature = "e2e-encryption")]
        let olm_machine = self.olm_machine().await;

        let mut context = Context::new(StateChanges::new(response.next_batch.clone()));

        #[cfg(feature = "e2e-encryption")]
        let processors::e2ee::to_device::Output { processed_to_device_events: to_device } =
            processors::e2ee::to_device::from_sync_v2(
                &response,
                olm_machine.as_ref(),
                &self.decryption_settings,
            )
            .await?;

        #[cfg(not(feature = "e2e-encryption"))]
        let to_device = response
            .to_device
            .events
            .into_iter()
            .map(|raw| {
                use matrix_sdk_common::deserialized_responses::{
                    ProcessedToDeviceEvent, ToDeviceUnableToDecryptInfo,
                    ToDeviceUnableToDecryptReason,
                };

                if let Ok(Some(event_type)) = raw.get_field::<String>("type") {
                    if event_type == "m.room.encrypted" {
                        ProcessedToDeviceEvent::UnableToDecrypt {
                            encrypted_event: raw,
                            utd_info: ToDeviceUnableToDecryptInfo {
                                reason: ToDeviceUnableToDecryptReason::EncryptionIsDisabled,
                            },
                        }
                    } else {
                        ProcessedToDeviceEvent::PlainText(raw)
                    }
                } else {
                    // Exclude events with no type
                    ProcessedToDeviceEvent::Invalid(raw)
                }
            })
            .collect();

        let mut ambiguity_cache = AmbiguityCache::new(self.state_store.inner.clone());

        let global_account_data_processor =
            processors::account_data::global(&response.account_data.events);

        let push_rules = self.get_push_rules(&global_account_data_processor).await?;

        let mut room_updates = RoomUpdates::default();
        let mut notifications = Default::default();

        let mut updated_members_in_room: BTreeMap<OwnedRoomId, BTreeSet<OwnedUserId>> =
            BTreeMap::new();

        for (room_id, joined_room) in response.rooms.join {
            let joined_room_update = processors::room::sync_v2::update_joined_room(
                &mut context,
                processors::room::RoomCreationData::new(
                    &room_id,
                    self.room_info_notable_update_sender.clone(),
                    requested_required_states,
                    &mut ambiguity_cache,
                ),
                joined_room,
                &mut updated_members_in_room,
                processors::notification::Notification::new(
                    &push_rules,
                    &mut notifications,
                    &self.state_store,
                ),
                #[cfg(feature = "e2e-encryption")]
                processors::e2ee::E2EE::new(
                    olm_machine.as_ref(),
                    &self.decryption_settings,
                    self.handle_verification_events,
                ),
            )
            .await?;

            room_updates.joined.insert(room_id, joined_room_update);
        }

        for (room_id, left_room) in response.rooms.leave {
            let left_room_update = processors::room::sync_v2::update_left_room(
                &mut context,
                processors::room::RoomCreationData::new(
                    &room_id,
                    self.room_info_notable_update_sender.clone(),
                    requested_required_states,
                    &mut ambiguity_cache,
                ),
                left_room,
                processors::notification::Notification::new(
                    &push_rules,
                    &mut notifications,
                    &self.state_store,
                ),
                #[cfg(feature = "e2e-encryption")]
                processors::e2ee::E2EE::new(
                    olm_machine.as_ref(),
                    &self.decryption_settings,
                    self.handle_verification_events,
                ),
            )
            .await?;

            room_updates.left.insert(room_id, left_room_update);
        }

        for (room_id, invited_room) in response.rooms.invite {
            let invited_room_update = processors::room::sync_v2::update_invited_room(
                &mut context,
                &room_id,
                invited_room,
                self.room_info_notable_update_sender.clone(),
                processors::notification::Notification::new(
                    &push_rules,
                    &mut notifications,
                    &self.state_store,
                ),
            )
            .await?;

            room_updates.invited.insert(room_id, invited_room_update);
        }

        for (room_id, knocked_room) in response.rooms.knock {
            let knocked_room_update = processors::room::sync_v2::update_knocked_room(
                &mut context,
                &room_id,
                knocked_room,
                self.room_info_notable_update_sender.clone(),
                processors::notification::Notification::new(
                    &push_rules,
                    &mut notifications,
                    &self.state_store,
                ),
            )
            .await?;

            room_updates.knocked.insert(room_id, knocked_room_update);
        }

        global_account_data_processor.apply(&mut context, &self.state_store).await;

        context.state_changes.presence = response
            .presence
            .events
            .iter()
            .filter_map(|e| {
                let event = e.deserialize().ok()?;
                Some((event.sender, e.clone()))
            })
            .collect();

        context.state_changes.ambiguity_maps = ambiguity_cache.cache;

        {
            let _state_store_lock = self.state_store_lock().lock().await;

            processors::changes::save_and_apply(
                context,
                &self.state_store,
                &self.ignore_user_list_changes,
                Some(response.next_batch.clone()),
            )
            .await?;
        }

        let mut context = Context::default();

        // Now that all the rooms information have been saved, update the display name
        // of the updated rooms (which relies on information stored in the database).
        processors::room::display_name::update_for_rooms(
            &mut context,
            &room_updates,
            &self.state_store,
        )
        .await;

        // Save the new display name updates if any.
        {
            let _state_store_lock = self.state_store_lock().lock().await;

            processors::changes::save_only(context, &self.state_store).await?;
        }

        for (room_id, member_ids) in updated_members_in_room {
            if let Some(room) = self.get_room(&room_id) {
                let _ =
                    room.room_member_updates_sender.send(RoomMembersUpdate::Partial(member_ids));
            }
        }

        if enabled!(Level::INFO) {
            info!("Processed a sync response in {:?}", now.map(|now| now.elapsed()));
        }

        let response = SyncResponse {
            rooms: room_updates,
            presence: response.presence.events,
            account_data: response.account_data.events,
            to_device,
            notifications,
        };

        Ok(response)
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

        let Some(room) = self.state_store.room(room_id) else {
            // The room is unknown to us: leave early.
            return Ok(());
        };

        let mut chunk = Vec::with_capacity(response.chunk.len());
        let mut context = Context::default();

        #[cfg(feature = "e2e-encryption")]
        let mut user_ids = BTreeSet::new();

        let mut ambiguity_map: HashMap<DisplayName, BTreeSet<OwnedUserId>> = Default::default();

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

            if let StateEvent::Original(e) = &member
                && let Some(d) = &e.content.displayname
            {
                let display_name = DisplayName::new(d);
                ambiguity_map.entry(display_name).or_default().insert(member.state_key().clone());
            }

            let sync_member: SyncRoomMemberEvent = member.clone().into();
            processors::profiles::upsert_or_delete(&mut context, room_id, &sync_member);

            context
                .state_changes
                .state
                .entry(room_id.to_owned())
                .or_default()
                .entry(member.event_type())
                .or_default()
                .insert(member.state_key().to_string(), raw_event.clone().cast());
            chunk.push(member);
        }

        #[cfg(feature = "e2e-encryption")]
        processors::e2ee::tracked_users::update(
            self.olm_machine().await.as_ref(),
            room.encryption_state(),
            &user_ids,
        )
        .await?;

        context.state_changes.ambiguity_maps.insert(room_id.to_owned(), ambiguity_map);

        {
            let _state_store_lock = self.state_store_lock().lock().await;

            let mut room_info = room.clone_info();
            room_info.mark_members_synced();
            context.state_changes.add_room(room_info);

            processors::changes::save_and_apply(
                context,
                &self.state_store,
                &self.ignore_user_list_changes,
                None,
            )
            .await?;
        }

        let _ = room.room_member_updates_sender.send(RoomMembersUpdate::FullReload);

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
            .state_store
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
            .state_store
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
                let Some(room) = self.get_room(room_id) else {
                    return Err(Error::InsufficientData);
                };

                let history_visibility = room.history_visibility_or_default();
                let Some(room_encryption_event) = room.encryption_settings() else {
                    return Err(Error::EncryptionNotEnabled);
                };

                // Don't share the group session with members that are invited
                // if the history visibility is set to `Joined`
                let filter = if history_visibility == HistoryVisibility::Joined {
                    RoomMemberships::JOIN
                } else {
                    RoomMemberships::ACTIVE
                };

                let members = self.state_store.get_user_ids(room_id, filter).await?;

                let settings = EncryptionSettings::new(
                    room_encryption_event,
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
        self.state_store.room(room_id)
    }

    /// Forget the room with the given room ID.
    ///
    /// The room will be dropped from the room list and the store.
    ///
    /// # Arguments
    ///
    /// * `room_id` - The id of the room that should be forgotten.
    pub async fn forget_room(&self, room_id: &RoomId) -> Result<()> {
        // Forget the room in the state store.
        self.state_store.forget_room(room_id).await?;

        // Remove the room in the event cache store too.
        match self.event_cache_store().lock().await? {
            // If the lock is clear, we can do the operation as expected.
            // If the lock is dirty, we can ignore to refresh the state, we just need to remove a
            // room. Also, we must not mark the lock as non-dirty because other operations may be
            // critical and may need to refresh the `EventCache`' state.
            EventCacheStoreLockState::Clean(guard) | EventCacheStoreLockState::Dirty(guard) => {
                guard.remove_room(room_id).await?
            }
        }

        Ok(())
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
        global_account_data_processor: &processors::account_data::Global,
    ) -> Result<Ruleset> {
        let _timer = timer!(Level::TRACE, "get_push_rules");
        if let Some(event) = global_account_data_processor
            .push_rules()
            .and_then(|ev| ev.deserialize_as_unchecked::<PushRulesEvent>().ok())
        {
            Ok(event.content.global)
        } else if let Some(event) = self
            .state_store
            .get_account_data_event_static::<PushRulesEventContent>()
            .await?
            .and_then(|ev| ev.deserialize().ok())
        {
            Ok(event.content.global)
        } else if let Some(session_meta) = self.state_store.session_meta() {
            Ok(Ruleset::server_default(&session_meta.user_id))
        } else {
            Ok(Ruleset::new())
        }
    }

    /// Returns a subscriber that publishes an event every time the ignore user
    /// list changes
    pub fn subscribe_to_ignore_user_list_changes(&self) -> Subscriber<Vec<String>> {
        self.ignore_user_list_changes.subscribe()
    }

    /// Returns a new receiver that gets future room info notable updates.
    ///
    /// Learn more by reading the [`RoomInfoNotableUpdate`] type.
    pub fn room_info_notable_update_receiver(&self) -> broadcast::Receiver<RoomInfoNotableUpdate> {
        self.room_info_notable_update_sender.subscribe()
    }

    /// Checks whether the provided `user_id` belongs to an ignored user.
    pub async fn is_user_ignored(&self, user_id: &UserId) -> bool {
        match self.state_store.get_account_data_event_static::<IgnoredUserListEventContent>().await
        {
            Ok(Some(raw_ignored_user_list)) => match raw_ignored_user_list.deserialize() {
                Ok(current_ignored_user_list) => {
                    current_ignored_user_list.content.ignored_users.contains_key(user_id)
                }
                Err(error) => {
                    warn!(?error, "Failed to deserialize the ignored user list event");
                    false
                }
            },
            Ok(None) => false,
            Err(error) => {
                warn!(?error, "Could not get the ignored user list from the state store");
                false
            }
        }
    }
}

/// Represent the `required_state` values sent by a sync request.
///
/// This is useful to track what state events have been requested when handling
/// a response.
///
/// For example, if a sync requests the `m.room.encryption` state event, and the
/// server replies with nothing, if means the room **is not** encrypted. Without
/// knowing which state event was required by the sync, it is impossible to
/// interpret the absence of state event from the server as _the room's
/// encryption state is **not encrypted**_ or _the room's encryption state is
/// **unknown**_.
#[derive(Debug, Default)]
pub struct RequestedRequiredStates {
    default: Vec<(StateEventType, String)>,
    for_rooms: HashMap<OwnedRoomId, Vec<(StateEventType, String)>>,
}

impl RequestedRequiredStates {
    /// Create a new `RequestedRequiredStates`.
    ///
    /// `default` represents the `required_state` value for all rooms.
    /// `for_rooms` is the `required_state` per room.
    pub fn new(
        default: Vec<(StateEventType, String)>,
        for_rooms: HashMap<OwnedRoomId, Vec<(StateEventType, String)>>,
    ) -> Self {
        Self { default, for_rooms }
    }

    /// Get the `required_state` value for a specific room.
    pub fn for_room(&self, room_id: &RoomId) -> &[(StateEventType, String)] {
        self.for_rooms.get(room_id).unwrap_or(&self.default)
    }
}

impl From<&v5::Request> for RequestedRequiredStates {
    fn from(request: &v5::Request) -> Self {
        // The following information is missing in the MSC4186 at the time of writing
        // (2025-03-12) but: the `required_state`s from all lists and from all room
        // subscriptions are combined by doing an union.
        //
        // Thus, we can do the same here, put the union in `default` and keep
        // `for_rooms` empty. The `Self::for_room` will automatically do the fallback.
        let mut default = BTreeSet::new();

        for list in request.lists.values() {
            default.extend(BTreeSet::from_iter(list.room_details.required_state.iter().cloned()));
        }

        for room_subscription in request.room_subscriptions.values() {
            default.extend(BTreeSet::from_iter(room_subscription.required_state.iter().cloned()));
        }

        Self { default: default.into_iter().collect(), for_rooms: HashMap::new() }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use assert_matches2::{assert_let, assert_matches};
    use futures_util::FutureExt as _;
    use matrix_sdk_test::{
        BOB, InvitedRoomBuilder, LeftRoomBuilder, StateTestEvent, StrippedStateTestEvent,
        SyncResponseBuilder, async_test, event_factory::EventFactory, ruma_response_from_json,
    };
    use ruma::{
        api::client::{self as api, sync::sync_events::v5},
        event_id,
        events::{StateEventType, room::member::MembershipState},
        room_id,
        serde::Raw,
        user_id,
    };
    use serde_json::{json, value::to_raw_value};

    use super::{BaseClient, RequestedRequiredStates};
    use crate::{
        RoomDisplayName, RoomState, SessionMeta,
        client::ThreadingSupport,
        store::{RoomLoadSettings, StateStoreExt, StoreConfig},
        test_utils::logged_in_base_client,
    };

    #[test]
    fn test_requested_required_states() {
        let room_id_0 = room_id!("!r0");
        let room_id_1 = room_id!("!r1");

        let requested_required_states = RequestedRequiredStates::new(
            vec![(StateEventType::RoomAvatar, "".to_owned())],
            HashMap::from([(
                room_id_0.to_owned(),
                vec![
                    (StateEventType::RoomMember, "foo".to_owned()),
                    (StateEventType::RoomEncryption, "".to_owned()),
                ],
            )]),
        );

        // A special set of state events exists for `room_id_0`.
        assert_eq!(
            requested_required_states.for_room(room_id_0),
            &[
                (StateEventType::RoomMember, "foo".to_owned()),
                (StateEventType::RoomEncryption, "".to_owned()),
            ]
        );

        // No special list for `room_id_1`, it should return the defaults.
        assert_eq!(
            requested_required_states.for_room(room_id_1),
            &[(StateEventType::RoomAvatar, "".to_owned()),]
        );
    }

    #[test]
    fn test_requested_required_states_from_sync_v5_request() {
        let room_id_0 = room_id!("!r0");
        let room_id_1 = room_id!("!r1");

        // Empty request.
        let mut request = v5::Request::new();

        {
            let requested_required_states = RequestedRequiredStates::from(&request);

            assert!(requested_required_states.default.is_empty());
            assert!(requested_required_states.for_rooms.is_empty());
        }

        // One list.
        request.lists.insert("foo".to_owned(), {
            let mut list = v5::request::List::default();
            list.room_details.required_state = vec![
                (StateEventType::RoomAvatar, "".to_owned()),
                (StateEventType::RoomEncryption, "".to_owned()),
            ];

            list
        });

        {
            let requested_required_states = RequestedRequiredStates::from(&request);

            assert_eq!(
                requested_required_states.default,
                &[
                    (StateEventType::RoomAvatar, "".to_owned()),
                    (StateEventType::RoomEncryption, "".to_owned())
                ]
            );
            assert!(requested_required_states.for_rooms.is_empty());
        }

        // Two lists.
        request.lists.insert("bar".to_owned(), {
            let mut list = v5::request::List::default();
            list.room_details.required_state = vec![
                (StateEventType::RoomEncryption, "".to_owned()),
                (StateEventType::RoomName, "".to_owned()),
            ];

            list
        });

        {
            let requested_required_states = RequestedRequiredStates::from(&request);

            // Union of the state events.
            assert_eq!(
                requested_required_states.default,
                &[
                    (StateEventType::RoomAvatar, "".to_owned()),
                    (StateEventType::RoomEncryption, "".to_owned()),
                    (StateEventType::RoomName, "".to_owned()),
                ]
            );
            assert!(requested_required_states.for_rooms.is_empty());
        }

        // One room subscription.
        request.room_subscriptions.insert(room_id_0.to_owned(), {
            let mut room_subscription = v5::request::RoomSubscription::default();

            room_subscription.required_state = vec![
                (StateEventType::RoomJoinRules, "".to_owned()),
                (StateEventType::RoomEncryption, "".to_owned()),
            ];

            room_subscription
        });

        {
            let requested_required_states = RequestedRequiredStates::from(&request);

            // Union of state events, all in `default`, still nothing in `for_rooms`.
            assert_eq!(
                requested_required_states.default,
                &[
                    (StateEventType::RoomAvatar, "".to_owned()),
                    (StateEventType::RoomEncryption, "".to_owned()),
                    (StateEventType::RoomJoinRules, "".to_owned()),
                    (StateEventType::RoomName, "".to_owned()),
                ]
            );
            assert!(requested_required_states.for_rooms.is_empty());
        }

        // Two room subscriptions.
        request.room_subscriptions.insert(room_id_1.to_owned(), {
            let mut room_subscription = v5::request::RoomSubscription::default();

            room_subscription.required_state = vec![
                (StateEventType::RoomName, "".to_owned()),
                (StateEventType::RoomTopic, "".to_owned()),
            ];

            room_subscription
        });

        {
            let requested_required_states = RequestedRequiredStates::from(&request);

            // Union of state events, all in `default`, still nothing in `for_rooms`.
            assert_eq!(
                requested_required_states.default,
                &[
                    (StateEventType::RoomAvatar, "".to_owned()),
                    (StateEventType::RoomEncryption, "".to_owned()),
                    (StateEventType::RoomJoinRules, "".to_owned()),
                    (StateEventType::RoomName, "".to_owned()),
                    (StateEventType::RoomTopic, "".to_owned()),
                ]
            );
        }
    }

    #[async_test]
    async fn test_invite_after_leaving() {
        let user_id = user_id!("@alice:example.org");
        let room_id = room_id!("!test:example.org");

        let client = logged_in_base_client(Some(user_id)).await;

        let mut sync_builder = SyncResponseBuilder::new();

        let response = sync_builder
            .add_left_room(
                LeftRoomBuilder::new(room_id).add_timeline_event(
                    EventFactory::new()
                        .member(user_id)
                        .membership(MembershipState::Leave)
                        .display_name("Alice")
                        .event_id(event_id!("$994173582443PhrSn:example.org")),
                ),
            )
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
            room.compute_display_name().await.expect("fetching display name failed").into_inner(),
            RoomDisplayName::Calculated("Kyra".to_owned())
        );
    }

    #[async_test]
    async fn test_deserialization_failure() {
        let user_id = user_id!("@alice:example.org");
        let room_id = room_id!("!ithpyNKDtmhneaTQja:example.org");

        let client = BaseClient::new(
            StoreConfig::new("cross-process-store-locks-holder-name".to_owned()),
            ThreadingSupport::Disabled,
        );
        client
            .activate(
                SessionMeta { user_id: user_id.to_owned(), device_id: "FOOBAR".into() },
                RoomLoadSettings::default(),
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
            .state_store()
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

        let client = BaseClient::new(
            StoreConfig::new("cross-process-store-locks-holder-name".to_owned()),
            ThreadingSupport::Disabled,
        );
        client
            .activate(
                SessionMeta { user_id: user_id.to_owned(), device_id: "FOOBAR".into() },
                RoomLoadSettings::default(),
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

        let client = BaseClient::new(
            StoreConfig::new("cross-process-store-locks-holder-name".to_owned()),
            ThreadingSupport::Disabled,
        );
        client
            .activate(
                SessionMeta { user_id: user_id.to_owned(), device_id: "FOOBAR".into() },
                RoomLoadSettings::default(),
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

    #[async_test]
    async fn test_ignored_user_list_changes() {
        let user_id = user_id!("@alice:example.org");
        let client = BaseClient::new(
            StoreConfig::new("cross-process-store-locks-holder-name".to_owned()),
            ThreadingSupport::Disabled,
        );

        client
            .activate(
                SessionMeta { user_id: user_id.to_owned(), device_id: "FOOBAR".into() },
                RoomLoadSettings::default(),
                #[cfg(feature = "e2e-encryption")]
                None,
            )
            .await
            .unwrap();

        let mut subscriber = client.subscribe_to_ignore_user_list_changes();
        assert!(subscriber.next().now_or_never().is_none());

        let f = EventFactory::new();
        let mut sync_builder = SyncResponseBuilder::new();
        let response = sync_builder
            .add_global_account_data(f.ignored_user_list([(*BOB).into()]))
            .build_sync_response();
        client.receive_sync_response(response).await.unwrap();

        assert_let!(Some(ignored) = subscriber.next().await);
        assert_eq!(ignored, [BOB.to_string()]);

        // Receive the same response.
        let response = sync_builder
            .add_global_account_data(f.ignored_user_list([(*BOB).into()]))
            .build_sync_response();
        client.receive_sync_response(response).await.unwrap();

        // No changes in the ignored list.
        assert!(subscriber.next().now_or_never().is_none());

        // Now remove Bob from the ignored list.
        let response =
            sync_builder.add_global_account_data(f.ignored_user_list([])).build_sync_response();
        client.receive_sync_response(response).await.unwrap();

        assert_let!(Some(ignored) = subscriber.next().await);
        assert!(ignored.is_empty());
    }

    #[async_test]
    async fn test_is_user_ignored() {
        let ignored_user_id = user_id!("@alice:example.org");
        let client = logged_in_base_client(None).await;

        let mut sync_builder = SyncResponseBuilder::new();
        let f = EventFactory::new();
        let response = sync_builder
            .add_global_account_data(f.ignored_user_list([ignored_user_id.to_owned()]))
            .build_sync_response();
        client.receive_sync_response(response).await.unwrap();

        assert!(client.is_user_ignored(ignored_user_id).await);
    }

    #[async_test]
    async fn test_invite_details_are_set() {
        let user_id = user_id!("@alice:localhost");
        let client = logged_in_base_client(Some(user_id)).await;
        let invited_room_id = room_id!("!invited:localhost");
        let unknown_room_id = room_id!("!unknown:localhost");

        let mut sync_builder = SyncResponseBuilder::new();
        let response = sync_builder
            .add_invited_room(InvitedRoomBuilder::new(invited_room_id))
            .build_sync_response();
        client.receive_sync_response(response).await.unwrap();

        // Let us first check the initial state, we should have a room in the invite
        // state.
        let invited_room = client
            .get_room(invited_room_id)
            .expect("The sync should have created a room in the invited state");

        assert_eq!(invited_room.state(), RoomState::Invited);
        assert!(invited_room.invite_acceptance_details().is_none());

        // Now we join the room.
        let joined_room = client
            .room_joined(invited_room_id, Some(user_id.to_owned()))
            .await
            .expect("We should be able to mark a room as joined");

        // Yup, we now have some invite details.
        assert_eq!(joined_room.state(), RoomState::Joined);
        assert_matches!(joined_room.invite_acceptance_details(), Some(details));
        assert_eq!(details.inviter, user_id);

        // If we didn't know about the room before the join, we assume that there wasn't
        // an invite and we don't record the timestamp.
        assert!(client.get_room(unknown_room_id).is_none());
        let unknown_room = client
            .room_joined(unknown_room_id, Some(user_id.to_owned()))
            .await
            .expect("We should be able to mark a room as joined");

        assert_eq!(unknown_room.state(), RoomState::Joined);
        assert!(unknown_room.invite_acceptance_details().is_none());

        sync_builder.clear();
        let response =
            sync_builder.add_left_room(LeftRoomBuilder::new(invited_room_id)).build_sync_response();
        client.receive_sync_response(response).await.unwrap();

        // Now that we left the room, we shouldn't have any details anymore.
        let left_room = client
            .get_room(invited_room_id)
            .expect("The sync should have created a room in the invited state");

        assert_eq!(left_room.state(), RoomState::Left);
        assert!(left_room.invite_acceptance_details().is_none());
    }
}
