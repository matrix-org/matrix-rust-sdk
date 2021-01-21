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

use std::{collections::BTreeMap, ops::Deref, path::Path, sync::Arc};

use dashmap::DashMap;
use matrix_sdk_common::{
    async_trait,
    events::{
        presence::PresenceEvent, room::member::MemberEventContent, AnyBasicEvent,
        AnyStrippedStateEvent, AnySyncStateEvent, EventContent, EventType,
    },
    identifiers::{RoomId, UserId},
    locks::RwLock,
    AsyncTraitDeps,
};

use crate::{
    deserialized_responses::{MemberEvent, StrippedMemberEvent},
    rooms::{RoomInfo, RoomType, StrippedRoom},
    InvitedRoom, JoinedRoom, LeftRoom, Room, RoomState, Session,
};

mod sled_store;
pub use sled_store::SledStore;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error(transparent)]
    Sled(#[from] sled::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Identifier(#[from] matrix_sdk_common::identifiers::Error),
    #[error("The store failed to be unlocked")]
    StoreLocked,
    #[error("The store is not encrypted but was tried to be opened with a passphrase")]
    UnencryptedStore,
    #[error("Error encrypting or decrypting data from the store: {0}")]
    Encryption(String),
}

/// A `StateStore` specific result type.
pub type Result<T> = std::result::Result<T, StoreError>;

#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
pub trait StateStore: AsyncTraitDeps {
    async fn save_filter(&self, filter_name: &str, filter_id: &str) -> Result<()>;

    async fn save_changes(&self, changes: &StateChanges) -> Result<()>;

    async fn get_filter(&self, filter_id: &str) -> Result<Option<String>>;

    async fn get_sync_token(&self) -> Result<Option<String>>;

    async fn get_presence_event(&self, user_id: &UserId) -> Result<Option<PresenceEvent>>;

    async fn get_state_event(
        &self,
        room_id: &RoomId,
        event_type: EventType,
        state_key: &str,
    ) -> Result<Option<AnySyncStateEvent>>;

    async fn get_profile(
        &self,
        room_id: &RoomId,
        user_id: &UserId,
    ) -> Result<Option<MemberEventContent>>;

    async fn get_member_event(
        &self,
        room_id: &RoomId,
        state_key: &UserId,
    ) -> Result<Option<MemberEvent>>;

    async fn get_invited_user_ids(&self, room_id: &RoomId) -> Result<Vec<UserId>>;

    async fn get_joined_user_ids(&self, room_id: &RoomId) -> Result<Vec<UserId>>;

    async fn get_room_infos(&self) -> Result<Vec<RoomInfo>>;
}

#[derive(Debug, Clone)]
pub struct Store {
    inner: Arc<Box<dyn StateStore>>,
    session: Arc<RwLock<Option<Session>>>,
    sync_token: Arc<RwLock<Option<String>>>,
    rooms: Arc<DashMap<RoomId, Room>>,
    stripped_rooms: Arc<DashMap<RoomId, StrippedRoom>>,
}

impl Store {
    pub fn new(
        session: Arc<RwLock<Option<Session>>>,
        sync_token: Arc<RwLock<Option<String>>>,
        inner: SledStore,
    ) -> Self {
        Self {
            inner: Arc::new(Box::new(inner)),
            session,
            sync_token,
            rooms: DashMap::new().into(),
            stripped_rooms: DashMap::new().into(),
        }
    }

    pub(crate) async fn restore_session(&self, session: Session) -> Result<()> {
        for info in self.inner.get_room_infos().await?.into_iter() {
            let room = Room::restore(&session.user_id, self.inner.clone(), info);
            self.rooms.insert(room.room_id().to_owned(), room);
        }

        let token = self.get_sync_token().await?;

        *self.sync_token.write().await = token;
        *self.session.write().await = Some(session);

        Ok(())
    }

    pub fn open_default(path: impl AsRef<Path>) -> Result<Self> {
        let inner = SledStore::open_with_path(path)?;
        Ok(Self::new(
            Arc::new(RwLock::new(None)),
            Arc::new(RwLock::new(None)),
            inner,
        ))
    }

    pub(crate) fn get_bare_room(&self, room_id: &RoomId) -> Option<Room> {
        #[allow(clippy::map_clone)]
        self.rooms.get(room_id).map(|r| r.clone())
    }

    pub fn get_rooms(&self) -> Vec<RoomState> {
        self.rooms
            .iter()
            .filter_map(|r| self.get_room(r.key()))
            .collect()
    }

    pub fn get_joined_room(&self, room_id: &RoomId) -> Option<JoinedRoom> {
        self.get_room(room_id).map(|r| r.joined()).flatten()
    }

    pub fn get_invited_room(&self, room_id: &RoomId) -> Option<InvitedRoom> {
        self.get_room(room_id).map(|r| r.invited()).flatten()
    }

    pub fn get_left_room(&self, room_id: &RoomId) -> Option<LeftRoom> {
        self.get_room(room_id).map(|r| r.left()).flatten()
    }

    pub fn get_room(&self, room_id: &RoomId) -> Option<RoomState> {
        self.get_bare_room(room_id)
            .map(|r| match r.room_type() {
                RoomType::Joined => Some(RoomState::Joined(JoinedRoom { inner: r })),
                RoomType::Left => Some(RoomState::Left(LeftRoom { inner: r })),
                RoomType::Invited => self
                    .get_stripped_room(room_id)
                    .map(|r| RoomState::Invited(InvitedRoom { inner: r })),
            })
            .flatten()
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

#[derive(Debug, Default)]
pub struct StateChanges {
    pub sync_token: Option<String>,
    pub session: Option<Session>,
    pub account_data: BTreeMap<String, AnyBasicEvent>,
    pub presence: BTreeMap<UserId, PresenceEvent>,

    pub members: BTreeMap<RoomId, BTreeMap<UserId, MemberEvent>>,
    pub profiles: BTreeMap<RoomId, BTreeMap<UserId, MemberEventContent>>,
    pub state: BTreeMap<RoomId, BTreeMap<String, BTreeMap<String, AnySyncStateEvent>>>,
    pub room_account_data: BTreeMap<RoomId, BTreeMap<String, AnyBasicEvent>>,
    pub room_infos: BTreeMap<RoomId, RoomInfo>,

    pub stripped_state: BTreeMap<RoomId, BTreeMap<String, BTreeMap<String, AnyStrippedStateEvent>>>,
    pub stripped_members: BTreeMap<RoomId, BTreeMap<UserId, StrippedMemberEvent>>,
    pub invited_room_info: BTreeMap<RoomId, RoomInfo>,
}

impl StateChanges {
    pub fn new(sync_token: String) -> Self {
        Self {
            sync_token: Some(sync_token),
            ..Default::default()
        }
    }

    pub fn add_presence_event(&mut self, event: PresenceEvent) {
        self.presence.insert(event.sender.clone(), event);
    }

    pub fn add_room(&mut self, room: RoomInfo) {
        self.room_infos
            .insert(room.room_id.as_ref().to_owned(), room);
    }

    pub fn add_account_data(&mut self, event: AnyBasicEvent) {
        self.account_data
            .insert(event.content().event_type().to_owned(), event);
    }

    pub fn add_room_account_data(&mut self, room_id: &RoomId, event: AnyBasicEvent) {
        self.room_account_data
            .entry(room_id.to_owned())
            .or_insert_with(BTreeMap::new)
            .insert(event.content().event_type().to_owned(), event);
    }

    pub fn add_stripped_state_event(&mut self, room_id: &RoomId, event: AnyStrippedStateEvent) {
        self.stripped_state
            .entry(room_id.to_owned())
            .or_insert_with(BTreeMap::new)
            .entry(event.content().event_type().to_string())
            .or_insert_with(BTreeMap::new)
            .insert(event.state_key().to_string(), event);
    }

    pub fn add_stripped_member(&mut self, room_id: &RoomId, event: StrippedMemberEvent) {
        let user_id = event.state_key.clone();

        self.stripped_members
            .entry(room_id.to_owned())
            .or_insert_with(BTreeMap::new)
            .insert(user_id, event);
    }

    pub fn add_state_event(&mut self, room_id: &RoomId, event: AnySyncStateEvent) {
        self.state
            .entry(room_id.to_owned())
            .or_insert_with(BTreeMap::new)
            .entry(event.content().event_type().to_string())
            .or_insert_with(BTreeMap::new)
            .insert(event.state_key().to_string(), event);
    }
}
