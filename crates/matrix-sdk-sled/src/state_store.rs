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
    path::{Path, PathBuf},
    sync::Arc,
    time::Instant,
};

#[cfg(feature = "experimental-timeline")]
use async_stream::stream;
use async_trait::async_trait;
use futures_core::stream::Stream;
use futures_util::stream::{self, StreamExt, TryStreamExt};
use matrix_sdk_base::{
    deserialized_responses::MemberEvent,
    media::{MediaRequest, UniqueKey},
    store::{Result as StoreResult, StateChanges, StateStore, StoreError},
    MinimalStateEvent, RoomInfo,
};
#[cfg(feature = "experimental-timeline")]
use matrix_sdk_base::{deserialized_responses::SyncRoomEvent, store::BoxStream};
use matrix_sdk_store_encryption::{Error as KeyEncryptionError, StoreCipher};
use ruma::{
    events::{
        presence::PresenceEvent,
        receipt::Receipt,
        room::member::{MembershipState, RoomMemberEventContent},
        AnyGlobalAccountDataEvent, AnyRoomAccountDataEvent, AnySyncStateEvent,
        GlobalAccountDataEventType, RoomAccountDataEventType, StateEventType,
    },
    receipt::ReceiptType,
    serde::Raw,
    EventId, IdParseError, MxcUri, OwnedEventId, OwnedUserId, RoomId, UserId,
};
#[cfg(feature = "experimental-timeline")]
use ruma::{
    events::{room::redaction::SyncRoomRedactionEvent, AnySyncMessageLikeEvent, AnySyncRoomEvent},
    signatures::{redact_in_place, CanonicalJsonObject},
    RoomVersionId,
};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use sled::{
    transaction::{ConflictableTransactionError, TransactionError},
    Config, Db, Transactional, Tree,
};
use tokio::task::spawn_blocking;

#[cfg(feature = "crypto-store")]
use super::OpenStoreError;
use crate::encode_key::EncodeKey;
#[cfg(feature = "experimental-timeline")]
use crate::encode_key::ENCODE_SEPARATOR;
#[cfg(feature = "crypto-store")]
pub use crate::CryptoStore;

#[derive(Debug, thiserror::Error)]
pub enum SledStoreError {
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Encryption(#[from] KeyEncryptionError),
    #[error(transparent)]
    StoreError(#[from] StoreError),
    #[error(transparent)]
    TransactionError(#[from] sled::Error),
    #[error(transparent)]
    Identifier(#[from] IdParseError),
    #[error(transparent)]
    Task(#[from] tokio::task::JoinError),
}

impl From<TransactionError<SledStoreError>> for SledStoreError {
    fn from(e: TransactionError<SledStoreError>) -> Self {
        match e {
            TransactionError::Abort(e) => e,
            TransactionError::Storage(e) => SledStoreError::TransactionError(e),
        }
    }
}

#[allow(clippy::from_over_into)]
impl Into<StoreError> for SledStoreError {
    fn into(self) -> StoreError {
        match self {
            SledStoreError::Json(e) => StoreError::Json(e),
            SledStoreError::Identifier(e) => StoreError::Identifier(e),
            SledStoreError::Encryption(e) => match e {
                KeyEncryptionError::Random(e) => StoreError::Encryption(e.to_string()),
                KeyEncryptionError::Serialization(e) => StoreError::Json(e),
                KeyEncryptionError::Encryption(e) => StoreError::Encryption(e.to_string()),
                KeyEncryptionError::Version(found, expected) => StoreError::Encryption(format!(
                    "Bad Database Encryption Version: expected {} found {}",
                    expected, found
                )),
                KeyEncryptionError::Length(found, expected) => StoreError::Encryption(format!(
                    "The database key an invalid length: expected {} found {}",
                    expected, found
                )),
            },
            SledStoreError::StoreError(e) => e,
            _ => StoreError::backend(self),
        }
    }
}
const DATABASE_VERSION: u8 = 1;

const VERSION_KEY: &str = "state-store-version";

const ACCOUNT_DATA: &str = "account-data";
const CUSTOM: &str = "custom";
const DISPLAY_NAME: &str = "display-name";
const INVITED_USER_ID: &str = "invited-user-id";
const JOINED_USER_ID: &str = "joined-user-id";
const MEDIA: &str = "media";
const MEMBER: &str = "member";
const PRESENCE: &str = "presence";
const PROFILE: &str = "profile";
const ROOM_ACCOUNT_DATA: &str = "room-account-data";
#[cfg(feature = "experimental-timeline")]
const ROOM_EVENT_ID_POSITION: &str = "room-event-id-to-position";
const ROOM_EVENT_RECEIPT: &str = "room-event-receipt";
const ROOM_INFO: &str = "room-info";
const ROOM_STATE: &str = "room-state";
const ROOM_USER_RECEIPT: &str = "room-user-receipt";
const ROOM: &str = "room";
const SESSION: &str = "session";
const STRIPPED_INVITED_USER_ID: &str = "stripped-invited-user-id";
const STRIPPED_JOINED_USER_ID: &str = "stripped-joined-user-id";
const STRIPPED_ROOM_INFO: &str = "stripped-room-info";
const STRIPPED_ROOM_MEMBER: &str = "stripped-room-member";
const STRIPPED_ROOM_STATE: &str = "stripped-room-state";
#[cfg(feature = "experimental-timeline")]
const TIMELINE_METADATA: &str = "timeline-metadata";
#[cfg(feature = "experimental-timeline")]
const TIMELINE: &str = "timeline";

type Result<A, E = SledStoreError> = std::result::Result<A, E>;

#[derive(Clone)]
pub struct SledStore {
    path: Option<PathBuf>,
    pub(crate) inner: Db,
    store_cipher: Option<Arc<StoreCipher>>,
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
    stripped_joined_user_ids: Tree,
    stripped_invited_user_ids: Tree,
    stripped_room_infos: Tree,
    stripped_room_state: Tree,
    stripped_members: Tree,
    presence: Tree,
    room_user_receipts: Tree,
    room_event_receipts: Tree,
    media: Tree,
    custom: Tree,
    #[cfg(feature = "experimental-timeline")]
    room_timeline: Tree,
    #[cfg(feature = "experimental-timeline")]
    room_timeline_metadata: Tree,
    #[cfg(feature = "experimental-timeline")]
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
    fn open_helper(
        db: Db,
        path: Option<PathBuf>,
        store_cipher: Option<Arc<StoreCipher>>,
    ) -> Result<Self> {
        let session = db.open_tree(SESSION)?;
        let account_data = db.open_tree(ACCOUNT_DATA)?;

        let members = db.open_tree(MEMBER)?;
        let profiles = db.open_tree(PROFILE)?;
        let display_names = db.open_tree(DISPLAY_NAME)?;
        let joined_user_ids = db.open_tree(JOINED_USER_ID)?;
        let invited_user_ids = db.open_tree(INVITED_USER_ID)?;

        let room_state = db.open_tree(ROOM_STATE)?;
        let room_info = db.open_tree(ROOM_INFO)?;
        let presence = db.open_tree(PRESENCE)?;
        let room_account_data = db.open_tree(ROOM_ACCOUNT_DATA)?;

        let stripped_joined_user_ids = db.open_tree(STRIPPED_JOINED_USER_ID)?;
        let stripped_invited_user_ids = db.open_tree(STRIPPED_INVITED_USER_ID)?;
        let stripped_room_infos = db.open_tree(STRIPPED_ROOM_INFO)?;
        let stripped_members = db.open_tree(STRIPPED_ROOM_MEMBER)?;
        let stripped_room_state = db.open_tree(STRIPPED_ROOM_STATE)?;

        let room_user_receipts = db.open_tree(ROOM_USER_RECEIPT)?;
        let room_event_receipts = db.open_tree(ROOM_EVENT_RECEIPT)?;

        let media = db.open_tree(MEDIA)?;

        let custom = db.open_tree(CUSTOM)?;

        #[cfg(feature = "experimental-timeline")]
        let room_timeline = db.open_tree(TIMELINE)?;
        #[cfg(feature = "experimental-timeline")]
        let room_timeline_metadata = db.open_tree(TIMELINE_METADATA)?;
        #[cfg(feature = "experimental-timeline")]
        let room_event_id_to_position = db.open_tree(ROOM_EVENT_ID_POSITION)?;

        let database = Self {
            path,
            inner: db,
            store_cipher,
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
            stripped_joined_user_ids,
            stripped_invited_user_ids,
            stripped_room_infos,
            stripped_members,
            stripped_room_state,
            room_user_receipts,
            room_event_receipts,
            media,
            custom,
            #[cfg(feature = "experimental-timeline")]
            room_timeline,
            #[cfg(feature = "experimental-timeline")]
            room_timeline_metadata,
            #[cfg(feature = "experimental-timeline")]
            room_event_id_to_position,
        };

        database.upgrade()?;
        Ok(database)
    }

    pub fn open() -> StoreResult<Self> {
        let db = Config::new().temporary(true).open().map_err(StoreError::backend)?;

        SledStore::open_helper(db, None, None).map_err(|e| e.into())
    }

    // testing only
    #[cfg(test)]
    fn open_encrypted() -> StoreResult<Self> {
        let db = Config::new().temporary(true).open().map_err(StoreError::backend)?;

        SledStore::open_helper(
            db,
            None,
            Some(StoreCipher::new().expect("can't create store cipher").into()),
        )
        .map_err(|e| e.into())
    }

    pub fn open_with_passphrase(path: impl AsRef<Path>, passphrase: &str) -> StoreResult<Self> {
        Self::inner_open_with_passphrase(path, passphrase).map_err(|e| e.into())
    }

    fn inner_open_with_passphrase(path: impl AsRef<Path>, passphrase: &str) -> Result<Self> {
        let path = path.as_ref().join("matrix-sdk-state");
        let db = Config::new().temporary(false).path(&path).open()?;

        let store_cipher = if let Some(inner) = db.get("store_cipher".encode())? {
            StoreCipher::import(passphrase, &inner)?
        } else {
            let cipher = StoreCipher::new()?;
            db.insert("store_cipher".encode(), cipher.export(passphrase)?)?;
            cipher
        }
        .into();

        SledStore::open_helper(db, Some(path), Some(store_cipher))
    }

    pub fn open_with_path(path: impl AsRef<Path>) -> StoreResult<Self> {
        Self::inner_open_with_path(path).map_err(|e| e.into())
    }

    fn inner_open_with_path(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().join("matrix-sdk-state");
        let db = Config::new().temporary(false).path(&path).open()?;

        SledStore::open_helper(db, Some(path), None)
    }

    fn upgrade(&self) -> StoreResult<()> {
        let db_version = self.inner.get(VERSION_KEY).map_err(StoreError::backend)?.map(|v| {
            let (version_bytes, _) = v.split_at(std::mem::size_of::<u8>());
            u8::from_be_bytes(version_bytes.try_into().unwrap_or_default())
        });

        let old_version = match db_version {
            None => {
                // we are fresh, let's write the current version
                self.inner
                    .insert(VERSION_KEY, DATABASE_VERSION.to_be_bytes().as_ref())
                    .map_err(StoreError::backend)?;
                self.inner.flush().map_err(StoreError::backend)?;
                return Ok(());
            }
            Some(version) if version == DATABASE_VERSION => {
                // current, we don't have to do anything
                return Ok(());
            }
            Some(version) => version,
        };

        tracing::debug!(
            old_version,
            new_version = DATABASE_VERSION,
            "Upgrading the Sled state store"
        );

        // FUTURE UPGRADE CODE GOES HERE

        // can't upgrade from that version to the new one
        Err(StoreError::UnsupportedDatabaseVersion(old_version.into(), DATABASE_VERSION.into()))
    }

    /// Open a `CryptoStore` that uses the same database as this store.
    ///
    /// The given passphrase will be used to encrypt private data.
    #[cfg(feature = "crypto-store")]
    pub fn open_crypto_store(&self) -> Result<CryptoStore, OpenStoreError> {
        CryptoStore::open_with_database(self.inner.clone(), self.store_cipher.clone())
    }

    fn serialize_event(&self, event: &impl Serialize) -> Result<Vec<u8>, SledStoreError> {
        if let Some(key) = &self.store_cipher {
            Ok(key.encrypt_value(event)?)
        } else {
            Ok(serde_json::to_vec(event)?)
        }
    }

    fn deserialize_event<T: DeserializeOwned>(&self, event: &[u8]) -> Result<T, SledStoreError> {
        if let Some(key) = &self.store_cipher {
            Ok(key.decrypt_value(event)?)
        } else {
            Ok(serde_json::from_slice(event)?)
        }
    }

    fn encode_key<T: EncodeKey>(&self, table_name: &str, key: T) -> Vec<u8> {
        if let Some(store_cipher) = &self.store_cipher {
            key.encode_secure(table_name, store_cipher).to_vec()
        } else {
            key.encode()
        }
    }

    #[cfg(feature = "experimental-timeline")]
    fn encode_key_with_counter<A: EncodeKey>(&self, tablename: &str, s: A, i: usize) -> Vec<u8> {
        [
            &self.encode_key(tablename, s),
            [ENCODE_SEPARATOR].as_slice(),
            (i as u64).to_be_bytes().as_ref(),
            [ENCODE_SEPARATOR].as_slice(),
        ]
        .concat()
    }

    pub async fn save_filter(&self, filter_name: &str, filter_id: &str) -> Result<()> {
        self.session.insert(self.encode_key(SESSION, ("filter", filter_name)), filter_id)?;
        Ok(())
    }

    pub async fn get_filter(&self, filter_name: &str) -> Result<Option<String>> {
        Ok(self
            .session
            .get(self.encode_key(SESSION, ("filter", filter_name)))?
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

        // room state & memberships
        let ret: Result<(), TransactionError<SledStoreError>> = (
            &self.members,
            &self.profiles,
            &self.display_names,
            &self.joined_user_ids,
            &self.invited_user_ids,
            &self.room_info,
            &self.room_state,
            &self.room_account_data,
            &self.stripped_joined_user_ids,
            &self.stripped_invited_user_ids,
            &self.stripped_room_infos,
            &self.stripped_members,
            &self.stripped_room_state,
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
                    stripped_joined,
                    stripped_invited,
                    striped_rooms,
                    stripped_members,
                    stripped_state,
                )| {
                    for (room, events) in &changes.members {
                        let profile_changes = changes.profiles.get(room);

                        for event in events.values() {
                            let key = (room, event.state_key());

                            match event.membership() {
                                MembershipState::Join => {
                                    joined.insert(
                                        self.encode_key(JOINED_USER_ID, &key),
                                        event.state_key().as_str(),
                                    )?;
                                    invited.remove(self.encode_key(INVITED_USER_ID, &key))?;
                                }
                                MembershipState::Invite => {
                                    invited.insert(
                                        self.encode_key(INVITED_USER_ID, &key),
                                        event.state_key().as_str(),
                                    )?;
                                    joined.remove(self.encode_key(JOINED_USER_ID, &key))?;
                                }
                                _ => {
                                    joined.remove(self.encode_key(JOINED_USER_ID, &key))?;
                                    invited.remove(self.encode_key(INVITED_USER_ID, &key))?;
                                }
                            }

                            members.insert(
                                self.encode_key(MEMBER, &key),
                                self.serialize_event(&event)
                                    .map_err(ConflictableTransactionError::Abort)?,
                            )?;

                            if let Some(profile) =
                                profile_changes.and_then(|p| p.get(event.state_key()))
                            {
                                profiles.insert(
                                    self.encode_key(PROFILE, &key),
                                    self.serialize_event(&profile)
                                        .map_err(ConflictableTransactionError::Abort)?,
                                )?;
                            }
                        }
                    }

                    for (room_id, ambiguity_maps) in &changes.ambiguity_maps {
                        for (display_name, map) in ambiguity_maps {
                            display_names.insert(
                                self.encode_key(DISPLAY_NAME, (room_id, display_name)),
                                self.serialize_event(&map)
                                    .map_err(ConflictableTransactionError::Abort)?,
                            )?;
                        }
                    }

                    for (room, events) in &changes.room_account_data {
                        for (event_type, event) in events {
                            room_account_data.insert(
                                self.encode_key(ROOM_ACCOUNT_DATA, &(room, event_type)),
                                self.serialize_event(&event)
                                    .map_err(ConflictableTransactionError::Abort)?,
                            )?;
                        }
                    }

                    for (room, event_types) in &changes.state {
                        for (event_type, events) in event_types {
                            for (state_key, event) in events {
                                state.insert(
                                    self.encode_key(ROOM_STATE, (room, event_type, state_key)),
                                    self.serialize_event(&event)
                                        .map_err(ConflictableTransactionError::Abort)?,
                                )?;
                            }
                        }
                    }

                    for (room_id, room_info) in &changes.room_infos {
                        rooms.insert(
                            self.encode_key(ROOM, room_id),
                            self.serialize_event(room_info)
                                .map_err(ConflictableTransactionError::Abort)?,
                        )?;
                    }

                    for (room_id, info) in &changes.stripped_room_infos {
                        striped_rooms.insert(
                            self.encode_key(STRIPPED_ROOM_INFO, room_id),
                            self.serialize_event(&info)
                                .map_err(ConflictableTransactionError::Abort)?,
                        )?;
                    }

                    for (room, events) in &changes.stripped_members {
                        for event in events.values() {
                            let key = (room, &event.state_key);

                            match event.content.membership {
                                MembershipState::Join => {
                                    stripped_joined.insert(
                                        self.encode_key(STRIPPED_JOINED_USER_ID, &key),
                                        event.state_key.as_str(),
                                    )?;
                                    stripped_invited
                                        .remove(self.encode_key(STRIPPED_INVITED_USER_ID, &key))?;
                                }
                                MembershipState::Invite => {
                                    stripped_invited.insert(
                                        self.encode_key(STRIPPED_INVITED_USER_ID, &key),
                                        event.state_key.as_str(),
                                    )?;
                                    stripped_joined
                                        .remove(self.encode_key(STRIPPED_JOINED_USER_ID, &key))?;
                                }
                                _ => {
                                    stripped_joined
                                        .remove(self.encode_key(STRIPPED_JOINED_USER_ID, &key))?;
                                    stripped_invited
                                        .remove(self.encode_key(STRIPPED_INVITED_USER_ID, &key))?;
                                }
                            }
                            stripped_members.insert(
                                self.encode_key(STRIPPED_ROOM_MEMBER, &key),
                                self.serialize_event(&event)
                                    .map_err(ConflictableTransactionError::Abort)?,
                            )?;
                        }
                    }

                    for (room, event_types) in &changes.stripped_state {
                        for (event_type, events) in event_types {
                            for (state_key, event) in events {
                                stripped_state.insert(
                                    self.encode_key(
                                        STRIPPED_ROOM_STATE,
                                        (room, event_type.to_string(), state_key),
                                    ),
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

        let ret: Result<(), TransactionError<SledStoreError>> =
            (&self.room_user_receipts, &self.room_event_receipts, &self.presence).transaction(
                |(room_user_receipts, room_event_receipts, presence)| {
                    for (room, content) in &changes.receipts {
                        for (event_id, receipts) in &content.0 {
                            for (receipt_type, receipts) in receipts {
                                for (user_id, receipt) in receipts {
                                    // Add the receipt to the room user receipts
                                    if let Some(old) = room_user_receipts.insert(
                                        self.encode_key(
                                            ROOM_USER_RECEIPT,
                                            (room, receipt_type, user_id),
                                        ),
                                        self.serialize_event(&(event_id, receipt))
                                            .map_err(ConflictableTransactionError::Abort)?,
                                    )? {
                                        // Remove the old receipt from the room event receipts
                                        let (old_event, _): (OwnedEventId, Receipt) = self
                                            .deserialize_event(&old)
                                            .map_err(ConflictableTransactionError::Abort)?;
                                        room_event_receipts.remove(self.encode_key(
                                            ROOM_EVENT_RECEIPT,
                                            (room, receipt_type, old_event, user_id),
                                        ))?;
                                    }

                                    // Add the receipt to the room event receipts
                                    room_event_receipts.insert(
                                        self.encode_key(
                                            ROOM_EVENT_RECEIPT,
                                            (room, receipt_type, event_id, user_id),
                                        ),
                                        self.serialize_event(&(user_id, receipt))
                                            .map_err(ConflictableTransactionError::Abort)?,
                                    )?;
                                }
                            }
                        }
                    }

                    for (sender, event) in &changes.presence {
                        presence.insert(
                            self.encode_key(PRESENCE, sender),
                            self.serialize_event(&event)
                                .map_err(ConflictableTransactionError::Abort)?,
                        )?;
                    }

                    Ok(())
                },
            );

        ret?;

        #[cfg(feature = "experimental-timeline")]
        self.save_room_timeline(changes).await?;

        // user state
        let ret: Result<(), TransactionError<SledStoreError>> = (&self.session, &self.account_data)
            .transaction(|(session, account_data)| {
                if let Some(s) = &changes.sync_token {
                    session.insert("sync_token".encode(), s.as_str())?;
                }

                for (event_type, event) in &changes.account_data {
                    account_data.insert(
                        self.encode_key(ACCOUNT_DATA, event_type),
                        self.serialize_event(&event)
                            .map_err(ConflictableTransactionError::Abort)?,
                    )?;
                }

                Ok(())
            });

        ret?;

        self.inner.flush_async().await?;

        tracing::info!("Saved changes in {:?}", now.elapsed());

        Ok(())
    }

    pub async fn get_presence_event(&self, user_id: &UserId) -> Result<Option<Raw<PresenceEvent>>> {
        let db = self.clone();
        let key = self.encode_key(PRESENCE, user_id);
        spawn_blocking(move || db.presence.get(key)?.map(|e| db.deserialize_event(&e)).transpose())
            .await?
    }

    pub async fn get_state_event(
        &self,
        room_id: &RoomId,
        event_type: StateEventType,
        state_key: &str,
    ) -> Result<Option<Raw<AnySyncStateEvent>>> {
        let db = self.clone();
        let key = self.encode_key(ROOM_STATE, (room_id, event_type.to_string(), state_key));
        spawn_blocking(move || {
            db.room_state.get(key)?.map(|e| db.deserialize_event(&e)).transpose()
        })
        .await?
    }

    pub async fn get_state_events(
        &self,
        room_id: &RoomId,
        event_type: StateEventType,
    ) -> Result<Vec<Raw<AnySyncStateEvent>>> {
        let db = self.clone();
        let key = self.encode_key(ROOM_STATE, (room_id, event_type.to_string()));
        spawn_blocking(move || {
            db.room_state
                .scan_prefix(key)
                .flat_map(|e| e.map(|(_, e)| db.deserialize_event(&e)))
                .collect::<Result<_, _>>()
        })
        .await?
    }

    pub async fn get_profile(
        &self,
        room_id: &RoomId,
        user_id: &UserId,
    ) -> Result<Option<MinimalStateEvent<RoomMemberEventContent>>> {
        let db = self.clone();
        let key = self.encode_key(PROFILE, (room_id, user_id));
        spawn_blocking(move || db.profiles.get(key)?.map(|p| db.deserialize_event(&p)).transpose())
            .await?
    }

    pub async fn get_member_event(
        &self,
        room_id: &RoomId,
        state_key: &UserId,
    ) -> Result<Option<MemberEvent>> {
        let db = self.clone();
        let key = self.encode_key(MEMBER, (room_id, state_key));
        let stripped_key = self.encode_key(STRIPPED_ROOM_MEMBER, (room_id, state_key));
        spawn_blocking(move || {
            if let Some(e) = db.members.get(key)?.map(|v| db.deserialize_event(&v)).transpose()? {
                Ok(Some(MemberEvent::Sync(e)))
            } else if let Some(e) = db
                .stripped_members
                .get(stripped_key)?
                .map(|v| db.deserialize_event(&v))
                .transpose()?
            {
                Ok(Some(MemberEvent::Stripped(e)))
            } else {
                Ok(None)
            }
        })
        .await?
    }

    pub async fn get_user_ids_stream(
        &self,
        room_id: &RoomId,
    ) -> StoreResult<impl Stream<Item = StoreResult<OwnedUserId>>> {
        Ok(self
            .get_joined_user_ids(room_id)
            .await?
            .chain(self.get_invited_user_ids(room_id).await?))
    }
    pub async fn get_stripped_user_ids_stream(
        &self,
        room_id: &RoomId,
    ) -> StoreResult<impl Stream<Item = StoreResult<OwnedUserId>>> {
        Ok(self
            .get_stripped_joined_user_ids(room_id)
            .await?
            .chain(self.get_stripped_invited_user_ids(room_id).await?))
    }

    pub async fn get_invited_user_ids(
        &self,
        room_id: &RoomId,
    ) -> StoreResult<impl Stream<Item = StoreResult<OwnedUserId>>> {
        let db = self.clone();
        let key = self.encode_key(INVITED_USER_ID, room_id);
        spawn_blocking(move || {
            stream::iter(db.invited_user_ids.scan_prefix(key).map(|u| {
                UserId::parse(String::from_utf8_lossy(&u.map_err(StoreError::backend)?.1))
                    .map_err(StoreError::Identifier)
            }))
        })
        .await
        .map_err(StoreError::backend)
    }

    pub async fn get_joined_user_ids(
        &self,
        room_id: &RoomId,
    ) -> StoreResult<impl Stream<Item = StoreResult<OwnedUserId>>> {
        let db = self.clone();
        let key = self.encode_key(JOINED_USER_ID, room_id);
        spawn_blocking(move || {
            stream::iter(db.joined_user_ids.scan_prefix(key).map(|u| {
                UserId::parse(String::from_utf8_lossy(&u.map_err(StoreError::backend)?.1))
                    .map_err(StoreError::Identifier)
            }))
        })
        .await
        .map_err(StoreError::backend)
    }

    pub async fn get_stripped_invited_user_ids(
        &self,
        room_id: &RoomId,
    ) -> StoreResult<impl Stream<Item = StoreResult<OwnedUserId>>> {
        let db = self.clone();
        let key = self.encode_key(STRIPPED_INVITED_USER_ID, room_id);
        spawn_blocking(move || {
            stream::iter(db.stripped_invited_user_ids.scan_prefix(key).map(|u| {
                UserId::parse(String::from_utf8_lossy(&u.map_err(StoreError::backend)?.1))
                    .map_err(StoreError::Identifier)
            }))
        })
        .await
        .map_err(StoreError::backend)
    }

    pub async fn get_stripped_joined_user_ids(
        &self,
        room_id: &RoomId,
    ) -> StoreResult<impl Stream<Item = StoreResult<OwnedUserId>>> {
        let db = self.clone();
        let key = self.encode_key(STRIPPED_JOINED_USER_ID, room_id);
        spawn_blocking(move || {
            stream::iter(db.stripped_joined_user_ids.scan_prefix(key).map(|u| {
                UserId::parse(String::from_utf8_lossy(&u.map_err(StoreError::backend)?.1))
                    .map_err(StoreError::Identifier)
            }))
        })
        .await
        .map_err(StoreError::backend)
    }

    pub async fn get_room_infos(&self) -> Result<impl Stream<Item = Result<RoomInfo>>> {
        let db = self.clone();
        spawn_blocking(move || {
            stream::iter(db.room_info.iter().map(move |r| db.deserialize_event(&r?.1)))
        })
        .await
        .map_err(Into::into)
    }

    pub async fn get_stripped_room_infos(&self) -> Result<impl Stream<Item = Result<RoomInfo>>> {
        let db = self.clone();
        spawn_blocking(move || {
            stream::iter(db.stripped_room_infos.iter().map(move |r| db.deserialize_event(&r?.1)))
        })
        .await
        .map_err(Into::into)
    }

    pub async fn get_users_with_display_name(
        &self,
        room_id: &RoomId,
        display_name: &str,
    ) -> Result<BTreeSet<OwnedUserId>> {
        let db = self.clone();
        let key = self.encode_key(DISPLAY_NAME, (room_id, display_name));
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
        event_type: GlobalAccountDataEventType,
    ) -> Result<Option<Raw<AnyGlobalAccountDataEvent>>> {
        let db = self.clone();
        let key = self.encode_key(ACCOUNT_DATA, event_type);
        spawn_blocking(move || {
            db.account_data.get(key)?.map(|m| db.deserialize_event(&m)).transpose()
        })
        .await?
    }

    pub async fn get_room_account_data_event(
        &self,
        room_id: &RoomId,
        event_type: RoomAccountDataEventType,
    ) -> Result<Option<Raw<AnyRoomAccountDataEvent>>> {
        let db = self.clone();
        let key = self.encode_key(ROOM_ACCOUNT_DATA, (room_id, event_type));
        spawn_blocking(move || {
            db.room_account_data.get(key)?.map(|m| db.deserialize_event(&m)).transpose()
        })
        .await?
    }

    async fn get_user_room_receipt_event(
        &self,
        room_id: &RoomId,
        receipt_type: ReceiptType,
        user_id: &UserId,
    ) -> Result<Option<(OwnedEventId, Receipt)>> {
        let db = self.clone();
        let key = self.encode_key(ROOM_USER_RECEIPT, (room_id, receipt_type, user_id));
        spawn_blocking(move || {
            db.room_user_receipts.get(key)?.map(|m| db.deserialize_event(&m)).transpose()
        })
        .await?
    }

    async fn get_event_room_receipt_events(
        &self,
        room_id: &RoomId,
        receipt_type: ReceiptType,
        event_id: &EventId,
    ) -> StoreResult<Vec<(OwnedUserId, Receipt)>> {
        let db = self.clone();
        let key = self.encode_key(ROOM_EVENT_RECEIPT, (room_id, receipt_type, event_id));
        spawn_blocking(move || {
            db.room_event_receipts
                .scan_prefix(key)
                .values()
                .map(|u| {
                    let v = u.map_err(StoreError::backend)?;
                    db.deserialize_event(&v).map_err(StoreError::backend)
                })
                .collect()
        })
        .await
        .map_err(StoreError::backend)?
    }

    async fn add_media_content(&self, request: &MediaRequest, data: Vec<u8>) -> Result<()> {
        self.media.insert(
            self.encode_key(MEDIA, (request.source.unique_key(), request.format.unique_key())),
            data,
        )?;

        self.inner.flush_async().await?;

        Ok(())
    }

    async fn get_media_content(&self, request: &MediaRequest) -> Result<Option<Vec<u8>>> {
        let db = self.clone();
        let key =
            self.encode_key(MEDIA, (request.source.unique_key(), request.format.unique_key()));

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
            self.encode_key(MEDIA, (request.source.unique_key(), request.format.unique_key())),
        )?;

        Ok(())
    }

    async fn remove_media_content_for_uri(&self, uri: &MxcUri) -> Result<()> {
        let keys = self.media.scan_prefix(self.encode_key(MEDIA, uri)).keys();

        let mut batch = sled::Batch::default();
        for key in keys {
            batch.remove(key?);
        }

        Ok(self.media.apply_batch(batch)?)
    }

    async fn remove_room(&self, room_id: &RoomId) -> Result<()> {
        let mut members_batch = sled::Batch::default();
        for key in self.members.scan_prefix(self.encode_key(MEMBER, room_id)).keys() {
            members_batch.remove(key?);
        }

        let mut profiles_batch = sled::Batch::default();
        for key in self.profiles.scan_prefix(self.encode_key(PROFILE, room_id)).keys() {
            profiles_batch.remove(key?);
        }

        let mut display_names_batch = sled::Batch::default();
        for key in self.display_names.scan_prefix(self.encode_key(DISPLAY_NAME, room_id)).keys() {
            display_names_batch.remove(key?);
        }

        let mut joined_user_ids_batch = sled::Batch::default();
        for key in self.joined_user_ids.scan_prefix(self.encode_key(JOINED_USER_ID, room_id)).keys()
        {
            joined_user_ids_batch.remove(key?);
        }

        let mut invited_user_ids_batch = sled::Batch::default();
        for key in
            self.invited_user_ids.scan_prefix(self.encode_key(INVITED_USER_ID, room_id)).keys()
        {
            invited_user_ids_batch.remove(key?);
        }

        let mut room_state_batch = sled::Batch::default();
        for key in self.room_state.scan_prefix(self.encode_key(ROOM_STATE, room_id)).keys() {
            room_state_batch.remove(key?);
        }

        let mut room_account_data_batch = sled::Batch::default();
        for key in
            self.room_account_data.scan_prefix(self.encode_key(ROOM_ACCOUNT_DATA, room_id)).keys()
        {
            room_account_data_batch.remove(key?);
        }

        let mut stripped_members_batch = sled::Batch::default();
        for key in
            self.stripped_members.scan_prefix(self.encode_key(STRIPPED_ROOM_MEMBER, room_id)).keys()
        {
            stripped_members_batch.remove(key?);
        }

        let mut stripped_room_state_batch = sled::Batch::default();
        for key in self
            .stripped_room_state
            .scan_prefix(self.encode_key(STRIPPED_ROOM_STATE, room_id))
            .keys()
        {
            stripped_room_state_batch.remove(key?);
        }

        let mut room_user_receipts_batch = sled::Batch::default();
        for key in
            self.room_user_receipts.scan_prefix(self.encode_key(ROOM_USER_RECEIPT, room_id)).keys()
        {
            room_user_receipts_batch.remove(key?);
        }

        let mut room_event_receipts_batch = sled::Batch::default();
        for key in self
            .room_event_receipts
            .scan_prefix(self.encode_key(ROOM_EVENT_RECEIPT, room_id))
            .keys()
        {
            room_event_receipts_batch.remove(key?);
        }

        let ret: Result<(), TransactionError<SledStoreError>> = (
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
                    rooms.remove(self.encode_key(ROOM, room_id))?;
                    stripped_rooms.remove(self.encode_key(STRIPPED_ROOM_INFO, room_id))?;

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

        #[cfg(feature = "experimental-timeline")]
        self.remove_room_timeline(room_id).await?;

        self.inner.flush_async().await?;

        Ok(())
    }

    #[cfg(feature = "experimental-timeline")]
    async fn room_timeline(
        &self,
        room_id: &RoomId,
    ) -> Result<Option<(BoxStream<StoreResult<SyncRoomEvent>>, Option<String>)>> {
        let db = self.clone();
        let key = self.encode_key(TIMELINE_METADATA, room_id);
        let r_id = room_id.to_owned();
        let metadata: Option<TimelineMetadata> = db
            .room_timeline_metadata
            .get(key.as_slice())?
            .map(|v| serde_json::from_slice(&v).map_err(StoreError::Json))
            .transpose()?;
        let metadata = match metadata {
            Some(m) => m,
            None => {
                tracing::info!("No timeline for {} was previously stored", r_id);
                return Ok(None);
            }
        };

        let mut position = metadata.start_position;
        let end_token = metadata.end;

        tracing::info!(
            "Found previously stored timeline for {}, with end token {:?}",
            r_id,
            end_token
        );

        let stream = stream! {
            while let Ok(Some(item)) = db.room_timeline.get(&db.encode_key_with_counter(TIMELINE, &r_id, position)) {
                position += 1;
                yield db.deserialize_event(&item).map_err(SledStoreError::from).map_err(|e| e.into());
            }
        };

        Ok(Some((Box::pin(stream), end_token)))
    }

    #[cfg(feature = "experimental-timeline")]
    async fn remove_room_timeline(&self, room_id: &RoomId) -> Result<()> {
        tracing::info!("Remove stored timeline for {}", room_id);

        let mut timeline_batch = sled::Batch::default();
        for key in self.room_timeline.scan_prefix(self.encode_key(TIMELINE, &room_id)).keys() {
            timeline_batch.remove(key?);
        }

        let mut event_id_to_position_batch = sled::Batch::default();
        for key in self
            .room_event_id_to_position
            .scan_prefix(self.encode_key(ROOM_EVENT_ID_POSITION, &room_id))
            .keys()
        {
            event_id_to_position_batch.remove(key?);
        }

        let ret: Result<(), TransactionError<SledStoreError>> =
            (&self.room_timeline, &self.room_timeline_metadata, &self.room_event_id_to_position)
                .transaction(
                    |(room_timeline, room_timeline_metadata, room_event_id_to_position)| {
                        room_timeline_metadata
                            .remove(self.encode_key(TIMELINE_METADATA, &room_id))?;

                        room_timeline.apply_batch(&timeline_batch)?;
                        room_event_id_to_position.apply_batch(&event_id_to_position_batch)?;

                        Ok(())
                    },
                );

        ret?;

        Ok(())
    }

    #[cfg(feature = "experimental-timeline")]
    async fn save_room_timeline(&self, changes: &StateChanges) -> Result<()> {
        let mut timeline_batch = sled::Batch::default();
        let mut event_id_to_position_batch = sled::Batch::default();
        let mut timeline_metadata_batch = sled::Batch::default();

        for (room_id, timeline) in &changes.timeline {
            if timeline.sync {
                tracing::info!("Save new timeline batch from sync response for {}", room_id);
            } else {
                tracing::info!("Save new timeline batch from messages response for {}", room_id);
            }

            let metadata: Option<TimelineMetadata> = if timeline.limited {
                tracing::info!(
                    "Delete stored timeline for {} because the sync response was limited",
                    room_id
                );
                self.remove_room_timeline(room_id).await?;
                None
            } else {
                let metadata: Option<TimelineMetadata> = self
                    .room_timeline_metadata
                    .get(self.encode_key(TIMELINE_METADATA, &room_id))?
                    .map(|v| serde_json::from_slice(&v).map_err(StoreError::Json))
                    .transpose()?;
                if let Some(mut metadata) = metadata {
                    if !timeline.sync && Some(&timeline.start) != metadata.end.as_ref() {
                        // This should only happen when a developer adds a wrong timeline
                        // batch to the `StateChanges` or the server returns a wrong response
                        // to our request.
                        tracing::warn!("Drop unexpected timeline batch for {}", room_id);
                        return Ok(());
                    }

                    // Check if the event already exists in the store
                    let mut delete_timeline = false;
                    for event in &timeline.events {
                        if let Some(event_id) = event.event_id() {
                            let event_key =
                                self.encode_key(ROOM_EVENT_ID_POSITION, (room_id, event_id));
                            if self.room_event_id_to_position.contains_key(event_key)? {
                                delete_timeline = true;
                                break;
                            }
                        }
                    }

                    if delete_timeline {
                        tracing::info!(
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
                    start_position: usize::MAX / 2 + 1,
                    end_position: usize::MAX / 2,
                }
            };
            let room_version = self
                .room_info
                .get(&self.encode_key(ROOM_INFO, room_id))?
                .map(|r| self.deserialize_event::<RoomInfo>(&r))
                .transpose()?
                .and_then(|info| info.room_version().cloned())
                .unwrap_or_else(|| {
                    tracing::warn!(
                        "Unable to find the room version for {}, assume version 9",
                        room_id
                    );
                    RoomVersionId::V9
                });

            if timeline.sync {
                for event in &timeline.events {
                    // Redact events already in store only on sync response
                    if let Ok(AnySyncRoomEvent::MessageLike(
                        AnySyncMessageLikeEvent::RoomRedaction(SyncRoomRedactionEvent::Original(
                            redaction,
                        )),
                    )) = event.event.deserialize()
                    {
                        let redacts_key =
                            self.encode_key(ROOM_EVENT_ID_POSITION, (room_id, redaction.redacts));
                        if let Some(position_key) =
                            self.room_event_id_to_position.get(redacts_key)?
                        {
                            if let Some(mut full_event) = self
                                .room_timeline
                                .get(position_key.as_ref())?
                                .map(|e| {
                                    self.deserialize_event::<SyncRoomEvent>(&e)
                                        .map_err(SledStoreError::from)
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
                    let key =
                        self.encode_key_with_counter(TIMELINE, room_id, metadata.start_position);
                    timeline_batch.insert(key.as_slice(), self.serialize_event(&event)?);
                    // Only add event with id to the position map
                    if let Some(event_id) = event.event_id() {
                        let event_key =
                            self.encode_key(ROOM_EVENT_ID_POSITION, (room_id, event_id));
                        event_id_to_position_batch.insert(event_key.as_slice(), key.as_slice());
                    }
                }
            } else {
                for event in &timeline.events {
                    metadata.end_position += 1;
                    let key =
                        self.encode_key_with_counter(TIMELINE, room_id, metadata.end_position);
                    timeline_batch.insert(key.as_slice(), self.serialize_event(&event)?);
                    // Only add event with id to the position map
                    if let Some(event_id) = event.event_id() {
                        let event_key =
                            self.encode_key(ROOM_EVENT_ID_POSITION, (room_id, event_id));
                        event_id_to_position_batch.insert(event_key.as_slice(), key.as_slice());
                    }
                }
            }

            timeline_metadata_batch.insert(
                self.encode_key(TIMELINE_METADATA, &room_id),
                serde_json::to_vec(&metadata)?,
            );
        }

        let ret: Result<(), TransactionError<SledStoreError>> =
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
    async fn save_filter(&self, filter_name: &str, filter_id: &str) -> StoreResult<()> {
        self.save_filter(filter_name, filter_id).await.map_err(Into::into)
    }

    async fn save_changes(&self, changes: &StateChanges) -> StoreResult<()> {
        self.save_changes(changes).await.map_err(Into::into)
    }

    async fn get_filter(&self, filter_id: &str) -> StoreResult<Option<String>> {
        self.get_filter(filter_id).await.map_err(Into::into)
    }

    async fn get_sync_token(&self) -> StoreResult<Option<String>> {
        self.get_sync_token().await.map_err(Into::into)
    }

    async fn get_presence_event(
        &self,
        user_id: &UserId,
    ) -> StoreResult<Option<Raw<PresenceEvent>>> {
        self.get_presence_event(user_id).await.map_err(Into::into)
    }

    async fn get_state_event(
        &self,
        room_id: &RoomId,
        event_type: StateEventType,
        state_key: &str,
    ) -> StoreResult<Option<Raw<AnySyncStateEvent>>> {
        self.get_state_event(room_id, event_type, state_key).await.map_err(Into::into)
    }

    async fn get_state_events(
        &self,
        room_id: &RoomId,
        event_type: StateEventType,
    ) -> StoreResult<Vec<Raw<AnySyncStateEvent>>> {
        self.get_state_events(room_id, event_type).await.map_err(Into::into)
    }

    async fn get_profile(
        &self,
        room_id: &RoomId,
        user_id: &UserId,
    ) -> StoreResult<Option<MinimalStateEvent<RoomMemberEventContent>>> {
        self.get_profile(room_id, user_id).await.map_err(Into::into)
    }

    async fn get_member_event(
        &self,
        room_id: &RoomId,
        state_key: &UserId,
    ) -> StoreResult<Option<MemberEvent>> {
        self.get_member_event(room_id, state_key).await.map_err(Into::into)
    }

    async fn get_user_ids(&self, room_id: &RoomId) -> StoreResult<Vec<OwnedUserId>> {
        let v: Vec<OwnedUserId> = self.get_user_ids_stream(room_id).await?.try_collect().await?;
        if !v.is_empty() {
            return Ok(v);
        }
        self.get_stripped_user_ids_stream(room_id).await?.try_collect().await
    }

    async fn get_invited_user_ids(&self, room_id: &RoomId) -> StoreResult<Vec<OwnedUserId>> {
        let v: Vec<OwnedUserId> = self.get_invited_user_ids(room_id).await?.try_collect().await?;
        if !v.is_empty() {
            return Ok(v);
        }
        self.get_stripped_invited_user_ids(room_id).await?.try_collect().await
    }

    async fn get_joined_user_ids(&self, room_id: &RoomId) -> StoreResult<Vec<OwnedUserId>> {
        let v: Vec<OwnedUserId> = self.get_joined_user_ids(room_id).await?.try_collect().await?;
        if !v.is_empty() {
            return Ok(v);
        }
        self.get_stripped_joined_user_ids(room_id).await?.try_collect().await
    }

    async fn get_room_infos(&self) -> StoreResult<Vec<RoomInfo>> {
        self.get_room_infos()
            .await
            .map_err::<StoreError, _>(Into::into)?
            .try_collect()
            .await
            .map_err::<StoreError, _>(Into::into)
    }

    async fn get_stripped_room_infos(&self) -> StoreResult<Vec<RoomInfo>> {
        self.get_stripped_room_infos()
            .await
            .map_err::<StoreError, _>(Into::into)?
            .try_collect()
            .await
            .map_err::<StoreError, _>(Into::into)
    }

    async fn get_users_with_display_name(
        &self,
        room_id: &RoomId,
        display_name: &str,
    ) -> StoreResult<BTreeSet<OwnedUserId>> {
        self.get_users_with_display_name(room_id, display_name).await.map_err(Into::into)
    }

    async fn get_account_data_event(
        &self,
        event_type: GlobalAccountDataEventType,
    ) -> StoreResult<Option<Raw<AnyGlobalAccountDataEvent>>> {
        self.get_account_data_event(event_type).await.map_err(Into::into)
    }

    async fn get_room_account_data_event(
        &self,
        room_id: &RoomId,
        event_type: RoomAccountDataEventType,
    ) -> StoreResult<Option<Raw<AnyRoomAccountDataEvent>>> {
        self.get_room_account_data_event(room_id, event_type).await.map_err(Into::into)
    }

    async fn get_user_room_receipt_event(
        &self,
        room_id: &RoomId,
        receipt_type: ReceiptType,
        user_id: &UserId,
    ) -> StoreResult<Option<(OwnedEventId, Receipt)>> {
        self.get_user_room_receipt_event(room_id, receipt_type, user_id).await.map_err(Into::into)
    }

    async fn get_event_room_receipt_events(
        &self,
        room_id: &RoomId,
        receipt_type: ReceiptType,
        event_id: &EventId,
    ) -> StoreResult<Vec<(OwnedUserId, Receipt)>> {
        self.get_event_room_receipt_events(room_id, receipt_type, event_id)
            .await
            .map_err(Into::into)
    }

    async fn get_custom_value(&self, key: &[u8]) -> StoreResult<Option<Vec<u8>>> {
        self.get_custom_value(key).await.map_err(Into::into)
    }

    async fn set_custom_value(&self, key: &[u8], value: Vec<u8>) -> StoreResult<Option<Vec<u8>>> {
        self.set_custom_value(key, value).await.map_err(Into::into)
    }

    async fn add_media_content(&self, request: &MediaRequest, data: Vec<u8>) -> StoreResult<()> {
        self.add_media_content(request, data).await.map_err(Into::into)
    }

    async fn get_media_content(&self, request: &MediaRequest) -> StoreResult<Option<Vec<u8>>> {
        self.get_media_content(request).await.map_err(Into::into)
    }

    async fn remove_media_content(&self, request: &MediaRequest) -> StoreResult<()> {
        self.remove_media_content(request).await.map_err(Into::into)
    }

    async fn remove_media_content_for_uri(&self, uri: &MxcUri) -> StoreResult<()> {
        self.remove_media_content_for_uri(uri).await.map_err(Into::into)
    }

    async fn remove_room(&self, room_id: &RoomId) -> StoreResult<()> {
        self.remove_room(room_id).await.map_err(Into::into)
    }

    #[cfg(feature = "experimental-timeline")]
    async fn room_timeline(
        &self,
        room_id: &RoomId,
    ) -> StoreResult<Option<(BoxStream<StoreResult<SyncRoomEvent>>, Option<String>)>> {
        self.room_timeline(room_id).await.map_err(|e| e.into())
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[cfg(feature = "experimental-timeline")]
struct TimelineMetadata {
    pub start: String,
    pub start_position: usize,
    pub end: Option<String>,
    pub end_position: usize,
}

#[cfg(test)]
mod tests {
    use matrix_sdk_base::statestore_integration_tests;

    use super::{SledStore, StateStore, StoreResult};

    async fn get_store() -> StoreResult<impl StateStore> {
        SledStore::open().map_err(Into::into)
    }

    statestore_integration_tests! { integration }
}

#[cfg(test)]
mod encrypted_tests {
    use matrix_sdk_base::statestore_integration_tests;

    use super::{SledStore, StateStore, StoreResult};

    async fn get_store() -> StoreResult<impl StateStore> {
        SledStore::open_encrypted().map_err(Into::into)
    }

    statestore_integration_tests! { integration }
}
