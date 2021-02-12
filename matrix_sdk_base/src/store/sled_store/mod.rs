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

mod store_key;

use std::{collections::BTreeSet, convert::TryFrom, path::Path, sync::Arc, time::SystemTime};

use futures::{
    stream::{self, Stream},
    TryStreamExt,
};
use matrix_sdk_common::{
    api::r0::message::get_message_events::{
        Direction, Request as MessagesRequest, Response as MessagesResponse,
    },
    async_trait,
    events::{
        presence::PresenceEvent,
        room::member::{MemberEventContent, MembershipState},
        AnyMessageEvent, AnyRoomEvent, AnySyncMessageEvent, AnySyncRoomEvent, AnySyncStateEvent,
        EventContent, EventType,
    },
    identifiers::{EventId, RoomId, UserId},
};
use serde::{Deserialize, Serialize};

use sled::{
    transaction::{ConflictableTransactionError, TransactionError},
    Config, Db, Transactional, Tree,
};
use tracing::info;

use crate::{deserialized_responses::MemberEvent, rooms::StrippedRoomInfo};

use self::store_key::{EncryptedEvent, StoreKey};

use super::{Result, RoomInfo, StateChanges, StateStore, StoreError};

#[derive(Debug, Serialize, Deserialize)]
pub enum DatabaseType {
    Unencrypted,
    Encrypted(store_key::EncryptedStoreKey),
}

#[derive(Debug, thiserror::Error)]
pub enum SerializationError {
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Encryption(#[from] store_key::Error),
}

impl From<TransactionError<SerializationError>> for StoreError {
    fn from(e: TransactionError<SerializationError>) -> Self {
        match e {
            TransactionError::Abort(e) => e.into(),
            TransactionError::Storage(e) => StoreError::Sled(e),
        }
    }
}

impl From<SerializationError> for StoreError {
    fn from(e: SerializationError) -> Self {
        match e {
            SerializationError::Json(e) => StoreError::Json(e),
            SerializationError::Encryption(e) => match e {
                store_key::Error::Random(e) => StoreError::Encryption(e.to_string()),
                store_key::Error::Serialization(e) => StoreError::Json(e),
                store_key::Error::Encryption(e) => StoreError::Encryption(e),
            },
        }
    }
}

trait EncodeKey {
    const SEPARATOR: u8 = 0xff;
    fn encode(&self) -> Vec<u8>;
}

impl EncodeKey for &UserId {
    fn encode(&self) -> Vec<u8> {
        self.as_str().encode()
    }
}

impl EncodeKey for &RoomId {
    fn encode(&self) -> Vec<u8> {
        self.as_str().encode()
    }
}

impl EncodeKey for &str {
    fn encode(&self) -> Vec<u8> {
        [self.as_bytes(), &[Self::SEPARATOR]].concat()
    }
}

impl EncodeKey for (&str, &str) {
    fn encode(&self) -> Vec<u8> {
        [
            self.0.as_bytes(),
            &[Self::SEPARATOR],
            self.1.as_bytes(),
            &[Self::SEPARATOR],
        ]
        .concat()
    }
}

impl EncodeKey for (&str, &str, &str) {
    fn encode(&self) -> Vec<u8> {
        [
            self.0.as_bytes(),
            &[Self::SEPARATOR],
            self.1.as_bytes(),
            &[Self::SEPARATOR],
            self.2.as_bytes(),
            &[Self::SEPARATOR],
        ]
        .concat()
    }
}

#[derive(Debug, Clone)]
pub struct SledStore {
    pub(crate) inner: Db,
    store_key: Arc<Option<StoreKey>>,
    session: Tree,
    account_data: Tree,
    members: Tree,
    profiles: Tree,
    display_names: Tree,
    joined_user_ids: Tree,
    invited_user_ids: Tree,
    room_info: Tree,
    room_state: Tree,
    room_account_data: Tree,
    stripped_room_info: Tree,
    stripped_room_state: Tree,
    stripped_members: Tree,
    presence: Tree,

    /// `RoomId` + count/index -> prev_batch token.
    ///
    /// We now have slices of the timeline ordered by position in the timeline. Since the events
    /// come in a predictable order if we find the prevbatchid location we know the location.
    roomcount_prevbatch: Tree,
    /// Since the `prev_batch` token can get long and we want an orderable key use a `u64`.
    /// This is also known as roomcount since the ID is `RoomId` + count.
    prevbatch_roomcount: Tree,
    /// This `Tree` consists of keys of `RoomId` + `prev_batch ID + count` and values of `EventId` bytes.
    ///
    /// `count` is the index of the MessageEvent in the original chunk or sync response.
    prevbatchid_eventid: Tree,
    eventid_prevbatchid: Tree,
    /// This `Tree` is `RoomId + EventId -> Event bytes`
    timeline: Tree,
}

impl SledStore {
    fn open_helper(db: Db, store_key: Option<StoreKey>) -> Result<Self> {
        let session = db.open_tree("session")?;
        let account_data = db.open_tree("account_data")?;

        let members = db.open_tree("members")?;
        let profiles = db.open_tree("profiles")?;
        let display_names = db.open_tree("display_names")?;
        let joined_user_ids = db.open_tree("joined_user_ids")?;
        let invited_user_ids = db.open_tree("invited_user_ids")?;

        let room_state = db.open_tree("room_state")?;
        let room_info = db.open_tree("room_infos")?;
        let presence = db.open_tree("presence")?;
        let room_account_data = db.open_tree("room_account_data")?;

        let stripped_room_info = db.open_tree("stripped_room_info")?;
        let stripped_members = db.open_tree("stripped_members")?;
        let stripped_room_state = db.open_tree("stripped_room_state")?;

        let roomcount_prevbatch = db.open_tree("roomcount_prevbatch")?;
        let prevbatch_roomcount = db.open_tree("prevbatch_roomcount")?;
        let prevbatchid_eventid = db.open_tree("prevbatchid_eventid")?;
        let eventid_prevbatchid = db.open_tree("eventid_prevbatchid")?;
        let timeline = db.open_tree("timeline")?;

        Ok(Self {
            inner: db,
            store_key: store_key.into(),
            session,
            account_data,
            members,
            profiles,
            display_names,
            joined_user_ids,
            invited_user_ids,
            room_account_data,
            presence,
            room_state,
            room_info,
            stripped_room_info,
            stripped_members,
            stripped_room_state,
            roomcount_prevbatch,
            prevbatch_roomcount,
            prevbatchid_eventid,
            eventid_prevbatchid,
            timeline,
        })
    }

    pub fn open() -> Result<Self> {
        let db = Config::new().temporary(true).open()?;

        SledStore::open_helper(db, None)
    }

    pub fn open_with_passphrase(path: impl AsRef<Path>, passphrase: &str) -> Result<Self> {
        let path = path.as_ref().join("matrix-sdk-state");
        let db = Config::new().temporary(false).path(path).open()?;

        let store_key: Option<DatabaseType> = db
            .get("store_key".encode())?
            .map(|k| serde_json::from_slice(&k).map_err(StoreError::Json))
            .transpose()?;

        let store_key = if let Some(key) = store_key {
            if let DatabaseType::Encrypted(k) = key {
                StoreKey::import(passphrase, k).map_err(|_| StoreError::StoreLocked)?
            } else {
                return Err(StoreError::UnencryptedStore);
            }
        } else {
            let key = StoreKey::new().map_err::<StoreError, _>(|e| e.into())?;
            let encrypted_key = DatabaseType::Encrypted(
                key.export(passphrase)
                    .map_err::<StoreError, _>(|e| e.into())?,
            );
            db.insert("store_key".encode(), serde_json::to_vec(&encrypted_key)?)?;
            key
        };

        SledStore::open_helper(db, Some(store_key))
    }

    pub fn open_with_path(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().join("matrix-sdk-state");
        let db = Config::new().temporary(false).path(path).open()?;

        SledStore::open_helper(db, None)
    }

    fn serialize_event(
        &self,
        event: &impl Serialize,
    ) -> std::result::Result<Vec<u8>, SerializationError> {
        if let Some(key) = &*self.store_key {
            let encrypted = key.encrypt(event)?;
            Ok(serde_json::to_vec(&encrypted)?)
        } else {
            Ok(serde_json::to_vec(event)?)
        }
    }

    fn deserialize_event<T: for<'b> Deserialize<'b>>(
        &self,
        event: &[u8],
    ) -> std::result::Result<T, SerializationError> {
        if let Some(key) = &*self.store_key {
            let encrypted: EncryptedEvent = serde_json::from_slice(&event)?;
            Ok(key.decrypt(encrypted)?)
        } else {
            Ok(serde_json::from_slice(event)?)
        }
    }

    pub async fn save_filter(&self, filter_name: &str, filter_id: &str) -> Result<()> {
        self.session
            .insert(("filter", filter_name).encode(), filter_id)?;

        Ok(())
    }

    pub async fn get_filter(&self, filter_name: &str) -> Result<Option<String>> {
        Ok(self
            .session
            .get(("filter", filter_name).encode())?
            .map(|f| String::from_utf8_lossy(&f).to_string()))
    }

    pub async fn get_sync_token(&self) -> Result<Option<String>> {
        Ok(self
            .session
            .get("sync_token".encode())?
            .map(|t| String::from_utf8_lossy(&t).to_string()))
    }

    pub async fn save_changes(&self, changes: &StateChanges) -> Result<()> {
        let now = SystemTime::now();

        let ret: std::result::Result<(), TransactionError<SerializationError>> = (
            &self.session,
            &self.account_data,
            &self.members,
            &self.profiles,
            &self.display_names,
            &self.joined_user_ids,
            &self.invited_user_ids,
            &self.room_info,
            &self.room_state,
            &self.room_account_data,
            &self.presence,
            &self.stripped_room_info,
            &self.stripped_members,
            &self.stripped_room_state,
        )
            .transaction(
                |(
                    session,
                    account_data,
                    members,
                    profiles,
                    display_names,
                    joined,
                    invited,
                    rooms,
                    state,
                    room_account_data,
                    presence,
                    striped_rooms,
                    stripped_members,
                    stripped_state,
                )| {
                    if let Some(s) = &changes.sync_token {
                        session.insert("sync_token".encode(), s.as_str())?;
                    }

                    for (room, events) in &changes.members {
                        let profile_changes = changes.profiles.get(room);

                        for event in events.values() {
                            let key = (room.as_str(), event.state_key.as_str()).encode();

                            match event.content.membership {
                                MembershipState::Join => {
                                    joined.insert(key.as_slice(), event.state_key.as_str())?;
                                    invited.remove(key.as_slice())?;
                                }
                                MembershipState::Invite => {
                                    invited.insert(key.as_slice(), event.state_key.as_str())?;
                                    joined.remove(key.as_slice())?;
                                }
                                _ => {
                                    joined.remove(key.as_slice())?;
                                    invited.remove(key.as_slice())?;
                                }
                            }

                            members.insert(
                                key.as_slice(),
                                self.serialize_event(&event)
                                    .map_err(ConflictableTransactionError::Abort)?,
                            )?;

                            if let Some(profile) =
                                profile_changes.and_then(|p| p.get(&event.state_key))
                            {
                                profiles.insert(
                                    key.as_slice(),
                                    self.serialize_event(&profile)
                                        .map_err(ConflictableTransactionError::Abort)?,
                                )?;
                            }
                        }
                    }

                    for (room_id, ambiguity_maps) in &changes.ambiguity_maps {
                        for (display_name, map) in ambiguity_maps {
                            display_names.insert(
                                (room_id.as_str(), display_name.as_str()).encode(),
                                self.serialize_event(&map)
                                    .map_err(ConflictableTransactionError::Abort)?,
                            )?;
                        }
                    }

                    for (event_type, event) in &changes.account_data {
                        account_data.insert(
                            event_type.as_str().encode(),
                            self.serialize_event(&event)
                                .map_err(ConflictableTransactionError::Abort)?,
                        )?;
                    }

                    for (room, events) in &changes.room_account_data {
                        for (event_type, event) in events {
                            room_account_data.insert(
                                (room.as_str(), event_type.as_str()).encode(),
                                self.serialize_event(&event)
                                    .map_err(ConflictableTransactionError::Abort)?,
                            )?;
                        }
                    }

                    for (room, event_types) in &changes.state {
                        for events in event_types.values() {
                            for event in events.values() {
                                state.insert(
                                    (
                                        room.as_str(),
                                        event.content().event_type(),
                                        event.state_key(),
                                    )
                                        .encode(),
                                    self.serialize_event(&event)
                                        .map_err(ConflictableTransactionError::Abort)?,
                                )?;
                            }
                        }
                    }

                    for (room_id, room_info) in &changes.room_infos {
                        rooms.insert(
                            room_id.encode(),
                            self.serialize_event(room_info)
                                .map_err(ConflictableTransactionError::Abort)?,
                        )?;
                    }

                    for (sender, event) in &changes.presence {
                        presence.insert(
                            sender.encode(),
                            self.serialize_event(&event)
                                .map_err(ConflictableTransactionError::Abort)?,
                        )?;
                    }

                    for (room_id, info) in &changes.invited_room_info {
                        striped_rooms.insert(
                            room_id.encode(),
                            self.serialize_event(&info)
                                .map_err(ConflictableTransactionError::Abort)?,
                        )?;
                    }

                    for (room, events) in &changes.stripped_members {
                        for event in events.values() {
                            stripped_members.insert(
                                (room.as_str(), event.state_key.as_str()).encode(),
                                self.serialize_event(&event)
                                    .map_err(ConflictableTransactionError::Abort)?,
                            )?;
                        }
                    }

                    for (room, event_types) in &changes.stripped_state {
                        for events in event_types.values() {
                            for event in events.values() {
                                stripped_state.insert(
                                    (
                                        room.as_str(),
                                        event.content().event_type(),
                                        event.state_key(),
                                    )
                                        .encode(),
                                    self.serialize_event(&event)
                                        .map_err(ConflictableTransactionError::Abort)?,
                                )?;
                            }
                        }
                    }

                    Ok(())
                },
            );
        ret?;

        for (rid, room) in &changes.sync_timeline {
            let mut tkn = None;

            // A key consists of `RoomId`
            let mut key = rid.as_bytes().to_vec();
            key.push(0xff);

            // plus index of prev_batch
            let count = self
                .roomcount_prevbatch
                .scan_prefix(rid.as_bytes())
                .last()
                .map(|pair| {
                    let bytes = pair.unwrap().0;
                    u64_from_bytes(&bytes).expect("saved invalid numeric key")
                })
                .unwrap_or_else(|| u64::MAX / 2)
                + 1;

            key.extend_from_slice(&count.to_be_bytes());

            for ((prev, idx), msg) in room {
                if tkn.is_none() {
                    tkn = Some(prev);
                }

                let mut k = key.to_vec();
                k.push(0xff);
                k.extend_from_slice(&idx.to_be_bytes());

                self.prevbatchid_eventid
                    .insert(k.as_slice(), msg.event_id().as_bytes())?;
                self.eventid_prevbatchid
                    .insert(msg.event_id().as_bytes(), k.as_slice())?;
                self.timeline.insert(
                    (rid.as_str(), msg.event_id().as_str()).encode(),
                    self.serialize_event(msg)?,
                )?;
            }

            if let Some(tkn) = tkn {
                self.roomcount_prevbatch
                    .insert(key.as_slice(), tkn.as_bytes())?;
                self.prevbatch_roomcount
                    .insert(tkn.as_bytes(), key.as_slice())?;
            }
        }

        for (rid, room) in &changes.messages_timeline {
            let mut tkn = None;

            // A key starts with `RoomId
            let mut key = rid.as_bytes().to_vec();
            key.push(0xff);

            // plus the bytes of the index of the prev_batch token
            let count = self
                        .roomcount_prevbatch
                        .scan_prefix(rid.as_bytes())
                        .next()
                        .map(|pair| {
                            let bytes = pair.unwrap().0;
                            u64_from_bytes(&bytes).expect("saved invalid numeric key")
                        })
                        .unwrap_or_else(|| u64::MAX / 2)
                        // So we know this is one less than the last prev_batch token in the stream
                        - 1;
            key.extend_from_slice(&count.to_be_bytes());

            for ((prev, idx), msg) in room {
                if tkn.is_none() {
                    tkn = Some(prev);
                }

                let mut k = key.to_vec();
                k.push(0xff);
                k.extend_from_slice(&idx.to_be_bytes());

                self.prevbatchid_eventid
                    .insert(k.as_slice(), msg.event_id().as_bytes())?;
                self.eventid_prevbatchid
                    .insert(msg.event_id().as_bytes(), k.as_slice())?;

                self.timeline.insert(
                    (rid.as_str(), msg.event_id().as_str()).encode(),
                    self.serialize_event(msg)?,
                )?;
            }

            if let Some(tkn) = tkn {
                self.roomcount_prevbatch
                    .insert(key.as_slice(), tkn.as_bytes())?;
                self.prevbatch_roomcount
                    .insert(tkn.as_bytes(), key.as_slice())?;
            }
        }

        self.inner.flush_async().await?;

        info!("Saved changes in {:?}", now.elapsed());

        Ok(())
    }

    pub async fn get_presence_event(&self, user_id: &UserId) -> Result<Option<PresenceEvent>> {
        Ok(self
            .presence
            .get(user_id.encode())?
            .map(|e| self.deserialize_event(&e))
            .transpose()?)
    }

    pub async fn get_state_event(
        &self,
        room_id: &RoomId,
        event_type: EventType,
        state_key: &str,
    ) -> Result<Option<AnySyncStateEvent>> {
        Ok(self
            .room_state
            .get((room_id.as_str(), event_type.to_string().as_str(), state_key).encode())?
            .map(|e| self.deserialize_event(&e))
            .transpose()?)
    }

    pub async fn get_profile(
        &self,
        room_id: &RoomId,
        user_id: &UserId,
    ) -> Result<Option<MemberEventContent>> {
        Ok(self
            .profiles
            .get((room_id.as_str(), user_id.as_str()).encode())?
            .map(|p| self.deserialize_event(&p))
            .transpose()?)
    }

    pub async fn get_member_event(
        &self,
        room_id: &RoomId,
        state_key: &UserId,
    ) -> Result<Option<MemberEvent>> {
        Ok(self
            .members
            .get((room_id.as_str(), state_key.as_str()).encode())?
            .map(|v| self.deserialize_event(&v))
            .transpose()?)
    }

    pub async fn get_invited_user_ids(
        &self,
        room_id: &RoomId,
    ) -> impl Stream<Item = Result<UserId>> {
        stream::iter(
            self.invited_user_ids
                .scan_prefix(room_id.encode())
                .map(|u| {
                    UserId::try_from(String::from_utf8_lossy(&u?.1).to_string())
                        .map_err(StoreError::Identifier)
                }),
        )
    }

    pub async fn get_joined_user_ids(
        &self,
        room_id: &RoomId,
    ) -> impl Stream<Item = Result<UserId>> {
        stream::iter(self.joined_user_ids.scan_prefix(room_id.encode()).map(|u| {
            UserId::try_from(String::from_utf8_lossy(&u?.1).to_string())
                .map_err(StoreError::Identifier)
        }))
    }

    pub async fn get_room_infos(&self) -> impl Stream<Item = Result<RoomInfo>> {
        let db = self.clone();
        stream::iter(
            self.room_info
                .iter()
                .map(move |r| db.deserialize_event(&r?.1).map_err(|e| e.into())),
        )
    }

    pub async fn get_stripped_room_infos(&self) -> impl Stream<Item = Result<StrippedRoomInfo>> {
        let db = self.clone();
        stream::iter(
            self.stripped_room_info
                .iter()
                .map(move |r| db.deserialize_event(&r?.1).map_err(|e| e.into())),
        )
    }

    pub async fn get_messages(
        &self,
        room_id: &RoomId,
        prev_batch: &str,
    ) -> impl Stream<Item = Result<AnySyncMessageEvent>> {
        let timeline = self.timeline.clone();
        let room_id = room_id.clone();
        let key = self
            .prevbatch_roomcount
            .get(prev_batch.as_bytes())
            .unwrap()
            .unwrap();
        stream::iter(
            self.prevbatchid_eventid
                .scan_prefix(key)
                .values()
                .map(move |id| {
                    let mut key = room_id.as_bytes().to_vec();
                    key.push(0xff);
                    key.extend_from_slice(&id?);
                    key.push(0xff);

                    timeline.get(key)
                })
                .flat_map(|bytes| {
                    if let Ok(bytes) = bytes {
                        Some(serde_json::from_slice(&bytes?).map_err(StoreError::Json))
                    } else {
                        None
                    }
                }), // timeline.iter().values(),
        )
    }

    pub async fn get_users_with_display_name(
        &self,
        room_id: &RoomId,
        display_name: &str,
    ) -> Result<BTreeSet<UserId>> {
        let key = (room_id.as_str(), display_name).encode();

        Ok(self
            .display_names
            .get(key)?
            .map(|m| self.deserialize_event(&m))
            .transpose()?
            .unwrap_or_default())
    }

    pub async fn unknown_timeline_events<'a>(
        &'a self,
        _: &RoomId,
        prev_batch: &str,
        events: &'a [AnySyncRoomEvent],
    ) -> Result<&'a [AnySyncRoomEvent]> {
        // On the off chance that we get half way through a chunk
        if let Some(found) = self
            .prevbatch_roomcount
            .get(prev_batch.as_bytes())?
            .map(|id| self.prevbatchid_eventid.scan_prefix(id).last())
            .flatten()
        {
            let (k, _) = found?;
            let start =
                u64_from_bytes(k.splitn(2, |b| *b == 0xff).last().unwrap()).unwrap() as usize;

            return Ok(&events[start..]);
        }

        Ok(&events[..])
    }
}

/// Parses the bytes into an u64.
pub fn u64_from_bytes(bytes: &[u8]) -> std::result::Result<u64, std::array::TryFromSliceError> {
    use std::convert::TryInto;
    let array: [u8; 8] = bytes.try_into()?;
    Ok(u64::from_be_bytes(array))
}

#[async_trait]
impl StateStore for SledStore {
    async fn save_filter(&self, filter_name: &str, filter_id: &str) -> Result<()> {
        self.save_filter(filter_name, filter_id).await
    }

    async fn save_changes(&self, changes: &StateChanges) -> Result<()> {
        self.save_changes(changes).await
    }

    async fn get_filter(&self, filter_id: &str) -> Result<Option<String>> {
        self.get_filter(filter_id).await
    }

    async fn get_sync_token(&self) -> Result<Option<String>> {
        self.get_sync_token().await
    }

    async fn get_presence_event(&self, user_id: &UserId) -> Result<Option<PresenceEvent>> {
        self.get_presence_event(user_id).await
    }

    async fn get_state_event(
        &self,
        room_id: &RoomId,
        event_type: EventType,
        state_key: &str,
    ) -> Result<Option<AnySyncStateEvent>> {
        self.get_state_event(room_id, event_type, state_key).await
    }

    async fn get_profile(
        &self,
        room_id: &RoomId,
        user_id: &UserId,
    ) -> Result<Option<MemberEventContent>> {
        self.get_profile(room_id, user_id).await
    }

    async fn get_member_event(
        &self,
        room_id: &RoomId,
        state_key: &UserId,
    ) -> Result<Option<MemberEvent>> {
        self.get_member_event(room_id, state_key).await
    }

    async fn get_invited_user_ids(&self, room_id: &RoomId) -> Result<Vec<UserId>> {
        self.get_invited_user_ids(room_id).await.try_collect().await
    }

    async fn get_joined_user_ids(&self, room_id: &RoomId) -> Result<Vec<UserId>> {
        self.get_joined_user_ids(room_id).await.try_collect().await
    }

    async fn get_room_infos(&self) -> Result<Vec<RoomInfo>> {
        self.get_room_infos().await.try_collect().await
    }

    async fn get_stripped_room_infos(&self) -> Result<Vec<StrippedRoomInfo>> {
        self.get_stripped_room_infos().await.try_collect().await
    }

    async fn get_users_with_display_name(
        &self,
        room_id: &RoomId,
        display_name: &str,
    ) -> Result<BTreeSet<UserId>> {
        self.get_users_with_display_name(room_id, display_name)
            .await
    }

    async fn get_messages(
        &self,
        room_id: &RoomId,
        prev_batch: &str,
    ) -> Result<Vec<AnySyncMessageEvent>> {
        self.get_messages(room_id, prev_batch)
            .await
            .try_collect()
            .await
    }

    async fn unknown_timeline_events<'a>(
        &'a self,
        room_id: &RoomId,
        prev_batch: &str,
        events: &'a [AnySyncRoomEvent],
    ) -> Result<&'a [AnySyncRoomEvent]> {
        self.unknown_timeline_events(room_id, prev_batch, events)
            .await
    }

    async fn contains_timeline_events(
        &self,
        room_id: &RoomId,
        request: &MessagesRequest<'_>,
    ) -> Result<Option<(String, Vec<AnyRoomEvent>)>> {
        match request.dir {
            // The end token is how to request older events
            // events are in reverse-chronological order
            Direction::Backward => {
                // We always save according to the oldest event prev_batch token
                if let Some(bytes) = self.prevbatch_roomcount.get(request.from.as_bytes())? {
                    if let Some(count) = bytes
                        .splitn(2, |b| *b == 0xff)
                        .next()
                        .map(|b| u64_from_bytes(b).expect("bad bytes in DB"))
                    {
                        let mut prefix = room_id.as_bytes().to_vec();
                        prefix.push(0xff);
                        prefix.extend_from_slice(&(count - 1).to_be_bytes());

                        let token = if let Some(tkn) = self.roomcount_prevbatch.get(&prefix)? {
                            String::from_utf8_lossy(&tkn).to_string()
                        } else {
                            return Ok(None);
                        };

                        let events_from_db = self
                            .prevbatchid_eventid
                            .scan_prefix(prefix)
                            .values()
                            .flat_map(|id| self.timeline.get(id.ok()?).transpose())
                            .map(|bytes| {
                                self.deserialize_event(&bytes.unwrap()).map_err(Into::into)
                            })
                            .collect::<Result<Vec<_>>>()?;

                        if events_from_db.is_empty() {
                            return Ok(None);
                        }

                        return Ok(Some((token, events_from_db)));
                    }
                }
                Ok(None)
            }
            Direction::Forward => {
                if let Some(bytes) = self.prevbatch_roomcount.get(request.from.as_bytes())? {
                    if let Some(count) = bytes
                        .splitn(2, |b| *b == 0xff)
                        .next()
                        .map(|b| u64_from_bytes(b).expect("bad bytes in DB"))
                    {
                        let mut prefix = room_id.as_bytes().to_vec();
                        prefix.push(0xff);
                        prefix.extend_from_slice(&(count + 1).to_be_bytes());

                        let token = if let Some(tkn) = self.roomcount_prevbatch.get(&prefix)? {
                            String::from_utf8_lossy(&tkn).to_string()
                        } else {
                            return Ok(None);
                        };

                        let events_from_db = self
                            .prevbatchid_eventid
                            .scan_prefix(prefix)
                            .values()
                            .flat_map(|id| self.timeline.get(id.ok()?).transpose())
                            .map(|bytes| {
                                self.deserialize_event(&bytes.unwrap()).map_err(Into::into)
                            })
                            .collect::<Result<Vec<_>>>()?;

                        if events_from_db.is_empty() {
                            return Ok(None);
                        }

                        return Ok(Some((token, events_from_db)));
                    }
                }
                Ok(None)
            }
        }
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
    use std::{convert::TryFrom, time::SystemTime};

    use matrix_sdk_common::{
        events::{
            room::member::{MemberEventContent, MembershipState},
            Unsigned,
        },
        identifiers::{room_id, user_id, EventId, UserId},
    };
    use matrix_sdk_test::async_test;

    use super::{u64_from_bytes, SledStore, StateChanges};
    use crate::deserialized_responses::MemberEvent;

    fn user_id() -> UserId {
        user_id!("@example:localhost")
    }

    fn membership_event() -> MemberEvent {
        let content = MemberEventContent {
            avatar_url: None,
            displayname: None,
            is_direct: None,
            third_party_invite: None,
            membership: MembershipState::Join,
        };

        MemberEvent {
            event_id: EventId::try_from("$h29iv0s8:example.com").unwrap(),
            content,
            sender: user_id(),
            origin_server_ts: SystemTime::now(),
            state_key: user_id(),
            prev_content: None,
            unsigned: Unsigned::default(),
        }
    }

    #[async_test]
    async fn test_member_saving() {
        let store = SledStore::open().unwrap();
        let room_id = room_id!("!test:localhost");
        let user_id = user_id();

        assert!(store
            .get_member_event(&room_id, &user_id)
            .await
            .unwrap()
            .is_none());
        let mut changes = StateChanges::default();
        changes
            .members
            .entry(room_id.clone())
            .or_default()
            .insert(user_id.clone(), membership_event());

        store.save_changes(&changes).await.unwrap();
        assert!(store
            .get_member_event(&room_id, &user_id)
            .await
            .unwrap()
            .is_some());
    }

    #[test]
    fn bytes_neg() {
        use super::u64_from_bytes;

        let mut list = vec![];

        let mut mid = 0_u64.to_be_bytes().to_vec();
        mid.insert(0, b'z');
        list.push(mid);

        let mut end = 5_u64.to_be_bytes().to_vec();
        end.insert(0, b'z');
        list.push(end);

        let mut start = 5_u64.to_be_bytes().to_vec();
        start.insert(0, 0xff);
        start.insert(0, b'a');
        list.push(start);

        println!("{:?}", list);
        list.sort();

        println!("{:?}", list);

        panic!(
            "{:?}",
            list.iter()
                .map(|b| u64_from_bytes(b).unwrap())
                .collect::<Vec<u64>>()
        );
    }

    #[test]
    fn bytes() {
        use super::u64_from_bytes;

        let mut list = vec![
            10_u64.to_be_bytes().to_vec(),
            10134_u64.to_be_bytes().to_vec(),
            5_u64.to_be_bytes().to_vec(),
            6_u64.to_be_bytes().to_vec(),
        ];

        let mut val = 6_u64.to_be_bytes().to_vec();
        val.push(1);
        list.push(val);
        let mut val = 6_u64.to_be_bytes().to_vec();
        val.push(1);
        val.push(1);
        list.push(val);

        println!("{:?}", list);
        list.sort();

        println!("{:?}", list);

        println!(
            "{:?}",
            list.iter()
                .map(|b| u64_from_bytes(b).unwrap())
                .collect::<Vec<u64>>()
        );
    }
}
