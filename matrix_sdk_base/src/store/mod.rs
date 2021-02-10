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

use std::{
    collections::{BTreeMap, BTreeSet},
    ops::Deref,
    path::Path,
    sync::Arc,
};

use dashmap::DashMap;
use matrix_sdk_common::{
    async_trait,
    events::{
        presence::PresenceEvent, room::member::MemberEventContent, AnyBasicEvent,
        AnyStrippedStateEvent, AnySyncMessageEvent, AnySyncRoomEvent, AnySyncStateEvent,
        EventContent, EventType,
    },
    identifiers::{RoomId, UserId},
    locks::RwLock,
    AsyncTraitDeps,
};
#[cfg(feature = "sled_state_store")]
use sled::Db;

use crate::{
    deserialized_responses::{MemberEvent, StrippedMemberEvent},
    rooms::{RoomInfo, RoomType, StrippedRoom, StrippedRoomInfo},
    InvitedRoom, JoinedRoom, LeftRoom, Room, RoomState, Session,
};

pub(crate) mod ambiguity_map;
mod memory_store;
#[cfg(feature = "sled_state_store")]
mod sled_store;

#[cfg(not(feature = "sled_state_store"))]
use self::memory_store::MemoryStore;
#[cfg(feature = "sled_state_store")]
use self::sled_store::SledStore;

/// State store specific error type.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// An error happened in the underlying sled database.
    #[cfg(feature = "sled_state_store")]
    #[error(transparent)]
    Sled(#[from] sled::Error),
    /// An error happened while serializing or deserializing some data.
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    /// An error happened while deserializing a Matrix identifier, e.g. an user
    /// id.
    #[error(transparent)]
    Identifier(#[from] matrix_sdk_common::identifiers::Error),
    /// The store is locked with a passphrase and an incorrect passphrase was
    /// given.
    #[error("The store failed to be unlocked")]
    StoreLocked,
    /// An unencrypted store was tried to be unlocked with a passphrase.
    #[error("The store is not encrypted but was tried to be opened with a passphrase")]
    UnencryptedStore,
    /// The store failed to encrypt or decrypt some data.
    #[error("Error encrypting or decrypting data from the store: {0}")]
    Encryption(String),
}

/// A `StateStore` specific result type.
pub type Result<T> = std::result::Result<T, StoreError>;

/// An abstract state store trait that can be used to implement different stores
/// for the SDK.
#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
pub trait StateStore: AsyncTraitDeps {
    /// Save the given filter id under the given name.
    ///
    /// # Arguments
    ///
    /// * `filter_name` - The name that should be used to store the filter id.
    ///
    /// * `filter_id` - The filter id that should be stored in the state store.
    async fn save_filter(&self, filter_name: &str, filter_id: &str) -> Result<()>;

    /// Save the set of state changes in the store.
    async fn save_changes(&self, changes: &StateChanges) -> Result<()>;

    /// Get the filter id that was stored under the given filter name.
    ///
    /// # Arguments
    ///
    /// * `filter_name` - The name that was used to store the filter id.
    async fn get_filter(&self, filter_name: &str) -> Result<Option<String>>;

    /// Get the last stored sync token.
    async fn get_sync_token(&self) -> Result<Option<String>>;

    /// Get the stored presence event for the given user.
    ///
    /// # Arguments
    ///
    /// * `user_id` - The id of the user for which we wish to fetch the presence
    /// event for.
    async fn get_presence_event(&self, user_id: &UserId) -> Result<Option<PresenceEvent>>;

    /// Get a state event out of the state store.
    ///
    /// # Arguments
    ///
    /// * `room_id` - The id of the room the state event was received for.
    ///
    /// * `event_type` - The event type of the state event.
    async fn get_state_event(
        &self,
        room_id: &RoomId,
        event_type: EventType,
        state_key: &str,
    ) -> Result<Option<AnySyncStateEvent>>;

    /// Get the current profile for the given user in the given room.
    ///
    /// # Arguments
    ///
    /// * `room_id` - The room id the profile is used in.
    ///
    /// * `user_id` - The id of the user the profile belongs to.
    async fn get_profile(
        &self,
        room_id: &RoomId,
        user_id: &UserId,
    ) -> Result<Option<MemberEventContent>>;

    /// Get a raw `MemberEvent` for the given state key in the given room id.
    ///
    /// # Arguments
    ///
    /// * `room_id` - The room id the member event belongs to.
    ///
    /// * `state_key` - The user id that the member event defines the state for.
    async fn get_member_event(
        &self,
        room_id: &RoomId,
        state_key: &UserId,
    ) -> Result<Option<MemberEvent>>;

    /// Get all the user ids of members that are in the invited state for a
    /// given room.
    async fn get_invited_user_ids(&self, room_id: &RoomId) -> Result<Vec<UserId>>;

    /// Get all the user ids of members that are in the joined state for a
    /// given room.
    async fn get_joined_user_ids(&self, room_id: &RoomId) -> Result<Vec<UserId>>;

    /// Get all the pure `RoomInfo`s the store knows about.
    async fn get_room_infos(&self) -> Result<Vec<RoomInfo>>;

    /// Get all the pure `StrippedRoomInfo`s the store knows about.
    async fn get_stripped_room_infos(&self) -> Result<Vec<StrippedRoomInfo>>;

    /// Get all the users that use the given display name in the given room.
    ///
    /// # Arguments
    ///
    /// * `room_id` - The id of the room for which the display name users should
    /// be fetched for.
    ///
    /// * `display_name` - The display name that the users use.
    async fn get_users_with_display_name(
        &self,
        room_id: &RoomId,
        display_name: &str,
    ) -> Result<BTreeSet<UserId>>;

    /// Get all timeline events that came with this `prev_batch` token.
    async fn get_messages(
        &self,
        room_id: &RoomId,
        prev_batch: &str,
    ) -> Result<Vec<AnySyncMessageEvent>>;

    /// Checks if the message events in this chunk are already contained in the store.
    ///
    /// # Arguments
    ///
    /// * `room_id` - The id of the room we are checking.
    async fn unknown_timeline_events<'a>(
        &'a self,
        room_id: &RoomId,
        prev_batch: &str,
        events: &'a [AnySyncRoomEvent],
    ) -> Result<&'a [AnySyncRoomEvent]>;
}

/// A state store wrapper for the SDK.
///
/// This adds additional higher level store functionality on top of a
/// `StateStore` implementation.
#[derive(Debug, Clone)]
pub struct Store {
    inner: Arc<Box<dyn StateStore>>,
    pub(crate) session: Arc<RwLock<Option<Session>>>,
    pub(crate) sync_token: Arc<RwLock<Option<String>>>,
    rooms: Arc<DashMap<RoomId, Room>>,
    stripped_rooms: Arc<DashMap<RoomId, StrippedRoom>>,
}

impl Store {
    fn new(inner: Box<dyn StateStore>) -> Self {
        let session = Arc::new(RwLock::new(None));
        let sync_token = Arc::new(RwLock::new(None));

        Self {
            inner: inner.into(),
            session,
            sync_token,
            rooms: DashMap::new().into(),
            stripped_rooms: DashMap::new().into(),
        }
    }

    pub(crate) async fn restore_session(&self, session: Session) -> Result<()> {
        for info in self.inner.get_room_infos().await? {
            let room = Room::restore(&session.user_id, self.inner.clone(), info);
            self.rooms.insert(room.room_id().to_owned(), room);
        }

        for info in self.inner.get_stripped_room_infos().await? {
            let room = StrippedRoom::restore(&session.user_id, self.inner.clone(), info);
            self.stripped_rooms.insert(room.room_id().to_owned(), room);
        }

        let token = self.get_sync_token().await?;

        *self.sync_token.write().await = token;
        *self.session.write().await = Some(session);

        Ok(())
    }

    #[cfg(not(feature = "sled_state_store"))]
    pub(crate) fn open_memory_store() -> Self {
        let inner = Box::new(MemoryStore::new());

        Self::new(inner)
    }

    /// Open the default Sled store.
    ///
    /// # Arguments
    ///
    /// * `path` - The path where the store should reside in.
    ///
    /// * `passphrase` - A passphrase that should be used to encrypt the state
    /// store.
    #[cfg(feature = "sled_state_store")]
    pub fn open_default(path: impl AsRef<Path>, passphrase: Option<&str>) -> Result<(Self, Db)> {
        let inner = if let Some(passphrase) = passphrase {
            SledStore::open_with_passphrase(path, passphrase)?
        } else {
            SledStore::open_with_path(path)?
        };

        Ok((Self::new(Box::new(inner.clone())), inner.inner))
    }

    #[cfg(feature = "sled_state_store")]
    pub(crate) fn open_temporary() -> Result<(Self, Db)> {
        let inner = SledStore::open()?;

        Ok((Self::new(Box::new(inner.clone())), inner.inner))
    }

    pub(crate) fn get_bare_room(&self, room_id: &RoomId) -> Option<Room> {
        #[allow(clippy::map_clone)]
        self.rooms.get(room_id).map(|r| r.clone())
    }

    /// Get all the rooms this store knows about.
    pub fn get_rooms(&self) -> Vec<RoomState> {
        self.rooms
            .iter()
            .filter_map(|r| self.get_room(r.key()))
            .collect()
    }

    /// Get the joined room with the given room id.
    ///
    /// *Note*: A room with the given id might exist in a different state, this
    /// will only return the room if it's in the joined state.
    pub fn get_joined_room(&self, room_id: &RoomId) -> Option<JoinedRoom> {
        self.get_room(room_id).and_then(|r| r.joined())
    }

    /// Get the joined room with the given room id.
    ///
    /// *Note*: A room with the given id might exist in a different state, this
    /// will only return the room if it's in the invited state.
    pub fn get_invited_room(&self, room_id: &RoomId) -> Option<InvitedRoom> {
        self.get_room(room_id).and_then(|r| r.invited())
    }

    /// Get the joined room with the given room id.
    ///
    /// *Note*: A room with the given id might exist in a different state, this
    /// will only return the room if it's in the left state.
    pub fn get_left_room(&self, room_id: &RoomId) -> Option<LeftRoom> {
        self.get_room(room_id).and_then(|r| r.left())
    }

    /// Get the room with the given room id.
    ///
    /// *Note*: This will return the room in the `RoomState` enum, a room might
    /// turn from an invited room to a joined one between sync requests, this
    /// room struct might have stale info in that case and a new one should be
    /// pulled out of the store.
    pub fn get_room(&self, room_id: &RoomId) -> Option<RoomState> {
        self.get_bare_room(room_id)
            .and_then(|r| match r.room_type() {
                RoomType::Joined => Some(RoomState::Joined(JoinedRoom { inner: r })),
                RoomType::Left => Some(RoomState::Left(LeftRoom { inner: r })),
                RoomType::Invited => self
                    .get_stripped_room(room_id)
                    .map(|r| RoomState::Invited(InvitedRoom { inner: r })),
            })
    }

    fn get_stripped_room(&self, room_id: &RoomId) -> Option<StrippedRoom> {
        #[allow(clippy::map_clone)]
        self.stripped_rooms.get(room_id).map(|r| r.clone())
    }

    pub(crate) async fn get_or_create_stripped_room(&self, room_id: &RoomId) -> StrippedRoom {
        let session = self.session.read().await;
        let user_id = &session
            .as_ref()
            .expect("Creating room while not being logged in")
            .user_id;

        self.stripped_rooms
            .entry(room_id.clone())
            .or_insert_with(|| StrippedRoom::new(user_id, self.inner.clone(), room_id))
            .clone()
    }

    pub(crate) async fn get_or_create_room(&self, room_id: &RoomId, room_type: RoomType) -> Room {
        let session = self.session.read().await;
        let user_id = &session
            .as_ref()
            .expect("Creating room while not being logged in")
            .user_id;

        self.rooms
            .entry(room_id.clone())
            .or_insert_with(|| Room::new(user_id, self.inner.clone(), room_id, room_type))
            .clone()
    }
}

impl Deref for Store {
    type Target = Box<dyn StateStore>;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

/// Store state changes and pass them to the StateStore.
#[derive(Debug, Default)]
pub struct StateChanges {
    /// The sync token that relates to this update.
    pub sync_token: Option<String>,
    /// A user session, containing an access token and information about the associated user account.
    pub session: Option<Session>,
    /// A mapping of event type string to `AnyBasicEvent`.
    pub account_data: BTreeMap<String, AnyBasicEvent>,
    /// A mapping of `UserId` to `PresenceEvent`.
    pub presence: BTreeMap<UserId, PresenceEvent>,

    /// A mapping of `RoomId` to a map of users and their `MemberEvent`.
    pub members: BTreeMap<RoomId, BTreeMap<UserId, MemberEvent>>,
    /// A mapping of `RoomId` to a map of users and their `MemberEventContent`.
    pub profiles: BTreeMap<RoomId, BTreeMap<UserId, MemberEventContent>>,

    pub(crate) ambiguity_maps: BTreeMap<RoomId, BTreeMap<String, BTreeSet<UserId>>>,
    /// A mapping of `RoomId` to a map of event type string to a state key and `AnySyncStateEvent`.
    pub state: BTreeMap<RoomId, BTreeMap<String, BTreeMap<String, AnySyncStateEvent>>>,
    /// A mapping of `RoomId` to a map of event type string to `AnyBasicEvent`.
    pub room_account_data: BTreeMap<RoomId, BTreeMap<String, AnyBasicEvent>>,
    /// A map of `RoomId` to `RoomInfo`.
    pub room_infos: BTreeMap<RoomId, RoomInfo>,

    /// A mapping of `RoomId` to a map of event type to a map of state key to `AnyStrippedStateEvent`.
    pub stripped_state: BTreeMap<RoomId, BTreeMap<String, BTreeMap<String, AnyStrippedStateEvent>>>,
    /// A mapping of `RoomId` to a map of users and their `StrippedMemberEvent`.
    pub stripped_members: BTreeMap<RoomId, BTreeMap<UserId, StrippedMemberEvent>>,
    /// A map of `RoomId` to `StrippedRoomInfo`.
    pub invited_room_info: BTreeMap<RoomId, StrippedRoomInfo>,

    /// A mapping of `RoomId` to message events ordered based on when they were received.
    pub joined_message_events: BTreeMap<RoomId, BTreeMap<(String, u64), AnySyncMessageEvent>>,
}

impl StateChanges {
    /// Create a new `StateChanges` struct with the given sync_token.
    pub fn new(sync_token: String) -> Self {
        Self {
            sync_token: Some(sync_token),
            ..Default::default()
        }
    }

    /// Update the `StateChanges` struct with the given `PresenceEvent`.
    pub fn add_presence_event(&mut self, event: PresenceEvent) {
        self.presence.insert(event.sender.clone(), event);
    }

    /// Update the `StateChanges` struct with the given `RoomInfo`.
    pub fn add_room(&mut self, room: RoomInfo) {
        self.room_infos
            .insert(room.room_id.as_ref().to_owned(), room);
    }

    /// Update the `StateChanges` struct with the given `StrippedRoomInfo`.
    pub fn add_stripped_room(&mut self, room: StrippedRoomInfo) {
        self.invited_room_info
            .insert(room.room_id.as_ref().to_owned(), room);
    }

    /// Update the `StateChanges` struct with the given `AnyBasicEvent`.
    pub fn add_account_data(&mut self, event: AnyBasicEvent) {
        self.account_data
            .insert(event.content().event_type().to_owned(), event);
    }

    /// Update the `StateChanges` struct with the given room with a new `AnyBasicEvent`.
    pub fn add_room_account_data(&mut self, room_id: &RoomId, event: AnyBasicEvent) {
        self.room_account_data
            .entry(room_id.to_owned())
            .or_insert_with(BTreeMap::new)
            .insert(event.content().event_type().to_owned(), event);
    }

    /// Update the `StateChanges` struct with the given room with a new `AnyStrippedStateEvent`.
    pub fn add_stripped_state_event(&mut self, room_id: &RoomId, event: AnyStrippedStateEvent) {
        self.stripped_state
            .entry(room_id.to_owned())
            .or_insert_with(BTreeMap::new)
            .entry(event.content().event_type().to_string())
            .or_insert_with(BTreeMap::new)
            .insert(event.state_key().to_string(), event);
    }

    /// Update the `StateChanges` struct with the given room with a new `StrippedMemberEvent`.
    pub fn add_stripped_member(&mut self, room_id: &RoomId, event: StrippedMemberEvent) {
        let user_id = event.state_key.clone();

        self.stripped_members
            .entry(room_id.to_owned())
            .or_insert_with(BTreeMap::new)
            .insert(user_id, event);
    }

    /// Update the `StateChanges` struct with the given room with a new `AnySyncStateEvent`.
    pub fn add_state_event(&mut self, room_id: &RoomId, event: AnySyncStateEvent) {
        self.state
            .entry(room_id.to_owned())
            .or_insert_with(BTreeMap::new)
            .entry(event.content().event_type().to_string())
            .or_insert_with(BTreeMap::new)
            .insert(event.state_key().to_string(), event);
    }

    ///
    pub fn handle_message_event(
        &mut self,
        room_id: &RoomId,
        prev_batch: &str,
        content: &[AnySyncRoomEvent],
    ) {
        use std::ops::Bound::*;

        let last = self
            .joined_message_events
            .get(room_id)
            .map(|map| {
                map.range((Included((prev_batch.to_string(), 0)), Unbounded))
                    .last()
                    .map_or(0, |(k, _)| k.1)
            })
            .unwrap_or_default();

        for (idx, msg) in content.iter().enumerate().filter_map(|(idx, ev)| {
            if let AnySyncRoomEvent::Message(msg) = ev {
                Some((idx, msg))
            } else {
                None
            }
        }) {
            let room = self
                .joined_message_events
                .entry(room_id.to_owned())
                .or_default();

            room.insert((prev_batch.to_string(), idx as u64 + last), msg.clone());
        }
    }
}
