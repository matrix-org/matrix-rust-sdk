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
use indexed_db_futures::prelude::*;
use wasm_bindgen::JsValue;
use indexed_db_futures::web_sys::IdbKeyRange;
use core::{
     convert::{TryFrom, TryInto},
};
use std::collections::BTreeSet;

use futures_core::stream::Stream;
use futures_util::stream::{self, TryStreamExt};
use matrix_sdk_common::async_trait;
use ruma::{
    events::{
        presence::PresenceEvent,
        receipt::Receipt,
        room::member::{MembershipState, RoomMemberEventContent},
        AnyGlobalAccountDataEvent, AnyRoomAccountDataEvent, AnySyncStateEvent, EventType,
    },
    receipt::ReceiptType,
    serde::Raw,
    EventId, MxcUri, RoomId, UserId,
};
use serde::{Deserialize, Serialize};
use tracing::info;

use self::store_key::{EncryptedEvent, StoreKey};
use super::{Result, RoomInfo, StateChanges, StateStore, StoreError};
use crate::{
    deserialized_responses::MemberEvent,
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

mod KEYS {

    // STORES

    pub const SESSION: &'static str = "session";
    pub const ACCOUNT_DATA: &'static str = "account_data";

    pub const MEMBERS: &'static str = "members";
    pub const PROFILES: &'static str = "profiles";
    pub const DISPLAY_NAMES: &'static str = "display_names";
    pub const JOINED_USER_IDS: &'static str = "joined_user_ids";
    pub const INVITED_USER_IDS: &'static str = "invited_user_ids";

    pub const ROOM_STATE: &'static str = "room_state";
    pub const ROOM_INFOS: &'static str = "room_infos";
    pub const PRESENCE: &'static str = "presence";
    pub const ROOM_ACCOUNT_DATA: &'static str = "room_account_data";

    pub const STRIPPED_ROOM_INFO: &'static str = "stripped_room_info";
    pub const STRIPPED_MEMBERS: &'static str = "stripped_members";
    pub const STRIPPED_ROOM_STATE: &'static str = "stripped_room_state";

    pub const ROOM_USER_RECEIPTS: &'static str = "room_user_receipts";
    pub const ROOM_EVENT_RECEIPTS: &'static str = "room_event_receipts";

    pub const MEDIA: &'static str = "media";

    pub const CUSTOM: &'static str = "custom";

    // static keys

    pub const STORE_KEY: &'static str = "store_key";
    pub const FILTER: &'static str = "filter";
    pub const SYNC_TOKEN: &'static str = "sync_token";
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

fn make_range(key: String) -> Result<IdbKeyRange, StoreError> {
    IdbKeyRange::bound(
        &JsValue::from_str(&format!("{}:", key)),
        &JsValue::from_str(&format!("{};", key)),
    ).map_err(|e| StoreError::Codec(e.as_string().unwrap_or(format!("Creating key range for {:} failed", key))))
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

impl IndexeddbStore {
    async fn open_helper(name: String, store_key: Option<StoreKey>) -> Result<Self> {
        // Open my_db v1
        let mut db_req: OpenDbRequest = IdbDatabase::open_f64(&name, 1.0)?;
        db_req.set_on_upgrade_needed(Some(|evt: &
            IdbVersionChangeEvent| -> Result<(), JsValue> {

            if evt.old_version() < 1.0 {
                // migrating to version 1
                let db = evt.db();

                db.create_object_store(KEYS::SESSION)?;
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

                db.create_object_store(KEYS::STRIPPED_ROOM_INFO)?;
                db.create_object_store(KEYS::STRIPPED_MEMBERS)?;
                db.create_object_store(KEYS::STRIPPED_ROOM_STATE)?;

                db.create_object_store(KEYS::ROOM_USER_RECEIPTS)?;
                db.create_object_store(KEYS::ROOM_EVENT_RECEIPTS)?;

                db.create_object_store(KEYS::MEDIA)?;

                db.create_object_store(KEYS::CUSTOM)?;

            }
            Ok(())
        }));

        let db: IdbDatabase = db_req.into_future().await?;

        Ok(Self {
            name,
            inner: db,
            store_key,
        })
    }

    pub async fn open() -> Result<Self> {
        IndexeddbStore::open_helper("state".to_owned(), None).await
    }

    pub async fn open_with_passphrase(name: String, passphrase: &str) -> Result<Self> {
        let path = format!("{:0}::matrix-sdk-state", name);

        let mut db_req: OpenDbRequest = IdbDatabase::open_u32(&path, 1)?;
        db_req.set_on_upgrade_needed(Some(|evt: &IdbVersionChangeEvent| -> Result<(), JsValue> {

            if evt.old_version() < 1.0 {
                // migrating to version 1
                let db = evt.db();

                db.create_object_store("matrix-sdk-state")?;
            }
            Ok(())
        }));

        let db: IdbDatabase = db_req.into_future().await?;

        let tx: IdbTransaction = db.transaction_on_one_with_mode(&path, IdbTransactionMode::Readwrite)?;
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
                return Err(StoreError::UnencryptedStore);
            }
        } else {
            let key = StoreKey::new().map_err::<StoreError, _>(|e| e.into())?;
            let encrypted_key = DatabaseType::Encrypted(
                key.export(passphrase).map_err::<StoreError, _>(|e| e.into())?,
            );
            ob.put_key_val(&JsValue::from_str(KEYS::STORE_KEY), &JsValue::from_serde(&encrypted_key)?)?;
            key
        };


        tx.await.into_result()?;

        IndexeddbStore::open_helper(name, Some(store_key)).await
    }

    pub async fn open_with_name(name: String) -> Result<Self> {
        IndexeddbStore::open_helper(name, None).await
    }

    fn serialize_event(&self, event: &impl Serialize) -> Result<JsValue, SerializationError> {
        Ok(match self.store_key {
            Some(ref key) => JsValue::from_serde(&key.encrypt(event)?)?,
            None => JsValue::from_serde(event)?
        })
    }

    fn deserialize_event<T: for<'b> Deserialize<'b>>(
        &self,
        event: JsValue,
    ) -> Result<T, SerializationError> {
        match self.store_key {
            Some(ref key) => {
                let encrypted: EncryptedEvent = event.into_serde()?;
                Ok(key.decrypt(encrypted)?)
            },
            None => Ok(event.into_serde()?)
        }
    }

    pub async fn save_filter(&self, filter_name: &str, filter_id: &str) -> Result<()> {
        let tx = self.inner
            .transaction_on_one_with_mode(KEYS::SESSION, IdbTransactionMode::Readwrite)?;

        let obj = tx.object_store(KEYS::SESSION)?;

        obj.put_key_val(
            &JsValue::from_str(&format!("{}:{}", KEYS::FILTER, filter_name)),
            &JsValue::from_str(filter_id)
        )?;

        tx.await.into_result()?;

        Ok(())
    }

    pub async fn get_filter(&self, filter_name: &str) -> Result<Option<String>> {

        Ok(self.inner
            .transaction_on_one_with_mode(KEYS::SESSION, IdbTransactionMode::Readonly)?
            .object_store(KEYS::SESSION)?
            .get(&JsValue::from_str(&format!("{}:{}", KEYS::FILTER, filter_name)))?
            .await?
            .map(|f| f.as_string())
            .flatten())
    }

    pub async fn get_sync_token(&self) -> Result<Option<String>> {

        Ok(self.inner
            .transaction_on_one_with_mode(KEYS::SESSION, IdbTransactionMode::Readonly)?
            .object_store(KEYS::SESSION)?
            .get(&JsValue::from_str(KEYS::SYNC_TOKEN))?
            .await?
            .map(|f| self.deserialize_event(f))
            .transpose()?)
    }

    pub async fn save_changes(&self, changes: &StateChanges) -> Result<()> {
        let mut stores : Vec<&'static str> = [
            (changes.sync_token.is_some(), KEYS::SYNC_TOKEN),
            (changes.session.is_some(), KEYS::SESSION ),

            (!changes.ambiguity_maps.is_empty() , KEYS::DISPLAY_NAMES ),
            (!changes.account_data.is_empty() , KEYS::ACCOUNT_DATA ),
            (!changes.presence.is_empty() , KEYS::PRESENCE ),
            (!changes.profiles.is_empty() , KEYS::PROFILES ),
            (!changes.state.is_empty() , KEYS::ROOM_STATE ),
            (!changes.room_account_data.is_empty() , KEYS::ROOM_ACCOUNT_DATA ),
            (!changes.room_infos.is_empty() , KEYS::ROOM_INFOS ),
            (!changes.receipts.is_empty() , KEYS::ROOM_EVENT_RECEIPTS ),
            (!changes.stripped_state.is_empty() , KEYS::STRIPPED_ROOM_STATE ),
            (!changes.stripped_members.is_empty() , KEYS::STRIPPED_MEMBERS ),
            (!changes.invited_room_info.is_empty() , KEYS::STRIPPED_ROOM_INFO ),
        ].iter()
        .filter_map(|(id, key)| if *id { Some(*key) } else { None })
        .collect();

        if !changes.members.is_empty() {
            stores.extend([KEYS::MEMBERS, KEYS::INVITED_USER_IDS, KEYS::JOINED_USER_IDS])
        }


        if !changes.receipts.is_empty() {
            stores.extend([KEYS::ROOM_EVENT_RECEIPTS, KEYS::ROOM_USER_RECEIPTS])
        }

        if stores.len() == 0 {
            // nothing to do, quit early
            return Ok(())
        }

        let tx = self.inner.transaction_on_multi_with_mode(&stores, IdbTransactionMode::Readwrite)?;

        if let Some(s) = &changes.sync_token {
            tx
                .object_store(KEYS::SYNC_TOKEN)?
                .put_key_val(&JsValue::from_str(KEYS::SYNC_TOKEN), &self.serialize_event(s)?)?;
        }



        if !changes.ambiguity_maps.is_empty() {
            let store = tx.object_store(KEYS::DISPLAY_NAMES)?;
            for (room_id, ambiguity_maps) in &changes.ambiguity_maps {
                for (display_name, map) in ambiguity_maps {
                    let key = format!("{}:{}", room_id.as_str(), display_name);

                    store.put_key_val(
                        &JsValue::from_str(&key),
                        &self.serialize_event(&map)?
                    )?;
                }
            }
        }


        if !changes.account_data.is_empty() {
            let store = tx.object_store(KEYS::ACCOUNT_DATA)?;
            for (event_type, event) in &changes.account_data {
                store.put_key_val(
                    &JsValue::from_str(event_type.as_str()),
                    &self.serialize_event(&event)?
                )?;
            }
        }

        if !changes.room_account_data.is_empty() {
            let store = tx.object_store(KEYS::ROOM_ACCOUNT_DATA)?;
            for (room, events) in &changes.room_account_data {
                for (event_type, event) in events {
                    let key = format!("{}:{}", room.as_str(), event_type.as_str());
                    store.put_key_val(
                        &JsValue::from_str(&key),
                        &self.serialize_event(&event)?
                    )?;
                }
            }
        }

        if !changes.state.is_empty() {
            let store = tx.object_store(KEYS::ROOM_STATE)?;
            for (room, event_types) in &changes.state {
                for (event_type, events) in event_types {
                    for (state_key, event) in events {
                        let key = format!("{}:{}:{}", room.as_str(), event_type.as_str(), state_key.as_str());
                        store.put_key_val(
                            &JsValue::from_str(&key),
                            &self.serialize_event(&event)?
                        )?;
                    }
                }
            }
        }

        if !changes.room_infos.is_empty() {
            let store = tx.object_store(KEYS::ROOM_INFOS)?;
            for (room_id, room_info) in &changes.room_infos {
                store.put_key_val(
                    &JsValue::from_str(room_id.as_str()),
                    &self.serialize_event(&room_info)?
                )?;
            }
        }

        if !changes.presence.is_empty() {
            let store = tx.object_store(KEYS::PRESENCE)?;
            for (sender, event) in &changes.presence {
                store.put_key_val(
                    &JsValue::from_str(sender.as_str()),
                    &self.serialize_event(&event)?
                )?;
            }
        }

        if !changes.invited_room_info.is_empty() {
            let store = tx.object_store(KEYS::STRIPPED_ROOM_INFO)?;
            for (room_id, info) in &changes.invited_room_info {
                store.put_key_val(
                    &JsValue::from_str(room_id.as_str()),
                    &self.serialize_event(&info)?
                )?;
            }
        }

        if !changes.stripped_members.is_empty() {
            let store = tx.object_store(KEYS::STRIPPED_MEMBERS)?;
            for (room, events) in &changes.stripped_members {
                for event in events.values() {
                    let key = format!("{}:{}", room.as_str(), event.state_key.as_str());
                    store.put_key_val(
                        &JsValue::from_str(&key),
                        &self.serialize_event(&event)?
                    )?;
                }
            }
        }

        if !changes.stripped_state.is_empty() {
            let store = tx.object_store(KEYS::STRIPPED_ROOM_STATE)?;
            for (room, event_types) in &changes.stripped_state {
                for (event_type, events) in event_types {
                    for (state_key, event) in events {
                        let key = format!("{}:{}:{}", room.as_str(), event_type.as_str(), state_key.as_str());
                        store.put_key_val(
                            &JsValue::from_str(&key),
                            &self.serialize_event(&event)?
                        )?;
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
                    let key = JsValue::from_str(&format!("{}:{}", room.as_str(), event.state_key.as_str()));

                    match event.content.membership {
                        MembershipState::Join => {
                            joined.put_key_val_owned(&key, &JsValue::from_str(event.state_key.as_str()))?;
                            invited.delete(&key)?;
                        }
                        MembershipState::Invite => {
                            invited.put_key_val_owned(&key, &JsValue::from_str(event.state_key.as_str()))?;
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

                            let key = JsValue::from_str(&format!("{}:{}:{}",
                                room.as_str(), receipt_type.as_str(), user_id.as_str()));

                            if let Some((old_event, _)) = room_user_receipts
                                .get(&key)?
                                .await?
                                .map(|f| self.deserialize_event::<(EventId, Receipt)>(f).ok())
                                .flatten()
                            {
                                room_event_receipts.delete(&JsValue::from_str(&format!("{}:{}:{}:{}",
                                    room.as_str(),
                                    receipt_type.as_ref(),
                                    old_event.as_str(),
                                    user_id.as_str(),
                                )))?;
                            }

                            room_user_receipts.put_key_val(&key, &self.serialize_event(&(event_id, receipt))?)?;

                            // Add the receipt to the room event receipts
                            room_event_receipts.put_key_val(
                                &JsValue::from_str(
                                    &format!("{}:{}:{}:{}",
                                        room.as_str(),
                                        receipt_type.as_ref(),
                                        event_id.as_str(),
                                        user_id.as_str(),
                                    )
                                ),
                                &self.serialize_event(&receipt)?
                            )?;
                        }
                    }
                }
            }
        }

        tx.await.into_result().map_err::<StoreError, _>(|e| e.into())
    }


    pub async fn get_presence_event(&self, user_id: &UserId) -> Result<Option<Raw<PresenceEvent>>> {
        Ok(self.inner
            .transaction_on_one_with_mode(KEYS::PRESENCE, IdbTransactionMode::Readonly)?
            .object_store(KEYS::PRESENCE)?
            .get(&JsValue::from_str(user_id.as_str()))?
            .await?
            .map(|f| self.deserialize_event(f))
            .transpose()?)
    }

    pub async fn get_state_event(
        &self,
        room_id: &RoomId,
        event_type: EventType,
        state_key: &str,
    ) -> Result<Option<Raw<AnySyncStateEvent>>> {
        Ok(self.inner
            .transaction_on_one_with_mode(KEYS::ROOM_STATE, IdbTransactionMode::Readonly)?
            .object_store(KEYS::ROOM_STATE)?
            .get(&JsValue::from_str(&format!("{}:{}:{}", room_id.as_str(), event_type.as_str(), state_key)))?
            .await?
            .map(|f| self.deserialize_event(f))
            .transpose()?)
    }

    pub async fn get_state_events(
        &self,
        room_id: &RoomId,
        event_type: EventType,
    ) -> Result<Vec<Raw<AnySyncStateEvent>>> {
        let range = make_range(format!("{}:{}", room_id.as_str(), event_type.as_str()))?;
        Ok(self.inner
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
        Ok(self.inner
            .transaction_on_one_with_mode(KEYS::PROFILES, IdbTransactionMode::Readonly)?
            .object_store(KEYS::PROFILES)?
            .get(&JsValue::from_str(&format!("{}:{}", room_id.as_str(), user_id.as_str())))?
            .await?
            .map(|f| self.deserialize_event(f))
            .transpose()?)
    }

    pub async fn get_member_event(
        &self,
        room_id: &RoomId,
        state_key: &UserId,
    ) -> Result<Option<MemberEvent>> {
        Ok(self.inner
            .transaction_on_one_with_mode(KEYS::MEMBERS, IdbTransactionMode::Readonly)?
            .object_store(KEYS::MEMBERS)?
            .get(&JsValue::from_str(&format!("{}:{}", room_id.as_str(), state_key.as_str())))?
            .await?
            .map(|f| self.deserialize_event(f))
            .transpose()?)
    }

    pub async fn get_user_ids_stream(
        &self,
        room_id: &RoomId,
    ) -> Result<impl Stream<Item = Result<UserId>>> {
        let range = make_range(room_id.as_str().to_owned())?;
        let skip = room_id.as_str().len();
        let entries = self.inner
            .transaction_on_one_with_mode(KEYS::MEMBERS, IdbTransactionMode::Readonly)?
            .object_store(KEYS::MEMBERS)?
            .get_all_keys_with_key(&range)?
            .await?
            .iter()
            .map(|key| {
                if let Some(k) = key.as_string() {
                    UserId::try_from(k[skip..].to_owned()).map_err(|e| StoreError::Codec(e.to_string()))
                } else {
                    Err(StoreError::Codec(format!("{:?}", key)))
                }
            })
            .collect::<Vec<_>>();

        Ok(stream::iter(entries))
    }

    pub async fn get_invited_user_ids(
        &self,
        room_id: &RoomId,
    ) -> Result<impl Stream<Item = Result<UserId>>> {
        let range = make_range(room_id.as_str().to_owned())?;
        let entries = self.inner
            .transaction_on_one_with_mode(KEYS::INVITED_USER_IDS, IdbTransactionMode::Readonly)?
            .object_store(KEYS::INVITED_USER_IDS)?
            .get_all_with_key(&range)?
            .await?
            .iter()
            .map(|f| self.deserialize_event(f).map_err::<StoreError, _>(|e| e.into()))
            .collect::<Vec<_>>();

        Ok(stream::iter(entries))
    }

    pub async fn get_joined_user_ids(
        &self,
        room_id: &RoomId,
    ) -> Result<impl Stream<Item = Result<UserId>>> {
        let range = make_range(room_id.as_str().to_owned())?;
        let entries = self.inner
            .transaction_on_one_with_mode(KEYS::JOINED_USER_IDS, IdbTransactionMode::Readonly)?
            .object_store(KEYS::JOINED_USER_IDS)?
            .get_all_with_key(&range)?
            .await?
            .iter()
            .map(|f| self.deserialize_event(f).map_err::<StoreError, _>(|e| e.into()))
            .collect::<Vec<_>>();

        Ok(stream::iter(entries))
    }

    pub async fn get_room_infos(&self) -> Result<impl Stream<Item = Result<RoomInfo>>> {
        let entries: Vec<_> = self.inner
            .transaction_on_one_with_mode(KEYS::ROOM_INFOS, IdbTransactionMode::Readonly)?
            .object_store(KEYS::ROOM_INFOS)?
            .get_all()?
            .await?
            .iter()
            .map(|f| self.deserialize_event(f).map_err::<StoreError, _>(|e| e.into()))
            .collect();

        Ok(stream::iter(entries))
    }

    pub async fn get_stripped_room_infos(&self) -> Result<impl Stream<Item = Result<RoomInfo>>> {
        let entries = self.inner
            .transaction_on_one_with_mode(KEYS::STRIPPED_ROOM_INFO, IdbTransactionMode::Readonly)?
            .object_store(KEYS::STRIPPED_ROOM_INFO)?
            .get_all()?
            .await?
            .iter()
            .map(|f| self.deserialize_event(f).map_err::<StoreError, _>(|e| e.into()))
            .collect::<Vec<_>>();

        Ok(stream::iter(entries))
    }

    pub async fn get_users_with_display_name(
        &self,
        room_id: &RoomId,
        display_name: &str,
    ) -> Result<BTreeSet<UserId>> {

        let range = make_range(format!("{}:{}", room_id.as_str(), display_name))?;
        Ok(self.inner
            .transaction_on_one_with_mode(KEYS::JOINED_USER_IDS, IdbTransactionMode::Readonly)?
            .object_store(KEYS::JOINED_USER_IDS)?
            .get_all_with_key(&range)?
            .await?
            .iter()
            .filter_map(|f| self.deserialize_event::<UserId>(f).ok())
            .collect::<BTreeSet<_>>())
    }

    pub async fn get_account_data_event(
        &self,
        event_type: EventType,
    ) -> Result<Option<Raw<AnyGlobalAccountDataEvent>>> {
        Ok(self.inner
            .transaction_on_one_with_mode(KEYS::ACCOUNT_DATA, IdbTransactionMode::Readonly)?
            .object_store(KEYS::ACCOUNT_DATA)?
            .get(&JsValue::from_str(event_type.as_str()))?
            .await?
            .map(|f| self.deserialize_event(f).map_err::<StoreError, _>(|e| e.into()))
            .transpose()?)
    }

    pub async fn get_room_account_data_event(
        &self,
        room_id: &RoomId,
        event_type: EventType,
    ) -> Result<Option<Raw<AnyRoomAccountDataEvent>>> {
        Ok(self.inner
            .transaction_on_one_with_mode(KEYS::ROOM_ACCOUNT_DATA, IdbTransactionMode::Readonly)?
            .object_store(KEYS::ROOM_ACCOUNT_DATA)?
            .get(&JsValue::from_str(&format!("{}:{}", room_id.as_str(), event_type)))?
            .await?
            .map(|f| self.deserialize_event(f).map_err::<StoreError, _>(|e| e.into()))
            .transpose()?)
    }

    async fn get_user_room_receipt_event(
        &self,
        room_id: &RoomId,
        receipt_type: ReceiptType,
        user_id: &UserId,
    ) -> Result<Option<(EventId, Receipt)>> {
        Ok(self.inner
            .transaction_on_one_with_mode(KEYS::ROOM_USER_RECEIPTS, IdbTransactionMode::Readonly)?
            .object_store(KEYS::ROOM_USER_RECEIPTS)?
            .get(&JsValue::from_str(&format!("{}:{}:{}", room_id.as_str(), receipt_type.as_ref(), user_id.as_str())))?
            .await?
            .map(|f| self.deserialize_event(f))
            .transpose()?)
    }

    async fn get_event_room_receipt_events(
        &self,
        room_id: &RoomId,
        receipt_type: ReceiptType,
        event_id: &EventId,
    ) -> Result<Vec<(UserId, Receipt)>> {
        unimplemented!()
        // let db = self.clone();
        // let key = (room_id.as_str(), receipt_type.as_ref(), event_id.as_str()).encode();
        // spawn_blocking(move || {
        //     db.room_event_receipts
        //         .scan_prefix(key)
        //         .map(|u| {
        //             u.map_err(StoreError::Indexeddb).and_then(|(key, value)| {
        //                 db.deserialize_event(&value)
        //                     // TODO remove this unwrapping
        //                     .map(|receipt| {
        //                         (decode_key_value(&key, 3).unwrap().try_into().unwrap(), receipt)
        //                     })
        //                     .map_err(Into::into)
        //             })
        //         })
        //         .collect()
        // })
        // .await?
    }

    async fn add_media_content(&self, request: &MediaRequest, data: Vec<u8>) -> Result<()> {
        unimplemented!()
        // self.media.insert(
        //     (request.media_type.unique_key().as_str(), request.format.unique_key().as_str())
        //         .encode(),
        //     data,
        // )?;

        // self.inner.flush_async().await?;

        // Ok(())
    }

    async fn get_media_content(&self, request: &MediaRequest) -> Result<Option<Vec<u8>>> {
        unimplemented!()
        // let db = self.clone();
        // let key = (request.media_type.unique_key().as_str(), request.format.unique_key().as_str())
        //     .encode();

        // spawn_blocking(move || Ok(db.media.get(key)?.map(|m| m.to_vec()))).await?
    }

    async fn get_custom_value(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        // let custom = self.custom.clone();
        // let key = key.to_owned();
        // spawn_blocking(move || Ok(custom.get(key)?.map(|v| v.to_vec()))).await?
        unimplemented!()
    }

    async fn set_custom_value(&self, key: &[u8], value: Vec<u8>) -> Result<Option<Vec<u8>>> {
        // let ret = self.custom.insert(key, value)?.map(|v| v.to_vec());
        // self.inner.flush_async().await?;

        // Ok(ret)
        unimplemented!()
    }

    async fn remove_media_content(&self, request: &MediaRequest) -> Result<()> {
        // self.media.remove(
        //     (request.media_type.unique_key().as_str(), request.format.unique_key().as_str())
        //         .encode(),
        // )?;

        // Ok(())
        unimplemented!()
    }

    async fn remove_media_content_for_uri(&self, uri: &MxcUri) -> Result<()> {
        unimplemented!()
    }
}

#[async_trait(?Send)]
impl StateStore for IndexeddbStore {
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

    async fn get_user_ids(&self, room_id: &RoomId) -> Result<Vec<UserId>> {
        self.get_user_ids_stream(room_id).await?.try_collect().await
    }

    async fn get_invited_user_ids(&self, room_id: &RoomId) -> Result<Vec<UserId>> {
        self.get_invited_user_ids(room_id).await?.try_collect().await
    }

    async fn get_joined_user_ids(&self, room_id: &RoomId) -> Result<Vec<UserId>> {
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
    ) -> Result<BTreeSet<UserId>> {
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
    ) -> Result<Option<(EventId, Receipt)>> {
        self.get_user_room_receipt_event(room_id, receipt_type, user_id).await
    }

    async fn get_event_room_receipt_events(
        &self,
        room_id: &RoomId,
        receipt_type: ReceiptType,
        event_id: &EventId,
    ) -> Result<Vec<(UserId, Receipt)>> {
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
}

#[cfg(test)]
mod test {
    use std::convert::TryFrom;

    use matrix_sdk_test::async_test;
    use ruma::{
        api::client::r0::media::get_content_thumbnail::Method,
        event_id,
        events::{
            room::{
                member::{MembershipState, RoomMemberEventContent},
                power_levels::RoomPowerLevelsEventContent,
            },
            AnySyncStateEvent, EventType, Unsigned,
        },
        mxc_uri,
        receipt::ReceiptType,
        room_id,
        serde::Raw,
        uint, user_id, EventId, MilliSecondsSinceUnixEpoch, UserId,
    };
    use serde_json::json;

    use super::{Result, IndexeddbStore, StateChanges};
    use crate::{
        deserialized_responses::MemberEvent,
        media::{MediaFormat, MediaRequest, MediaThumbnailSize, MediaType},
        StateStore,
    };

    fn user_id() -> UserId {
        user_id!("@example:localhost")
    }

    fn power_level_event() -> Raw<AnySyncStateEvent> {
        let content = RoomPowerLevelsEventContent::default();

        let event = json!({
            "event_id": EventId::try_from("$h29iv0s8:example.com").unwrap(),
            "content": content,
            "sender": user_id(),
            "type": "m.room.power_levels",
            "origin_server_ts": 0u64,
            "state_key": "",
            "unsigned": Unsigned::default(),
        });

        serde_json::from_value(event).unwrap()
    }

    fn membership_event() -> MemberEvent {
        MemberEvent {
            event_id: EventId::try_from("$h29iv0s8:example.com").unwrap(),
            content: RoomMemberEventContent::new(MembershipState::Join),
            sender: user_id(),
            origin_server_ts: MilliSecondsSinceUnixEpoch::now(),
            state_key: user_id(),
            prev_content: None,
            unsigned: Unsigned::default(),
        }
    }

    #[async_test]
    async fn test_member_saving() {
        let store = IndexeddbStore::open().unwrap();
        let room_id = room_id!("!test:localhost");
        let user_id = user_id();

        assert!(store.get_member_event(&room_id, &user_id).await.unwrap().is_none());
        let mut changes = StateChanges::default();
        changes
            .members
            .entry(room_id.clone())
            .or_default()
            .insert(user_id.clone(), membership_event());

        store.save_changes(&changes).await.unwrap();
        assert!(store.get_member_event(&room_id, &user_id).await.unwrap().is_some());

        let members = store.get_user_ids(&room_id).await.unwrap();
        assert!(!members.is_empty())
    }

    #[async_test]
    async fn test_power_level_saving() {
        let store = IndexeddbStore::open().unwrap();
        let room_id = room_id!("!test:localhost");

        let raw_event = power_level_event();
        let event = raw_event.deserialize().unwrap();

        assert!(store
            .get_state_event(&room_id, EventType::RoomPowerLevels, "")
            .await
            .unwrap()
            .is_none());
        let mut changes = StateChanges::default();
        changes.add_state_event(&room_id, event, raw_event);

        store.save_changes(&changes).await.unwrap();
        assert!(store
            .get_state_event(&room_id, EventType::RoomPowerLevels, "")
            .await
            .unwrap()
            .is_some());
    }

    #[async_test]
    async fn test_receipts_saving() {
        let store = IndexeddbStore::open().unwrap();

        let room_id = room_id!("!test:localhost");

        let first_event_id = event_id!("$1435641916114394fHBLK:matrix.org");
        let second_event_id = event_id!("$fHBLK1435641916114394:matrix.org");

        let first_receipt_event = serde_json::from_value(json!({
            first_event_id.clone(): {
                "m.read": {
                    user_id(): {
                        "ts": 1436451550453u64
                    }
                }
            }
        }))
        .unwrap();

        let second_receipt_event = serde_json::from_value(json!({
            second_event_id.clone(): {
                "m.read": {
                    user_id(): {
                        "ts": 1436451551453u64
                    }
                }
            }
        }))
        .unwrap();

        assert!(store
            .get_user_room_receipt_event(&room_id, ReceiptType::Read, &user_id())
            .await
            .unwrap()
            .is_none());
        assert!(store
            .get_event_room_receipt_events(&room_id, ReceiptType::Read, &first_event_id)
            .await
            .unwrap()
            .is_empty());
        assert!(store
            .get_event_room_receipt_events(&room_id, ReceiptType::Read, &second_event_id)
            .await
            .unwrap()
            .is_empty());

        let mut changes = StateChanges::default();
        changes.add_receipts(&room_id, first_receipt_event);

        store.save_changes(&changes).await.unwrap();
        assert!(store
            .get_user_room_receipt_event(&room_id, ReceiptType::Read, &user_id())
            .await
            .unwrap()
            .is_some(),);
        assert_eq!(
            store
                .get_event_room_receipt_events(&room_id, ReceiptType::Read, &first_event_id)
                .await
                .unwrap()
                .len(),
            1
        );
        assert!(store
            .get_event_room_receipt_events(&room_id, ReceiptType::Read, &second_event_id)
            .await
            .unwrap()
            .is_empty());

        let mut changes = StateChanges::default();
        changes.add_receipts(&room_id, second_receipt_event);

        store.save_changes(&changes).await.unwrap();
        assert!(store
            .get_user_room_receipt_event(&room_id, ReceiptType::Read, &user_id())
            .await
            .unwrap()
            .is_some());
        assert!(store
            .get_event_room_receipt_events(&room_id, ReceiptType::Read, &first_event_id)
            .await
            .unwrap()
            .is_empty());
        assert_eq!(
            store
                .get_event_room_receipt_events(&room_id, ReceiptType::Read, &second_event_id)
                .await
                .unwrap()
                .len(),
            1
        );
    }

    #[async_test]
    async fn test_media_content() {
        let store = IndexeddbStore::open().unwrap();

        let uri = mxc_uri!("mxc://localhost/media");
        let content: Vec<u8> = "somebinarydata".into();

        let request_file =
            MediaRequest { media_type: MediaType::Uri(uri.clone()), format: MediaFormat::File };

        let request_thumbnail = MediaRequest {
            media_type: MediaType::Uri(uri.clone()),
            format: MediaFormat::Thumbnail(MediaThumbnailSize {
                method: Method::Crop,
                width: uint!(100),
                height: uint!(100),
            }),
        };

        assert!(store.get_media_content(&request_file).await.unwrap().is_none());
        assert!(store.get_media_content(&request_thumbnail).await.unwrap().is_none());

        store.add_media_content(&request_file, content.clone()).await.unwrap();
        assert!(store.get_media_content(&request_file).await.unwrap().is_some());

        store.remove_media_content(&request_file).await.unwrap();
        assert!(store.get_media_content(&request_file).await.unwrap().is_none());

        store.add_media_content(&request_file, content.clone()).await.unwrap();
        assert!(store.get_media_content(&request_file).await.unwrap().is_some());

        store.add_media_content(&request_thumbnail, content.clone()).await.unwrap();
        assert!(store.get_media_content(&request_thumbnail).await.unwrap().is_some());

        store.remove_media_content_for_uri(&uri).await.unwrap();
        assert!(store.get_media_content(&request_file).await.unwrap().is_none());
        assert!(store.get_media_content(&request_thumbnail).await.unwrap().is_none());
    }

    #[async_test]
    async fn test_custom_storage() -> Result<()> {
        let key = "my_key";
        let value = &[0, 1, 2, 3];
        let store = IndexeddbStore::open()?;

        store.set_custom_value(key.as_bytes(), value.to_vec()).await?;

        let read = store.get_custom_value(key.as_bytes()).await?;

        assert_eq!(Some(value.as_ref()), read.as_deref());

        Ok(())
    }
}
