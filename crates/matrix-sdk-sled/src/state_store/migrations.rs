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

use matrix_sdk_base::{
    store::{Result as StoreResult, StoreError},
    StateStoreDataKey,
};
use serde_json::value::{RawValue as RawJsonValue, Value as JsonValue};
use sled::{transaction::TransactionError, Batch, Transactional, Tree};
use tracing::debug;

use super::{keys, Result, SledStateStore, SledStoreError};
use crate::encode_key::EncodeKey;

const DATABASE_VERSION: u8 = 4;

const VERSION_KEY: &str = "state-store-version";

/// Sometimes Migrations can't proceed without having to drop existing
/// data. This allows you to configure, how these cases should be handled.
#[derive(PartialEq, Eq, Clone, Debug)]
pub enum MigrationConflictStrategy {
    /// Just drop the data, we don't care that we have to sync again
    Drop,
    /// Raise a `SledStoreError::MigrationConflict` error with the path to the
    /// DB in question. The caller then has to take care about what they want
    /// to do and try again after.
    Raise,
    /// _Default_: The _entire_ database is backed up under
    /// `$path.$timestamp.backup` (this includes the crypto store if they
    /// are linked), before the state tables are dropped.
    BackupAndDrop,
}

impl SledStateStore {
    pub(super) fn upgrade(&mut self) -> Result<()> {
        let old_version = self.db_version()?;

        if old_version == 0 {
            // we are fresh, let's write the current version
            return self.set_db_version(DATABASE_VERSION);
        }
        if old_version == DATABASE_VERSION {
            // current, we don't have to do anything
            return Ok(());
        };

        debug!(old_version, new_version = DATABASE_VERSION, "Upgrading the Sled state store");

        if old_version == 1 && self.store_cipher.is_some() {
            // we stored some fields un-encrypted. Drop them to force re-creation
            return Err(SledStoreError::MigrationConflict {
                path: self.path.take().expect("Path must exist for a migration to fail"),
                old_version: old_version.into(),
                new_version: DATABASE_VERSION.into(),
            });
        }

        if old_version < 3 {
            self.migrate_to_v3()?;
        }

        if old_version < 4 {
            self.migrate_to_v4()?;
            return Ok(());
        }

        // FUTURE UPGRADE CODE GOES HERE

        // can't upgrade from that version to the new one
        Err(SledStoreError::MigrationConflict {
            path: self.path.take().expect("Path must exist for a migration to fail"),
            old_version: old_version.into(),
            new_version: DATABASE_VERSION.into(),
        })
    }

    /// Get the version of the database.
    ///
    /// Returns `0` for a new database.
    fn db_version(&self) -> Result<u8> {
        Ok(self
            .inner
            .get(VERSION_KEY)?
            .map(|v| {
                let (version_bytes, _) = v.split_at(std::mem::size_of::<u8>());
                u8::from_be_bytes(version_bytes.try_into().unwrap_or_default())
            })
            .unwrap_or_default())
    }

    fn set_db_version(&self, version: u8) -> Result<()> {
        self.inner.insert(VERSION_KEY, version.to_be_bytes().as_ref())?;
        self.inner.flush()?;
        Ok(())
    }

    pub fn drop_v1_tables(self) -> StoreResult<()> {
        for name in V1_DB_STORES {
            self.inner.drop_tree(name).map_err(StoreError::backend)?;
        }
        self.inner.remove(VERSION_KEY).map_err(StoreError::backend)?;

        Ok(())
    }

    fn v3_fix_tree(&self, tree: &Tree, batch: &mut Batch) -> Result<()> {
        fn maybe_fix_json(raw_json: &RawJsonValue) -> Result<Option<JsonValue>> {
            let json = raw_json.get();

            if json.contains(r#""content":null"#) {
                let mut value: JsonValue = serde_json::from_str(json)?;
                if let Some(content) = value.get_mut("content") {
                    if matches!(content, JsonValue::Null) {
                        *content = JsonValue::Object(Default::default());
                        return Ok(Some(value));
                    }
                }
            }

            Ok(None)
        }

        for entry in tree.iter() {
            let (key, value) = entry?;
            let raw_json: Box<RawJsonValue> = self.deserialize_value(&value)?;

            if let Some(fixed_json) = maybe_fix_json(&raw_json)? {
                batch.insert(key, self.serialize_value(&fixed_json)?);
            }
        }

        Ok(())
    }

    fn migrate_to_v3(&self) -> Result<()> {
        let mut room_info_batch = sled::Batch::default();
        self.v3_fix_tree(&self.room_info, &mut room_info_batch)?;

        let mut room_state_batch = sled::Batch::default();
        self.v3_fix_tree(&self.room_state, &mut room_state_batch)?;

        let ret: Result<(), TransactionError<SledStoreError>> = (&self.room_info, &self.room_state)
            .transaction(|(room_info, room_state)| {
                room_info.apply_batch(&room_info_batch)?;
                room_state.apply_batch(&room_state_batch)?;

                Ok(())
            });
        ret?;

        self.set_db_version(3u8)
    }

    /// Replace the SYNC_TOKEN and SESSION trees by KV.
    fn migrate_to_v4(&self) -> Result<()> {
        {
            let session = &self.inner.open_tree(old_keys::SESSION)?;
            let mut batch = sled::Batch::default();

            // Sync token
            let sync_token = session.get(StateStoreDataKey::SyncToken.encoding_key().encode())?;
            if let Some(sync_token) = sync_token {
                batch.insert(StateStoreDataKey::SyncToken.encoding_key().encode(), sync_token);
            }

            // Filters
            let key = self.encode_key(keys::SESSION, StateStoreDataKey::Filter("").encoding_key());
            for res in session.scan_prefix(key) {
                let (key, value) = res?;
                batch.insert(key, value);
            }
            self.kv.apply_batch(batch)?;
        }

        // This was unused so we can just drop it.
        self.inner.drop_tree(old_keys::SYNC_TOKEN)?;
        self.inner.drop_tree(old_keys::SESSION)?;

        self.set_db_version(4)
    }
}

mod old_keys {
    /// Old stores.
    pub const SYNC_TOKEN: &str = "sync_token";
    pub const SESSION: &str = "session";
}

pub const V1_DB_STORES: &[&str] = &[
    keys::ACCOUNT_DATA,
    old_keys::SYNC_TOKEN,
    keys::DISPLAY_NAME,
    keys::INVITED_USER_ID,
    keys::JOINED_USER_ID,
    keys::MEDIA,
    keys::MEMBER,
    keys::PRESENCE,
    keys::PROFILE,
    keys::ROOM_ACCOUNT_DATA,
    keys::ROOM_EVENT_RECEIPT,
    keys::ROOM_INFO,
    keys::ROOM_STATE,
    keys::ROOM_USER_RECEIPT,
    keys::ROOM,
    old_keys::SESSION,
    keys::STRIPPED_INVITED_USER_ID,
    keys::STRIPPED_JOINED_USER_ID,
    keys::STRIPPED_ROOM_INFO,
    keys::STRIPPED_ROOM_MEMBER,
    keys::STRIPPED_ROOM_STATE,
    keys::CUSTOM,
];

#[cfg(test)]
mod test {
    use matrix_sdk_base::StateStoreDataKey;
    use matrix_sdk_test::async_test;
    use ruma::{
        events::{AnySyncStateEvent, StateEventType},
        room_id,
    };
    use serde_json::json;
    use tempfile::TempDir;

    use super::{old_keys, MigrationConflictStrategy};
    use crate::{
        encode_key::EncodeKey,
        state_store::{keys, Result, SledStateStore, SledStoreError},
    };

    #[async_test]
    pub async fn migrating_v1_to_2_plain() -> Result<()> {
        let folder = TempDir::new()?;

        let store = SledStateStore::builder().path(folder.path().to_path_buf()).build()?;

        store.set_db_version(1u8)?;
        drop(store);

        // this transparently migrates to the latest version
        let _store = SledStateStore::builder().path(folder.path().to_path_buf()).build()?;
        Ok(())
    }

    #[async_test]
    pub async fn migrating_v1_to_2_with_pw_backed_up() -> Result<()> {
        let folder = TempDir::new()?;

        let store = SledStateStore::builder()
            .path(folder.path().to_path_buf())
            .passphrase("something".to_owned())
            .build()?;

        store.set_db_version(1u8)?;
        drop(store);

        // this transparently creates a backup and a fresh db
        let _store = SledStateStore::builder()
            .path(folder.path().to_path_buf())
            .passphrase("something".to_owned())
            .build()?;
        assert_eq!(std::fs::read_dir(folder.path())?.count(), 2);
        Ok(())
    }

    #[async_test]
    pub async fn migrating_v1_to_2_with_pw_drop() -> Result<()> {
        let folder = TempDir::new()?;

        let store = SledStateStore::builder()
            .path(folder.path().to_path_buf())
            .passphrase("other thing".to_owned())
            .build()?;

        store.set_db_version(1u8)?;
        drop(store);

        // this transparently creates a backup and a fresh db
        let _store = SledStateStore::builder()
            .path(folder.path().to_path_buf())
            .passphrase("other thing".to_owned())
            .migration_conflict_strategy(MigrationConflictStrategy::Drop)
            .build()?;
        assert_eq!(std::fs::read_dir(folder.path())?.count(), 1);
        Ok(())
    }

    #[async_test]
    pub async fn migrating_v1_to_2_with_pw_raises() -> Result<()> {
        let folder = TempDir::new()?;

        let store = SledStateStore::builder()
            .path(folder.path().to_path_buf())
            .passphrase("secret".to_owned())
            .build()?;

        store.set_db_version(1u8)?;
        drop(store);

        // this transparently creates a backup and a fresh db
        let res = SledStateStore::builder()
            .path(folder.path().to_path_buf())
            .passphrase("secret".to_owned())
            .migration_conflict_strategy(MigrationConflictStrategy::Raise)
            .build();
        if let Err(SledStoreError::MigrationConflict { .. }) = res {
            // all good
        } else {
            panic!("Didn't raise the expected error: {res:?}");
        }
        assert_eq!(std::fs::read_dir(folder.path())?.count(), 1);
        Ok(())
    }

    #[async_test]
    pub async fn migrating_v2_to_v3() {
        // An event that fails to deserialize.
        let wrong_redacted_state_event = json!({
            "content": null,
            "event_id": "$wrongevent",
            "origin_server_ts": 1673887516047_u64,
            "sender": "@example:localhost",
            "state_key": "",
            "type": "m.room.topic",
            "unsigned": {
                "redacted_because": {
                    "type": "m.room.redaction",
                    "sender": "@example:localhost",
                    "content": {},
                    "redacts": "$wrongevent",
                    "origin_server_ts": 1673893816047_u64,
                    "unsigned": {},
                    "event_id": "$redactionevent",
                },
            },
        });
        serde_json::from_value::<AnySyncStateEvent>(wrong_redacted_state_event.clone())
            .unwrap_err();

        let room_id = room_id!("!some_room:localhost");
        let folder = TempDir::new().unwrap();

        let store = SledStateStore::builder()
            .path(folder.path().to_path_buf())
            .passphrase("secret".to_owned())
            .build()
            .unwrap();

        store
            .room_state
            .insert(
                store.encode_key(keys::ROOM_STATE, (room_id, StateEventType::RoomTopic, "")),
                store.serialize_value(&wrong_redacted_state_event).unwrap(),
            )
            .unwrap();
        store.set_db_version(2u8).unwrap();
        drop(store);

        let store = SledStateStore::builder()
            .path(folder.path().to_path_buf())
            .passphrase("secret".to_owned())
            .build()
            .unwrap();
        let event =
            store.get_state_event(room_id, StateEventType::RoomTopic, "").await.unwrap().unwrap();
        event.deserialize().unwrap();
    }

    #[async_test]
    pub async fn migrating_v3_to_v4() {
        let sync_token = "a_very_unique_string";
        let filter_1 = "filter_1";
        let filter_1_id = "filter_1_id";
        let filter_2 = "filter_2";
        let filter_2_id = "filter_2_id";

        let folder = TempDir::new().unwrap();
        let store = SledStateStore::builder()
            .path(folder.path().to_path_buf())
            .passphrase("secret".to_owned())
            .build()
            .unwrap();

        let session = store.inner.open_tree(old_keys::SESSION).unwrap();
        let mut batch = sled::Batch::default();
        batch.insert(
            StateStoreDataKey::SyncToken.encoding_key().encode(),
            store.serialize_value(&sync_token).unwrap(),
        );
        batch.insert(
            store.encode_key(
                keys::SESSION,
                (StateStoreDataKey::Filter("").encoding_key(), filter_1),
            ),
            store.serialize_value(&filter_1_id).unwrap(),
        );
        batch.insert(
            store.encode_key(
                keys::SESSION,
                (StateStoreDataKey::Filter("").encoding_key(), filter_2),
            ),
            store.serialize_value(&filter_2_id).unwrap(),
        );
        session.apply_batch(batch).unwrap();

        store.set_db_version(3).unwrap();
        drop(session);
        drop(store);

        let store = SledStateStore::builder()
            .path(folder.path().to_path_buf())
            .passphrase("secret".to_owned())
            .build()
            .unwrap();

        let stored_sync_token = store
            .get_kv_data(StateStoreDataKey::SyncToken)
            .await
            .unwrap()
            .unwrap()
            .into_sync_token()
            .unwrap();
        assert_eq!(stored_sync_token, sync_token);

        let stored_filter_1_id = store
            .get_kv_data(StateStoreDataKey::Filter(filter_1))
            .await
            .unwrap()
            .unwrap()
            .into_filter()
            .unwrap();
        assert_eq!(stored_filter_1_id, filter_1_id);

        let stored_filter_2_id = store
            .get_kv_data(StateStoreDataKey::Filter(filter_2))
            .await
            .unwrap()
            .unwrap()
            .into_filter()
            .unwrap();
        assert_eq!(stored_filter_2_id, filter_2_id);
    }
}
