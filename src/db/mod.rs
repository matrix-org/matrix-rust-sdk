//! Database code for matrix-sdk-statestore-sql

use std::collections::BTreeSet;

use crate::{StateStore, SupportedDatabase};
use anyhow::Result;
use async_trait::async_trait;
use matrix_sdk_base::{
    deserialized_responses::MemberEvent, media::MediaRequest, MinimalRoomMemberEvent, RoomInfo,
    StateChanges, StoreError,
};
use ruma::{
    events::{
        presence::PresenceEvent, receipt::Receipt, AnyGlobalAccountDataEvent,
        AnyRoomAccountDataEvent, AnySyncStateEvent, GlobalAccountDataEventType,
        RoomAccountDataEventType, StateEventType,
    },
    receipt::ReceiptType,
    serde::Raw,
    EventId, MxcUri, OwnedEventId, OwnedUserId, RoomId, UserId,
};
use sqlx::{
    database::HasArguments, ColumnIndex, Database, Decode, Encode, Executor, IntoArguments, Row,
    Type,
};

mod filters;

impl<DB: SupportedDatabase> StateStore<DB> {
    /// Insert a key-value pair into the kv table
    ///
    /// # Errors
    /// This function will return an error if the upsert cannot be performed
    pub async fn insert_kv(&self, key: Vec<u8>, value: Vec<u8>) -> Result<()>
    where
        for<'a> <DB as HasArguments<'a>>::Arguments: IntoArguments<'a, DB>,
        for<'c> &'c mut <DB as sqlx::Database>::Connection: Executor<'c, Database = DB>,
        for<'q> Vec<u8>: Encode<'q, DB>,
        Vec<u8>: Type<DB>,
    {
        DB::kv_upsert_query()
            .bind(key)
            .bind(value)
            .execute(&*self.db)
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
        for<'q> Vec<u8>: Encode<'q, DB>,
        Vec<u8>: Type<DB>,
        for<'r> Vec<u8>: Decode<'r, DB>,
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
}

/// Shorthand for the store error type
type StoreResult<T> = Result<T, StoreError>;

#[async_trait]
impl<DB: SupportedDatabase> matrix_sdk_base::StateStore for StateStore<DB>
where
    for<'a> <DB as HasArguments<'a>>::Arguments: IntoArguments<'a, DB>,
    for<'c> &'c mut <DB as sqlx::Database>::Connection: Executor<'c, Database = DB>,
    for<'q> Vec<u8>: Encode<'q, DB>,
    Vec<u8>: Type<DB>,
    for<'r> Vec<u8>: Decode<'r, DB>,
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
        todo!();
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
        todo!();
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
        todo!();
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
        todo!();
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
        todo!();
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
        todo!();
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
        todo!();
    }

    /// Get all the user ids of members for a given room, for stripped and
    /// regular rooms alike.
    async fn get_user_ids(&self, room_id: &RoomId) -> StoreResult<Vec<OwnedUserId>> {
        todo!();
    }

    /// Get all the user ids of members that are in the invited state for a
    /// given room, for stripped and regular rooms alike.
    async fn get_invited_user_ids(&self, room_id: &RoomId) -> StoreResult<Vec<OwnedUserId>> {
        todo!();
    }

    /// Get all the user ids of members that are in the joined state for a
    /// given room, for stripped and regular rooms alike.
    async fn get_joined_user_ids(&self, room_id: &RoomId) -> StoreResult<Vec<OwnedUserId>> {
        todo!();
    }

    /// Get all the pure `RoomInfo`s the store knows about.
    async fn get_room_infos(&self) -> StoreResult<Vec<RoomInfo>> {
        todo!();
    }

    /// Get all the pure `RoomInfo`s the store knows about.
    async fn get_stripped_room_infos(&self) -> StoreResult<Vec<RoomInfo>> {
        todo!();
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
        todo!();
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
        todo!();
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
        todo!();
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
        todo!();
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
        todo!();
    }

    /// Get arbitrary data from the custom store
    ///
    /// # Arguments
    ///
    /// * `key` - The key to fetch data for
    async fn get_custom_value(&self, key: &[u8]) -> StoreResult<Option<Vec<u8>>> {
        todo!();
    }

    /// Put arbitrary data into the custom store
    ///
    /// # Arguments
    ///
    /// * `key` - The key to insert data into
    ///
    /// * `value` - The value to insert
    async fn set_custom_value(&self, key: &[u8], value: Vec<u8>) -> StoreResult<Option<Vec<u8>>> {
        todo!();
    }

    /// Add a media file's content in the media store.
    ///
    /// # Arguments
    ///
    /// * `request` - The `MediaRequest` of the file.
    ///
    /// * `content` - The content of the file.
    async fn add_media_content(&self, request: &MediaRequest, content: Vec<u8>) -> StoreResult<()> {
        todo!();
    }

    /// Get a media file's content out of the media store.
    ///
    /// # Arguments
    ///
    /// * `request` - The `MediaRequest` of the file.
    async fn get_media_content(&self, request: &MediaRequest) -> StoreResult<Option<Vec<u8>>> {
        todo!();
    }

    /// Removes a media file's content from the media store.
    ///
    /// # Arguments
    ///
    /// * `request` - The `MediaRequest` of the file.
    async fn remove_media_content(&self, request: &MediaRequest) -> StoreResult<()> {
        todo!();
    }

    /// Removes all the media files' content associated to an `MxcUri` from the
    /// media store.
    ///
    /// # Arguments
    ///
    /// * `uri` - The `MxcUri` of the media files.
    async fn remove_media_content_for_uri(&self, uri: &MxcUri) -> StoreResult<()> {
        todo!();
    }

    /// Removes a room and all elements associated from the state store.
    ///
    /// # Arguments
    ///
    /// * `room_id` - The `RoomId` of the room to delete.
    async fn remove_room(&self, room_id: &RoomId) -> StoreResult<()> {
        todo!();
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

    #[cfg(feature = "mysql")]
    pub async fn open_mysql_database() -> Result<StateStore<sqlx::MySql>> {
        let db =
            Arc::new(sqlx::MySqlPool::connect("mysql://mysql:mysql@localhost:3306/mysql").await?);
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

    #[cfg(feature = "mysql")]
    #[tokio::test]
    #[cfg_attr(not(feature = "ci"), ignore)]
    async fn test_mysql_kv_store() {
        let store = open_mysql_database().await.unwrap();
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
