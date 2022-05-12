//! Database code for matrix-sdk-statestore-sql

use std::collections::BTreeSet;

use crate::{helpers::SqlType, StateStore, SupportedDatabase};
use anyhow::Result;
use async_trait::async_trait;
use matrix_sdk_base::{
    deserialized_responses::MemberEvent, media::MediaRequest, MinimalRoomMemberEvent, RoomInfo,
    StateChanges, StoreError,
};
use ruma::{
    events::{
        presence::PresenceEvent,
        receipt::Receipt,
        room::member::{StrippedRoomMemberEvent, SyncRoomMemberEvent},
        AnyGlobalAccountDataEvent, AnyRoomAccountDataEvent, AnyStrippedStateEvent,
        AnySyncStateEvent, GlobalAccountDataEventType, RoomAccountDataEventType, StateEventType,
    },
    receipt::ReceiptType,
    serde::Raw,
    EventId, MxcUri, OwnedEventId, OwnedUserId, RoomId, UserId,
};
use sqlx::{
    database::HasArguments, types::Json, ColumnIndex, Database, Executor, IntoArguments, Row,
    Transaction,
};

mod custom;
mod filters;
mod media;
mod room;
mod sync_token;

impl<DB: SupportedDatabase> StateStore<DB> {
    /// Insert a key-value pair into the kv table
    ///
    /// # Errors
    /// This function will return an error if the upsert cannot be performed
    pub async fn insert_kv(&self, key: Vec<u8>, value: Vec<u8>) -> Result<()>
    where
        for<'a> <DB as HasArguments<'a>>::Arguments: IntoArguments<'a, DB>,
        for<'c> &'c mut <DB as sqlx::Database>::Connection: Executor<'c, Database = DB>,
        Vec<u8>: SqlType<DB>,
    {
        DB::kv_upsert_query()
            .bind(key)
            .bind(value)
            .execute(&*self.db)
            .await?;
        Ok(())
    }

    /// Insert a key-value pair into the kv table as part of a transaction
    ///
    /// # Errors
    /// This function will return an error if the upsert cannot be performed
    pub async fn insert_kv_txn<'c>(
        txn: &mut Transaction<'c, DB>,
        key: Vec<u8>,
        value: Vec<u8>,
    ) -> Result<()>
    where
        for<'a> <DB as HasArguments<'a>>::Arguments: IntoArguments<'a, DB>,
        for<'a> &'a mut Transaction<'c, DB>: Executor<'a, Database = DB>,
        Vec<u8>: SqlType<DB>,
    {
        DB::kv_upsert_query()
            .bind(key)
            .bind(value)
            .execute(txn)
            .await?;
        Ok(())
    }

    /// Get a value from the kv table
    ///
    /// # Errors
    /// This function will return an error if the database query fails
    pub async fn get_kv(&self, key: Vec<u8>) -> Result<Option<Vec<u8>>>
    where
        for<'a> <DB as HasArguments<'a>>::Arguments: IntoArguments<'a, DB>,
        for<'c> &'c mut <DB as sqlx::Database>::Connection: Executor<'c, Database = DB>,
        Vec<u8>: SqlType<DB>,
        for<'a> &'a str: ColumnIndex<<DB as Database>::Row>,
    {
        let row = DB::kv_load_query()
            .bind(key)
            .fetch_optional(&*self.db)
            .await?;

        let row = if let Some(row) = row {
            row
        } else {
            return Ok(None);
        };

        Ok(row.try_get("kv_value")?)
    }

    /// Save state changes to the database in a transaction
    ///
    /// # Errors
    /// This function will return an error if the database query fails
    pub async fn save_state_changes_txn<'c>(
        txn: &mut Transaction<'c, DB>,
        state_changes: &StateChanges,
    ) -> Result<()>
    where
        for<'a> <DB as HasArguments<'a>>::Arguments: IntoArguments<'a, DB>,
        for<'a> &'a mut Transaction<'c, DB>: Executor<'a, Database = DB>,
        Vec<u8>: SqlType<DB>,
        Option<String>: SqlType<DB>,
        String: SqlType<DB>,
        Json<Raw<AnyGlobalAccountDataEvent>>: SqlType<DB>,
        Json<Raw<PresenceEvent>>: SqlType<DB>,
        Json<SyncRoomMemberEvent>: SqlType<DB>,
        Json<MinimalRoomMemberEvent>: SqlType<DB>,
        bool: SqlType<DB>,
        Json<Raw<AnySyncStateEvent>>: SqlType<DB>,
        Json<Raw<AnyRoomAccountDataEvent>>: SqlType<DB>,
        Json<RoomInfo>: SqlType<DB>,
        Json<Receipt>: SqlType<DB>,
        Json<Raw<AnyStrippedStateEvent>>: SqlType<DB>,
        Json<StrippedRoomMemberEvent>: SqlType<DB>,
    {
        if let Some(sync_token) = &state_changes.sync_token {
            Self::save_sync_token(txn, sync_token).await?;
        }

        for (event_type, event_data) in &state_changes.account_data {
            Self::set_global_account_data(txn, event_type, event_data.clone()).await?;
        }

        for (user_id, presence) in &state_changes.presence {
            Self::set_presence_event(txn, user_id, presence.clone()).await?;
        }

        for (room_id, room_info) in &state_changes.room_infos {
            Self::set_room_info(txn, room_id, room_info.clone()).await?;
        }
        for (room_id, room_info) in &state_changes.stripped_room_infos {
            Self::set_stripped_room_info(txn, room_id, room_info.clone()).await?;
        }

        for (room_id, members) in &state_changes.members {
            for (user_id, member_event) in members {
                Self::set_room_membership(txn, room_id, user_id, member_event.clone()).await?;
            }
        }

        for (room_id, members) in &state_changes.stripped_members {
            for (user_id, member_event) in members {
                Self::set_stripped_room_membership(txn, room_id, user_id, member_event.clone())
                    .await?;
            }
        }

        for (room_id, profiles) in &state_changes.profiles {
            for (user_id, profile) in profiles {
                Self::set_room_profile(txn, room_id, user_id, profile.clone()).await?;
            }
        }

        for (room_id, state_events) in &state_changes.state {
            for (event_type, event_data) in state_events {
                for (state_key, event_data) in event_data {
                    Self::set_room_state(txn, room_id, event_type, state_key, event_data.clone())
                        .await?;
                }
            }
        }

        for (room_id, state_events) in &state_changes.stripped_state {
            for (event_type, event_data) in state_events {
                for (state_key, event_data) in event_data {
                    Self::set_stripped_room_state(
                        txn,
                        room_id,
                        event_type,
                        state_key,
                        event_data.clone(),
                    )
                    .await?;
                }
            }
        }

        for (room_id, account_data) in &state_changes.room_account_data {
            for (event_type, event_data) in account_data {
                Self::set_room_account_data(txn, room_id, event_type, event_data.clone()).await?;
            }
        }

        for (room_id, receipt) in &state_changes.receipts {
            for (event_id, receipt) in &receipt.0 {
                for (receipt_type, receipt) in receipt {
                    for (user_id, receipt) in receipt {
                        Self::set_receipt(
                            txn,
                            room_id,
                            event_id,
                            receipt_type,
                            user_id,
                            receipt.clone(),
                        )
                        .await?;
                    }
                }
            }
        }

        Ok(())
    }

    /// Save state changes to the database
    ///
    /// # Errors
    /// This function will return an error if the database query fails
    pub async fn save_state_changes(&self, state_changes: &StateChanges) -> Result<()>
    where
        for<'a> <DB as HasArguments<'a>>::Arguments: IntoArguments<'a, DB>,
        for<'a, 'c> &'a mut Transaction<'c, DB>: Executor<'a, Database = DB>,
        Vec<u8>: SqlType<DB>,
        Option<String>: SqlType<DB>,
        String: SqlType<DB>,
        Json<Raw<AnyGlobalAccountDataEvent>>: SqlType<DB>,
        Json<Raw<PresenceEvent>>: SqlType<DB>,
        Json<SyncRoomMemberEvent>: SqlType<DB>,
        Json<MinimalRoomMemberEvent>: SqlType<DB>,
        bool: SqlType<DB>,
        Json<Raw<AnySyncStateEvent>>: SqlType<DB>,
        Json<Raw<AnyRoomAccountDataEvent>>: SqlType<DB>,
        Json<RoomInfo>: SqlType<DB>,
        Json<Receipt>: SqlType<DB>,
        Json<Raw<AnyStrippedStateEvent>>: SqlType<DB>,
        Json<StrippedRoomMemberEvent>: SqlType<DB>,
    {
        let mut txn = self.db.begin().await?;
        Self::save_state_changes_txn(&mut txn, state_changes).await?;
        txn.commit().await?;
        Ok(())
    }
}

/// Shorthand for the store error type
type StoreResult<T> = Result<T, StoreError>;

#[async_trait]
impl<DB: SupportedDatabase> matrix_sdk_base::StateStore for StateStore<DB>
where
    for<'a> <DB as HasArguments<'a>>::Arguments: IntoArguments<'a, DB>,
    for<'c> &'c mut <DB as sqlx::Database>::Connection: Executor<'c, Database = DB>,
    for<'a, 'c> &'c mut Transaction<'a, DB>: Executor<'c, Database = DB>,
    Vec<u8>: SqlType<DB>,
    Option<String>: SqlType<DB>,
    String: SqlType<DB>,
    Json<Raw<AnyGlobalAccountDataEvent>>: SqlType<DB>,
    Json<Raw<PresenceEvent>>: SqlType<DB>,
    Json<SyncRoomMemberEvent>: SqlType<DB>,
    Json<MinimalRoomMemberEvent>: SqlType<DB>,
    bool: SqlType<DB>,
    Json<Raw<AnySyncStateEvent>>: SqlType<DB>,
    Json<Raw<AnyRoomAccountDataEvent>>: SqlType<DB>,
    Json<RoomInfo>: SqlType<DB>,
    Json<Receipt>: SqlType<DB>,
    Json<Raw<AnyStrippedStateEvent>>: SqlType<DB>,
    Json<StrippedRoomMemberEvent>: SqlType<DB>,
    Json<MemberEvent>: SqlType<DB>,
    for<'a> &'a str: ColumnIndex<<DB as Database>::Row>,
{
    /// Save the given filter id under the given name.
    ///
    /// # Arguments
    ///
    /// * `filter_name` - The name that should be used to store the filter id.
    ///
    /// * `filter_id` - The filter id that should be stored in the state store.
    async fn save_filter(&self, filter_name: &str, filter_id: &str) -> StoreResult<()> {
        self.save_filter(filter_name, filter_id)
            .await
            .map_err(|e| StoreError::Backend(e.into()))
    }

    /// Save the set of state changes in the store.
    async fn save_changes(&self, changes: &StateChanges) -> StoreResult<()> {
        self.save_state_changes(changes)
            .await
            .map_err(|e| StoreError::Backend(e.into()))
    }

    /// Get the filter id that was stored under the given filter name.
    ///
    /// # Arguments
    ///
    /// * `filter_name` - The name that was used to store the filter id.
    async fn get_filter(&self, filter_name: &str) -> StoreResult<Option<String>> {
        self.get_filter(filter_name)
            .await
            .map_err(|e| StoreError::Backend(e.into()))
    }

    /// Get the last stored sync token.
    async fn get_sync_token(&self) -> StoreResult<Option<String>> {
        self.get_sync_token()
            .await
            .map_err(|e| StoreError::Backend(e.into()))
    }

    /// Get the stored presence event for the given user.
    ///
    /// # Arguments
    ///
    /// * `user_id` - The id of the user for which we wish to fetch the presence
    /// event for.
    async fn get_presence_event(
        &self,
        user_id: &UserId,
    ) -> StoreResult<Option<Raw<PresenceEvent>>> {
        self.get_presence_event(user_id)
            .await
            .map_err(|e| StoreError::Backend(e.into()))
    }

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
        event_type: StateEventType,
        state_key: &str,
    ) -> StoreResult<Option<Raw<AnySyncStateEvent>>> {
        self.get_state_event(room_id, event_type, state_key)
            .await
            .map_err(|e| StoreError::Backend(e.into()))
    }

    /// Get a list of state events for a given room and `StateEventType`.
    ///
    /// # Arguments
    ///
    /// * `room_id` - The id of the room to find events for.
    ///
    /// * `event_type` - The event type.
    async fn get_state_events(
        &self,
        room_id: &RoomId,
        event_type: StateEventType,
    ) -> StoreResult<Vec<Raw<AnySyncStateEvent>>> {
        self.get_state_events(room_id, event_type)
            .await
            .map_err(|e| StoreError::Backend(e.into()))
    }

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
    ) -> StoreResult<Option<MinimalRoomMemberEvent>> {
        self.get_profile(room_id, user_id)
            .await
            .map_err(|e| StoreError::Backend(e.into()))
    }

    /// Get the `MemberEvent` for the given state key in the given room id.
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
    ) -> StoreResult<Option<MemberEvent>> {
        self.get_member_event(room_id, state_key)
            .await
            .map_err(|e| StoreError::Backend(e.into()))
    }

    /// Get all the user ids of members for a given room, for stripped and
    /// regular rooms alike.
    async fn get_user_ids(&self, room_id: &RoomId) -> StoreResult<Vec<OwnedUserId>> {
        self.get_user_ids(room_id)
            .await
            .map_err(|e| StoreError::Backend(e.into()))
    }

    /// Get all the user ids of members that are in the invited state for a
    /// given room, for stripped and regular rooms alike.
    async fn get_invited_user_ids(&self, room_id: &RoomId) -> StoreResult<Vec<OwnedUserId>> {
        self.get_invited_user_ids(room_id)
            .await
            .map_err(|e| StoreError::Backend(e.into()))
    }

    /// Get all the user ids of members that are in the joined state for a
    /// given room, for stripped and regular rooms alike.
    async fn get_joined_user_ids(&self, room_id: &RoomId) -> StoreResult<Vec<OwnedUserId>> {
        self.get_joined_user_ids(room_id)
            .await
            .map_err(|e| StoreError::Backend(e.into()))
    }

    /// Get all the pure `RoomInfo`s the store knows about.
    async fn get_room_infos(&self) -> StoreResult<Vec<RoomInfo>> {
        self.get_room_infos()
            .await
            .map_err(|e| StoreError::Backend(e.into()))
    }

    /// Get all the pure `RoomInfo`s the store knows about.
    async fn get_stripped_room_infos(&self) -> StoreResult<Vec<RoomInfo>> {
        self.get_stripped_room_infos()
            .await
            .map_err(|e| StoreError::Backend(e.into()))
    }

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
    ) -> StoreResult<BTreeSet<OwnedUserId>> {
        self.get_users_with_display_name(room_id, display_name)
            .await
            .map_err(|e| StoreError::Backend(e.into()))
    }

    /// Get an event out of the account data store.
    ///
    /// # Arguments
    ///
    /// * `event_type` - The event type of the account data event.
    async fn get_account_data_event(
        &self,
        event_type: GlobalAccountDataEventType,
    ) -> StoreResult<Option<Raw<AnyGlobalAccountDataEvent>>> {
        self.get_account_data_event(event_type)
            .await
            .map_err(|e| StoreError::Backend(e.into()))
    }

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
        event_type: RoomAccountDataEventType,
    ) -> StoreResult<Option<Raw<AnyRoomAccountDataEvent>>> {
        self.get_room_account_data_event(room_id, event_type)
            .await
            .map_err(|e| StoreError::Backend(e.into()))
    }

    /// Get an event out of the user room receipt store.
    ///
    /// # Arguments
    ///
    /// * `room_id` - The id of the room for which the receipt should be
    ///   fetched.
    ///
    /// * `receipt_type` - The type of the receipt.
    ///
    /// * `user_id` - The id of the user for who the receipt should be fetched.
    async fn get_user_room_receipt_event(
        &self,
        room_id: &RoomId,
        receipt_type: ReceiptType,
        user_id: &UserId,
    ) -> StoreResult<Option<(OwnedEventId, Receipt)>> {
        self.get_user_room_receipt_event(room_id, receipt_type, user_id)
            .await
            .map_err(|e| StoreError::Backend(e.into()))
    }

    /// Get events out of the event room receipt store.
    ///
    /// # Arguments
    ///
    /// * `room_id` - The id of the room for which the receipts should be
    ///   fetched.
    ///
    /// * `receipt_type` - The type of the receipts.
    ///
    /// * `event_id` - The id of the event for which the receipts should be
    ///   fetched.
    async fn get_event_room_receipt_events(
        &self,
        room_id: &RoomId,
        receipt_type: ReceiptType,
        event_id: &EventId,
    ) -> StoreResult<Vec<(OwnedUserId, Receipt)>> {
        self.get_event_room_receipt_events(room_id, receipt_type, event_id)
            .await
            .map_err(|e| StoreError::Backend(e.into()))
    }

    /// Get arbitrary data from the custom store
    ///
    /// # Arguments
    ///
    /// * `key` - The key to fetch data for
    async fn get_custom_value(&self, key: &[u8]) -> StoreResult<Option<Vec<u8>>> {
        self.get_custom_value(key)
            .await
            .map_err(|e| StoreError::Backend(e.into()))
    }

    /// Put arbitrary data into the custom store
    ///
    /// # Arguments
    ///
    /// * `key` - The key to insert data into
    ///
    /// * `value` - The value to insert
    async fn set_custom_value(&self, key: &[u8], value: Vec<u8>) -> StoreResult<Option<Vec<u8>>> {
        let old_val = self
            .get_custom_value(key)
            .await
            .map_err(|e| StoreError::Backend(e.into()))?;
        self.set_custom_value(key, value)
            .await
            .map_err(|e| StoreError::Backend(e.into()))?;
        Ok(old_val)
    }

    /// Add a media file's content in the media store.
    ///
    /// # Arguments
    ///
    /// * `request` - The `MediaRequest` of the file.
    ///
    /// * `content` - The content of the file.
    async fn add_media_content(&self, request: &MediaRequest, content: Vec<u8>) -> StoreResult<()> {
        self.insert_media(Self::extract_media_url(request), content)
            .await
            .map_err(|e| StoreError::Backend(e.into()))
    }

    /// Get a media file's content out of the media store.
    ///
    /// # Arguments
    ///
    /// * `request` - The `MediaRequest` of the file.
    async fn get_media_content(&self, request: &MediaRequest) -> StoreResult<Option<Vec<u8>>> {
        self.get_media(Self::extract_media_url(request))
            .await
            .map_err(|e| StoreError::Backend(e.into()))
    }

    /// Removes a media file's content from the media store.
    ///
    /// # Arguments
    ///
    /// * `request` - The `MediaRequest` of the file.
    async fn remove_media_content(&self, request: &MediaRequest) -> StoreResult<()> {
        self.delete_media(Self::extract_media_url(request))
            .await
            .map_err(|e| StoreError::Backend(e.into()))
    }

    /// Removes all the media files' content associated to an `MxcUri` from the
    /// media store.
    ///
    /// # Arguments
    ///
    /// * `uri` - The `MxcUri` of the media files.
    async fn remove_media_content_for_uri(&self, uri: &MxcUri) -> StoreResult<()> {
        self.delete_media(uri)
            .await
            .map_err(|e| StoreError::Backend(e.into()))
    }

    /// Removes a room and all elements associated from the state store.
    ///
    /// # Arguments
    ///
    /// * `room_id` - The `RoomId` of the room to delete.
    async fn remove_room(&self, room_id: &RoomId) -> StoreResult<()> {
        self.remove_room(room_id)
            .await
            .map_err(|e| StoreError::Backend(e.into()))
    }
}

#[cfg(test)]
#[allow(unused_imports, unreachable_pub, clippy::unwrap_used)]
pub mod tests {
    use crate::{StateStore, SupportedDatabase};
    use anyhow::Result;
    use sqlx::{
        database::HasArguments, migrate::Migrate, ColumnIndex, Database, Decode, Encode, Executor,
        IntoArguments, Pool, Type,
    };
    use std::sync::Arc;
    #[cfg(feature = "sqlite")]
    pub async fn open_sqlite_database() -> Result<StateStore<sqlx::Sqlite>> {
        let db = Arc::new(sqlx::SqlitePool::connect("sqlite://:memory:").await?);
        let store = StateStore::new(&db).await?;
        Ok(store)
    }

    #[cfg(feature = "postgres")]
    pub async fn open_postgres_database() -> Result<StateStore<sqlx::Postgres>> {
        let db = Arc::new(
            sqlx::PgPool::connect("postgres://postgres:postgres@localhost:5432/postgres").await?,
        );
        let store = StateStore::new(&db).await?;
        Ok(store)
    }

    #[cfg(feature = "sqlite")]
    #[tokio::test]
    async fn test_sqlite_kv_store() {
        let store = open_sqlite_database().await.unwrap();
        store
            .insert_kv(b"key".to_vec(), b"value".to_vec())
            .await
            .unwrap();
        let value = store.get_kv(b"key".to_vec()).await.unwrap();
        assert_eq!(value, Some(b"value".to_vec()));
        store
            .insert_kv(b"key".to_vec(), b"value2".to_vec())
            .await
            .unwrap();
        let value = store.get_kv(b"key".to_vec()).await.unwrap();
        assert_eq!(value, Some(b"value2".to_vec()));
    }

    #[cfg(feature = "postgres")]
    #[tokio::test]
    #[cfg_attr(not(feature = "ci"), ignore)]
    async fn test_postgres_kv_store() {
        let store = open_postgres_database().await.unwrap();
        store
            .insert_kv(b"key".to_vec(), b"value".to_vec())
            .await
            .unwrap();
        let value = store.get_kv(b"key".to_vec()).await.unwrap();
        assert_eq!(value, Some(b"value".to_vec()));
        store
            .insert_kv(b"key".to_vec(), b"value2".to_vec())
            .await
            .unwrap();
        let value = store.get_kv(b"key".to_vec()).await.unwrap();
        assert_eq!(value, Some(b"value2".to_vec()));
    }
}

#[allow(clippy::redundant_pub_crate)]
#[cfg(all(test, feature = "sqlite"))]
mod sqlite_integration_test {
    use matrix_sdk_base::{statestore_integration_tests, StateStore, StoreError};

    use super::StoreResult;
    async fn get_store() -> StoreResult<impl StateStore> {
        super::tests::open_sqlite_database()
            .await
            .map_err(|e| StoreError::Backend(e.into()))
    }

    statestore_integration_tests! { integration }
}

#[allow(clippy::redundant_pub_crate)]
#[cfg(all(test, feature = "postgres", feature = "ci"))]
mod postgres_integration_test {
    use matrix_sdk_base::{statestore_integration_tests, StateStore, StoreError};

    use super::StoreResult;
    async fn get_store() -> StoreResult<impl StateStore> {
        super::tests::open_postgres_database()
            .await
            .map_err(|e| StoreError::Backend(e.into()))
    }

    statestore_integration_tests! { integration }
}
