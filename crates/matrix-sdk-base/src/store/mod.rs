// Copyright 2021 The Matrix.org Foundation C.I.C.
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

//! The state store holds the overall state for rooms, users and their
//! profiles and their timelines. It is an overall cache for faster access
//! and convenience- accessible through `Store`.
//!
//! Implementing the `StateStore` trait, you can plug any storage backend
//! into the store for the actual storage. By default this brings an in-memory
//! store.

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    ops::Deref,
    pin::Pin,
    result::Result as StdResult,
    str::Utf8Error,
    sync::Arc,
};

use eyeball::{shared::Observable as SharedObservable, Subscriber};
use once_cell::sync::OnceCell;

#[cfg(any(test, feature = "testing"))]
#[macro_use]
pub mod integration_tests;
mod traits;

use dashmap::DashMap;
#[cfg(feature = "e2e-encryption")]
use matrix_sdk_crypto::store::{DynCryptoStore, IntoCryptoStore};
pub use matrix_sdk_store_encryption::Error as StoreEncryptionError;
use ruma::{
    api::client::push::get_notifications::v3::Notification,
    events::{
        presence::PresenceEvent,
        receipt::ReceiptEventContent,
        room::{member::StrippedRoomMemberEvent, redaction::OriginalSyncRoomRedactionEvent},
        AnyGlobalAccountDataEvent, AnyRoomAccountDataEvent, AnyStrippedStateEvent,
        AnySyncStateEvent, GlobalAccountDataEventType, RoomAccountDataEventType, StateEventType,
    },
    serde::Raw,
    EventId, OwnedEventId, OwnedRoomId, OwnedUserId, RoomId, UserId,
};
use tokio::sync::RwLock;

/// BoxStream of owned Types
pub type BoxStream<T> = Pin<Box<dyn futures_util::Stream<Item = T> + Send>>;

use crate::{
    rooms::{RoomInfo, RoomState},
    MinimalRoomMemberEvent, Room, Session, SessionMeta, SessionTokens,
};

pub(crate) mod ambiguity_map;
mod memory_store;

#[cfg(any(test, feature = "testing"))]
pub use self::integration_tests::StateStoreIntegrationTests;
pub use self::{
    memory_store::MemoryStore,
    traits::{
        DynStateStore, IntoStateStore, StateStore, StateStoreDataKey, StateStoreDataValue,
        StateStoreExt,
    },
};

/// State store specific error type.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// An error happened in the underlying database backend.
    #[error(transparent)]
    Backend(Box<dyn std::error::Error + Send + Sync>),
    /// An error happened while serializing or deserializing some data.
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    /// An error happened while deserializing a Matrix identifier, e.g. an user
    /// id.
    #[error(transparent)]
    Identifier(#[from] ruma::IdParseError),
    /// The store is locked with a passphrase and an incorrect passphrase was
    /// given.
    #[error("The store failed to be unlocked")]
    StoreLocked,
    /// An unencrypted store was tried to be unlocked with a passphrase.
    #[error("The store is not encrypted but was tried to be opened with a passphrase")]
    UnencryptedStore,
    /// The store failed to encrypt or decrypt some data.
    #[error("Error encrypting or decrypting data from the store: {0}")]
    Encryption(#[from] StoreEncryptionError),

    /// The store failed to encode or decode some data.
    #[error("Error encoding or decoding data from the store: {0}")]
    Codec(#[from] Utf8Error),

    /// The database format has changed in a backwards incompatible way.
    #[error(
        "The database format changed in an incompatible way, current \
        version: {0}, latest version: {1}"
    )]
    UnsupportedDatabaseVersion(usize, usize),
    /// Redacting an event in the store has failed.
    ///
    /// This should never happen.
    #[error("Redaction failed: {0}")]
    Redaction(#[source] ruma::canonical_json::RedactionError),
}

impl StoreError {
    /// Create a new [`Backend`][Self::Backend] error.
    ///
    /// Shorthand for `StoreError::Backend(Box::new(error))`.
    #[inline]
    pub fn backend<E>(error: E) -> Self
    where
        E: std::error::Error + Send + Sync + 'static,
    {
        Self::Backend(Box::new(error))
    }
}

/// A `StateStore` specific result type.
pub type Result<T, E = StoreError> = std::result::Result<T, E>;

/// A state store wrapper for the SDK.
///
/// This adds additional higher level store functionality on top of a
/// `StateStore` implementation.
#[derive(Clone)]
pub(crate) struct Store {
    pub(super) inner: Arc<DynStateStore>,
    session_meta: Arc<OnceCell<SessionMeta>>,
    pub(super) session_tokens: SharedObservable<Option<SessionTokens>>,
    /// The current sync token that should be used for the next sync call.
    pub(super) sync_token: Arc<RwLock<Option<String>>>,
    rooms: Arc<DashMap<OwnedRoomId, Room>>,
    stripped_rooms: Arc<DashMap<OwnedRoomId, Room>>,
    /// A lock to synchronize access to the store, such that data by the sync is
    /// never overwritten. The sync processing is supposed to use write access,
    /// such that only it is currently accessing the store overall. Other things
    /// might acquire read access, such that access to different rooms can be
    /// parallelized.
    sync_lock: Arc<RwLock<()>>,
}

impl Store {
    /// Create a new store, wrapping the given `StateStore`
    pub fn new(inner: Arc<DynStateStore>) -> Self {
        Self {
            inner,
            session_meta: Default::default(),
            session_tokens: Default::default(),
            sync_token: Default::default(),
            rooms: Default::default(),
            stripped_rooms: Default::default(),
            sync_lock: Default::default(),
        }
    }

    /// Get access to the syncing lock.
    pub fn sync_lock(&self) -> &RwLock<()> {
        &self.sync_lock
    }

    /// Set the meta of the session.
    ///
    /// Restores the state of this `Store` from the given `SessionMeta` and the
    /// inner `StateStore`.
    ///
    /// This method panics if it is called twice.
    pub async fn set_session_meta(&self, session_meta: SessionMeta) -> Result<()> {
        for info in self.inner.get_room_infos().await? {
            let room = Room::restore(&session_meta.user_id, self.inner.clone(), info);
            self.rooms.insert(room.room_id().to_owned(), room);
        }

        for info in self.inner.get_stripped_room_infos().await? {
            let room = Room::restore(&session_meta.user_id, self.inner.clone(), info);
            self.stripped_rooms.insert(room.room_id().to_owned(), room);
        }

        let token =
            self.get_kv_data(StateStoreDataKey::SyncToken).await?.and_then(|s| s.into_sync_token());
        *self.sync_token.write().await = token;

        self.session_meta.set(session_meta).expect("Session Meta was already set");

        Ok(())
    }

    /// The current [`SessionMeta`] containing our user ID and device ID.
    pub fn session_meta(&self) -> Option<&SessionMeta> {
        self.session_meta.get()
    }

    /// The [`SessionTokens`] containing our access token and optional refresh
    /// token.
    pub fn session_tokens(&self) -> Subscriber<Option<SessionTokens>> {
        self.session_tokens.subscribe()
    }

    /// Set the current [`SessionTokens`].
    pub fn set_session_tokens(&self, tokens: SessionTokens) {
        self.session_tokens.set(Some(tokens));
    }

    /// The current [`Session`] containing our user id, device ID, access
    /// token and optional refresh token.
    pub fn session(&self) -> Option<Session> {
        let meta = self.session_meta.get()?;
        let tokens = self.session_tokens().get()?;
        Some(Session::from_parts(meta.to_owned(), tokens))
    }

    /// Get all the rooms this store knows about.
    pub fn get_rooms(&self) -> Vec<Room> {
        self.rooms.iter().filter_map(|r| self.get_room(r.key())).collect()
    }

    /// Get the room with the given room id.
    pub fn get_room(&self, room_id: &RoomId) -> Option<Room> {
        self.rooms
            .get(room_id)
            .and_then(|r| match r.state() {
                RoomState::Joined => Some(r.clone()),
                RoomState::Left => Some(r.clone()),
                RoomState::Invited => self.get_stripped_room(room_id),
            })
            .or_else(|| self.get_stripped_room(room_id))
    }

    /// Get all the rooms this store knows about.
    pub fn get_stripped_rooms(&self) -> Vec<Room> {
        self.stripped_rooms.iter().filter_map(|r| self.get_stripped_room(r.key())).collect()
    }

    /// Get the stripped room with the given room id.
    pub fn get_stripped_room(&self, room_id: &RoomId) -> Option<Room> {
        self.stripped_rooms.get(room_id).map(|r| r.clone())
    }

    /// Lookup the stripped Room for the given RoomId, or create one, if it
    /// didn't exist yet in the store
    pub async fn get_or_create_stripped_room(&self, room_id: &RoomId) -> Room {
        let user_id =
            &self.session_meta.get().expect("Creating room while not being logged in").user_id;

        // Remove the respective room from non-stripped rooms.
        self.rooms.remove(room_id);

        self.stripped_rooms
            .entry(room_id.to_owned())
            .or_insert_with(|| Room::new(user_id, self.inner.clone(), room_id, RoomState::Invited))
            .clone()
    }

    /// Lookup the Room for the given RoomId, or create one, if it didn't exist
    /// yet in the store
    pub async fn get_or_create_room(&self, room_id: &RoomId, room_type: RoomState) -> Room {
        if room_type == RoomState::Invited {
            return self.get_or_create_stripped_room(room_id).await;
        }

        let user_id =
            &self.session_meta.get().expect("Creating room while not being logged in").user_id;

        // Remove the respective room from stripped rooms.
        self.stripped_rooms.remove(room_id);

        self.rooms
            .entry(room_id.to_owned())
            .or_insert_with(|| Room::new(user_id, self.inner.clone(), room_id, room_type))
            .clone()
    }
}

#[cfg(not(tarpaulin_include))]
impl fmt::Debug for Store {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Store")
            .field("inner", &self.inner)
            .field("session_meta", &self.session_meta)
            .field("sync_token", &self.sync_token)
            .field("rooms", &self.rooms)
            .field("stripped_rooms", &self.stripped_rooms)
            .finish_non_exhaustive()
    }
}

impl Deref for Store {
    type Target = DynStateStore;

    fn deref(&self) -> &Self::Target {
        self.inner.deref()
    }
}

/// Store state changes and pass them to the StateStore.
#[derive(Clone, Debug, Default)]
pub struct StateChanges {
    /// The sync token that relates to this update.
    pub sync_token: Option<String>,
    /// A user session, containing an access token and information about the
    /// associated user account.
    pub session: Option<Session>,
    /// A mapping of event type string to `AnyBasicEvent`.
    pub account_data: BTreeMap<GlobalAccountDataEventType, Raw<AnyGlobalAccountDataEvent>>,
    /// A mapping of `UserId` to `PresenceEvent`.
    pub presence: BTreeMap<OwnedUserId, Raw<PresenceEvent>>,

    /// A mapping of `RoomId` to a map of users and their
    /// `MinimalRoomMemberEvent`.
    pub profiles: BTreeMap<OwnedRoomId, BTreeMap<OwnedUserId, MinimalRoomMemberEvent>>,

    /// A mapping of `RoomId` to a map of event type string to a state key and
    /// `AnySyncStateEvent`.
    pub state:
        BTreeMap<OwnedRoomId, BTreeMap<StateEventType, BTreeMap<String, Raw<AnySyncStateEvent>>>>,
    /// A mapping of `RoomId` to a map of event type string to `AnyBasicEvent`.
    pub room_account_data:
        BTreeMap<OwnedRoomId, BTreeMap<RoomAccountDataEventType, Raw<AnyRoomAccountDataEvent>>>,
    /// A map of `RoomId` to `RoomInfo`.
    pub room_infos: BTreeMap<OwnedRoomId, RoomInfo>,
    /// A map of `RoomId` to `ReceiptEventContent`.
    pub receipts: BTreeMap<OwnedRoomId, ReceiptEventContent>,

    /// A map of `RoomId` to maps of `OwnedEventId` to be redacted by
    /// `SyncRoomRedactionEvent`.
    pub redactions:
        BTreeMap<OwnedRoomId, BTreeMap<OwnedEventId, Raw<OriginalSyncRoomRedactionEvent>>>,

    /// A mapping of `RoomId` to a map of event type to a map of state key to
    /// `AnyStrippedStateEvent`.
    pub stripped_state: BTreeMap<
        OwnedRoomId,
        BTreeMap<StateEventType, BTreeMap<String, Raw<AnyStrippedStateEvent>>>,
    >,

    /// A map from room id to a map of a display name and a set of user ids that
    /// share that display name in the given room.
    pub ambiguity_maps: BTreeMap<OwnedRoomId, BTreeMap<String, BTreeSet<OwnedUserId>>>,
    /// A map of `RoomId` to a vector of `Notification`s
    pub notifications: BTreeMap<OwnedRoomId, Vec<Notification>>,
}

impl StateChanges {
    /// Create a new `StateChanges` struct with the given sync_token.
    pub fn new(sync_token: String) -> Self {
        Self { sync_token: Some(sync_token), ..Default::default() }
    }

    /// Update the `StateChanges` struct with the given `PresenceEvent`.
    pub fn add_presence_event(&mut self, event: PresenceEvent, raw_event: Raw<PresenceEvent>) {
        self.presence.insert(event.sender, raw_event);
    }

    /// Update the `StateChanges` struct with the given `RoomInfo`.
    pub fn add_room(&mut self, room: RoomInfo) {
        self.room_infos.insert(room.room_id.clone(), room);
    }

    /// Update the `StateChanges` struct with the given `AnyBasicEvent`.
    pub fn add_account_data(
        &mut self,
        event: AnyGlobalAccountDataEvent,
        raw_event: Raw<AnyGlobalAccountDataEvent>,
    ) {
        self.account_data.insert(event.event_type(), raw_event);
    }

    /// Update the `StateChanges` struct with the given room with a new
    /// `AnyBasicEvent`.
    pub fn add_room_account_data(
        &mut self,
        room_id: &RoomId,
        event: AnyRoomAccountDataEvent,
        raw_event: Raw<AnyRoomAccountDataEvent>,
    ) {
        self.room_account_data
            .entry(room_id.to_owned())
            .or_default()
            .insert(event.event_type(), raw_event);
    }

    /// Update the `StateChanges` struct with the given room with a new
    /// `StrippedMemberEvent`.
    pub fn add_stripped_member(
        &mut self,
        room_id: &RoomId,
        user_id: &UserId,
        event: Raw<StrippedRoomMemberEvent>,
    ) {
        self.stripped_state
            .entry(room_id.to_owned())
            .or_default()
            .entry(StateEventType::RoomMember)
            .or_default()
            .insert(user_id.into(), event.cast());
    }

    /// Update the `StateChanges` struct with the given room with a new
    /// `AnySyncStateEvent`.
    pub fn add_state_event(
        &mut self,
        room_id: &RoomId,
        event: AnySyncStateEvent,
        raw_event: Raw<AnySyncStateEvent>,
    ) {
        self.state
            .entry(room_id.to_owned())
            .or_default()
            .entry(event.event_type())
            .or_default()
            .insert(event.state_key().to_owned(), raw_event);
    }

    /// Redact an event in the room
    pub fn add_redaction(
        &mut self,
        room_id: &RoomId,
        redacted_event_id: &EventId,
        redaction: Raw<OriginalSyncRoomRedactionEvent>,
    ) {
        self.redactions
            .entry(room_id.to_owned())
            .or_default()
            .insert(redacted_event_id.to_owned(), redaction);
    }

    /// Update the `StateChanges` struct with the given room with a new
    /// `Notification`.
    pub fn add_notification(&mut self, room_id: &RoomId, notification: Notification) {
        self.notifications.entry(room_id.to_owned()).or_default().push(notification);
    }

    /// Update the `StateChanges` struct with the given room with a new
    /// `Receipts`.
    pub fn add_receipts(&mut self, room_id: &RoomId, event: ReceiptEventContent) {
        self.receipts.insert(room_id.to_owned(), event);
    }
}

/// Configuration for the state store and, when `encryption` is enabled, for the
/// crypto store.
///
/// # Examples
///
/// ```
/// # use matrix_sdk_base::store::StoreConfig;
///
/// let store_config = StoreConfig::new();
/// ```
#[derive(Clone)]
pub struct StoreConfig {
    #[cfg(feature = "e2e-encryption")]
    pub(crate) crypto_store: Arc<DynCryptoStore>,
    pub(crate) state_store: Arc<DynStateStore>,
}

#[cfg(not(tarpaulin_include))]
impl std::fmt::Debug for StoreConfig {
    fn fmt(&self, fmt: &mut std::fmt::Formatter<'_>) -> StdResult<(), std::fmt::Error> {
        fmt.debug_struct("StoreConfig").finish()
    }
}

impl StoreConfig {
    /// Create a new default `StoreConfig`.
    #[must_use]
    pub fn new() -> Self {
        Self {
            #[cfg(feature = "e2e-encryption")]
            crypto_store: matrix_sdk_crypto::store::MemoryStore::new().into_crypto_store(),
            state_store: Arc::new(MemoryStore::new()),
        }
    }

    /// Set a custom implementation of a `CryptoStore`.
    ///
    /// The crypto store must be opened before being set.
    #[cfg(feature = "e2e-encryption")]
    pub fn crypto_store(mut self, store: impl IntoCryptoStore) -> Self {
        self.crypto_store = store.into_crypto_store();
        self
    }

    /// Set a custom implementation of a `StateStore`.
    pub fn state_store(mut self, store: impl IntoStateStore) -> Self {
        self.state_store = store.into_state_store();
        self
    }
}

impl Default for StoreConfig {
    fn default() -> Self {
        Self::new()
    }
}
