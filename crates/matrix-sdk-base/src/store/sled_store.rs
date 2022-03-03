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
    collections::BTreeSet,
    convert::TryInto,
    path::{Path, PathBuf},
    sync::Arc,
    time::Instant,
};

use async_stream::stream;
use futures_core::stream::Stream;
use futures_util::stream::{self, TryStreamExt};
use matrix_sdk_common::async_trait;
use ruma::{
    events::{
        presence::PresenceEvent,
        receipt::Receipt,
        room::member::{MembershipState, RoomMemberEventContent},
        AnyGlobalAccountDataEvent, AnyRoomAccountDataEvent, AnySyncMessageEvent, AnySyncRoomEvent,
        AnySyncStateEvent, EventType,
    },
    receipt::ReceiptType,
    serde::Raw,
    signatures::{redact_in_place, CanonicalJsonObject},
    EventId, MxcUri, RoomId, RoomVersionId, UserId,
};
use serde::{Deserialize, Serialize};
use sled::{
    transaction::{ConflictableTransactionError, TransactionError},
    Config, Db, Transactional, Tree,
};
use tokio::task::spawn_blocking;
use tracing::{info, warn};

use self::store_key::{EncryptedEvent, StoreKey};
use super::{store_key, BoxStream, Result, RoomInfo, StateChanges, StateStore, StoreError};
use crate::{
    deserialized_responses::{MemberEvent, SyncRoomEvent},
    media::{MediaRequest, UniqueKey},
};

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

const ENCODE_SEPARATOR: u8 = 0xff;

trait EncodeKey {
    fn encode(&self) -> Vec<u8>;
}

impl<T: EncodeKey> EncodeKey for &T {
    fn encode(&self) -> Vec<u8> {
        T::encode(self)
    }
}

impl<T: EncodeKey> EncodeKey for Box<T> {
    fn encode(&self) -> Vec<u8> {
        T::encode(self)
    }
}

impl EncodeKey for UserId {
    fn encode(&self) -> Vec<u8> {
        self.as_str().encode()
    }
}

impl EncodeKey for RoomId {
    fn encode(&self) -> Vec<u8> {
        self.as_str().encode()
    }
}

impl EncodeKey for String {
    fn encode(&self) -> Vec<u8> {
        self.as_str().encode()
    }
}

impl EncodeKey for str {
    fn encode(&self) -> Vec<u8> {
        [self.as_bytes(), &[ENCODE_SEPARATOR]].concat()
    }
}

impl EncodeKey for (&str, &str) {
    fn encode(&self) -> Vec<u8> {
        [self.0.as_bytes(), &[ENCODE_SEPARATOR], self.1.as_bytes(), &[ENCODE_SEPARATOR]].concat()
    }
}

impl EncodeKey for (&str, &str, &str) {
    fn encode(&self) -> Vec<u8> {
        [
            self.0.as_bytes(),
            &[ENCODE_SEPARATOR],
            self.1.as_bytes(),
            &[ENCODE_SEPARATOR],
            self.2.as_bytes(),
            &[ENCODE_SEPARATOR],
        ]
        .concat()
    }
}

impl EncodeKey for (&str, &str, &str, &str) {
    fn encode(&self) -> Vec<u8> {
        [
            self.0.as_bytes(),
            &[ENCODE_SEPARATOR],
            self.1.as_bytes(),
            &[ENCODE_SEPARATOR],
            self.2.as_bytes(),
            &[ENCODE_SEPARATOR],
            self.3.as_bytes(),
            &[ENCODE_SEPARATOR],
        ]
        .concat()
    }
}

impl EncodeKey for EventType {
    fn encode(&self) -> Vec<u8> {
        self.as_str().encode()
    }
}

impl EncodeKey for EventId {
    fn encode(&self) -> Vec<u8> {
        self.as_str().encode()
    }
}

impl EncodeKey for (&RoomId, usize) {
    fn encode(&self) -> Vec<u8> {
        [self.0.as_bytes(), &[ENCODE_SEPARATOR], self.1.to_be_bytes().as_ref(), &[ENCODE_SEPARATOR]]
            .concat()
    }
}

/// Get the value at `position` in encoded `key`.
///
/// The key must have been encoded with the `EncodeKey` trait. `position`
/// corresponds to the position in the tuple before the key was encoded. If it
/// wasn't encoded in a tuple, use `0`.
///
/// Returns `None` if there is no key at `position`.
pub fn decode_key_value(key: &[u8], position: usize) -> Option<String> {
    let values: Vec<&[u8]> = key.split(|v| *v == ENCODE_SEPARATOR).collect();

    values.get(position).map(|s| String::from_utf8_lossy(s).to_string())
}

#[derive(Clone)]
pub struct SledStore {
    path: Option<PathBuf>,
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
    stripped_room_infos: Tree,
    stripped_room_state: Tree,
    stripped_members: Tree,
    presence: Tree,
    room_user_receipts: Tree,
    room_event_receipts: Tree,
    media: Tree,
    custom: Tree,
    room_timeline: Tree,
    room_timeline_metadata: Tree,
    room_event_id_to_position: Tree,
}

impl std::fmt::Debug for SledStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(path) = &self.path {
            f.debug_struct("SledStore").field("path", &path).finish()
        } else {
            f.debug_struct("SledStore").field("path", &"memory store").finish()
        }
    }
}

impl SledStore {
    fn open_helper(db: Db, path: Option<PathBuf>, store_key: Option<StoreKey>) -> Result<Self> {
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

        let stripped_room_infos = db.open_tree("stripped_room_infos")?;
        let stripped_members = db.open_tree("stripped_members")?;
        let stripped_room_state = db.open_tree("stripped_room_state")?;

        let room_user_receipts = db.open_tree("room_user_receipts")?;
        let room_event_receipts = db.open_tree("room_event_receipts")?;

        let media = db.open_tree("media")?;

        let custom = db.open_tree("custom")?;

        let room_timeline = db.open_tree("room_timeline")?;
        let room_timeline_metadata = db.open_tree("room_timeline_metadata")?;
        let room_event_id_to_position = db.open_tree("room_event_id_to_position")?;

        Ok(Self {
            path,
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
            stripped_room_infos,
            stripped_members,
            stripped_room_state,
            room_user_receipts,
            room_event_receipts,
            media,
            custom,
            room_timeline,
            room_timeline_metadata,
            room_event_id_to_position,
        })
    }

    pub fn open() -> Result<Self> {
        let db = Config::new().temporary(true).open()?;

        SledStore::open_helper(db, None, None)
    }

    pub fn open_with_passphrase(path: impl AsRef<Path>, passphrase: &str) -> Result<Self> {
        let path = path.as_ref().join("matrix-sdk-state");
        let db = Config::new().temporary(false).path(&path).open()?;

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
                key.export(passphrase).map_err::<StoreError, _>(|e| e.into())?,
            );
            db.insert("store_key".encode(), serde_json::to_vec(&encrypted_key)?)?;
            key
        };

        SledStore::open_helper(db, Some(path), Some(store_key))
    }

    pub fn open_with_path(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().join("matrix-sdk-state");
        let db = Config::new().temporary(false).path(&path).open()?;

        SledStore::open_helper(db, Some(path), None)
    }

    fn serialize_event(&self, event: &impl Serialize) -> Result<Vec<u8>, SerializationError> {
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
    ) -> Result<T, SerializationError> {
        if let Some(key) = &*self.store_key {
            let encrypted: EncryptedEvent = serde_json::from_slice(event)?;
            Ok(key.decrypt(encrypted)?)
        } else {
            Ok(serde_json::from_slice(event)?)
        }
    }

    pub async fn save_filter(&self, filter_name: &str, filter_id: &str) -> Result<()> {
        self.session.insert(("filter", filter_name).encode(), filter_id)?;

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
        let now = Instant::now();

        let ret: Result<(), TransactionError<SerializationError>> = (
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
            &self.stripped_room_infos,
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
                            event_type.encode(),
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
                        for (event_type, events) in event_types {
                            for (state_key, event) in events {
                                state.insert(
                                    (room.as_str(), event_type.as_str(), state_key.as_str())
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

                    for (room_id, info) in &changes.stripped_room_infos {
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
                        for (event_type, events) in event_types {
                            for (state_key, event) in events {
                                stripped_state.insert(
                                    (room.as_str(), event_type.as_str(), state_key.as_str())
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

        let ret: Result<(), TransactionError<SerializationError>> =
            (&self.room_user_receipts, &self.room_event_receipts).transaction(
                |(room_user_receipts, room_event_receipts)| {
                    for (room, content) in &changes.receipts {
                        for (event_id, receipts) in &content.0 {
                            for (receipt_type, receipts) in receipts {
                                for (user_id, receipt) in receipts {
                                    // Add the receipt to the room user receipts
                                    if let Some(old) = room_user_receipts.insert(
                                        (room.as_str(), receipt_type.as_ref(), user_id.as_str())
                                            .encode(),
                                        self.serialize_event(&(event_id, receipt))
                                            .map_err(ConflictableTransactionError::Abort)?,
                                    )? {
                                        // Remove the old receipt from the room event receipts
                                        let (old_event, _): (Box<EventId>, Receipt) = self
                                            .deserialize_event(&old)
                                            .map_err(ConflictableTransactionError::Abort)?;
                                        room_event_receipts.remove(
                                            (
                                                room.as_str(),
                                                receipt_type.as_ref(),
                                                old_event.as_str(),
                                                user_id.as_str(),
                                            )
                                                .encode(),
                                        )?;
                                    }

                                    // Add the receipt to the room event receipts
                                    room_event_receipts.insert(
                                        (
                                            room.as_str(),
                                            receipt_type.as_ref(),
                                            event_id.as_str(),
                                            user_id.as_str(),
                                        )
                                            .encode(),
                                        self.serialize_event(receipt)
                                            .map_err(ConflictableTransactionError::Abort)?,
                                    )?;
                                }
                            }
                        }
                    }

                    Ok(())
                },
            );

        ret?;

        self.save_room_timeline(changes).await?;

        self.inner.flush_async().await?;

        info!("Saved changes in {:?}", now.elapsed());

        Ok(())
    }

    pub async fn get_presence_event(&self, user_id: &UserId) -> Result<Option<Raw<PresenceEvent>>> {
        let db = self.clone();
        let key = user_id.encode();
        spawn_blocking(move || {
            Ok(db.presence.get(key)?.map(|e| db.deserialize_event(&e)).transpose()?)
        })
        .await?
    }

    pub async fn get_state_event(
        &self,
        room_id: &RoomId,
        event_type: EventType,
        state_key: &str,
    ) -> Result<Option<Raw<AnySyncStateEvent>>> {
        let db = self.clone();
        let key = (room_id.as_str(), event_type.as_str(), state_key).encode();
        spawn_blocking(move || {
            Ok(db.room_state.get(key)?.map(|e| db.deserialize_event(&e)).transpose()?)
        })
        .await?
    }

    pub async fn get_state_events(
        &self,
        room_id: &RoomId,
        event_type: EventType,
    ) -> Result<Vec<Raw<AnySyncStateEvent>>> {
        let db = self.clone();
        let key = (room_id.as_str(), event_type.as_str()).encode();
        spawn_blocking(move || {
            Ok(db
                .room_state
                .scan_prefix(key)
                .flat_map(|e| e.map(|(_, e)| db.deserialize_event(&e)))
                .collect::<Result<_, _>>()?)
        })
        .await?
    }

    pub async fn get_profile(
        &self,
        room_id: &RoomId,
        user_id: &UserId,
    ) -> Result<Option<RoomMemberEventContent>> {
        let db = self.clone();
        let key = (room_id.as_str(), user_id.as_str()).encode();
        spawn_blocking(move || {
            Ok(db.profiles.get(key)?.map(|p| db.deserialize_event(&p)).transpose()?)
        })
        .await?
    }

    pub async fn get_member_event(
        &self,
        room_id: &RoomId,
        state_key: &UserId,
    ) -> Result<Option<MemberEvent>> {
        let db = self.clone();
        let key = (room_id.as_str(), state_key.as_str()).encode();
        spawn_blocking(move || {
            Ok(db.members.get(key)?.map(|v| db.deserialize_event(&v)).transpose()?)
        })
        .await?
    }

    pub async fn get_user_ids_stream(
        &self,
        room_id: &RoomId,
    ) -> Result<impl Stream<Item = Result<Box<UserId>>>> {
        let decode = |key: &[u8]| -> Result<Box<UserId>> {
            let mut iter = key.split(|c| c == &ENCODE_SEPARATOR);
            // Our key is a the room id separated from the user id by a null
            // byte, discard the first value of the split.
            iter.next();

            let user_id = iter.next().expect("User ids weren't properly encoded");

            Ok(UserId::parse(String::from_utf8_lossy(user_id).to_string())?)
        };

        let members = self.members.clone();
        let key = room_id.encode();

        spawn_blocking(move || stream::iter(members.scan_prefix(key).map(move |u| decode(&u?.0))))
            .await
            .map_err(Into::into)
    }

    pub async fn get_invited_user_ids(
        &self,
        room_id: &RoomId,
    ) -> Result<impl Stream<Item = Result<Box<UserId>>>> {
        let db = self.clone();
        let key = room_id.encode();
        spawn_blocking(move || {
            stream::iter(db.invited_user_ids.scan_prefix(key).map(|u| {
                UserId::parse(String::from_utf8_lossy(&u?.1).to_string())
                    .map_err(StoreError::Identifier)
            }))
        })
        .await
        .map_err(Into::into)
    }

    pub async fn get_joined_user_ids(
        &self,
        room_id: &RoomId,
    ) -> Result<impl Stream<Item = Result<Box<UserId>>>> {
        let db = self.clone();
        let key = room_id.encode();
        spawn_blocking(move || {
            stream::iter(db.joined_user_ids.scan_prefix(key).map(|u| {
                UserId::parse(String::from_utf8_lossy(&u?.1).to_string())
                    .map_err(StoreError::Identifier)
            }))
        })
        .await
        .map_err(Into::into)
    }

    pub async fn get_room_infos(&self) -> Result<impl Stream<Item = Result<RoomInfo>>> {
        let db = self.clone();
        spawn_blocking(move || {
            stream::iter(
                db.room_info.iter().map(move |r| db.deserialize_event(&r?.1).map_err(|e| e.into())),
            )
        })
        .await
        .map_err(Into::into)
    }

    pub async fn get_stripped_room_infos(&self) -> Result<impl Stream<Item = Result<RoomInfo>>> {
        let db = self.clone();
        spawn_blocking(move || {
            stream::iter(
                db.stripped_room_infos
                    .iter()
                    .map(move |r| db.deserialize_event(&r?.1).map_err(|e| e.into())),
            )
        })
        .await
        .map_err(Into::into)
    }

    pub async fn get_users_with_display_name(
        &self,
        room_id: &RoomId,
        display_name: &str,
    ) -> Result<BTreeSet<Box<UserId>>> {
        let db = self.clone();
        let key = (room_id.as_str(), display_name).encode();
        spawn_blocking(move || {
            Ok(db
                .display_names
                .get(key)?
                .map(|m| db.deserialize_event(&m))
                .transpose()?
                .unwrap_or_default())
        })
        .await?
    }

    pub async fn get_account_data_event(
        &self,
        event_type: EventType,
    ) -> Result<Option<Raw<AnyGlobalAccountDataEvent>>> {
        let db = self.clone();
        let key = event_type.encode();
        spawn_blocking(move || {
            Ok(db.account_data.get(key)?.map(|m| db.deserialize_event(&m)).transpose()?)
        })
        .await?
    }

    pub async fn get_room_account_data_event(
        &self,
        room_id: &RoomId,
        event_type: EventType,
    ) -> Result<Option<Raw<AnyRoomAccountDataEvent>>> {
        let db = self.clone();
        let key = (room_id.as_str(), event_type.as_str()).encode();
        spawn_blocking(move || {
            Ok(db.room_account_data.get(key)?.map(|m| db.deserialize_event(&m)).transpose()?)
        })
        .await?
    }

    async fn get_user_room_receipt_event(
        &self,
        room_id: &RoomId,
        receipt_type: ReceiptType,
        user_id: &UserId,
    ) -> Result<Option<(Box<EventId>, Receipt)>> {
        let db = self.clone();
        let key = (room_id.as_str(), receipt_type.as_ref(), user_id.as_str()).encode();
        spawn_blocking(move || {
            Ok(db.room_user_receipts.get(key)?.map(|m| db.deserialize_event(&m)).transpose()?)
        })
        .await?
    }

    async fn get_event_room_receipt_events(
        &self,
        room_id: &RoomId,
        receipt_type: ReceiptType,
        event_id: &EventId,
    ) -> Result<Vec<(Box<UserId>, Receipt)>> {
        let db = self.clone();
        let key = (room_id.as_str(), receipt_type.as_ref(), event_id.as_str()).encode();
        spawn_blocking(move || {
            db.room_event_receipts
                .scan_prefix(key)
                .map(|u| {
                    u.map_err(StoreError::Sled).and_then(|(key, value)| {
                        db.deserialize_event(&value)
                            // TODO remove this unwrapping
                            .map(|receipt| {
                                (decode_key_value(&key, 3).unwrap().try_into().unwrap(), receipt)
                            })
                            .map_err(Into::into)
                    })
                })
                .collect()
        })
        .await?
    }

    async fn add_media_content(&self, request: &MediaRequest, data: Vec<u8>) -> Result<()> {
        self.media.insert(
            (request.media_type.unique_key().as_str(), request.format.unique_key().as_str())
                .encode(),
            data,
        )?;

        self.inner.flush_async().await?;

        Ok(())
    }

    async fn get_media_content(&self, request: &MediaRequest) -> Result<Option<Vec<u8>>> {
        let db = self.clone();
        let key = (request.media_type.unique_key().as_str(), request.format.unique_key().as_str())
            .encode();

        spawn_blocking(move || Ok(db.media.get(key)?.map(|m| m.to_vec()))).await?
    }

    async fn get_custom_value(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        let custom = self.custom.clone();
        let key = key.to_owned();
        spawn_blocking(move || Ok(custom.get(key)?.map(|v| v.to_vec()))).await?
    }

    async fn set_custom_value(&self, key: &[u8], value: Vec<u8>) -> Result<Option<Vec<u8>>> {
        let ret = self.custom.insert(key, value)?.map(|v| v.to_vec());
        self.inner.flush_async().await?;

        Ok(ret)
    }

    async fn remove_media_content(&self, request: &MediaRequest) -> Result<()> {
        self.media.remove(
            (request.media_type.unique_key().as_str(), request.format.unique_key().as_str())
                .encode(),
        )?;

        Ok(())
    }

    async fn remove_media_content_for_uri(&self, uri: &MxcUri) -> Result<()> {
        let keys = self.media.scan_prefix(uri.as_str().encode()).keys();

        let mut batch = sled::Batch::default();
        for key in keys {
            batch.remove(key?);
        }

        Ok(self.media.apply_batch(batch)?)
    }

    async fn remove_room(&self, room_id: &RoomId) -> Result<()> {
        let room_key = room_id.encode();

        let mut members_batch = sled::Batch::default();
        for key in self.members.scan_prefix(room_key.as_slice()).keys() {
            members_batch.remove(key?)
        }

        let mut profiles_batch = sled::Batch::default();
        for key in self.profiles.scan_prefix(room_key.as_slice()).keys() {
            profiles_batch.remove(key?)
        }

        let mut display_names_batch = sled::Batch::default();
        for key in self.display_names.scan_prefix(room_key.as_slice()).keys() {
            display_names_batch.remove(key?)
        }

        let mut joined_user_ids_batch = sled::Batch::default();
        for key in self.joined_user_ids.scan_prefix(room_key.as_slice()).keys() {
            joined_user_ids_batch.remove(key?)
        }

        let mut invited_user_ids_batch = sled::Batch::default();
        for key in self.invited_user_ids.scan_prefix(room_key.as_slice()).keys() {
            invited_user_ids_batch.remove(key?)
        }

        let mut room_state_batch = sled::Batch::default();
        for key in self.room_state.scan_prefix(room_key.as_slice()).keys() {
            room_state_batch.remove(key?)
        }

        let mut room_account_data_batch = sled::Batch::default();
        for key in self.room_account_data.scan_prefix(room_key.as_slice()).keys() {
            room_account_data_batch.remove(key?)
        }

        let mut stripped_members_batch = sled::Batch::default();
        for key in self.stripped_members.scan_prefix(room_key.as_slice()).keys() {
            stripped_members_batch.remove(key?)
        }

        let mut stripped_room_state_batch = sled::Batch::default();
        for key in self.stripped_room_state.scan_prefix(room_key.as_slice()).keys() {
            stripped_room_state_batch.remove(key?)
        }

        let mut room_user_receipts_batch = sled::Batch::default();
        for key in self.room_user_receipts.scan_prefix(room_key.as_slice()).keys() {
            room_user_receipts_batch.remove(key?)
        }

        let mut room_event_receipts_batch = sled::Batch::default();
        for key in self.room_event_receipts.scan_prefix(room_key.as_slice()).keys() {
            room_event_receipts_batch.remove(key?)
        }

        let ret: Result<(), TransactionError<SerializationError>> = (
            &self.members,
            &self.profiles,
            &self.display_names,
            &self.joined_user_ids,
            &self.invited_user_ids,
            &self.room_info,
            &self.room_state,
            &self.room_account_data,
            &self.stripped_room_infos,
            &self.stripped_members,
            &self.stripped_room_state,
            &self.room_user_receipts,
            &self.room_event_receipts,
        )
            .transaction(
                |(
                    members,
                    profiles,
                    display_names,
                    joined,
                    invited,
                    rooms,
                    state,
                    room_account_data,
                    stripped_rooms,
                    stripped_members,
                    stripped_state,
                    room_user_receipts,
                    room_event_receipts,
                )| {
                    rooms.remove(room_key.as_slice())?;
                    stripped_rooms.remove(room_key.as_slice())?;

                    members.apply_batch(&members_batch)?;
                    profiles.apply_batch(&profiles_batch)?;
                    display_names.apply_batch(&display_names_batch)?;
                    joined.apply_batch(&joined_user_ids_batch)?;
                    invited.apply_batch(&invited_user_ids_batch)?;
                    state.apply_batch(&room_state_batch)?;
                    room_account_data.apply_batch(&room_account_data_batch)?;
                    stripped_members.apply_batch(&stripped_members_batch)?;
                    stripped_state.apply_batch(&stripped_room_state_batch)?;
                    room_user_receipts.apply_batch(&room_user_receipts_batch)?;
                    room_event_receipts.apply_batch(&room_event_receipts_batch)?;

                    Ok(())
                },
            );

        ret?;

        self.remove_room_timeline(room_id).await?;

        self.inner.flush_async().await?;

        Ok(())
    }

    async fn room_timeline(
        &self,
        room_id: &RoomId,
    ) -> Result<Option<(BoxStream<Result<SyncRoomEvent>>, Option<String>)>> {
        let db = self.clone();
        let key = room_id.encode();
        let r_id = room_id.to_owned();
        let metadata: Option<TimelineMetadata> = db
            .room_timeline_metadata
            .get(key.as_slice())?
            .map(|v| serde_json::from_slice(&v).map_err(StoreError::Json))
            .transpose()?;
        let metadata = match metadata {
            Some(m) => m,
            None => {
                info!("No timeline for {} was previously stored", r_id);
                return Ok(None);
            }
        };

        let mut position = metadata.start_position;
        let end_token = metadata.end;

        info!("Found previously stored timeline for {}, with end token {:?}", r_id, end_token);

        let stream = stream! {
            while let Ok(Some(item)) = db.room_timeline.get(&(r_id.as_ref(), position).encode()) {
                position += 1;
                yield db.deserialize_event(&item).map_err(|e| e.into());
            }
        };

        Ok(Some((Box::pin(stream), end_token)))
    }

    async fn remove_room_timeline(&self, room_id: &RoomId) -> Result<()> {
        let room_key = room_id.encode();
        info!("Remove stored timeline for {}", room_id);

        let mut timeline_batch = sled::Batch::default();
        for key in self.room_timeline.scan_prefix(room_key.as_slice()).keys() {
            timeline_batch.remove(key?)
        }

        let mut event_id_to_position_batch = sled::Batch::default();
        for key in self.room_event_id_to_position.scan_prefix(room_key.as_slice()).keys() {
            event_id_to_position_batch.remove(key?)
        }

        let ret: Result<(), TransactionError<SerializationError>> =
            (&self.room_timeline, &self.room_timeline_metadata, &self.room_event_id_to_position)
                .transaction(
                    |(room_timeline, room_timeline_metadata, room_event_id_to_position)| {
                        room_timeline_metadata.remove(room_key.as_slice())?;

                        room_timeline.apply_batch(&timeline_batch)?;
                        room_event_id_to_position.apply_batch(&event_id_to_position_batch)?;

                        Ok(())
                    },
                );

        ret?;

        Ok(())
    }

    async fn save_room_timeline(&self, changes: &StateChanges) -> Result<()> {
        let mut timeline_batch = sled::Batch::default();
        let mut event_id_to_position_batch = sled::Batch::default();
        let mut timeline_metadata_batch = sled::Batch::default();

        for (room_id, timeline) in &changes.timeline {
            if timeline.sync {
                info!("Save new timeline batch from sync response for {}", room_id);
            } else {
                info!("Save new timeline batch from messages response for {}", room_id);
            }
            let room_key = room_id.encode();

            let metadata: Option<TimelineMetadata> = if timeline.limited {
                info!(
                    "Delete stored timeline for {} because the sync response was limited",
                    room_id
                );
                self.remove_room_timeline(room_id).await?;
                None
            } else {
                let metadata: Option<TimelineMetadata> = self
                    .room_timeline_metadata
                    .get(room_key.as_slice())?
                    .map(|v| serde_json::from_slice(&v).map_err(StoreError::Json))
                    .transpose()?;
                if let Some(mut metadata) = metadata {
                    if !timeline.sync && Some(&timeline.start) != metadata.end.as_ref() {
                        // This should only happen when a developer adds a wrong timeline
                        // batch to the `StateChanges` or the server returns a wrong response
                        // to our request.
                        warn!("Drop unexpected timeline batch for {}", room_id);
                        return Ok(());
                    }

                    // Check if the event already exists in the store
                    let mut delete_timeline = false;
                    for event in &timeline.events {
                        if let Some(event_id) = event.event_id() {
                            let event_key = (room_id.as_ref(), event_id.as_ref()).encode();
                            if self.room_event_id_to_position.contains_key(event_key)? {
                                delete_timeline = true;
                                break;
                            }
                        }
                    }

                    if delete_timeline {
                        info!(
                            "Delete stored timeline for {} because of duplicated events",
                            room_id
                        );
                        self.remove_room_timeline(room_id).await?;
                        None
                    } else if timeline.sync {
                        metadata.start = timeline.start.clone();
                        Some(metadata)
                    } else {
                        metadata.end = timeline.end.clone();
                        Some(metadata)
                    }
                } else {
                    None
                }
            };

            let mut metadata = if let Some(metadata) = metadata {
                metadata
            } else {
                TimelineMetadata {
                    start: timeline.start.clone(),
                    end: timeline.end.clone(),
                    start_position: usize::MAX / 2,
                    end_position: usize::MAX / 2,
                }
            };
            let room_version = self
                .room_info
                .get(room_id.encode())?
                .map(|r| self.deserialize_event::<RoomInfo>(&r))
                .transpose()?
                .and_then(|info| info.base_info.create.map(|event| event.room_version))
                .unwrap_or_else(|| {
                    warn!("Unable to find the room version for {}, assume version 9", room_id);
                    RoomVersionId::V9
                });

            if timeline.sync {
                for event in timeline.events.iter().rev() {
                    // Redact events already in store only on sync response
                    if let Ok(AnySyncRoomEvent::Message(AnySyncMessageEvent::RoomRedaction(
                        redaction,
                    ))) = event.event.deserialize()
                    {
                        let redacts_key = (room_id.as_ref(), redaction.redacts.as_ref()).encode();
                        if let Some(position_key) =
                            self.room_event_id_to_position.get(redacts_key)?
                        {
                            if let Some(mut full_event) = self
                                .room_timeline
                                .get(position_key.as_ref())?
                                .map(|e| {
                                    self.deserialize_event::<SyncRoomEvent>(&e)
                                        .map_err(StoreError::from)
                                })
                                .transpose()?
                            {
                                let mut event_json: CanonicalJsonObject =
                                    full_event.event.deserialize_as()?;
                                redact_in_place(&mut event_json, &room_version)
                                    .map_err(StoreError::Redaction)?;
                                full_event.event = Raw::new(&event_json)?.cast();
                                timeline_batch
                                    .insert(position_key, self.serialize_event(&full_event)?);
                            }
                        }
                    }

                    metadata.start_position -= 1;
                    let key = (room_id.as_ref(), metadata.start_position).encode();
                    timeline_batch.insert(key.as_slice(), self.serialize_event(&event)?);
                    // Only add event with id to the position map
                    if let Some(event_id) = event.event_id() {
                        let event_key = (room_id.as_ref(), event_id.as_ref()).encode();
                        event_id_to_position_batch.insert(event_key.as_slice(), key.as_slice());
                    }
                }
            } else {
                for event in timeline.events.iter() {
                    metadata.end_position += 1;
                    let key = (room_id.as_ref(), metadata.end_position).encode();
                    timeline_batch.insert(key.as_slice(), self.serialize_event(&event)?);
                    // Only add event with id to the position map
                    if let Some(event_id) = event.event_id() {
                        let event_key = (room_id.as_ref(), event_id.as_ref()).encode();
                        event_id_to_position_batch.insert(event_key.as_slice(), key.as_slice());
                    }
                }
            }

            timeline_metadata_batch.insert(room_key, serde_json::to_vec(&metadata)?);
        }

        let ret: Result<(), TransactionError<SerializationError>> =
            (&self.room_timeline, &self.room_timeline_metadata, &self.room_event_id_to_position)
                .transaction(
                    |(room_timeline, room_timeline_metadata, room_event_id_to_position)| {
                        room_timeline_metadata.apply_batch(&timeline_metadata_batch)?;

                        room_timeline.apply_batch(&timeline_batch)?;
                        room_event_id_to_position.apply_batch(&event_id_to_position_batch)?;

                        Ok(())
                    },
                );

        ret?;

        Ok(())
    }
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

    async fn get_presence_event(&self, user_id: &UserId) -> Result<Option<Raw<PresenceEvent>>> {
        self.get_presence_event(user_id).await
    }

    async fn get_state_event(
        &self,
        room_id: &RoomId,
        event_type: EventType,
        state_key: &str,
    ) -> Result<Option<Raw<AnySyncStateEvent>>> {
        self.get_state_event(room_id, event_type, state_key).await
    }

    async fn get_state_events(
        &self,
        room_id: &RoomId,
        event_type: EventType,
    ) -> Result<Vec<Raw<AnySyncStateEvent>>> {
        self.get_state_events(room_id, event_type).await
    }

    async fn get_profile(
        &self,
        room_id: &RoomId,
        user_id: &UserId,
    ) -> Result<Option<RoomMemberEventContent>> {
        self.get_profile(room_id, user_id).await
    }

    async fn get_member_event(
        &self,
        room_id: &RoomId,
        state_key: &UserId,
    ) -> Result<Option<MemberEvent>> {
        self.get_member_event(room_id, state_key).await
    }

    async fn get_user_ids(&self, room_id: &RoomId) -> Result<Vec<Box<UserId>>> {
        self.get_user_ids_stream(room_id).await?.try_collect().await
    }

    async fn get_invited_user_ids(&self, room_id: &RoomId) -> Result<Vec<Box<UserId>>> {
        self.get_invited_user_ids(room_id).await?.try_collect().await
    }

    async fn get_joined_user_ids(&self, room_id: &RoomId) -> Result<Vec<Box<UserId>>> {
        self.get_joined_user_ids(room_id).await?.try_collect().await
    }

    async fn get_room_infos(&self) -> Result<Vec<RoomInfo>> {
        self.get_room_infos().await?.try_collect().await
    }

    async fn get_stripped_room_infos(&self) -> Result<Vec<RoomInfo>> {
        self.get_stripped_room_infos().await?.try_collect().await
    }

    async fn get_users_with_display_name(
        &self,
        room_id: &RoomId,
        display_name: &str,
    ) -> Result<BTreeSet<Box<UserId>>> {
        self.get_users_with_display_name(room_id, display_name).await
    }

    async fn get_account_data_event(
        &self,
        event_type: EventType,
    ) -> Result<Option<Raw<AnyGlobalAccountDataEvent>>> {
        self.get_account_data_event(event_type).await
    }

    async fn get_room_account_data_event(
        &self,
        room_id: &RoomId,
        event_type: EventType,
    ) -> Result<Option<Raw<AnyRoomAccountDataEvent>>> {
        self.get_room_account_data_event(room_id, event_type).await
    }

    async fn get_user_room_receipt_event(
        &self,
        room_id: &RoomId,
        receipt_type: ReceiptType,
        user_id: &UserId,
    ) -> Result<Option<(Box<EventId>, Receipt)>> {
        self.get_user_room_receipt_event(room_id, receipt_type, user_id).await
    }

    async fn get_event_room_receipt_events(
        &self,
        room_id: &RoomId,
        receipt_type: ReceiptType,
        event_id: &EventId,
    ) -> Result<Vec<(Box<UserId>, Receipt)>> {
        self.get_event_room_receipt_events(room_id, receipt_type, event_id).await
    }

    async fn get_custom_value(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        self.get_custom_value(key).await
    }

    async fn set_custom_value(&self, key: &[u8], value: Vec<u8>) -> Result<Option<Vec<u8>>> {
        self.set_custom_value(key, value).await
    }

    async fn add_media_content(&self, request: &MediaRequest, data: Vec<u8>) -> Result<()> {
        self.add_media_content(request, data).await
    }

    async fn get_media_content(&self, request: &MediaRequest) -> Result<Option<Vec<u8>>> {
        self.get_media_content(request).await
    }

    async fn remove_media_content(&self, request: &MediaRequest) -> Result<()> {
        self.remove_media_content(request).await
    }

    async fn remove_media_content_for_uri(&self, uri: &MxcUri) -> Result<()> {
        self.remove_media_content_for_uri(uri).await
    }

    async fn remove_room(&self, room_id: &RoomId) -> Result<()> {
        self.remove_room(room_id).await
    }

    async fn room_timeline(
        &self,
        room_id: &RoomId,
    ) -> Result<Option<(BoxStream<Result<SyncRoomEvent>>, Option<String>)>> {
        self.room_timeline(room_id).await
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct TimelineMetadata {
    pub start: String,
    pub start_position: usize,
    pub end: Option<String>,
    pub end_position: usize,
}

#[cfg(test)]
mod test {
    use super::{Result, SledStore};

    async fn get_store() -> Result<SledStore> {
        SledStore::open()
    }

    statestore_integration_tests! { integration }
}
