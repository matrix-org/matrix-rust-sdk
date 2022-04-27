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

use std::{collections::BTreeSet, sync::Arc};

use anyhow::anyhow;
use futures_util::stream;
use indexed_db_futures::prelude::*;
use matrix_sdk_base::{
    async_trait,
    deserialized_responses::{MemberEvent, SyncRoomEvent},
    media::{MediaRequest, UniqueKey},
    ruma::{
        events::{
            presence::PresenceEvent,
            receipt::Receipt,
            room::{
                member::{MembershipState, RoomMemberEventContent},
                redaction::SyncRoomRedactionEvent,
            },
            AnyGlobalAccountDataEvent, AnyRoomAccountDataEvent, AnySyncMessageLikeEvent,
            AnySyncRoomEvent, AnySyncStateEvent, GlobalAccountDataEventType,
            RoomAccountDataEventType, StateEventType,
        },
        receipt::ReceiptType,
        serde::Raw,
        signatures::{redact_in_place, CanonicalJsonObject},
        EventId, MxcUri, OwnedEventId, OwnedUserId, RoomId, RoomVersionId, UserId,
    },
    store::{BoxStream, Result as StoreResult, StateChanges, StateStore, StoreError},
    RoomInfo,
};
use matrix_sdk_store_encryption::{Error as EncryptionError, StoreCipher};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};
use wasm_bindgen::JsValue;
use web_sys::IdbKeyRange;

use crate::safe_encode::SafeEncode;

#[derive(Clone, Serialize, Deserialize)]
struct StoreKeyWrapper(Vec<u8>);

#[derive(Debug, thiserror::Error)]
pub enum SerializationError {
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Encryption(#[from] EncryptionError),
    #[error("DomException {name} ({code}): {message}")]
    DomException { name: String, message: String, code: u16 },
    #[error(transparent)]
    StoreError(#[from] StoreError),
}

impl From<indexed_db_futures::web_sys::DomException> for SerializationError {
    fn from(frm: indexed_db_futures::web_sys::DomException) -> SerializationError {
        SerializationError::DomException {
            name: frm.name(),
            message: frm.message(),
            code: frm.code(),
        }
    }
}

impl From<SerializationError> for StoreError {
    fn from(e: SerializationError) -> Self {
        match e {
            SerializationError::Json(e) => StoreError::Json(e),
            SerializationError::StoreError(e) => e,
            SerializationError::Encryption(e) => match e {
                EncryptionError::Random(e) => StoreError::Encryption(e.to_string()),
                EncryptionError::Serialization(e) => StoreError::Json(e),
                EncryptionError::Encryption(e) => StoreError::Encryption(e.to_string()),
                EncryptionError::Version(found, expected) => StoreError::Encryption(format!(
                    "Bad Database Encryption Version: expected {} found {}",
                    expected, found
                )),
                EncryptionError::Length(found, expected) => StoreError::Encryption(format!(
                    "The database key an invalid length: expected {} found {}",
                    expected, found
                )),
            },
            _ => StoreError::Backend(anyhow!(e)),
        }
    }
}

#[allow(non_snake_case)]
mod KEYS {
    // STORES

    pub const SESSION: &str = "session";
    pub const ACCOUNT_DATA: &str = "account_data";

    pub const MEMBERS: &str = "members";
    pub const PROFILES: &str = "profiles";
    pub const DISPLAY_NAMES: &str = "display_names";
    pub const JOINED_USER_IDS: &str = "joined_user_ids";
    pub const INVITED_USER_IDS: &str = "invited_user_ids";

    pub const ROOM_STATE: &str = "room_state";
    pub const ROOM_INFOS: &str = "room_infos";
    pub const PRESENCE: &str = "presence";
    pub const ROOM_ACCOUNT_DATA: &str = "room_account_data";

    pub const STRIPPED_ROOM_INFOS: &str = "stripped_room_infos";
    pub const STRIPPED_MEMBERS: &str = "stripped_members";
    pub const STRIPPED_ROOM_STATE: &str = "stripped_room_state";

    pub const ROOM_USER_RECEIPTS: &str = "room_user_receipts";
    pub const ROOM_EVENT_RECEIPTS: &str = "room_event_receipts";

    pub const ROOM_TIMELINE: &str = "room_timeline";
    pub const ROOM_TIMELINE_METADATA: &str = "room_timeline_metadata";
    pub const ROOM_EVENT_ID_TO_POSITION: &str = "room_event_id_to_position";

    pub const MEDIA: &str = "media";

    pub const CUSTOM: &str = "custom";

    // static keys

    pub const STORE_KEY: &str = "store_key";
    pub const FILTER: &str = "filter";
    pub const SYNC_TOKEN: &str = "sync_token";
}

pub struct IndexeddbStore {
    name: String,
    pub(crate) inner: IdbDatabase,
    pub(crate) store_cipher: Option<Arc<StoreCipher>>,
}

impl std::fmt::Debug for IndexeddbStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IndexeddbStore").field("name", &self.name).finish()
    }
}

type Result<A, E = SerializationError> = std::result::Result<A, E>;

impl IndexeddbStore {
    async fn open_helper(name: String, store_cipher: Option<Arc<StoreCipher>>) -> Result<Self> {
        // Open my_db v1
        let mut db_req: OpenDbRequest = IdbDatabase::open_f64(&name, 1.0)?;
        db_req.set_on_upgrade_needed(Some(|evt: &IdbVersionChangeEvent| -> Result<(), JsValue> {
            if evt.old_version() < 1.0 {
                // migrating to version 1
                let db = evt.db();

                db.create_object_store(KEYS::SESSION)?;
                db.create_object_store(KEYS::SYNC_TOKEN)?;
                db.create_object_store(KEYS::ACCOUNT_DATA)?;

                db.create_object_store(KEYS::MEMBERS)?;
                db.create_object_store(KEYS::PROFILES)?;
                db.create_object_store(KEYS::DISPLAY_NAMES)?;
                db.create_object_store(KEYS::JOINED_USER_IDS)?;
                db.create_object_store(KEYS::INVITED_USER_IDS)?;

                db.create_object_store(KEYS::ROOM_STATE)?;
                db.create_object_store(KEYS::ROOM_INFOS)?;
                db.create_object_store(KEYS::PRESENCE)?;
                db.create_object_store(KEYS::ROOM_ACCOUNT_DATA)?;

                db.create_object_store(KEYS::STRIPPED_ROOM_INFOS)?;
                db.create_object_store(KEYS::STRIPPED_MEMBERS)?;
                db.create_object_store(KEYS::STRIPPED_ROOM_STATE)?;

                db.create_object_store(KEYS::ROOM_USER_RECEIPTS)?;
                db.create_object_store(KEYS::ROOM_EVENT_RECEIPTS)?;

                db.create_object_store(KEYS::ROOM_TIMELINE)?;
                db.create_object_store(KEYS::ROOM_TIMELINE_METADATA)?;
                db.create_object_store(KEYS::ROOM_EVENT_ID_TO_POSITION)?;

                db.create_object_store(KEYS::MEDIA)?;

                db.create_object_store(KEYS::CUSTOM)?;
            }
            Ok(())
        }));

        let db: IdbDatabase = db_req.into_future().await?;

        Ok(Self { name, inner: db, store_cipher })
    }

    #[allow(dead_code)]
    pub async fn open() -> StoreResult<Self> {
        Ok(IndexeddbStore::open_helper("state".to_owned(), None).await?)
    }

    pub async fn open_with_passphrase(name: String, passphrase: &str) -> StoreResult<Self> {
        Ok(Self::inner_open_with_passphrase(name, passphrase).await?)
    }

    pub(crate) async fn inner_open_with_passphrase(name: String, passphrase: &str) -> Result<Self> {
        let name = format!("{:0}::matrix-sdk-state", name);

        let mut db_req: OpenDbRequest = IdbDatabase::open_u32(&name, 1)?;
        db_req.set_on_upgrade_needed(Some(|evt: &IdbVersionChangeEvent| -> Result<(), JsValue> {
            if evt.old_version() < 1.0 {
                // migrating to version 1
                let db = evt.db();

                db.create_object_store("matrix-sdk-state")?;
            }
            Ok(())
        }));

        let db: IdbDatabase = db_req.into_future().await?;

        let tx: IdbTransaction<'_> =
            db.transaction_on_one_with_mode("matrix-sdk-state", IdbTransactionMode::Readwrite)?;
        let ob = tx.object_store("matrix-sdk-state")?;

        let cipher = if let Some(StoreKeyWrapper(inner)) = ob
            .get(&JsValue::from_str(KEYS::STORE_KEY))?
            .await?
            .map(|v| v.into_serde())
            .transpose()?
        {
            StoreCipher::import(passphrase, &inner)?
        } else {
            let cipher = StoreCipher::new()?;
            ob.put_key_val(
                &JsValue::from_str(KEYS::STORE_KEY),
                &JsValue::from_serde(&StoreKeyWrapper(cipher.export(passphrase)?))?,
            )?;
            cipher
        };

        tx.await.into_result()?;

        IndexeddbStore::open_helper(name, Some(cipher.into())).await
    }

    pub async fn open_with_name(name: String) -> StoreResult<Self> {
        Ok(IndexeddbStore::open_helper(name, None).await?)
    }

    fn serialize_event(
        &self,
        event: &impl Serialize,
    ) -> std::result::Result<JsValue, SerializationError> {
        Ok(match self.store_cipher {
            Some(ref cipher) => JsValue::from_serde(&cipher.encrypt_value_typed(event)?)?,
            None => JsValue::from_serde(event)?,
        })
    }

    fn deserialize_event<T: for<'b> Deserialize<'b>>(
        &self,
        event: JsValue,
    ) -> std::result::Result<T, SerializationError> {
        match self.store_cipher {
            Some(ref cipher) => Ok(cipher.decrypt_value_typed(event.into_serde()?)?),
            None => Ok(event.into_serde()?),
        }
    }

    fn encode_key<T>(&self, table_name: &str, key: T) -> JsValue
    where
        T: SafeEncode,
    {
        match self.store_cipher {
            Some(ref cipher) => key.encode_secure(table_name, cipher),
            None => key.encode(),
        }
    }

    fn encode_to_range<T>(
        &self,
        table_name: &str,
        key: T,
    ) -> Result<IdbKeyRange, SerializationError>
    where
        T: SafeEncode,
    {
        match self.store_cipher {
            Some(ref cipher) => key.encode_to_range_secure(table_name, cipher),
            None => key.encode_to_range(),
        }
        .map_err(|e| SerializationError::StoreError(StoreError::Backend(anyhow!(e))))
    }

    fn encode_key_with_counter<T>(&self, table_name: &str, key: &T, i: usize) -> JsValue
    where
        T: SafeEncode,
    {
        match self.store_cipher {
            Some(ref cipher) => key.encode_with_counter_secure(table_name, cipher, i),
            None => key.encode_with_counter(i),
        }
    }

    pub async fn save_filter(&self, filter_name: &str, filter_id: &str) -> Result<()> {
        let tx = self
            .inner
            .transaction_on_one_with_mode(KEYS::SESSION, IdbTransactionMode::Readwrite)?;

        let obj = tx.object_store(KEYS::SESSION)?;

        obj.put_key_val(
            &self.encode_key(KEYS::FILTER, (KEYS::FILTER, filter_name)),
            &JsValue::from_str(filter_id),
        )?;

        tx.await.into_result()?;

        Ok(())
    }

    pub async fn get_filter(&self, filter_name: &str) -> Result<Option<String>> {
        Ok(self
            .inner
            .transaction_on_one_with_mode(KEYS::SESSION, IdbTransactionMode::Readonly)?
            .object_store(KEYS::SESSION)?
            .get(&self.encode_key(KEYS::FILTER, (KEYS::FILTER, filter_name)))?
            .await?
            .and_then(|f| f.as_string()))
    }

    pub async fn get_sync_token(&self) -> Result<Option<String>> {
        self.inner
            .transaction_on_one_with_mode(KEYS::SYNC_TOKEN, IdbTransactionMode::Readonly)?
            .object_store(KEYS::SYNC_TOKEN)?
            .get(&JsValue::from_str(KEYS::SYNC_TOKEN))?
            .await?
            .map(|f| self.deserialize_event(f))
            .transpose()
    }

    pub async fn save_changes(&self, changes: &StateChanges) -> Result<()> {
        let mut stores: Vec<&'static str> = [
            (changes.sync_token.is_some(), KEYS::SYNC_TOKEN),
            (changes.session.is_some(), KEYS::SESSION),
            (!changes.ambiguity_maps.is_empty(), KEYS::DISPLAY_NAMES),
            (!changes.account_data.is_empty(), KEYS::ACCOUNT_DATA),
            (!changes.presence.is_empty(), KEYS::PRESENCE),
            (!changes.profiles.is_empty(), KEYS::PROFILES),
            (!changes.state.is_empty(), KEYS::ROOM_STATE),
            (!changes.room_account_data.is_empty(), KEYS::ROOM_ACCOUNT_DATA),
            (!changes.room_infos.is_empty() || !changes.timeline.is_empty(), KEYS::ROOM_INFOS),
            (!changes.receipts.is_empty(), KEYS::ROOM_EVENT_RECEIPTS),
            (!changes.stripped_state.is_empty(), KEYS::STRIPPED_ROOM_STATE),
            (!changes.stripped_room_infos.is_empty(), KEYS::STRIPPED_ROOM_INFOS),
        ]
        .iter()
        .filter_map(|(id, key)| if *id { Some(*key) } else { None })
        .collect();

        if !changes.members.is_empty() {
            stores.extend([
                KEYS::PROFILES,
                KEYS::MEMBERS,
                KEYS::INVITED_USER_IDS,
                KEYS::JOINED_USER_IDS,
            ])
        }

        if !changes.stripped_members.is_empty() {
            stores.extend([KEYS::STRIPPED_MEMBERS, KEYS::INVITED_USER_IDS, KEYS::JOINED_USER_IDS])
        }

        if !changes.receipts.is_empty() {
            stores.extend([KEYS::ROOM_EVENT_RECEIPTS, KEYS::ROOM_USER_RECEIPTS])
        }

        if !changes.timeline.is_empty() {
            stores.extend([
                KEYS::ROOM_TIMELINE,
                KEYS::ROOM_TIMELINE_METADATA,
                KEYS::ROOM_EVENT_ID_TO_POSITION,
            ])
        }

        if stores.is_empty() {
            // nothing to do, quit early
            return Ok(());
        }

        let tx =
            self.inner.transaction_on_multi_with_mode(&stores, IdbTransactionMode::Readwrite)?;

        if let Some(s) = &changes.sync_token {
            tx.object_store(KEYS::SYNC_TOKEN)?
                .put_key_val(&JsValue::from_str(KEYS::SYNC_TOKEN), &self.serialize_event(s)?)?;
        }

        if !changes.ambiguity_maps.is_empty() {
            let store = tx.object_store(KEYS::DISPLAY_NAMES)?;
            for (room_id, ambiguity_maps) in &changes.ambiguity_maps {
                for (display_name, map) in ambiguity_maps {
                    let key = self.encode_key(KEYS::DISPLAY_NAMES, (room_id, display_name));

                    store.put_key_val(&key, &self.serialize_event(&map)?)?;
                }
            }
        }

        if !changes.account_data.is_empty() {
            let store = tx.object_store(KEYS::ACCOUNT_DATA)?;
            for (event_type, event) in &changes.account_data {
                store.put_key_val(
                    &self.encode_key(KEYS::ACCOUNT_DATA, event_type),
                    &self.serialize_event(&event)?,
                )?;
            }
        }

        if !changes.room_account_data.is_empty() {
            let store = tx.object_store(KEYS::ROOM_ACCOUNT_DATA)?;
            for (room, events) in &changes.room_account_data {
                for (event_type, event) in events {
                    let key = self.encode_key(KEYS::ROOM_ACCOUNT_DATA, (room, event_type));
                    store.put_key_val(&key, &self.serialize_event(&event)?)?;
                }
            }
        }

        if !changes.state.is_empty() {
            let store = tx.object_store(KEYS::ROOM_STATE)?;
            for (room, event_types) in &changes.state {
                for (event_type, events) in event_types {
                    for (state_key, event) in events {
                        let key = self.encode_key(KEYS::ROOM_STATE, (room, event_type, state_key));
                        store.put_key_val(&key, &self.serialize_event(&event)?)?;
                    }
                }
            }
        }

        if !changes.room_infos.is_empty() {
            let store = tx.object_store(KEYS::ROOM_INFOS)?;
            for (room_id, room_info) in &changes.room_infos {
                store.put_key_val(
                    &self.encode_key(KEYS::ROOM_INFOS, room_id),
                    &self.serialize_event(&room_info)?,
                )?;
            }
        }

        if !changes.presence.is_empty() {
            let store = tx.object_store(KEYS::PRESENCE)?;
            for (sender, event) in &changes.presence {
                store.put_key_val(
                    &self.encode_key(KEYS::PRESENCE, sender),
                    &self.serialize_event(&event)?,
                )?;
            }
        }

        if !changes.stripped_room_infos.is_empty() {
            let store = tx.object_store(KEYS::STRIPPED_ROOM_INFOS)?;
            for (room_id, info) in &changes.stripped_room_infos {
                store.put_key_val(
                    &self.encode_key(KEYS::STRIPPED_ROOM_INFOS, room_id),
                    &self.serialize_event(&info)?,
                )?;
            }
        }

        if !changes.stripped_members.is_empty() {
            let store = tx.object_store(KEYS::STRIPPED_MEMBERS)?;
            let joined = tx.object_store(KEYS::JOINED_USER_IDS)?;
            let invited = tx.object_store(KEYS::INVITED_USER_IDS)?;
            for (room, events) in &changes.stripped_members {
                for event in events.values() {
                    let key = (room, &event.state_key);

                    match event.content.membership {
                        MembershipState::Join => {
                            joined.put_key_val_owned(
                                &self.encode_key(KEYS::JOINED_USER_IDS, key),
                                &self.serialize_event(&event.state_key)?,
                            )?;
                            invited.delete(&self.encode_key(KEYS::INVITED_USER_IDS, key))?;
                        }
                        MembershipState::Invite => {
                            invited.put_key_val_owned(
                                &self.encode_key(KEYS::INVITED_USER_IDS, key),
                                &self.serialize_event(&event.state_key)?,
                            )?;
                            joined.delete(&self.encode_key(KEYS::JOINED_USER_IDS, key))?;
                        }
                        _ => {
                            joined.delete(&self.encode_key(KEYS::JOINED_USER_IDS, key))?;
                            invited.delete(&self.encode_key(KEYS::INVITED_USER_IDS, key))?;
                        }
                    }
                    store.put_key_val(
                        &self.encode_key(KEYS::STRIPPED_MEMBERS, key),
                        &self.serialize_event(&event)?,
                    )?;
                }
            }
        }

        if !changes.stripped_state.is_empty() {
            let store = tx.object_store(KEYS::STRIPPED_ROOM_STATE)?;
            for (room, event_types) in &changes.stripped_state {
                for (event_type, events) in event_types {
                    for (state_key, event) in events {
                        let key = self
                            .encode_key(KEYS::STRIPPED_ROOM_STATE, (room, event_type, state_key));
                        store.put_key_val(&key, &self.serialize_event(&event)?)?;
                    }
                }
            }
        }

        if !changes.members.is_empty() {
            let profiles_store = tx.object_store(KEYS::PROFILES)?;
            let joined = tx.object_store(KEYS::JOINED_USER_IDS)?;
            let invited = tx.object_store(KEYS::INVITED_USER_IDS)?;
            let members = tx.object_store(KEYS::MEMBERS)?;

            for (room, events) in &changes.members {
                let profile_changes = changes.profiles.get(room);

                for event in events.values() {
                    let key = (room, &event.state_key);

                    match event.content.membership {
                        MembershipState::Join => {
                            joined.put_key_val_owned(
                                &self.encode_key(KEYS::JOINED_USER_IDS, key),
                                &self.serialize_event(&event.state_key)?,
                            )?;
                            invited.delete(&self.encode_key(KEYS::INVITED_USER_IDS, key))?;
                        }
                        MembershipState::Invite => {
                            invited.put_key_val_owned(
                                &self.encode_key(KEYS::INVITED_USER_IDS, key),
                                &self.serialize_event(&event.state_key)?,
                            )?;
                            joined.delete(&self.encode_key(KEYS::JOINED_USER_IDS, key))?;
                        }
                        _ => {
                            joined.delete(&self.encode_key(KEYS::JOINED_USER_IDS, key))?;
                            invited.delete(&self.encode_key(KEYS::INVITED_USER_IDS, key))?;
                        }
                    }

                    members.put_key_val_owned(
                        &self.encode_key(KEYS::MEMBERS, key),
                        &self.serialize_event(&event)?,
                    )?;

                    if let Some(profile) = profile_changes.and_then(|p| p.get(&event.state_key)) {
                        profiles_store.put_key_val_owned(
                            &self.encode_key(KEYS::PROFILES, key),
                            &self.serialize_event(&profile)?,
                        )?;
                    }
                }
            }
        }

        if !changes.receipts.is_empty() {
            let room_user_receipts = tx.object_store(KEYS::ROOM_USER_RECEIPTS)?;
            let room_event_receipts = tx.object_store(KEYS::ROOM_EVENT_RECEIPTS)?;

            for (room, content) in &changes.receipts {
                for (event_id, receipts) in &content.0 {
                    for (receipt_type, receipts) in receipts {
                        for (user_id, receipt) in receipts {
                            let key = self.encode_key(
                                KEYS::ROOM_USER_RECEIPTS,
                                (room, receipt_type, user_id),
                            );

                            if let Some((old_event, _)) =
                                room_user_receipts.get(&key)?.await?.and_then(|f| {
                                    self.deserialize_event::<(OwnedEventId, Receipt)>(f).ok()
                                })
                            {
                                room_event_receipts.delete(&self.encode_key(
                                    KEYS::ROOM_EVENT_RECEIPTS,
                                    (room, receipt_type, &old_event, user_id),
                                ))?;
                            }

                            room_user_receipts
                                .put_key_val(&key, &self.serialize_event(&(event_id, receipt))?)?;

                            // Add the receipt to the room event receipts
                            room_event_receipts.put_key_val(
                                &self.encode_key(
                                    KEYS::ROOM_EVENT_RECEIPTS,
                                    (room, receipt_type, event_id, user_id),
                                ),
                                &self.serialize_event(&(user_id, receipt))?,
                            )?;
                        }
                    }
                }
            }
        }

        if !changes.timeline.is_empty() {
            let timeline_store = tx.object_store(KEYS::ROOM_TIMELINE)?;
            let timeline_metadata_store = tx.object_store(KEYS::ROOM_TIMELINE_METADATA)?;
            let event_id_to_position_store = tx.object_store(KEYS::ROOM_EVENT_ID_TO_POSITION)?;
            let room_infos = tx.object_store(KEYS::ROOM_INFOS)?;

            for (room_id, timeline) in &changes.timeline {
                if timeline.sync {
                    info!("Save new timeline batch from sync response for {}", room_id);
                } else {
                    info!("Save new timeline batch from messages response for {}", room_id);
                }
                let metadata: Option<TimelineMetadata> = if timeline.limited {
                    info!(
                        "Delete stored timeline for {} because the sync response was limited",
                        room_id
                    );

                    let stores = &[
                        (KEYS::ROOM_TIMELINE, &timeline_store),
                        (KEYS::ROOM_TIMELINE_METADATA, &timeline_metadata_store),
                        (KEYS::ROOM_EVENT_ID_TO_POSITION, &event_id_to_position_store),
                    ];
                    for (table_name, store) in stores {
                        let range = self.encode_to_range(table_name, room_id)?;
                        for key in store.get_all_keys_with_key(&range)?.await?.iter() {
                            store.delete(&key)?;
                        }
                    }

                    None
                } else {
                    let metadata: Option<TimelineMetadata> = timeline_metadata_store
                        .get(&self.encode_key(KEYS::ROOM_TIMELINE_METADATA, room_id))?
                        .await?
                        .map(|v| v.into_serde())
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
                                let event_key = self.encode_key(
                                    KEYS::ROOM_EVENT_ID_TO_POSITION,
                                    (room_id, event_id),
                                );
                                if event_id_to_position_store
                                    .count_with_key_owned(event_key)?
                                    .await?
                                    > 0
                                {
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

                            let stores = &[
                                (KEYS::ROOM_TIMELINE, &timeline_store),
                                (KEYS::ROOM_TIMELINE_METADATA, &timeline_metadata_store),
                                (KEYS::ROOM_EVENT_ID_TO_POSITION, &event_id_to_position_store),
                            ];
                            for (table_name, store) in stores {
                                let range = self.encode_to_range(table_name, room_id)?;
                                for key in store.get_all_keys_with_key(&range)?.await?.iter() {
                                    store.delete(&key)?;
                                }
                            }

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

                if timeline.sync {
                    let room_version = room_infos
                        .get(&self.encode_key(KEYS::ROOM_INFOS, room_id))?
                        .await?
                        .map(|r| self.deserialize_event::<RoomInfo>(r))
                        .transpose()?
                        .and_then(|info| info.room_version().cloned())
                        .unwrap_or_else(|| {
                            warn!(
                                "Unable to find the room version for {}, assume version 9",
                                room_id
                            );
                            RoomVersionId::V9
                        });
                    for event in &timeline.events {
                        // Redact events already in store only on sync response
                        if let Ok(AnySyncRoomEvent::MessageLike(
                            AnySyncMessageLikeEvent::RoomRedaction(
                                SyncRoomRedactionEvent::Original(redaction),
                            ),
                        )) = event.event.deserialize()
                        {
                            let redacts_key = self.encode_key(
                                KEYS::ROOM_EVENT_ID_TO_POSITION,
                                (room_id, &redaction.redacts),
                            );
                            if let Some(position_key) =
                                event_id_to_position_store.get_owned(redacts_key)?.await?
                            {
                                if let Some(mut full_event) = timeline_store
                                    .get(&position_key)?
                                    .await?
                                    .map(|e| {
                                        self.deserialize_event::<SyncRoomEvent>(e)
                                            .map_err(StoreError::from)
                                    })
                                    .transpose()?
                                {
                                    let mut event_json: CanonicalJsonObject =
                                        full_event.event.deserialize_as()?;
                                    redact_in_place(&mut event_json, &room_version)
                                        .map_err(StoreError::Redaction)?;
                                    full_event.event = Raw::new(&event_json)?.cast();
                                    timeline_store.put_key_val_owned(
                                        position_key,
                                        &self.serialize_event(&full_event)?,
                                    )?;
                                }
                            }
                        }

                        metadata.start_position -= 1;
                        let key = self.encode_key_with_counter(
                            KEYS::ROOM_TIMELINE,
                            room_id,
                            metadata.start_position,
                        );
                        // Only add event with id to the position map
                        if let Some(event_id) = event.event_id() {
                            let event_key = self
                                .encode_key(KEYS::ROOM_EVENT_ID_TO_POSITION, (room_id, event_id));
                            event_id_to_position_store.put_key_val(&event_key, &key)?;
                        }

                        timeline_store.put_key_val_owned(&key, &self.serialize_event(&event)?)?;
                    }
                } else {
                    for event in &timeline.events {
                        metadata.end_position += 1;
                        let key = self.encode_key_with_counter(
                            KEYS::ROOM_TIMELINE,
                            room_id,
                            metadata.end_position,
                        );
                        // Only add event with id to the position map
                        if let Some(event_id) = event.event_id() {
                            let event_key = self
                                .encode_key(KEYS::ROOM_EVENT_ID_TO_POSITION, (room_id, event_id));
                            event_id_to_position_store.put_key_val(&event_key, &key)?;
                        }

                        timeline_store.put_key_val_owned(key, &self.serialize_event(&event)?)?;
                    }
                }

                timeline_metadata_store.put_key_val_owned(
                    &self.encode_key(KEYS::ROOM_TIMELINE_METADATA, room_id),
                    &JsValue::from_serde(&metadata)?,
                )?;
            }
        }

        tx.await.into_result().map_err::<SerializationError, _>(|e| e.into())
    }

    pub async fn get_presence_event(&self, user_id: &UserId) -> Result<Option<Raw<PresenceEvent>>> {
        self.inner
            .transaction_on_one_with_mode(KEYS::PRESENCE, IdbTransactionMode::Readonly)?
            .object_store(KEYS::PRESENCE)?
            .get(&self.encode_key(KEYS::PRESENCE, user_id))?
            .await?
            .map(|f| self.deserialize_event(f))
            .transpose()
    }

    pub async fn get_state_event(
        &self,
        room_id: &RoomId,
        event_type: StateEventType,
        state_key: &str,
    ) -> Result<Option<Raw<AnySyncStateEvent>>> {
        self.inner
            .transaction_on_one_with_mode(KEYS::ROOM_STATE, IdbTransactionMode::Readonly)?
            .object_store(KEYS::ROOM_STATE)?
            .get(&self.encode_key(KEYS::ROOM_STATE, (room_id, event_type, state_key)))?
            .await?
            .map(|f| self.deserialize_event(f))
            .transpose()
    }

    pub async fn get_state_events(
        &self,
        room_id: &RoomId,
        event_type: StateEventType,
    ) -> Result<Vec<Raw<AnySyncStateEvent>>> {
        let range = self.encode_to_range(KEYS::ROOM_STATE, (room_id, event_type))?;
        Ok(self
            .inner
            .transaction_on_one_with_mode(KEYS::ROOM_STATE, IdbTransactionMode::Readonly)?
            .object_store(KEYS::ROOM_STATE)?
            .get_all_with_key(&range)?
            .await?
            .iter()
            .filter_map(|f| self.deserialize_event(f).ok())
            .collect::<Vec<_>>())
    }

    pub async fn get_profile(
        &self,
        room_id: &RoomId,
        user_id: &UserId,
    ) -> Result<Option<RoomMemberEventContent>> {
        self.inner
            .transaction_on_one_with_mode(KEYS::PROFILES, IdbTransactionMode::Readonly)?
            .object_store(KEYS::PROFILES)?
            .get(&self.encode_key(KEYS::PROFILES, (room_id, user_id)))?
            .await?
            .map(|f| self.deserialize_event(f))
            .transpose()
    }

    pub async fn get_member_event(
        &self,
        room_id: &RoomId,
        state_key: &UserId,
    ) -> Result<Option<MemberEvent>> {
        if let Some(e) = self
            .inner
            .transaction_on_one_with_mode(KEYS::MEMBERS, IdbTransactionMode::Readonly)?
            .object_store(KEYS::MEMBERS)?
            .get(&self.encode_key(KEYS::MEMBERS, (room_id, state_key)))?
            .await?
            .map(|f| self.deserialize_event(f))
            .transpose()?
        {
            Ok(Some(MemberEvent::Original(e)))
        } else if let Some(e) = self
            .inner
            .transaction_on_one_with_mode(KEYS::STRIPPED_MEMBERS, IdbTransactionMode::Readonly)?
            .object_store(KEYS::STRIPPED_MEMBERS)?
            .get(&self.encode_key(KEYS::STRIPPED_MEMBERS, (room_id, state_key)))?
            .await?
            .map(|f| self.deserialize_event(f))
            .transpose()?
        {
            Ok(Some(MemberEvent::Stripped(e)))
        } else {
            Ok(None)
        }
    }

    pub async fn get_user_ids_stream(&self, room_id: &RoomId) -> Result<Vec<OwnedUserId>> {
        Ok([self.get_invited_user_ids(room_id).await?, self.get_joined_user_ids(room_id).await?]
            .concat())
    }

    pub async fn get_invited_user_ids(&self, room_id: &RoomId) -> Result<Vec<OwnedUserId>> {
        let range = self.encode_to_range(KEYS::INVITED_USER_IDS, room_id)?;
        let entries = self
            .inner
            .transaction_on_one_with_mode(KEYS::INVITED_USER_IDS, IdbTransactionMode::Readonly)?
            .object_store(KEYS::INVITED_USER_IDS)?
            .get_all_with_key(&range)?
            .await?
            .iter()
            .filter_map(|f| self.deserialize_event::<OwnedUserId>(f).ok())
            .collect::<Vec<_>>();

        Ok(entries)
    }

    pub async fn get_joined_user_ids(&self, room_id: &RoomId) -> Result<Vec<OwnedUserId>> {
        let range = self.encode_to_range(KEYS::JOINED_USER_IDS, room_id)?;
        Ok(self
            .inner
            .transaction_on_one_with_mode(KEYS::JOINED_USER_IDS, IdbTransactionMode::Readonly)?
            .object_store(KEYS::JOINED_USER_IDS)?
            .get_all_with_key(&range)?
            .await?
            .iter()
            .filter_map(|f| self.deserialize_event::<OwnedUserId>(f).ok())
            .collect::<Vec<_>>())
    }

    pub async fn get_room_infos(&self) -> Result<Vec<RoomInfo>> {
        let entries: Vec<_> = self
            .inner
            .transaction_on_one_with_mode(KEYS::ROOM_INFOS, IdbTransactionMode::Readonly)?
            .object_store(KEYS::ROOM_INFOS)?
            .get_all()?
            .await?
            .iter()
            .filter_map(|f| self.deserialize_event::<RoomInfo>(f).ok())
            .collect();

        Ok(entries)
    }

    pub async fn get_stripped_room_infos(&self) -> Result<Vec<RoomInfo>> {
        let entries = self
            .inner
            .transaction_on_one_with_mode(KEYS::STRIPPED_ROOM_INFOS, IdbTransactionMode::Readonly)?
            .object_store(KEYS::STRIPPED_ROOM_INFOS)?
            .get_all()?
            .await?
            .iter()
            .filter_map(|f| self.deserialize_event(f).ok())
            .collect::<Vec<_>>();

        Ok(entries)
    }

    pub async fn get_users_with_display_name(
        &self,
        room_id: &RoomId,
        display_name: &str,
    ) -> Result<BTreeSet<OwnedUserId>> {
        self.inner
            .transaction_on_one_with_mode(KEYS::DISPLAY_NAMES, IdbTransactionMode::Readonly)?
            .object_store(KEYS::DISPLAY_NAMES)?
            .get(&self.encode_key(KEYS::DISPLAY_NAMES, (room_id, display_name)))?
            .await?
            .map(|f| {
                self.deserialize_event::<BTreeSet<OwnedUserId>>(f)
                    .map_err::<SerializationError, _>(|e| e)
            })
            .unwrap_or_else(|| Ok(Default::default()))
    }

    pub async fn get_account_data_event(
        &self,
        event_type: GlobalAccountDataEventType,
    ) -> Result<Option<Raw<AnyGlobalAccountDataEvent>>> {
        self.inner
            .transaction_on_one_with_mode(KEYS::ACCOUNT_DATA, IdbTransactionMode::Readonly)?
            .object_store(KEYS::ACCOUNT_DATA)?
            .get(&self.encode_key(KEYS::ACCOUNT_DATA, event_type))?
            .await?
            .map(|f| self.deserialize_event(f).map_err::<SerializationError, _>(|e| e))
            .transpose()
    }

    pub async fn get_room_account_data_event(
        &self,
        room_id: &RoomId,
        event_type: RoomAccountDataEventType,
    ) -> Result<Option<Raw<AnyRoomAccountDataEvent>>> {
        self.inner
            .transaction_on_one_with_mode(KEYS::ROOM_ACCOUNT_DATA, IdbTransactionMode::Readonly)?
            .object_store(KEYS::ROOM_ACCOUNT_DATA)?
            .get(&self.encode_key(KEYS::ROOM_ACCOUNT_DATA, (room_id, event_type)))?
            .await?
            .map(|f| self.deserialize_event(f).map_err::<SerializationError, _>(|e| e))
            .transpose()
    }

    async fn get_user_room_receipt_event(
        &self,
        room_id: &RoomId,
        receipt_type: ReceiptType,
        user_id: &UserId,
    ) -> Result<Option<(OwnedEventId, Receipt)>> {
        self.inner
            .transaction_on_one_with_mode(KEYS::ROOM_USER_RECEIPTS, IdbTransactionMode::Readonly)?
            .object_store(KEYS::ROOM_USER_RECEIPTS)?
            .get(&self.encode_key(KEYS::ROOM_USER_RECEIPTS, (room_id, receipt_type, user_id)))?
            .await?
            .map(|f| self.deserialize_event(f))
            .transpose()
    }

    async fn get_event_room_receipt_events(
        &self,
        room_id: &RoomId,
        receipt_type: ReceiptType,
        event_id: &EventId,
    ) -> Result<Vec<(OwnedUserId, Receipt)>> {
        let range =
            self.encode_to_range(KEYS::ROOM_EVENT_RECEIPTS, (room_id, &receipt_type, event_id))?;
        let tx = self.inner.transaction_on_one_with_mode(
            KEYS::ROOM_EVENT_RECEIPTS,
            IdbTransactionMode::Readonly,
        )?;
        let store = tx.object_store(KEYS::ROOM_EVENT_RECEIPTS)?;

        Ok(store
            .get_all_with_key(&range)?
            .await?
            .iter()
            .filter_map(|f| self.deserialize_event(f).ok())
            .collect::<Vec<_>>())
    }

    async fn add_media_content(&self, request: &MediaRequest, data: Vec<u8>) -> Result<()> {
        let key = self
            .encode_key(KEYS::MEDIA, (request.source.unique_key(), request.format.unique_key()));
        let tx =
            self.inner.transaction_on_one_with_mode(KEYS::MEDIA, IdbTransactionMode::Readwrite)?;

        tx.object_store(KEYS::MEDIA)?.put_key_val(&key, &self.serialize_event(&data)?)?;

        tx.await.into_result().map_err(|e| e.into())
    }

    async fn get_media_content(&self, request: &MediaRequest) -> Result<Option<Vec<u8>>> {
        let key = self
            .encode_key(KEYS::MEDIA, (request.source.unique_key(), request.format.unique_key()));
        self.inner
            .transaction_on_one_with_mode(KEYS::MEDIA, IdbTransactionMode::Readonly)?
            .object_store(KEYS::MEDIA)?
            .get(&key)?
            .await?
            .map(|f| self.deserialize_event(f))
            .transpose()
    }

    async fn get_custom_value(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        let jskey = &JsValue::from_str(
            core::str::from_utf8(key).map_err(|e| StoreError::Codec(format!("{:}", e)))?,
        );
        self.get_custom_value_for_js(jskey).await
    }

    async fn get_custom_value_for_js(&self, jskey: &JsValue) -> Result<Option<Vec<u8>>> {
        self.inner
            .transaction_on_one_with_mode(KEYS::CUSTOM, IdbTransactionMode::Readonly)?
            .object_store(KEYS::CUSTOM)?
            .get(jskey)?
            .await?
            .map(|f| self.deserialize_event(f))
            .transpose()
    }

    async fn set_custom_value(&self, key: &[u8], value: Vec<u8>) -> Result<Option<Vec<u8>>> {
        let jskey = JsValue::from_str(
            core::str::from_utf8(key).map_err(|e| StoreError::Codec(format!("{:}", e)))?,
        );

        let prev = self.get_custom_value_for_js(&jskey).await?;

        let tx =
            self.inner.transaction_on_one_with_mode(KEYS::CUSTOM, IdbTransactionMode::Readwrite)?;

        tx.object_store(KEYS::CUSTOM)?.put_key_val(&jskey, &self.serialize_event(&value)?)?;

        tx.await.into_result().map_err::<SerializationError, _>(|e| e.into())?;
        Ok(prev)
    }

    async fn remove_media_content(&self, request: &MediaRequest) -> Result<()> {
        let key = self
            .encode_key(KEYS::MEDIA, (request.source.unique_key(), request.format.unique_key()));
        let tx =
            self.inner.transaction_on_one_with_mode(KEYS::MEDIA, IdbTransactionMode::Readwrite)?;

        tx.object_store(KEYS::MEDIA)?.delete(&key)?;

        tx.await.into_result().map_err(|e| e.into())
    }

    async fn remove_media_content_for_uri(&self, uri: &MxcUri) -> Result<()> {
        let range = self.encode_to_range(KEYS::MEDIA, uri)?;
        let tx =
            self.inner.transaction_on_one_with_mode(KEYS::MEDIA, IdbTransactionMode::Readwrite)?;
        let store = tx.object_store(KEYS::MEDIA)?;

        for k in store.get_all_keys_with_key(&range)?.await?.iter() {
            store.delete(&k)?;
        }

        tx.await.into_result().map_err(|e| e.into())
    }

    async fn remove_room(&self, room_id: &RoomId) -> Result<()> {
        let direct_stores = [KEYS::ROOM_INFOS, KEYS::STRIPPED_ROOM_INFOS];

        let prefixed_stores = [
            KEYS::MEMBERS,
            KEYS::PROFILES,
            KEYS::DISPLAY_NAMES,
            KEYS::INVITED_USER_IDS,
            KEYS::JOINED_USER_IDS,
            KEYS::ROOM_STATE,
            KEYS::ROOM_ACCOUNT_DATA,
            KEYS::ROOM_EVENT_RECEIPTS,
            KEYS::ROOM_USER_RECEIPTS,
            KEYS::STRIPPED_ROOM_STATE,
            KEYS::STRIPPED_MEMBERS,
            KEYS::ROOM_TIMELINE,
            KEYS::ROOM_TIMELINE_METADATA,
            KEYS::ROOM_EVENT_ID_TO_POSITION,
        ];

        let all_stores = {
            let mut v = Vec::new();
            v.extend(prefixed_stores);
            v.extend(direct_stores);
            v
        };

        let tx = self
            .inner
            .transaction_on_multi_with_mode(&all_stores, IdbTransactionMode::Readwrite)?;

        for store_name in direct_stores {
            tx.object_store(store_name)?.delete(&self.encode_key(store_name, room_id))?;
        }

        for store_name in prefixed_stores {
            let store = tx.object_store(store_name)?;
            let range = self.encode_to_range(store_name, room_id)?;
            for key in store.get_all_keys_with_key(&range)?.await?.iter() {
                store.delete(&key)?;
            }
        }
        tx.await.into_result().map_err::<SerializationError, _>(|e| e.into())
    }

    async fn room_timeline(
        &self,
        room_id: &RoomId,
    ) -> Result<Option<(BoxStream<StoreResult<SyncRoomEvent>>, Option<String>)>> {
        let tx = self.inner.transaction_on_multi_with_mode(
            &[KEYS::ROOM_TIMELINE, KEYS::ROOM_TIMELINE_METADATA],
            IdbTransactionMode::Readonly,
        )?;
        let timeline = tx.object_store(KEYS::ROOM_TIMELINE)?;
        let metadata = tx.object_store(KEYS::ROOM_TIMELINE_METADATA)?;

        let tlm: TimelineMetadata = match metadata
            .get(&self.encode_key(KEYS::ROOM_TIMELINE_METADATA, room_id))?
            .await?
            .map(|v| v.into_serde())
            .transpose()?
        {
            Some(tl) => tl,
            _ => {
                info!("No timeline for {} was previously stored", room_id);
                return Ok(None);
            }
        };

        let end_token = tlm.end;
        #[allow(clippy::needless_collect)]
        let timeline: Vec<StoreResult<SyncRoomEvent>> = timeline
            .get_all_with_key(&self.encode_to_range(KEYS::ROOM_TIMELINE, room_id)?)?
            .await?
            .iter()
            .map(|v| self.deserialize_event(v).map_err(|e| e.into()))
            .collect();

        let stream = Box::pin(stream::iter(timeline.into_iter()));

        info!("Found previously stored timeline for {}, with end token {:?}", room_id, end_token);

        Ok(Some((stream, end_token)))
    }
}

#[async_trait(?Send)]
impl StateStore for IndexeddbStore {
    async fn save_filter(&self, filter_name: &str, filter_id: &str) -> StoreResult<()> {
        self.save_filter(filter_name, filter_id).await.map_err(|e| e.into())
    }

    async fn save_changes(&self, changes: &StateChanges) -> StoreResult<()> {
        self.save_changes(changes).await.map_err(|e| e.into())
    }

    async fn get_filter(&self, filter_id: &str) -> StoreResult<Option<String>> {
        self.get_filter(filter_id).await.map_err(|e| e.into())
    }

    async fn get_sync_token(&self) -> StoreResult<Option<String>> {
        self.get_sync_token().await.map_err(|e| e.into())
    }

    async fn get_presence_event(
        &self,
        user_id: &UserId,
    ) -> StoreResult<Option<Raw<PresenceEvent>>> {
        self.get_presence_event(user_id).await.map_err(|e| e.into())
    }

    async fn get_state_event(
        &self,
        room_id: &RoomId,
        event_type: StateEventType,
        state_key: &str,
    ) -> StoreResult<Option<Raw<AnySyncStateEvent>>> {
        self.get_state_event(room_id, event_type, state_key).await.map_err(|e| e.into())
    }

    async fn get_state_events(
        &self,
        room_id: &RoomId,
        event_type: StateEventType,
    ) -> StoreResult<Vec<Raw<AnySyncStateEvent>>> {
        self.get_state_events(room_id, event_type).await.map_err(|e| e.into())
    }

    async fn get_profile(
        &self,
        room_id: &RoomId,
        user_id: &UserId,
    ) -> StoreResult<Option<RoomMemberEventContent>> {
        self.get_profile(room_id, user_id).await.map_err(|e| e.into())
    }

    async fn get_member_event(
        &self,
        room_id: &RoomId,
        state_key: &UserId,
    ) -> StoreResult<Option<MemberEvent>> {
        self.get_member_event(room_id, state_key).await.map_err(|e| e.into())
    }

    async fn get_user_ids(&self, room_id: &RoomId) -> StoreResult<Vec<OwnedUserId>> {
        self.get_user_ids_stream(room_id).await.map_err(|e| e.into())
    }

    async fn get_invited_user_ids(&self, room_id: &RoomId) -> StoreResult<Vec<OwnedUserId>> {
        self.get_invited_user_ids(room_id).await.map_err(|e| e.into())
    }

    async fn get_joined_user_ids(&self, room_id: &RoomId) -> StoreResult<Vec<OwnedUserId>> {
        self.get_joined_user_ids(room_id).await.map_err(|e| e.into())
    }

    async fn get_room_infos(&self) -> StoreResult<Vec<RoomInfo>> {
        self.get_room_infos().await.map_err(|e| e.into())
    }

    async fn get_stripped_room_infos(&self) -> StoreResult<Vec<RoomInfo>> {
        self.get_stripped_room_infos().await.map_err(|e| e.into())
    }

    async fn get_users_with_display_name(
        &self,
        room_id: &RoomId,
        display_name: &str,
    ) -> StoreResult<BTreeSet<OwnedUserId>> {
        self.get_users_with_display_name(room_id, display_name).await.map_err(|e| e.into())
    }

    async fn get_account_data_event(
        &self,
        event_type: GlobalAccountDataEventType,
    ) -> StoreResult<Option<Raw<AnyGlobalAccountDataEvent>>> {
        self.get_account_data_event(event_type).await.map_err(|e| e.into())
    }

    async fn get_room_account_data_event(
        &self,
        room_id: &RoomId,
        event_type: RoomAccountDataEventType,
    ) -> StoreResult<Option<Raw<AnyRoomAccountDataEvent>>> {
        self.get_room_account_data_event(room_id, event_type).await.map_err(|e| e.into())
    }

    async fn get_user_room_receipt_event(
        &self,
        room_id: &RoomId,
        receipt_type: ReceiptType,
        user_id: &UserId,
    ) -> StoreResult<Option<(OwnedEventId, Receipt)>> {
        self.get_user_room_receipt_event(room_id, receipt_type, user_id).await.map_err(|e| e.into())
    }

    async fn get_event_room_receipt_events(
        &self,
        room_id: &RoomId,
        receipt_type: ReceiptType,
        event_id: &EventId,
    ) -> StoreResult<Vec<(OwnedUserId, Receipt)>> {
        self.get_event_room_receipt_events(room_id, receipt_type, event_id)
            .await
            .map_err(|e| e.into())
    }

    async fn get_custom_value(&self, key: &[u8]) -> StoreResult<Option<Vec<u8>>> {
        self.get_custom_value(key).await.map_err(|e| e.into())
    }

    async fn set_custom_value(&self, key: &[u8], value: Vec<u8>) -> StoreResult<Option<Vec<u8>>> {
        self.set_custom_value(key, value).await.map_err(|e| e.into())
    }

    async fn add_media_content(&self, request: &MediaRequest, data: Vec<u8>) -> StoreResult<()> {
        self.add_media_content(request, data).await.map_err(|e| e.into())
    }

    async fn get_media_content(&self, request: &MediaRequest) -> StoreResult<Option<Vec<u8>>> {
        self.get_media_content(request).await.map_err(|e| e.into())
    }

    async fn remove_media_content(&self, request: &MediaRequest) -> StoreResult<()> {
        self.remove_media_content(request).await.map_err(|e| e.into())
    }

    async fn remove_media_content_for_uri(&self, uri: &MxcUri) -> StoreResult<()> {
        self.remove_media_content_for_uri(uri).await.map_err(|e| e.into())
    }

    async fn remove_room(&self, room_id: &RoomId) -> StoreResult<()> {
        self.remove_room(room_id).await.map_err(|e| e.into())
    }

    async fn room_timeline(
        &self,
        room_id: &RoomId,
    ) -> StoreResult<Option<(BoxStream<StoreResult<SyncRoomEvent>>, Option<String>)>> {
        self.room_timeline(room_id).await.map_err(|e| e.into())
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
mod tests {
    #[cfg(target_arch = "wasm32")]
    wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_browser);

    use matrix_sdk_base::statestore_integration_tests;

    use super::{IndexeddbStore, Result};

    async fn get_store() -> Result<IndexeddbStore> {
        Ok(IndexeddbStore::open().await?)
    }

    statestore_integration_tests! { integration }
}

#[cfg(test)]
mod encrypted_tests {
    #[cfg(target_arch = "wasm32")]
    wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_browser);

    use matrix_sdk_base::statestore_integration_tests;
    use uuid::Uuid;

    use super::{IndexeddbStore, Result, StoreCipher};

    async fn get_store() -> Result<IndexeddbStore> {
        let db_name =
            format!("test-state-encrypted-{}", Uuid::new_v4().to_hyphenated().to_string());
        let key = StoreCipher::new()?;
        Ok(IndexeddbStore::open_helper(db_name, Some(key)).await?)
    }

    statestore_integration_tests! { integration }
}
