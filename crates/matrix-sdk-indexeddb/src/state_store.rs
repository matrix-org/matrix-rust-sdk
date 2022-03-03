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

use std::collections::BTreeSet;

use anyhow::anyhow;
use futures_util::stream;
use indexed_db_futures::prelude::*;
use matrix_sdk_base::{
    deserialized_responses::{MemberEvent, SyncRoomEvent},
    media::{MediaRequest, UniqueKey},
    store::{
        store_key::{self, EncryptedEvent, StoreKey},
        BoxStream, Result as StoreResult, StateChanges, StateStore, StoreError,
    },
    RoomInfo,
};
use matrix_sdk_common::{
    async_trait,
    ruma::{
        events::{
            presence::PresenceEvent,
            receipt::Receipt,
            room::member::{MembershipState, RoomMemberEventContent},
            AnyGlobalAccountDataEvent, AnyRoomAccountDataEvent, AnySyncMessageEvent,
            AnySyncRoomEvent, AnySyncStateEvent, EventType,
        },
        receipt::ReceiptType,
        serde::Raw,
        signatures::{redact_in_place, CanonicalJsonObject},
        EventId, MxcUri, RoomId, RoomVersionId, UserId,
    },
};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};
use wasm_bindgen::JsValue;

use crate::safe_encode::SafeEncode;

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
                store_key::Error::Random(e) => StoreError::Encryption(e.to_string()),
                store_key::Error::Serialization(e) => StoreError::Json(e),
                store_key::Error::Encryption(e) => StoreError::Encryption(e),
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
    store_key: Option<StoreKey>,
}

impl std::fmt::Debug for IndexeddbStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IndexeddbStore").field("name", &self.name).finish()
    }
}

type Result<A, E = SerializationError> = std::result::Result<A, E>;

impl IndexeddbStore {
    async fn open_helper(name: String, store_key: Option<StoreKey>) -> Result<Self> {
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

        Ok(Self { name, inner: db, store_key })
    }

    #[allow(dead_code)]
    pub async fn open() -> StoreResult<Self> {
        Ok(IndexeddbStore::open_helper("state".to_owned(), None).await?)
    }

    pub async fn open_with_passphrase(name: String, passphrase: &str) -> StoreResult<Self> {
        Ok(Self::inner_open_with_passphrase(name, passphrase).await?)
    }

    async fn inner_open_with_passphrase(name: String, passphrase: &str) -> Result<Self> {
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

        let tx: IdbTransaction =
            db.transaction_on_one_with_mode("matrix-sdk-state", IdbTransactionMode::Readwrite)?;
        let ob = tx.object_store("matrix-sdk-state")?;

        let store_key: Option<DatabaseType> = ob
            .get(&JsValue::from_str(KEYS::STORE_KEY))?
            .await?
            .map(|k| k.into_serde())
            .transpose()?;

        let store_key = if let Some(key) = store_key {
            if let DatabaseType::Encrypted(k) = key {
                StoreKey::import(passphrase, k).map_err(|_| StoreError::StoreLocked)?
            } else {
                return Err(StoreError::UnencryptedStore.into());
            }
        } else {
            let key = StoreKey::new().map_err::<SerializationError, _>(|e| e.into())?;
            let encrypted_key = DatabaseType::Encrypted(
                key.export(passphrase).map_err::<SerializationError, _>(|e| e.into())?,
            );
            ob.put_key_val(
                &JsValue::from_str(KEYS::STORE_KEY),
                &JsValue::from_serde(&encrypted_key)?,
            )?;
            key
        };

        tx.await.into_result()?;

        IndexeddbStore::open_helper(name, Some(store_key)).await
    }

    pub async fn open_with_name(name: String) -> StoreResult<Self> {
        Ok(IndexeddbStore::open_helper(name, None).await?)
    }

    fn serialize_event(
        &self,
        event: &impl Serialize,
    ) -> std::result::Result<JsValue, SerializationError> {
        Ok(match self.store_key {
            Some(ref key) => JsValue::from_serde(&key.encrypt(event)?)?,
            None => JsValue::from_serde(event)?,
        })
    }

    fn deserialize_event<T: for<'b> Deserialize<'b>>(
        &self,
        event: JsValue,
    ) -> std::result::Result<T, SerializationError> {
        match self.store_key {
            Some(ref key) => {
                let encrypted: EncryptedEvent = event.into_serde()?;
                Ok(key.decrypt(encrypted)?)
            }
            None => Ok(event.into_serde()?),
        }
    }

    pub async fn save_filter(&self, filter_name: &str, filter_id: &str) -> Result<()> {
        let tx = self
            .inner
            .transaction_on_one_with_mode(KEYS::SESSION, IdbTransactionMode::Readwrite)?;

        let obj = tx.object_store(KEYS::SESSION)?;

        obj.put_key_val(&(KEYS::FILTER, filter_name).encode(), &JsValue::from_str(filter_id))?;

        tx.await.into_result()?;

        Ok(())
    }

    pub async fn get_filter(&self, filter_name: &str) -> Result<Option<String>> {
        Ok(self
            .inner
            .transaction_on_one_with_mode(KEYS::SESSION, IdbTransactionMode::Readonly)?
            .object_store(KEYS::SESSION)?
            .get(&(KEYS::FILTER, filter_name).encode())?
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
            (!changes.stripped_members.is_empty(), KEYS::STRIPPED_MEMBERS),
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
                    let key = (room_id, display_name).encode();

                    store.put_key_val(&key, &self.serialize_event(&map)?)?;
                }
            }
        }

        if !changes.account_data.is_empty() {
            let store = tx.object_store(KEYS::ACCOUNT_DATA)?;
            for (event_type, event) in &changes.account_data {
                store.put_key_val(&event_type.encode(), &self.serialize_event(&event)?)?;
            }
        }

        if !changes.room_account_data.is_empty() {
            let store = tx.object_store(KEYS::ROOM_ACCOUNT_DATA)?;
            for (room, events) in &changes.room_account_data {
                for (event_type, event) in events {
                    let key = (room, event_type).encode();
                    store.put_key_val(&key, &self.serialize_event(&event)?)?;
                }
            }
        }

        if !changes.state.is_empty() {
            let store = tx.object_store(KEYS::ROOM_STATE)?;
            for (room, event_types) in &changes.state {
                for (event_type, events) in event_types {
                    for (state_key, event) in events {
                        let key = (room, event_type, state_key).encode();
                        store.put_key_val(&key, &self.serialize_event(&event)?)?;
                    }
                }
            }
        }

        if !changes.room_infos.is_empty() {
            let store = tx.object_store(KEYS::ROOM_INFOS)?;
            for (room_id, room_info) in &changes.room_infos {
                store.put_key_val(&room_id.encode(), &self.serialize_event(&room_info)?)?;
            }
        }

        if !changes.presence.is_empty() {
            let store = tx.object_store(KEYS::PRESENCE)?;
            for (sender, event) in &changes.presence {
                store.put_key_val(&sender.encode(), &self.serialize_event(&event)?)?;
            }
        }

        if !changes.stripped_room_infos.is_empty() {
            let store = tx.object_store(KEYS::STRIPPED_ROOM_INFOS)?;
            for (room_id, info) in &changes.stripped_room_infos {
                store.put_key_val(&room_id.encode(), &self.serialize_event(&info)?)?;
            }
        }

        if !changes.stripped_members.is_empty() {
            let store = tx.object_store(KEYS::STRIPPED_MEMBERS)?;
            for (room, events) in &changes.stripped_members {
                for event in events.values() {
                    let key = (room, &event.state_key).encode();
                    store.put_key_val(&key, &self.serialize_event(&event)?)?;
                }
            }
        }

        if !changes.stripped_state.is_empty() {
            let store = tx.object_store(KEYS::STRIPPED_ROOM_STATE)?;
            for (room, event_types) in &changes.stripped_state {
                for (event_type, events) in event_types {
                    for (state_key, event) in events {
                        let key = (room, event_type, state_key).encode();
                        store.put_key_val(&key, &self.serialize_event(&event)?)?;
                    }
                }
            }
        }

        if !changes.members.is_empty() {
            for (room, events) in &changes.members {
                let profile_changes = changes.profiles.get(room);

                let profiles_store = tx.object_store(KEYS::PROFILES)?;
                let joined = tx.object_store(KEYS::JOINED_USER_IDS)?;
                let invited = tx.object_store(KEYS::INVITED_USER_IDS)?;
                let members = tx.object_store(KEYS::MEMBERS)?;

                for event in events.values() {
                    let key = (room, &event.state_key).encode();

                    match event.content.membership {
                        MembershipState::Join => {
                            joined.put_key_val_owned(&key, &event.state_key.encode())?;
                            invited.delete(&key)?;
                        }
                        MembershipState::Invite => {
                            invited.put_key_val_owned(&key, &event.state_key.encode())?;
                            joined.delete(&key)?;
                        }
                        _ => {
                            joined.delete(&key)?;
                            invited.delete(&key)?;
                        }
                    }

                    members.put_key_val_owned(&key, &self.serialize_event(&event)?)?;

                    if let Some(profile) = profile_changes.and_then(|p| p.get(&event.state_key)) {
                        profiles_store.put_key_val_owned(&key, &self.serialize_event(&profile)?)?;
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
                            let key = (room, receipt_type, user_id).encode();

                            if let Some((old_event, _)) =
                                room_user_receipts.get(&key)?.await?.and_then(|f| {
                                    self.deserialize_event::<(Box<EventId>, Receipt)>(f).ok()
                                })
                            {
                                room_event_receipts
                                    .delete(&(room, receipt_type, &old_event, user_id).encode())?;
                            }

                            room_user_receipts
                                .put_key_val(&key, &self.serialize_event(&(event_id, receipt))?)?;

                            // Add the receipt to the room event receipts
                            room_event_receipts.put_key_val(
                                &(room, receipt_type, event_id, user_id).encode(),
                                &self.serialize_event(&receipt)?,
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
                let room_key = room_id.encode();

                let metadata: Option<TimelineMetadata> = if timeline.limited {
                    info!(
                        "Delete stored timeline for {} because the sync response was limited",
                        room_id
                    );

                    let range = room_id.encode_to_range().map_err(StoreError::Codec)?;
                    let stores =
                        &[&timeline_store, &timeline_metadata_store, &event_id_to_position_store];
                    for store in stores {
                        for key in store.get_all_keys_with_key(&range)?.await?.iter() {
                            store.delete(&key)?;
                        }
                    }

                    None
                } else {
                    let metadata: Option<TimelineMetadata> = timeline_metadata_store
                        .get(&room_key)?
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
                                let event_key = (room_id, &event_id).encode();
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

                            let range = room_id.encode_to_range().map_err(StoreError::Codec)?;
                            let stores = &[
                                &timeline_store,
                                &timeline_metadata_store,
                                &event_id_to_position_store,
                            ];
                            for store in stores {
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
                        start_position: usize::MAX / 2,
                        end_position: usize::MAX / 2,
                    }
                };

                if timeline.sync {
                    let room_version = room_infos
                        .get(&room_key)?
                        .await?
                        .map(|r| self.deserialize_event::<RoomInfo>(r))
                        .transpose()?
                        .and_then(|info| info.base_info.create.map(|event| event.room_version))
                        .unwrap_or_else(|| {
                            warn!(
                                "Unable to find the room version for {}, assume version 9",
                                room_id
                            );
                            RoomVersionId::V9
                        });
                    for event in timeline.events.iter().rev() {
                        // Redact events already in store only on sync response
                        if let Ok(AnySyncRoomEvent::Message(AnySyncMessageEvent::RoomRedaction(
                            redaction,
                        ))) = event.event.deserialize()
                        {
                            let redacts_key = (room_id, &redaction.redacts).encode();
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
                        let key = (room_id, &metadata.start_position).encode();
                        // Only add event with id to the position map
                        if let Some(event_id) = event.event_id() {
                            let event_key = (room_id, &event_id).encode();
                            event_id_to_position_store.put_key_val(&event_key, &key)?;
                        }

                        timeline_store.put_key_val_owned(key, &self.serialize_event(&event)?)?;
                    }
                } else {
                    for event in timeline.events.iter() {
                        metadata.end_position += 1;
                        let key = (room_id, &metadata.end_position).encode();
                        // Only add event with id to the position map
                        if let Some(event_id) = event.event_id() {
                            let event_key = (room_id, &event_id).encode();
                            event_id_to_position_store.put_key_val(&event_key, &key)?;
                        }

                        timeline_store.put_key_val_owned(key, &self.serialize_event(&event)?)?;
                    }
                }

                timeline_metadata_store
                    .put_key_val_owned(room_key, &JsValue::from_serde(&metadata)?)?;
            }
        }

        tx.await.into_result().map_err::<SerializationError, _>(|e| e.into())
    }

    pub async fn get_presence_event(&self, user_id: &UserId) -> Result<Option<Raw<PresenceEvent>>> {
        self.inner
            .transaction_on_one_with_mode(KEYS::PRESENCE, IdbTransactionMode::Readonly)?
            .object_store(KEYS::PRESENCE)?
            .get(&user_id.encode())?
            .await?
            .map(|f| self.deserialize_event(f))
            .transpose()
    }

    pub async fn get_state_event(
        &self,
        room_id: &RoomId,
        event_type: EventType,
        state_key: &str,
    ) -> Result<Option<Raw<AnySyncStateEvent>>> {
        self.inner
            .transaction_on_one_with_mode(KEYS::ROOM_STATE, IdbTransactionMode::Readonly)?
            .object_store(KEYS::ROOM_STATE)?
            .get(&(room_id, &event_type, state_key).encode())?
            .await?
            .map(|f| self.deserialize_event(f))
            .transpose()
    }

    pub async fn get_state_events(
        &self,
        room_id: &RoomId,
        event_type: EventType,
    ) -> Result<Vec<Raw<AnySyncStateEvent>>> {
        let range = (room_id, &event_type).encode_to_range().map_err(StoreError::Codec)?;
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
            .get(&(room_id, user_id).encode())?
            .await?
            .map(|f| self.deserialize_event(f))
            .transpose()
    }

    pub async fn get_member_event(
        &self,
        room_id: &RoomId,
        state_key: &UserId,
    ) -> Result<Option<MemberEvent>> {
        self.inner
            .transaction_on_one_with_mode(KEYS::MEMBERS, IdbTransactionMode::Readonly)?
            .object_store(KEYS::MEMBERS)?
            .get(&(room_id, state_key).encode())?
            .await?
            .map(|f| self.deserialize_event(f))
            .transpose()
    }

    pub async fn get_user_ids_stream(&self, room_id: &RoomId) -> Result<Vec<Box<UserId>>> {
        let range = room_id.encode_to_range().map_err(StoreError::Codec)?;
        let skip = room_id.as_encoded_string().len() + 1;
        Ok(self
            .inner
            .transaction_on_one_with_mode(KEYS::MEMBERS, IdbTransactionMode::Readonly)?
            .object_store(KEYS::MEMBERS)?
            .get_all_keys_with_key(&range)?
            .await?
            .iter()
            .filter_map(|key| match key.as_string() {
                Some(k) => UserId::parse(&k[skip..]).ok(),
                _ => None,
            })
            .collect::<Vec<_>>())
    }

    pub async fn get_invited_user_ids(&self, room_id: &RoomId) -> Result<Vec<Box<UserId>>> {
        let range = room_id.encode_to_range().map_err(StoreError::Codec)?;
        let entries = self
            .inner
            .transaction_on_one_with_mode(KEYS::INVITED_USER_IDS, IdbTransactionMode::Readonly)?
            .object_store(KEYS::INVITED_USER_IDS)?
            .get_all_with_key(&range)?
            .await?
            .iter()
            .filter_map(|f| self.deserialize_event::<Box<UserId>>(f).ok())
            .collect::<Vec<_>>();

        Ok(entries)
    }

    pub async fn get_joined_user_ids(&self, room_id: &RoomId) -> Result<Vec<Box<UserId>>> {
        let range = room_id.encode_to_range().map_err(StoreError::Codec)?;
        Ok(self
            .inner
            .transaction_on_one_with_mode(KEYS::JOINED_USER_IDS, IdbTransactionMode::Readonly)?
            .object_store(KEYS::JOINED_USER_IDS)?
            .get_all_with_key(&range)?
            .await?
            .iter()
            .filter_map(|f| self.deserialize_event::<Box<UserId>>(f).ok())
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
    ) -> Result<BTreeSet<Box<UserId>>> {
        self.inner
            .transaction_on_one_with_mode(KEYS::DISPLAY_NAMES, IdbTransactionMode::Readonly)?
            .object_store(KEYS::DISPLAY_NAMES)?
            .get(&(room_id, display_name).encode())?
            .await?
            .map(|f| {
                self.deserialize_event::<BTreeSet<Box<UserId>>>(f)
                    .map_err::<SerializationError, _>(|e| e)
            })
            .unwrap_or_else(|| Ok(Default::default()))
    }

    pub async fn get_account_data_event(
        &self,
        event_type: EventType,
    ) -> Result<Option<Raw<AnyGlobalAccountDataEvent>>> {
        self.inner
            .transaction_on_one_with_mode(KEYS::ACCOUNT_DATA, IdbTransactionMode::Readonly)?
            .object_store(KEYS::ACCOUNT_DATA)?
            .get(&JsValue::from_str(event_type.as_str()))?
            .await?
            .map(|f| self.deserialize_event(f).map_err::<SerializationError, _>(|e| e))
            .transpose()
    }

    pub async fn get_room_account_data_event(
        &self,
        room_id: &RoomId,
        event_type: EventType,
    ) -> Result<Option<Raw<AnyRoomAccountDataEvent>>> {
        self.inner
            .transaction_on_one_with_mode(KEYS::ROOM_ACCOUNT_DATA, IdbTransactionMode::Readonly)?
            .object_store(KEYS::ROOM_ACCOUNT_DATA)?
            .get(&(room_id.as_str(), event_type.as_str()).encode())?
            .await?
            .map(|f| self.deserialize_event(f).map_err::<SerializationError, _>(|e| e))
            .transpose()
    }

    async fn get_user_room_receipt_event(
        &self,
        room_id: &RoomId,
        receipt_type: ReceiptType,
        user_id: &UserId,
    ) -> Result<Option<(Box<EventId>, Receipt)>> {
        self.inner
            .transaction_on_one_with_mode(KEYS::ROOM_USER_RECEIPTS, IdbTransactionMode::Readonly)?
            .object_store(KEYS::ROOM_USER_RECEIPTS)?
            .get(&(room_id.as_str(), receipt_type.as_ref(), user_id.as_str()).encode())?
            .await?
            .map(|f| self.deserialize_event(f))
            .transpose()
    }

    async fn get_event_room_receipt_events(
        &self,
        room_id: &RoomId,
        receipt_type: ReceiptType,
        event_id: &EventId,
    ) -> Result<Vec<(Box<UserId>, Receipt)>> {
        let key = (room_id, &receipt_type, event_id);
        let prefix_len = key.as_encoded_string().len() + 1;
        let range = key.encode_to_range().map_err(StoreError::Codec)?;
        let tx = self.inner.transaction_on_one_with_mode(
            KEYS::ROOM_EVENT_RECEIPTS,
            IdbTransactionMode::Readonly,
        )?;
        let store = tx.object_store(KEYS::ROOM_EVENT_RECEIPTS)?;

        let mut all = Vec::new();
        for k in store.get_all_keys_with_key(&range)?.await?.iter() {
            // FIXME: we should probably parallelize this...
            let res = store
                .get(&k)?
                .await?
                .ok_or_else(|| StoreError::Codec(format!("no data at {:?}", k)))?;
            let u = if let Some(k_str) = k.as_string() {
                UserId::parse(&k_str[prefix_len..])
                    .map_err(|e| StoreError::Codec(format!("{:?}", e)))?
            } else {
                return Err(StoreError::Codec(format!("{:?}", k)).into());
            };
            let r = self
                .deserialize_event::<Receipt>(res)
                .map_err(|e| StoreError::Codec(e.to_string()))?;
            all.push((u, r));
        }
        Ok(all)
    }

    async fn add_media_content(&self, request: &MediaRequest, data: Vec<u8>) -> Result<()> {
        let key = (&request.media_type.unique_key(), &request.format.unique_key()).encode();
        let tx =
            self.inner.transaction_on_one_with_mode(KEYS::MEDIA, IdbTransactionMode::Readwrite)?;

        tx.object_store(KEYS::MEDIA)?.put_key_val(&key, &self.serialize_event(&data)?)?;

        tx.await.into_result().map_err(|e| e.into())
    }

    async fn get_media_content(&self, request: &MediaRequest) -> Result<Option<Vec<u8>>> {
        let key = (&request.media_type.unique_key(), &request.format.unique_key()).encode();
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
        let key = (&request.media_type.unique_key(), &request.format.unique_key()).encode();
        let tx =
            self.inner.transaction_on_one_with_mode(KEYS::MEDIA, IdbTransactionMode::Readwrite)?;

        tx.object_store(KEYS::MEDIA)?.delete(&key)?;

        tx.await.into_result().map_err(|e| e.into())
    }

    async fn remove_media_content_for_uri(&self, uri: &MxcUri) -> Result<()> {
        let range = uri.encode_to_range().map_err(StoreError::Codec)?;
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

        let room_key = room_id.encode();
        for store_name in direct_stores {
            tx.object_store(store_name)?.delete(&room_key)?;
        }

        let range = room_id.encode_to_range().map_err(StoreError::Codec)?;
        for store_name in prefixed_stores {
            let store = tx.object_store(store_name)?;
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
        let key = room_id.encode();
        let tx = self.inner.transaction_on_multi_with_mode(
            &[KEYS::ROOM_TIMELINE, KEYS::ROOM_TIMELINE_METADATA],
            IdbTransactionMode::Readonly,
        )?;
        let timeline = tx.object_store(KEYS::ROOM_TIMELINE)?;
        let metadata = tx.object_store(KEYS::ROOM_TIMELINE_METADATA)?;

        let metadata: Option<TimelineMetadata> =
            metadata.get(&key)?.await?.map(|v| v.into_serde()).transpose()?;
        if metadata.is_none() {
            info!("No timeline for {} was previously stored", room_id);
            return Ok(None);
        }
        let end_token = metadata.and_then(|m| m.end);
        #[allow(clippy::needless_collect)]
        let timeline: Vec<StoreResult<SyncRoomEvent>> = timeline
            .get_all_with_key(&key)?
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
        event_type: EventType,
        state_key: &str,
    ) -> StoreResult<Option<Raw<AnySyncStateEvent>>> {
        self.get_state_event(room_id, event_type, state_key).await.map_err(|e| e.into())
    }

    async fn get_state_events(
        &self,
        room_id: &RoomId,
        event_type: EventType,
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

    async fn get_user_ids(&self, room_id: &RoomId) -> StoreResult<Vec<Box<UserId>>> {
        self.get_user_ids_stream(room_id).await.map_err(|e| e.into())
    }

    async fn get_invited_user_ids(&self, room_id: &RoomId) -> StoreResult<Vec<Box<UserId>>> {
        self.get_invited_user_ids(room_id).await.map_err(|e| e.into())
    }

    async fn get_joined_user_ids(&self, room_id: &RoomId) -> StoreResult<Vec<Box<UserId>>> {
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
    ) -> StoreResult<BTreeSet<Box<UserId>>> {
        self.get_users_with_display_name(room_id, display_name).await.map_err(|e| e.into())
    }

    async fn get_account_data_event(
        &self,
        event_type: EventType,
    ) -> StoreResult<Option<Raw<AnyGlobalAccountDataEvent>>> {
        self.get_account_data_event(event_type).await.map_err(|e| e.into())
    }

    async fn get_room_account_data_event(
        &self,
        room_id: &RoomId,
        event_type: EventType,
    ) -> StoreResult<Option<Raw<AnyRoomAccountDataEvent>>> {
        self.get_room_account_data_event(room_id, event_type).await.map_err(|e| e.into())
    }

    async fn get_user_room_receipt_event(
        &self,
        room_id: &RoomId,
        receipt_type: ReceiptType,
        user_id: &UserId,
    ) -> StoreResult<Option<(Box<EventId>, Receipt)>> {
        self.get_user_room_receipt_event(room_id, receipt_type, user_id).await.map_err(|e| e.into())
    }

    async fn get_event_room_receipt_events(
        &self,
        room_id: &RoomId,
        receipt_type: ReceiptType,
        event_id: &EventId,
    ) -> StoreResult<Vec<(Box<UserId>, Receipt)>> {
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
mod test {
    #[cfg(target_arch = "wasm32")]
    wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_browser);

    use matrix_sdk_base::statestore_integration_tests;

    use super::{IndexeddbStore, Result};

    async fn get_store() -> Result<IndexeddbStore> {
        Ok(IndexeddbStore::open().await?)
    }

    statestore_integration_tests! { integration }
}
