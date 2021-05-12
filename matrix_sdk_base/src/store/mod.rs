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

#[cfg(feature = "sled_state_store")]
use std::path::Path;
use std::{
    collections::{BTreeMap, BTreeSet},
    ops::Deref,
    sync::Arc,
};

use dashmap::DashMap;
use matrix_sdk_common::{
    api::r0::{
        message::get_message_events::{
            Direction, Request as MessagesRequest, Response as MessagesResponse,
        },
        push::get_notifications::Notification,
    },
    async_trait,
    events::{
        presence::PresenceEvent, room::member::MemberEventContent, AnyGlobalAccountDataEvent,
        AnyRoomAccountDataEvent, AnyStrippedStateEvent, AnySyncStateEvent, EventContent, EventType,
    },
    identifiers::{EventId, RoomId, UserId},
    locks::RwLock,
    AsyncTraitDeps, Raw,
};
#[cfg(feature = "sled_state_store")]
use sled::Db;

use crate::{
    deserialized_responses::{MemberEvent, StrippedMemberEvent},
    rooms::{RoomInfo, RoomType},
    Room, Session,
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
pub type Result<T, E = StoreError> = std::result::Result<T, E>;

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
    async fn get_presence_event(&self, user_id: &UserId) -> Result<Option<Raw<PresenceEvent>>>;

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
    ) -> Result<Option<Raw<AnySyncStateEvent>>>;

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

    /// Get all the user ids of members for a given room.
    async fn get_user_ids(&self, room_id: &RoomId) -> Result<Vec<UserId>>;

    /// Get all the user ids of members that are in the invited state for a
    /// given room.
    async fn get_invited_user_ids(&self, room_id: &RoomId) -> Result<Vec<UserId>>;

    /// Get all the user ids of members that are in the joined state for a
    /// given room.
    async fn get_joined_user_ids(&self, room_id: &RoomId) -> Result<Vec<UserId>>;

    /// Get all the pure `RoomInfo`s the store knows about.
    async fn get_room_infos(&self) -> Result<Vec<RoomInfo>>;

    /// Get all the pure `RoomInfo`s the store knows about.
    async fn get_stripped_room_infos(&self) -> Result<Vec<RoomInfo>>;

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

    /// Get an event out of the account data store.
    ///
    /// # Arguments
    ///
    /// * `event_type` - The event type of the account data event.
    async fn get_account_data_event(
        &self,
        event_type: EventType,
    ) -> Result<Option<Raw<AnyGlobalAccountDataEvent>>>;

    /// Get an event out of the room account data store.
    ///
    /// # Arguments
    ///
    /// * `room_id` - The id of the room for which the room account data event
    ///   should
    /// be fetched.
    ///
    /// * `event_type` - The event type of the room account data event.
    async fn get_room_account_data_event(
        &self,
        room_id: &RoomId,
        event_type: EventType,
    ) -> Result<Option<Raw<AnyRoomAccountDataEvent>>>;
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
    stripped_rooms: Arc<DashMap<RoomId, Room>>,
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
            let room = Room::restore(&session.user_id, self.inner.clone(), info);
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
    pub fn get_rooms(&self) -> Vec<Room> {
        self.rooms.iter().filter_map(|r| self.get_room(r.key())).collect()
    }

    /// Get the room with the given room id.
    pub fn get_room(&self, room_id: &RoomId) -> Option<Room> {
        self.get_bare_room(room_id).and_then(|r| match r.room_type() {
            RoomType::Joined => Some(r),
            RoomType::Left => Some(r),
            RoomType::Invited => self.get_stripped_room(room_id),
        })
    }

    fn get_stripped_room(&self, room_id: &RoomId) -> Option<Room> {
        #[allow(clippy::map_clone)]
        self.stripped_rooms.get(room_id).map(|r| r.clone())
    }

    pub(crate) async fn get_or_create_stripped_room(&self, room_id: &RoomId) -> Room {
        let session = self.session.read().await;
        let user_id = &session.as_ref().expect("Creating room while not being logged in").user_id;

        self.stripped_rooms
            .entry(room_id.clone())
            .or_insert_with(|| Room::new(user_id, self.inner.clone(), room_id, RoomType::Invited))
            .clone()
    }

    pub(crate) async fn get_or_create_room(&self, room_id: &RoomId, room_type: RoomType) -> Room {
        let session = self.session.read().await;
        let user_id = &session.as_ref().expect("Creating room while not being logged in").user_id;

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
    /// A user session, containing an access token and information about the
    /// associated user account.
    pub session: Option<Session>,
    /// A mapping of event type string to `AnyBasicEvent`.
    pub account_data: BTreeMap<String, Raw<AnyGlobalAccountDataEvent>>,
    /// A mapping of `UserId` to `PresenceEvent`.
    pub presence: BTreeMap<UserId, Raw<PresenceEvent>>,

    /// A mapping of `RoomId` to a map of users and their `MemberEvent`.
    pub members: BTreeMap<RoomId, BTreeMap<UserId, MemberEvent>>,
    /// A mapping of `RoomId` to a map of users and their `MemberEventContent`.
    pub profiles: BTreeMap<RoomId, BTreeMap<UserId, MemberEventContent>>,

    /// A mapping of `RoomId` to a map of event type string to a state key and
    /// `AnySyncStateEvent`.
    pub state: BTreeMap<RoomId, BTreeMap<String, BTreeMap<String, Raw<AnySyncStateEvent>>>>,
    /// A mapping of `RoomId` to a map of event type string to `AnyBasicEvent`.
    pub room_account_data: BTreeMap<RoomId, BTreeMap<String, Raw<AnyRoomAccountDataEvent>>>,
    /// A map of `RoomId` to `RoomInfo`.
    pub room_infos: BTreeMap<RoomId, RoomInfo>,

    /// A mapping of `RoomId` to a map of event type to a map of state key to
    /// `AnyStrippedStateEvent`.
    pub stripped_state:
        BTreeMap<RoomId, BTreeMap<String, BTreeMap<String, Raw<AnyStrippedStateEvent>>>>,
    /// A mapping of `RoomId` to a map of users and their `StrippedMemberEvent`.
    pub stripped_members: BTreeMap<RoomId, BTreeMap<UserId, StrippedMemberEvent>>,
    /// A map of `RoomId` to `RoomInfo`.
    pub invited_room_info: BTreeMap<RoomId, RoomInfo>,

    /// A map from room id to a map of a display name and a set of user ids that
    /// share that display name in the given room.
    pub ambiguity_maps: BTreeMap<RoomId, BTreeMap<String, BTreeSet<UserId>>>,
    /// A map of `RoomId` to a vector of `Notification`s
    pub notifications: BTreeMap<RoomId, Vec<Notification>>,
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
        self.room_infos.insert(room.room_id.as_ref().to_owned(), room);
    }

    /// Update the `StateChanges` struct with the given `RoomInfo`.
    pub fn add_stripped_room(&mut self, room: RoomInfo) {
        self.invited_room_info.insert(room.room_id.as_ref().to_owned(), room);
    }

    /// Update the `StateChanges` struct with the given `AnyBasicEvent`.
    pub fn add_account_data(
        &mut self,
        event: AnyGlobalAccountDataEvent,
        raw_event: Raw<AnyGlobalAccountDataEvent>,
    ) {
        self.account_data.insert(event.content().event_type().to_owned(), raw_event);
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
            .or_insert_with(BTreeMap::new)
            .insert(event.content().event_type().to_owned(), raw_event);
    }

    /// Update the `StateChanges` struct with the given room with a new
    /// `StrippedMemberEvent`.
    pub fn add_stripped_member(&mut self, room_id: &RoomId, event: StrippedMemberEvent) {
        let user_id = event.state_key.clone();

        self.stripped_members
            .entry(room_id.to_owned())
            .or_insert_with(BTreeMap::new)
            .insert(user_id, event);
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
            .or_insert_with(BTreeMap::new)
            .entry(event.content().event_type().to_string())
            .or_insert_with(BTreeMap::new)
            .insert(event.state_key().to_string(), raw_event);
    }

    /// Update the `StateChanges` struct with the given room with a new
    /// `Notification`.
    pub fn add_notification(&mut self, room_id: &RoomId, notification: Notification) {
        self.notifications.entry(room_id.to_owned()).or_insert_with(Vec::new).push(notification);
    }
}

/// A token that represents the last known event before incoming events
/// are added via /sync or /messages.
pub type PrevBatchToken = String;

/// The ending of a chunk of events from /messages. This will match the
/// next prev_batch token from a /sync.
pub type NextBatchToken = String;

/// The position of a chunk of events in the event stream. This is
/// based on the ordering of `PrevBatchToken` and `NextBatchToken`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SliceIdx(u128);

/// The position of an event within a slice or chunk.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EventIdx(u128);

/// Represents a chunk of events from either /sync or /messages.
#[derive(Clone, Debug)]
pub struct TimelineSlice {
    start: PrevBatchToken,
    end: NextBatchToken,
}

impl TimelineSlice {
    pub fn new(start: PrevBatchToken, end: NextBatchToken) -> Self {
        Self { start, end }
    }
}

/// `EventOwnerMap` keeps track of the ordering of all events and allows getting
/// events from a `PrevBatchToken`, `EventId` or `SliceIdx`.
#[derive(Clone, Debug)]
pub struct EventOwnerMap {
    slice_map: BTreeMap<SliceIdx, BTreeMap<EventIdx, EventId>>,
    event_map: BTreeMap<EventId, SliceIdx>,
    // Now we can go from an eventId to a sub-slice of it's parent SliceId, I don't think
    // we could before?
    event_index: BTreeMap<EventId, EventIdx>,
}

/// Store the new events from /sync or /messages.
#[derive(Clone, Debug)]
pub struct Timeline {
    slices: BTreeMap<SliceIdx, TimelineSlice>,
    /// Maps a `prev_batch` token to a `SliceIdx` "forwards".
    prev_slice_map: BTreeMap<PrevBatchToken, SliceIdx>,
    /// Maps a `next_batch` token to a `SliceIdx` "backwards".
    ///
    /// This points to the same `SliceIdx` `prev_slice_map` does but with the
    /// `next_batch` token.
    next_slice_map: BTreeMap<NextBatchToken, SliceIdx>,
    events: EventOwnerMap,
}

impl Timeline {
    /// Create an empty `Timeline`.
    pub fn new() -> Self {
        Self {
            slices: BTreeMap::new(),
            prev_slice_map: BTreeMap::new(),
            next_slice_map: BTreeMap::new(),
            events: EventOwnerMap {
                slice_map: BTreeMap::new(),
                event_map: BTreeMap::new(),
                event_index: BTreeMap::new(),
            },
        }
    }

    /// Is this `Timeline` empty it has received no /sync or /message responses.
    pub fn is_empty(&self) -> bool {
        self.slices.is_empty() && self.prev_slice_map.is_empty()
    }

    ///
    pub fn handle_sync_timeline(
        &mut self,
        room_id: &RoomId,
        prev_batch: &str,
        next_batch: &str,
        content: &[AnySyncRoomEvent],
    ) {
        let timeline_slice = TimelineSlice::new(prev_batch.to_owned(), next_batch.to_owned());

        // The new `prev_batch` token is the `next_batch` token of previous /sync
        if let Some(SliceIdx(idx)) = self.next_slice_map.get(prev_batch) {
            let next_idx = SliceIdx(idx + 1);
            let last_event_index = self
                .events
                .slice_map
                .values()
                .last()
                .and_then(|map| map.keys().last())
                .map(|e| {
                    let EventIdx(idx) = e;
                    *idx
                })
                .unwrap_or_default();

            self.prev_slice_map.insert(prev_batch.to_owned(), next_idx);
            self.next_slice_map.insert(next_batch.to_owned(), next_idx);
            self.slices.insert(next_idx, timeline_slice);

            self.update_events_map_forward(content, last_event_index, next_idx);
        } else if self.is_empty() {
            let next_idx = SliceIdx(u128::MAX / 2);
            self.prev_slice_map.insert(prev_batch.to_owned(), next_idx);
            self.next_slice_map.insert(next_batch.to_owned(), next_idx);
            self.slices.insert(next_idx, timeline_slice);

            self.update_events_map_forward(content, u128::MAX / 2, next_idx);
        } else {
            todo!("hmmm we got a problem or a gap")
        }
    }

    ///
    pub fn handle_messages_response(
        &mut self,
        room_id: &RoomId,
        resp: &MessagesResponse,
        dir: Direction,
    ) {
        match dir {
            // the end token is how to request older events
            // events are in reverse-chronological order
            Direction::Backward => {
                match (&resp.end, &resp.start) {
                    (Some(prev_batch), Some(end)) => {
                        let timeline_slice =
                            TimelineSlice::new(prev_batch.to_owned(), end.to_owned());

                        let old = self.prev_slice_map.get(prev_batch);
                        let recent = self.prev_slice_map.get(end);
                        match (old, recent) {
                            // We have the full chunk already
                            (Some(_), Some(_)) => {}
                            (Some(_), None) => {
                                // We have a gap
                                // A -> B -> gap -> F
                                // we know B but we must have gotten an
                                // incomplete chunk or
                                // something
                            }
                            // we have the recent token but not the older so fill
                            // backwards
                            (None, Some(SliceIdx(idx))) => {
                                let prev_idx = SliceIdx(idx - 1);
                                let prev_event_index = self
                                    .events
                                    .slice_map
                                    .get(&SliceIdx(*idx))
                                    .and_then(|map| map.keys().next())
                                    .map(|e| {
                                        let EventIdx(idx) = e;
                                        *idx
                                    })
                                    .unwrap_or_default();

                                self.prev_slice_map.insert(prev_batch.to_owned(), prev_idx);
                                self.next_slice_map.insert(end.to_owned(), prev_idx);
                                self.slices.insert(prev_idx, timeline_slice);

                                // We reverse so our slice is oldest -> most recent
                                self.update_events_map_backward(
                                    &resp.chunk,
                                    prev_event_index,
                                    prev_idx,
                                )
                            }
                            (None, None) => {}
                        }
                    }
                    (Some(prev), None) => {
                        // TODO: is this an incomplete chunk do these have
                        // meanings like if there is no
                        // prev/next_batch token?
                    }
                    (None, Some(end)) => {}
                    (None, None) => todo!("problems"),
                }
            }
            // the start token is the oldest events
            // events are in chronological order
            Direction::Forward => {
                match (&resp.start, &resp.end) {
                    (Some(prev_batch), Some(end)) => {
                        let timeline_slice =
                            TimelineSlice::new(prev_batch.to_owned(), end.to_owned());

                        let old = self.next_slice_map.get(prev_batch);
                        let recent = self.next_slice_map.get(end);
                        match (old, recent) {
                            // We have the full chunk already
                            (Some(_), Some(_)) => {}
                            (Some(SliceIdx(idx)), None) => {
                                let next_idx = SliceIdx(idx + 1);
                                let last_event_index = self
                                    .events
                                    .slice_map
                                    .get(&SliceIdx(*idx))
                                    .and_then(|map| map.keys().next())
                                    .map(|e| {
                                        let EventIdx(idx) = e;
                                        *idx
                                    })
                                    .unwrap_or_default();

                                self.prev_slice_map.insert(prev_batch.to_owned(), next_idx);
                                self.next_slice_map.insert(end.to_owned(), next_idx);
                                self.slices.insert(next_idx, timeline_slice);

                                self.update_events_map_forward(
                                    // TODO:
                                    // make the methods more general or specific or
                                    // is this hacky conversion ok?
                                    &resp
                                        .chunk
                                        .iter()
                                        .filter_map(|e| {
                                            // Not ideal but all we need is the eventId
                                            serde_json::from_str(e.json().get()).ok()
                                        })
                                        .collect::<Vec<_>>(),
                                    last_event_index,
                                    next_idx,
                                );
                            }
                            // We have the recent token but not the older so fill
                            // backwards
                            (None, Some(SliceIdx(idx))) => {
                                let prev_idx = SliceIdx(idx - 1);
                                let prev_event_index = self
                                    .events
                                    .slice_map
                                    .get(&SliceIdx(*idx))
                                    .and_then(|map| map.keys().next())
                                    .map(|e| {
                                        let EventIdx(idx) = e;
                                        *idx
                                    })
                                    .unwrap_or_default();

                                self.prev_slice_map.insert(prev_batch.to_owned(), prev_idx);
                                self.next_slice_map.insert(end.to_owned(), prev_idx);
                                self.slices.insert(prev_idx, timeline_slice);
                                // We reverse so our slice is oldest -> most recent
                                self.update_events_map_backward(
                                    &resp.chunk,
                                    prev_event_index,
                                    prev_idx,
                                )
                            }
                            (None, None) => {}
                        }
                    }
                    (Some(prev), None) => {}
                    (None, Some(end)) => {}
                    (None, None) => todo!("problems"),
                }
            }
        }
    }

    fn update_events_map_forward(
        &mut self,
        content: &[AnySyncRoomEvent],
        last_event_index: u128,
        slice_idx: SliceIdx,
    ) {
        let mut index_event = BTreeMap::new();
        for (i, event) in content.iter().enumerate() {
            let e_idx = EventIdx(last_event_index + ((i + 1) as u128));
            let e_id = event.event_id();

            index_event.insert(e_idx, e_id.clone());
            self.events.event_index.insert(e_id.clone(), e_idx);
            self.events.event_map.insert(e_id.clone(), slice_idx);
        }
        self.events.slice_map.insert(slice_idx, index_event);
    }

    fn update_events_map_backward(
        &mut self,
        content: &[Raw<AnyRoomEvent>],
        last_event_index: u128,
        slice_idx: SliceIdx,
    ) {
        let mut index_event = BTreeMap::new();
        // Reverse so that newer events have a smaller index from enumerate
        for (i, event) in content
            .iter()
            // TODO: don't eat events or is this ok?
            .filter_map(|e| e.deserialize().ok())
            .rev()
            .enumerate()
        {
            let e_idx = EventIdx(last_event_index - ((i + 1) as u128));
            let e_id = event.event_id();

            index_event.insert(e_idx, e_id.clone());
            self.events.event_index.insert(e_id.clone(), e_idx);
            self.events.event_map.insert(e_id.clone(), slice_idx);
        }
        self.events.slice_map.insert(slice_idx, index_event);
    }
}

// TODO: do this in ruma?
trait EventIdExt {
    fn event_id(&self) -> &EventId;
}

impl EventIdExt for AnySyncRoomEvent {
    fn event_id(&self) -> &EventId {
        match self {
            AnySyncRoomEvent::Message(ev) => ev.event_id(),
            AnySyncRoomEvent::State(ev) => ev.event_id(),
            AnySyncRoomEvent::RedactedMessage(ev) => ev.event_id(),
            AnySyncRoomEvent::RedactedState(ev) => ev.event_id(),
        }
    }
}

impl EventIdExt for AnyRoomEvent {
    fn event_id(&self) -> &EventId {
        match self {
            AnyRoomEvent::Message(ev) => ev.event_id(),
            AnyRoomEvent::State(ev) => ev.event_id(),
            AnyRoomEvent::RedactedMessage(ev) => ev.event_id(),
            AnyRoomEvent::RedactedState(ev) => ev.event_id(),
        }
    }
}

#[cfg(test)]
mod test {
    use matrix_sdk_common::{
        api::r0::{
            account::register::Request as RegistrationRequest,
            directory::get_public_rooms_filtered::Request as PublicRoomsFilterRequest,
            message::get_message_events::{Direction, Response as MessagesResponse},
            sync,
            typing::create_typing_event::Typing,
            uiaa::AuthData,
        },
        assign,
        directory::Filter,
        events::{
            room::message::MessageEventContent, AnyMessageEventContent, AnyRoomEvent,
            AnySyncRoomEvent,
        },
        identifiers::{event_id, room_id, user_id},
        thirdparty, Raw,
    };
    use matrix_sdk_test::{test_json, EventBuilder, EventsJson};
    use serde_json::json;

    use super::Timeline;

    #[test]
    fn messages() {
        let room_id = room_id!("!SVkFJHzfwvuaIEawgC:localhost");

        let messages = serde_json::from_value::<Vec<Raw<AnyRoomEvent>>>(
            test_json::ROOM_MESSAGES["chunk"].clone(),
        )
        .unwrap();
        let sync = serde_json::from_value::<Vec<AnySyncRoomEvent>>(
            test_json::SYNC["rooms"]["join"][room_id.as_str()]["timeline"]["events"].clone(),
        )
        .unwrap();

        let mut timeline = Timeline::new();

        timeline.handle_sync_timeline(
            &room_id,
            "t392-516_47314_0_7_1_1_1_11444_1",
            "s526_47314_0_7_1_1_1_11444_1",
            &sync,
        );

        let mut resp = MessagesResponse::new();
        resp.chunk = messages;
        resp.start = Some("s526_47314_0_7_1_1_1_11444_1".to_owned());
        resp.end = Some("s_end__end".to_owned());

        timeline.handle_messages_response(&room_id, &resp, Direction::Forward);

        println!("{:#?}", timeline);
        // end: t3336-1714379051_757284961_10998365_725145800_588037087_1999191_200821144_689020759_166049
        // start: t3356-1714663804_757284961_10998365_725145800_588037087_1999191_200821144_689020759_166049

        // end: t3316-1714212736_757284961_10998365_725145800_588037087_1999191_200821144_689020759_166049
        // start: t3336-1714379051_757284961_10998365_725145800_588037087_1999191_200821144_689020759_166049
    }
}
