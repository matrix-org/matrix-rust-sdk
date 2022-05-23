//! Database code for matrix-sdk-statestore-sql

use std::collections::BTreeSet;

use crate::{
    helpers::{BorrowedSqlType, SqlType},
    StateStore, SupportedDatabase,
};
use anyhow::Result;
use async_trait::async_trait;
use futures::TryStreamExt;
use matrix_sdk_base::{
    deserialized_responses::MemberEvent, media::MediaRequest, MinimalRoomMemberEvent, RoomInfo,
    StateChanges, StoreError,
};
use ruma::{
    events::{
        presence::PresenceEvent,
        receipt::Receipt,
        room::{
            member::{MembershipState, StrippedRoomMemberEvent, SyncRoomMemberEvent},
            MediaSource,
        },
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

impl<DB: SupportedDatabase> StateStore<DB>
where
    for<'a> <DB as HasArguments<'a>>::Arguments: IntoArguments<'a, DB>,
    for<'c> &'c mut <DB as sqlx::Database>::Connection: Executor<'c, Database = DB>,
    for<'a, 'c> &'c mut Transaction<'a, DB>: Executor<'c, Database = DB>,
    for<'a> &'a [u8]: BorrowedSqlType<'a, DB>,
    for<'a> &'a str: BorrowedSqlType<'a, DB>,
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
    /// Put arbitrary data into the custom store
    ///
    /// # Errors
    /// This function will return an error if the upsert cannot be performed
    pub(crate) async fn set_custom_value(&self, key_ref: &[u8], val: &[u8]) -> Result<()> {
        let mut key = Vec::with_capacity(7 + key_ref.len());
        key.extend_from_slice(b"custom:");
        key.extend_from_slice(key_ref);

        self.insert_kv(&key, val).await
    }

    /// Get arbitrary data from the custom store
    ///
    /// # Errors
    /// This function will return an error if the database query fails
    pub(crate) async fn get_custom_value(&self, key_ref: &[u8]) -> Result<Option<Vec<u8>>> {
        let mut key = Vec::with_capacity(7 + key_ref.len());
        key.extend_from_slice(b"custom:");
        key.extend_from_slice(key_ref);
        self.get_kv(&key).await
    }

    /// Save the given filter id under the given name
    ///
    /// # Errors
    /// This function will return an error if the upsert cannot be performed
    pub(crate) async fn save_filter(&self, name: &str, filter_id: &str) -> Result<()> {
        let mut key = Vec::with_capacity(7 + name.len());
        key.extend_from_slice(b"filter:");
        key.extend_from_slice(name.as_bytes());

        self.insert_kv(&key, filter_id.as_bytes()).await
    }

    /// Get the filter id that was stored under the given filter name.
    ///
    /// # Errors
    /// This function will return an error if the database query fails
    pub(crate) async fn get_filter(&self, name: &str) -> Result<Option<String>> {
        let mut key = Vec::with_capacity(7 + name.len());
        key.extend_from_slice(b"filter:");
        key.extend_from_slice(name.as_bytes());
        let result = self.get_kv(&key).await?;
        match result {
            Some(value) => Ok(Some(String::from_utf8(value)?)),
            None => Ok(None),
        }
    }

    /// Insert media into the media store
    ///
    /// # Errors
    /// This function will return an error if the media cannot be inserted
    pub(crate) async fn insert_media(&self, url: &MxcUri, media: &[u8]) -> Result<()> {
        let mut txn = self.db.begin().await?;

        DB::media_insert_query_1()
            .bind(url.as_str())
            .bind(media)
            .execute(&mut txn)
            .await?;
        DB::media_insert_query_2().execute(&mut txn).await?;

        txn.commit().await?;
        Ok(())
    }

    /// Deletes media from the media store
    ///
    /// # Errors
    /// This function will return an error if the media cannot be deleted
    pub(crate) async fn delete_media(&self, url: &MxcUri) -> Result<()> {
        DB::media_delete_query()
            .bind(url.as_str())
            .execute(&*self.db)
            .await?;
        Ok(())
    }

    /// Gets media from the media store
    ///
    /// # Errors
    /// This function will return an error if the query fails
    pub(crate) async fn get_media(&self, url: &MxcUri) -> Result<Option<Vec<u8>>> {
        let row = DB::media_load_query()
            .bind(url.as_str())
            .fetch_optional(&*self.db)
            .await?;
        let row = if let Some(row) = row {
            row
        } else {
            return Ok(None);
        };
        Ok(row.try_get("media_data")?)
    }

    /// Extracts an [`MxcUri`] from a media query
    ///
    /// [`MxcUri`]: ruma::identifiers::MxcUri
    #[must_use]
    pub(crate) fn extract_media_url(request: &MediaRequest) -> &MxcUri {
        match request.source {
            MediaSource::Plain(ref p) => p,
            MediaSource::Encrypted(ref e) => &e.url,
        }
    }

    /// Deletes a room from the room store
    ///
    /// # Errors
    /// This function will return an error if the the query fails
    pub(crate) async fn remove_room(&self, room_id: &RoomId) -> Result<()> {
        let mut txn = self.db.begin().await?;

        for query in DB::room_remove_queries() {
            query.bind(room_id.as_str()).execute(&mut txn).await?;
        }

        txn.commit().await?;
        Ok(())
    }

    /// Sets global account data for an account data event
    ///
    /// # Errors
    /// This function will return an error if the the query fails
    pub(crate) async fn set_global_account_data<'c>(
        txn: &mut Transaction<'c, DB>,
        event_type: &GlobalAccountDataEventType,
        event_data: Raw<AnyGlobalAccountDataEvent>,
    ) -> Result<()> {
        DB::account_data_upsert_query()
            .bind("")
            .bind(event_type.to_string())
            .bind(Json(event_data))
            .execute(txn)
            .await?;

        Ok(())
    }

    /// Get global account data for an account data event type
    ///
    /// # Errors
    /// This function will return an error if the the query fails
    pub(crate) async fn get_account_data_event(
        &self,
        event_type: GlobalAccountDataEventType,
    ) -> Result<Option<Raw<AnyGlobalAccountDataEvent>>> {
        let row = DB::account_data_load_query()
            .bind("")
            .bind(event_type.to_string())
            .fetch_optional(&*self.db)
            .await?;
        let row = if let Some(row) = row {
            row
        } else {
            return Ok(None);
        };
        let row: Json<Raw<AnyGlobalAccountDataEvent>> = row.try_get("account_data")?;
        Ok(Some(row.0))
    }

    /// Get global account data for an account data event type
    ///
    /// # Errors
    /// This function will return an error if the the query fails
    pub(crate) async fn get_room_account_data_event(
        &self,
        room_id: &RoomId,
        event_type: RoomAccountDataEventType,
    ) -> Result<Option<Raw<AnyRoomAccountDataEvent>>> {
        let row = DB::account_data_load_query()
            .bind(room_id.as_str())
            .bind(event_type.to_string())
            .fetch_optional(&*self.db)
            .await?;
        let row = if let Some(row) = row {
            row
        } else {
            return Ok(None);
        };
        let row: Json<Raw<AnyRoomAccountDataEvent>> = row.try_get("account_data")?;
        Ok(Some(row.0))
    }

    /// Sets presence for a user
    ///
    /// # Errors
    /// This function will return an error if the the query fails
    pub(crate) async fn set_presence_event<'c>(
        txn: &mut Transaction<'c, DB>,
        user_id: &UserId,
        presence: Raw<PresenceEvent>,
    ) -> Result<()> {
        DB::presence_upsert_query()
            .bind(user_id.as_str())
            .bind(Json(presence))
            .execute(txn)
            .await?;
        Ok(())
    }

    /// Gets presence for a user
    ///
    /// # Errors
    /// This function will return an error if the the query fails
    pub(crate) async fn get_presence_event(
        &self,
        user_id: &UserId,
    ) -> Result<Option<Raw<PresenceEvent>>> {
        let row = DB::presence_load_query()
            .bind(user_id.as_str())
            .fetch_optional(&*self.db)
            .await?;
        let row = if let Some(row) = row {
            row
        } else {
            return Ok(None);
        };
        let row: Json<Raw<PresenceEvent>> = row.try_get("presence")?;
        Ok(Some(row.0))
    }

    /// Removes a member from a channel
    ///
    /// # Errors
    /// This function will return an error if the the query fails
    async fn remove_member<'c>(
        txn: &mut Transaction<'c, DB>,
        room_id: &RoomId,
        user_id: &UserId,
    ) -> Result<()> {
        DB::member_remove_query()
            .bind(room_id.as_str())
            .bind(user_id.as_str())
            .execute(txn)
            .await?;
        Ok(())
    }

    /// Stores room membership info for a user
    ///
    /// # Errors
    /// This function will return an error if the the query fails
    pub(crate) async fn set_room_membership<'c>(
        txn: &mut Transaction<'c, DB>,
        room_id: &RoomId,
        user_id: &UserId,
        member_event: SyncRoomMemberEvent,
    ) -> Result<()> {
        let displayname = member_event
            .as_original()
            .and_then(|v| v.content.displayname.clone());
        let joined = match member_event.as_original().map(|v| &v.content.membership) {
            Some(MembershipState::Join) => true,
            Some(MembershipState::Invite) => false,
            _ => return Self::remove_member(txn, room_id, user_id).await,
        };
        DB::member_upsert_query()
            .bind(room_id.as_str())
            .bind(user_id.as_str())
            .bind(false)
            .bind(Json(member_event))
            .bind(displayname)
            .bind(joined)
            .execute(txn)
            .await?;
        Ok(())
    }

    /// Stores stripped room membership info for a user
    ///
    /// # Errors
    /// This function will return an error if the the query fails
    pub(crate) async fn set_stripped_room_membership<'c>(
        txn: &mut Transaction<'c, DB>,
        room_id: &RoomId,
        user_id: &UserId,
        member_event: StrippedRoomMemberEvent,
    ) -> Result<()> {
        let displayname = member_event.content.displayname.clone();
        let joined = match member_event.content.membership {
            MembershipState::Join => true,
            MembershipState::Invite => false,
            _ => return Self::remove_member(txn, room_id, user_id).await,
        };
        DB::member_upsert_query()
            .bind(room_id.as_str())
            .bind(user_id.as_str())
            .bind(true)
            .bind(Json(member_event))
            .bind(displayname)
            .bind(joined)
            .execute(txn)
            .await?;
        Ok(())
    }

    /// Stores user profile in room
    ///
    /// # Errors
    /// This function will return an error if the the query fails
    pub(crate) async fn set_room_profile<'c>(
        txn: &mut Transaction<'c, DB>,
        room_id: &RoomId,
        user_id: &UserId,
        profile: MinimalRoomMemberEvent,
    ) -> Result<()> {
        DB::member_profile_upsert_query()
            .bind(room_id.as_str())
            .bind(user_id.as_str())
            .bind(false)
            .bind(Json(profile))
            .execute(txn)
            .await?;
        Ok(())
    }

    /// Stores a state event for a room
    ///
    /// # Errors
    /// This function will return an error if the the query fails
    pub(crate) async fn set_room_state<'c>(
        txn: &mut Transaction<'c, DB>,
        room_id: &RoomId,
        event_type: &StateEventType,
        state_key: &str,
        state: Raw<AnySyncStateEvent>,
    ) -> Result<()> {
        DB::state_upsert_query()
            .bind(room_id.as_str())
            .bind(event_type.to_string())
            .bind(state_key)
            .bind(false)
            .bind(Json(state))
            .execute(txn)
            .await?;
        Ok(())
    }

    /// Stores a stripped state event for a room
    ///
    /// # Errors
    /// This function will return an error if the the query fails
    pub(crate) async fn set_stripped_room_state<'c>(
        txn: &mut Transaction<'c, DB>,
        room_id: &RoomId,
        event_type: &StateEventType,
        state_key: &str,
        state: Raw<AnyStrippedStateEvent>,
    ) -> Result<()> {
        DB::state_upsert_query()
            .bind(room_id.as_str())
            .bind(event_type.to_string())
            .bind(state_key)
            .bind(true)
            .bind(Json(state))
            .execute(txn)
            .await?;
        Ok(())
    }

    /// Stores account data for a room
    ///
    /// # Errors
    /// This function will return an error if the the query fails
    pub(crate) async fn set_room_account_data<'c>(
        txn: &mut Transaction<'c, DB>,
        room_id: &RoomId,
        event_type: &RoomAccountDataEventType,
        event_data: Raw<AnyRoomAccountDataEvent>,
    ) -> Result<()> {
        DB::account_data_upsert_query()
            .bind(room_id.as_str())
            .bind(event_type.to_string())
            .bind(Json(event_data))
            .execute(txn)
            .await?;
        Ok(())
    }

    /// Stores info for a room
    ///
    /// # Errors
    /// This function will return an error if the the query fails
    pub(crate) async fn set_room_info<'c>(
        txn: &mut Transaction<'c, DB>,
        room_id: &RoomId,
        room_info: RoomInfo,
    ) -> Result<()> {
        DB::room_upsert_query()
            .bind(room_id.as_str())
            .bind(false)
            .bind(Json(room_info))
            .execute(txn)
            .await?;
        Ok(())
    }

    /// Stores stripped info for a room
    ///
    /// # Errors
    /// This function will return an error if the the query fails
    pub(crate) async fn set_stripped_room_info<'c>(
        txn: &mut Transaction<'c, DB>,
        room_id: &RoomId,
        room_info: RoomInfo,
    ) -> Result<()> {
        DB::room_upsert_query()
            .bind(room_id.as_str())
            .bind(true)
            .bind(Json(room_info))
            .execute(txn)
            .await?;
        Ok(())
    }

    /// Stores receipt for an event
    ///
    /// # Errors
    /// This function will return an error if the the query fails
    pub(crate) async fn set_receipt<'c>(
        txn: &mut Transaction<'c, DB>,
        room_id: &RoomId,
        event_id: &EventId,
        receipt_type: &ReceiptType,
        user_id: &UserId,
        receipt: Receipt,
    ) -> Result<()> {
        DB::receipt_upsert_query()
            .bind(room_id.as_str())
            .bind(event_id.as_str())
            .bind(receipt_type.as_str())
            .bind(user_id.as_str())
            .bind(Json(receipt))
            .execute(txn)
            .await?;
        Ok(())
    }

    /// Retrieves a state event in room by event type and state key
    ///
    /// # Errors
    /// This function will return an error if the the query fails
    pub(crate) async fn get_state_event(
        &self,
        room_id: &RoomId,
        event_type: StateEventType,
        state_key: &str,
    ) -> Result<Option<Raw<AnySyncStateEvent>>> {
        let row = DB::state_load_query()
            .bind(room_id.as_str())
            .bind(event_type.to_string())
            .bind(state_key)
            .fetch_optional(&*self.db)
            .await?;
        let row = if let Some(row) = row {
            row
        } else {
            return Ok(None);
        };
        let row: Json<Raw<AnySyncStateEvent>> = row.try_get("state_event")?;
        Ok(Some(row.0))
    }

    /// Retrieves all state events of a given type in a room
    ///
    /// # Errors
    /// This function will return an error if the the query fails
    pub(crate) async fn get_state_events(
        &self,
        room_id: &RoomId,
        event_type: StateEventType,
    ) -> Result<Vec<Raw<AnySyncStateEvent>>> {
        let mut rows = DB::states_load_query()
            .bind(room_id.as_str())
            .bind(event_type.to_string())
            .bind(false)
            .fetch(&*self.db);
        let mut result = Vec::new();
        while let Some(row) = rows.try_next().await? {
            result.push(
                row.try_get::<'_, Json<Raw<AnySyncStateEvent>>, _>("state_event")?
                    .0,
            );
        }
        Ok(result)
    }

    /// Retrieves the profile of a user in a room
    ///
    /// # Errors
    /// This function will return an error if the the query fails
    pub(crate) async fn get_profile(
        &self,
        room_id: &RoomId,
        user_id: &UserId,
    ) -> Result<Option<MinimalRoomMemberEvent>> {
        let row = DB::profile_load_query()
            .bind(room_id.as_str())
            .bind(user_id.as_str())
            .fetch_optional(&*self.db)
            .await?;
        let row = if let Some(row) = row {
            row
        } else {
            return Ok(None);
        };
        let row: Json<MinimalRoomMemberEvent> = row.try_get("user_profile")?;
        Ok(Some(row.0))
    }

    /// Retrieves a list of user ids in a room
    ///
    /// # Errors
    /// This function will return an error if the the query fails
    pub(crate) async fn get_user_ids(&self, room_id: &RoomId) -> Result<Vec<OwnedUserId>> {
        let mut rows = DB::members_load_query()
            .bind(room_id.as_str())
            .fetch(&*self.db);
        let mut result = Vec::new();
        while let Some(row) = rows.try_next().await? {
            result.push(row.try_get::<'_, String, _>("user_id")?.try_into()?);
        }
        Ok(result)
    }

    /// Retrieves a list of invited user ids in a room
    ///
    /// # Errors
    /// This function will return an error if the the query fails
    pub(crate) async fn get_invited_user_ids(&self, room_id: &RoomId) -> Result<Vec<OwnedUserId>> {
        let mut rows = DB::members_load_query_with_join_status()
            .bind(room_id.as_str())
            .bind(false)
            .fetch(&*self.db);
        let mut result = Vec::new();
        while let Some(row) = rows.try_next().await? {
            result.push(row.try_get::<'_, String, _>("user_id")?.try_into()?);
        }
        Ok(result)
    }

    /// Retrieves a list of joined user ids in a room
    ///
    /// # Errors
    /// This function will return an error if the the query fails
    pub(crate) async fn get_joined_user_ids(&self, room_id: &RoomId) -> Result<Vec<OwnedUserId>> {
        let mut rows = DB::members_load_query_with_join_status()
            .bind(room_id.as_str())
            .bind(true)
            .fetch(&*self.db);
        let mut result = Vec::new();
        while let Some(row) = rows.try_next().await? {
            result.push(row.try_get::<'_, String, _>("user_id")?.try_into()?);
        }
        Ok(result)
    }

    /// Retrieves a member event for a user in a room
    ///
    /// # Errors
    /// This function will return an error if the the query fails
    pub(crate) async fn get_member_event(
        &self,
        room_id: &RoomId,
        user_id: &UserId,
    ) -> Result<Option<MemberEvent>> {
        let row = DB::member_load_query()
            .bind(room_id.as_str())
            .bind(user_id.as_str())
            .fetch_optional(&*self.db)
            .await?;
        let row = if let Some(row) = row {
            row
        } else {
            return Ok(None);
        };
        if row.try_get::<'_, bool, _>("is_partial")? {
            let row: Json<StrippedRoomMemberEvent> = row.try_get("member_event")?;
            Ok(Some(MemberEvent::Stripped(row.0)))
        } else {
            let row: Json<SyncRoomMemberEvent> = row.try_get("member_event")?;
            Ok(Some(MemberEvent::Sync(row.0)))
        }
    }

    /// Get room infos
    ///
    /// # Errors
    /// This function will return an error if the the query fails
    async fn get_room_infos_internal(&self, partial: bool) -> Result<Vec<RoomInfo>> {
        let mut rows = DB::room_info_load_query().bind(partial).fetch(&*self.db);
        let mut result = Vec::new();
        while let Some(row) = rows.try_next().await? {
            result.push((row.try_get::<'_, Json<RoomInfo>, _>("room_info")?).0);
        }
        Ok(result)
    }

    /// Get room infos
    ///
    /// # Errors
    /// This function will return an error if the the query fails
    pub(crate) async fn get_room_infos(&self) -> Result<Vec<RoomInfo>> {
        self.get_room_infos_internal(false).await
    }
    /// Get partial room infos
    ///
    /// # Errors
    /// This function will return an error if the the query fails
    pub(crate) async fn get_stripped_room_infos(&self) -> Result<Vec<RoomInfo>> {
        self.get_room_infos_internal(true).await
    }

    /// Get users with display names in room
    ///
    /// # Errors
    /// This function will return an error if the the query fails
    pub(crate) async fn get_users_with_display_name(
        &self,
        room_id: &RoomId,
        display_name: &str,
    ) -> Result<BTreeSet<OwnedUserId>> {
        let mut rows = DB::users_with_display_name_load_query()
            .bind(room_id.as_ref())
            .bind(display_name)
            .fetch(&*self.db);
        let mut result = BTreeSet::new();
        while let Some(row) = rows.try_next().await? {
            result.insert(row.try_get::<'_, String, _>("user_id")?.try_into()?);
        }
        Ok(result)
    }

    /// Get latest receipt for user in room
    ///
    /// # Errors
    /// This function will return an error if the the query fails
    pub(crate) async fn get_user_room_receipt_event(
        &self,
        room_id: &RoomId,
        receipt_type: ReceiptType,
        user_id: &UserId,
    ) -> Result<Option<(OwnedEventId, Receipt)>> {
        let row = DB::receipt_load_query()
            .bind(room_id.as_ref())
            .bind(receipt_type.as_ref())
            .bind(user_id.as_ref())
            .fetch_optional(&*self.db)
            .await?;
        let row = if let Some(row) = row {
            row
        } else {
            return Ok(None);
        };
        let event_id = row.try_get::<'_, String, _>("event_id")?.try_into()?;
        let receipt = row.try_get::<'_, Json<Receipt>, _>("receipt")?.0;
        Ok(Some((event_id, receipt)))
    }

    /// Get all receipts for event in room
    ///
    /// # Errors
    /// This function will return an error if the the query fails
    pub(crate) async fn get_event_room_receipt_events(
        &self,
        room_id: &RoomId,
        receipt_type: ReceiptType,
        event_id: &EventId,
    ) -> Result<Vec<(OwnedUserId, Receipt)>> {
        let mut rows = DB::event_receipt_load_query()
            .bind(room_id.as_ref())
            .bind(receipt_type.as_ref())
            .bind(event_id.as_ref())
            .fetch(&*self.db);
        let mut result = Vec::new();
        while let Some(row) = rows.try_next().await? {
            let user_id = row.try_get::<'_, String, _>("user_id")?.try_into()?;
            let receipt = row.try_get::<'_, Json<Receipt>, _>("receipt")?.0;
            result.push((user_id, receipt));
        }
        Ok(result)
    }

    /// Put a sync token into the sync token store
    ///
    /// # Errors
    /// This function will return an error if the upsert cannot be performed
    #[cfg(test)]
    async fn save_sync_token_test(&self, token: &str) -> Result<()> {
        self.insert_kv(b"sync_token", token.as_bytes()).await
    }

    /// Put a sync token into the sync token store
    ///
    /// # Errors
    /// This function will return an error if the upsert cannot be performed
    pub(crate) async fn save_sync_token<'c>(
        txn: &mut Transaction<'c, DB>,
        token: &str,
    ) -> Result<()> {
        Self::insert_kv_txn(txn, b"sync_token", token.as_bytes()).await
    }

    /// Get the last stored sync token
    ///
    /// # Errors
    /// This function will return an error if the database query fails
    pub(crate) async fn get_sync_token(&self) -> Result<Option<String>> {
        let result = self.get_kv(b"sync_token").await?;
        match result {
            Some(value) => Ok(Some(String::from_utf8(value)?)),
            None => Ok(None),
        }
    }

    /// Insert a key-value pair into the kv table
    ///
    /// # Errors
    /// This function will return an error if the upsert cannot be performed
    pub(crate) async fn insert_kv(&self, key: &[u8], value: &[u8]) -> Result<()> {
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
    pub(crate) async fn insert_kv_txn<'c>(
        txn: &mut Transaction<'c, DB>,
        key: &[u8],
        value: &[u8],
    ) -> Result<()> {
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
    pub(crate) async fn get_kv(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
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
    pub(crate) async fn save_state_changes_txn<'c>(
        txn: &mut Transaction<'c, DB>,
        state_changes: &StateChanges,
    ) -> Result<()> {
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
    pub(crate) async fn save_state_changes(&self, state_changes: &StateChanges) -> Result<()> {
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
    for<'a> &'a [u8]: BorrowedSqlType<'a, DB>,
    for<'a> &'a str: BorrowedSqlType<'a, DB>,
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
        self.set_custom_value(key, &value)
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
        self.insert_media(Self::extract_media_url(request), &content)
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
mod tests {
    use crate::{StateStore, SupportedDatabase};
    use anyhow::Result;
    use ruma::{MxcUri, OwnedMxcUri};
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
    async fn open_postgres_database() -> Result<StateStore<sqlx::Postgres>> {
        let db = Arc::new(
            sqlx::PgPool::connect("postgres://postgres:postgres@localhost:5432/postgres").await?,
        );
        let store = StateStore::new(&db).await?;
        Ok(store)
    }

    #[cfg(feature = "sqlite")]
    #[tokio::test]
    async fn test_sqlite_custom_values() {
        let store = open_sqlite_database().await.unwrap();
        assert_eq!(store.get_custom_value(b"test").await.unwrap(), None);
        store.set_custom_value(b"test", b"test").await.unwrap();
        assert_eq!(
            store.get_custom_value(b"test").await.unwrap(),
            Some(b"test".to_vec())
        );
        store.set_custom_value(b"test2", b"test3").await.unwrap();
        assert_eq!(
            store.get_custom_value(b"test2").await.unwrap(),
            Some(b"test3".to_vec())
        );
        assert_eq!(
            store.get_custom_value(b"test").await.unwrap(),
            Some(b"test".to_vec())
        );
        store.set_custom_value(b"test", b"test4").await.unwrap();
        assert_eq!(
            store.get_custom_value(b"test").await.unwrap(),
            Some(b"test4".to_vec())
        );
        assert_eq!(
            store.get_custom_value(b"test2").await.unwrap(),
            Some(b"test3".to_vec())
        );
    }

    #[cfg(feature = "postgres")]
    #[tokio::test]
    #[cfg_attr(not(feature = "ci"), ignore)]
    async fn test_postgres_custom_values() {
        let store = open_postgres_database().await.unwrap();
        assert_eq!(store.get_custom_value(b"test").await.unwrap(), None);
        store.set_custom_value(b"test", b"test").await.unwrap();
        assert_eq!(
            store.get_custom_value(b"test").await.unwrap(),
            Some(b"test".to_vec())
        );
        store.set_custom_value(b"test2", b"test3").await.unwrap();
        assert_eq!(
            store.get_custom_value(b"test2").await.unwrap(),
            Some(b"test3".to_vec())
        );
        assert_eq!(
            store.get_custom_value(b"test").await.unwrap(),
            Some(b"test".to_vec())
        );
        store.set_custom_value(b"test", b"test4").await.unwrap();
        assert_eq!(
            store.get_custom_value(b"test").await.unwrap(),
            Some(b"test4".to_vec())
        );
        assert_eq!(
            store.get_custom_value(b"test2").await.unwrap(),
            Some(b"test3".to_vec())
        );
    }

    #[cfg(feature = "sqlite")]
    #[tokio::test]
    async fn test_sqlite_filters() {
        let store = open_sqlite_database().await.unwrap();
        assert_eq!(store.get_filter("test").await.unwrap(), None);
        store.save_filter("test", "test").await.unwrap();
        assert_eq!(
            store.get_filter("test").await.unwrap(),
            Some("test".to_owned())
        );
        store.save_filter("test2", "test3").await.unwrap();
        assert_eq!(
            store.get_filter("test2").await.unwrap(),
            Some("test3".to_owned())
        );
        assert_eq!(
            store.get_filter("test").await.unwrap(),
            Some("test".to_owned())
        );
        store.save_filter("test", "test4").await.unwrap();
        assert_eq!(
            store.get_filter("test").await.unwrap(),
            Some("test4".to_owned())
        );
        assert_eq!(
            store.get_filter("test2").await.unwrap(),
            Some("test3".to_owned())
        );
    }

    #[cfg(feature = "postgres")]
    #[tokio::test]
    #[cfg_attr(not(feature = "ci"), ignore)]
    async fn test_postgres_filters() {
        let store = open_postgres_database().await.unwrap();
        assert_eq!(store.get_filter("test").await.unwrap(), None);
        store.save_filter("test", "test").await.unwrap();
        assert_eq!(
            store.get_filter("test").await.unwrap(),
            Some("test".to_owned())
        );
        store.save_filter("test2", "test3").await.unwrap();
        assert_eq!(
            store.get_filter("test2").await.unwrap(),
            Some("test3".to_owned())
        );
        assert_eq!(
            store.get_filter("test").await.unwrap(),
            Some("test".to_owned())
        );
        store.save_filter("test", "test4").await.unwrap();
        assert_eq!(
            store.get_filter("test").await.unwrap(),
            Some("test4".to_owned())
        );
        assert_eq!(
            store.get_filter("test2").await.unwrap(),
            Some("test3".to_owned())
        );
    }

    #[cfg(feature = "sqlite")]
    #[tokio::test]
    async fn test_sqlite_mediastore() {
        let store = open_sqlite_database().await.unwrap();
        let entry_0 = <&MxcUri>::from("mxc://localhost:8080/media/0");
        let entry_1 = <&MxcUri>::from("mxc://localhost:8080/media/1");

        store.insert_media(entry_0, b"media_0").await.unwrap();
        store.insert_media(entry_1, b"media_1").await.unwrap();

        for entry in 2..101 {
            let entry = OwnedMxcUri::from(format!("mxc://localhost:8080/media/{}", entry));
            store.insert_media(&entry, b"media_0").await.unwrap();
        }

        assert_eq!(store.get_media(entry_0).await.unwrap(), None);
        assert_eq!(
            store.get_media(entry_1).await.unwrap(),
            Some(b"media_1".to_vec())
        );
    }

    #[cfg(feature = "postgres")]
    #[tokio::test]
    #[cfg_attr(not(feature = "ci"), ignore)]
    async fn test_postgres_mediastore() {
        let store = open_postgres_database().await.unwrap();
        let entry_0 = <&MxcUri>::from("mxc://localhost:8080/media/0");
        let entry_1 = <&MxcUri>::from("mxc://localhost:8080/media/1");

        store.insert_media(entry_0, b"media_0").await.unwrap();
        store.insert_media(entry_1, b"media_1").await.unwrap();

        for entry in 2..101 {
            let entry = OwnedMxcUri::from(format!("mxc://localhost:8080/media/{}", entry));
            store.insert_media(&entry, b"media_0").await.unwrap();
        }

        assert_eq!(store.get_media(entry_0).await.unwrap(), None);
        assert_eq!(
            store.get_media(entry_1).await.unwrap(),
            Some(b"media_1".to_vec())
        );
    }

    #[cfg(feature = "sqlite")]
    #[tokio::test]
    async fn test_sqlite_sync_token() {
        let store = open_sqlite_database().await.unwrap();
        assert_eq!(store.get_sync_token().await.unwrap(), None);
        store.save_sync_token_test("test").await.unwrap();
        assert_eq!(
            store.get_sync_token().await.unwrap(),
            Some("test".to_owned())
        );
    }

    #[cfg(feature = "postgres")]
    #[tokio::test]
    #[cfg_attr(not(feature = "ci"), ignore)]
    async fn test_postgres_sync_token() {
        let store = open_postgres_database().await.unwrap();
        assert_eq!(store.get_sync_token().await.unwrap(), None);
        store.save_sync_token_test("test").await.unwrap();
        assert_eq!(
            store.get_sync_token().await.unwrap(),
            Some("test".to_owned())
        );
    }

    #[cfg(feature = "sqlite")]
    #[tokio::test]
    async fn test_sqlite_kv_store() {
        let store = open_sqlite_database().await.unwrap();
        store.insert_kv(b"key", b"value").await.unwrap();
        let value = store.get_kv(b"key").await.unwrap();
        assert_eq!(value, Some(b"value".to_vec()));
        store.insert_kv(b"key", b"value2").await.unwrap();
        let value = store.get_kv(b"key").await.unwrap();
        assert_eq!(value, Some(b"value2".to_vec()));
    }

    #[cfg(feature = "postgres")]
    #[tokio::test]
    #[cfg_attr(not(feature = "ci"), ignore)]
    async fn test_postgres_kv_store() {
        let store = open_postgres_database().await.unwrap();
        store.insert_kv(b"key", b"value").await.unwrap();
        let value = store.get_kv(b"key").await.unwrap();
        assert_eq!(value, Some(b"value".to_vec()));
        store.insert_kv(b"key", b"value2").await.unwrap();
        let value = store.get_kv(b"key").await.unwrap();
        assert_eq!(value, Some(b"value2".to_vec()));
    }
}

#[allow(clippy::redundant_pub_crate)]
#[cfg(all(test, feature = "postgres", feature = "ci"))]
mod postgres_integration_test {
    use std::sync::Arc;

    use matrix_sdk_base::{statestore_integration_tests, StateStore, StoreError};
    use rand::distributions::{Alphanumeric, DistString};
    use sqlx::migrate::MigrateDatabase;

    use super::StoreResult;
    async fn get_store_anyhow() -> anyhow::Result<impl StateStore> {
        let name = Alphanumeric.sample_string(&mut rand::thread_rng(), 16);
        let db_url = format!("postgres://postgres:postgres@localhost:5432/{}", name);
        if !sqlx::Postgres::database_exists(&db_url).await? {
            sqlx::Postgres::create_database(&db_url).await?;
        }
        let db = Arc::new(sqlx::PgPool::connect(&db_url).await?);
        let store = crate::StateStore::new(&db).await?;
        Ok(store)
    }
    async fn get_store() -> StoreResult<impl StateStore> {
        get_store_anyhow()
            .await
            .map_err(|e| StoreError::Backend(e.into()))
    }

    statestore_integration_tests! { integration }
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
