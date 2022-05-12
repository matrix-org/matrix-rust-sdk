//! Room database code
use std::{collections::BTreeSet, convert::TryInto};

use anyhow::Result;
use futures::TryStreamExt;
use matrix_sdk_base::{deserialized_responses::MemberEvent, MinimalRoomMemberEvent, RoomInfo};
use ruma::{
    events::{
        presence::PresenceEvent,
        receipt::Receipt,
        room::member::{MembershipState, StrippedRoomMemberEvent, SyncRoomMemberEvent},
        AnyGlobalAccountDataEvent, AnyRoomAccountDataEvent, AnyStrippedStateEvent,
        AnySyncStateEvent, GlobalAccountDataEventType, RoomAccountDataEventType, StateEventType,
    },
    receipt::ReceiptType,
    serde::Raw,
    EventId, OwnedEventId, OwnedUserId, RoomId, UserId,
};
use sqlx::{
    database::HasArguments, types::Json, ColumnIndex, Database, Executor, IntoArguments, Row,
    Transaction,
};

use crate::{helpers::SqlType, StateStore, SupportedDatabase};

impl<DB: SupportedDatabase> StateStore<DB> {
    /// Deletes a room from the room store
    ///
    /// # Errors
    /// This function will return an error if the the query fails
    pub async fn remove_room(&self, room_id: &RoomId) -> Result<()>
    where
        for<'a> <DB as HasArguments<'a>>::Arguments: IntoArguments<'a, DB>,
        for<'c> &'c mut <DB as sqlx::Database>::Connection: Executor<'c, Database = DB>,
        String: SqlType<DB>,
    {
        DB::room_remove_query()
            .bind(room_id.to_string())
            .execute(&*self.db)
            .await?;
        Ok(())
    }

    /// Sets global account data for an account data event
    ///
    /// # Errors
    /// This function will return an error if the the query fails
    pub async fn set_global_account_data<'c>(
        txn: &mut Transaction<'c, DB>,
        event_type: &GlobalAccountDataEventType,
        event_data: Raw<AnyGlobalAccountDataEvent>,
    ) -> Result<()>
    where
        for<'a> <DB as HasArguments<'a>>::Arguments: IntoArguments<'a, DB>,
        for<'a> &'a mut Transaction<'c, DB>: Executor<'a, Database = DB>,
        String: SqlType<DB>,
        Json<Raw<AnyGlobalAccountDataEvent>>: SqlType<DB>,
    {
        DB::account_data_upsert_query()
            .bind("".to_owned())
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
    pub async fn get_account_data_event(
        &self,
        event_type: GlobalAccountDataEventType,
    ) -> Result<Option<Raw<AnyGlobalAccountDataEvent>>>
    where
        for<'a> <DB as HasArguments<'a>>::Arguments: IntoArguments<'a, DB>,
        for<'c> &'c mut <DB as sqlx::Database>::Connection: Executor<'c, Database = DB>,
        String: SqlType<DB>,
        Json<Raw<AnyGlobalAccountDataEvent>>: SqlType<DB>,
        for<'a> &'a str: ColumnIndex<<DB as Database>::Row>,
    {
        let row = DB::account_data_load_query()
            .bind("".to_owned())
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
    pub async fn get_room_account_data_event(
        &self,
        room_id: &RoomId,
        event_type: RoomAccountDataEventType,
    ) -> Result<Option<Raw<AnyRoomAccountDataEvent>>>
    where
        for<'a> <DB as HasArguments<'a>>::Arguments: IntoArguments<'a, DB>,
        for<'c> &'c mut <DB as sqlx::Database>::Connection: Executor<'c, Database = DB>,
        String: SqlType<DB>,
        Json<Raw<AnyRoomAccountDataEvent>>: SqlType<DB>,
        for<'a> &'a str: ColumnIndex<<DB as Database>::Row>,
    {
        let row = DB::account_data_load_query()
            .bind(room_id.to_string())
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
    pub async fn set_presence_event<'c>(
        txn: &mut Transaction<'c, DB>,
        user_id: &UserId,
        presence: Raw<PresenceEvent>,
    ) -> Result<()>
    where
        for<'a> <DB as HasArguments<'a>>::Arguments: IntoArguments<'a, DB>,
        for<'a> &'a mut Transaction<'c, DB>: Executor<'a, Database = DB>,
        String: SqlType<DB>,
        Json<Raw<PresenceEvent>>: SqlType<DB>,
    {
        DB::presence_upsert_query()
            .bind(user_id.to_string())
            .bind(Json(presence))
            .execute(txn)
            .await?;
        Ok(())
    }

    /// Gets presence for a user
    ///
    /// # Errors
    /// This function will return an error if the the query fails
    pub async fn get_presence_event(&self, user_id: &UserId) -> Result<Option<Raw<PresenceEvent>>>
    where
        for<'a> <DB as HasArguments<'a>>::Arguments: IntoArguments<'a, DB>,
        for<'c> &'c mut <DB as sqlx::Database>::Connection: Executor<'c, Database = DB>,
        String: SqlType<DB>,
        Json<Raw<PresenceEvent>>: SqlType<DB>,
        for<'a> &'a str: ColumnIndex<<DB as Database>::Row>,
    {
        let row = DB::presence_load_query()
            .bind(user_id.to_string())
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
    ) -> Result<()>
    where
        for<'a> <DB as HasArguments<'a>>::Arguments: IntoArguments<'a, DB>,
        for<'a> &'a mut Transaction<'c, DB>: Executor<'a, Database = DB>,
        String: SqlType<DB>,
    {
        DB::member_remove_query()
            .bind(room_id.to_string())
            .bind(user_id.to_string())
            .execute(txn)
            .await?;
        Ok(())
    }

    /// Stores room membership info for a user
    ///
    /// # Errors
    /// This function will return an error if the the query fails
    pub async fn set_room_membership<'c>(
        txn: &mut Transaction<'c, DB>,
        room_id: &RoomId,
        user_id: &UserId,
        member_event: SyncRoomMemberEvent,
    ) -> Result<()>
    where
        for<'a> <DB as HasArguments<'a>>::Arguments: IntoArguments<'a, DB>,
        for<'a> &'a mut Transaction<'c, DB>: Executor<'a, Database = DB>,
        String: SqlType<DB>,
        Json<SyncRoomMemberEvent>: SqlType<DB>,
        bool: SqlType<DB>,
        Option<String>: SqlType<DB>,
    {
        let displayname = member_event
            .as_original()
            .and_then(|v| v.content.displayname.clone());
        let joined = match member_event.as_original().map(|v| &v.content.membership) {
            Some(MembershipState::Join) => true,
            Some(MembershipState::Invite) => false,
            _ => return Self::remove_member(txn, room_id, user_id).await,
        };
        DB::member_upsert_query()
            .bind(room_id.to_string())
            .bind(user_id.to_string())
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
    pub async fn set_stripped_room_membership<'c>(
        txn: &mut Transaction<'c, DB>,
        room_id: &RoomId,
        user_id: &UserId,
        member_event: StrippedRoomMemberEvent,
    ) -> Result<()>
    where
        for<'a> <DB as HasArguments<'a>>::Arguments: IntoArguments<'a, DB>,
        for<'a> &'a mut Transaction<'c, DB>: Executor<'a, Database = DB>,
        String: SqlType<DB>,
        Json<StrippedRoomMemberEvent>: SqlType<DB>,
        bool: SqlType<DB>,
        Option<String>: SqlType<DB>,
    {
        let displayname = member_event.content.displayname.clone();
        let joined = match member_event.content.membership {
            MembershipState::Join => true,
            MembershipState::Invite => false,
            _ => return Self::remove_member(txn, room_id, user_id).await,
        };
        DB::member_upsert_query()
            .bind(room_id.to_string())
            .bind(user_id.to_string())
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
    pub async fn set_room_profile<'c>(
        txn: &mut Transaction<'c, DB>,
        room_id: &RoomId,
        user_id: &UserId,
        profile: MinimalRoomMemberEvent,
    ) -> Result<()>
    where
        for<'a> <DB as HasArguments<'a>>::Arguments: IntoArguments<'a, DB>,
        for<'a> &'a mut Transaction<'c, DB>: Executor<'a, Database = DB>,
        String: SqlType<DB>,
        Json<MinimalRoomMemberEvent>: SqlType<DB>,
    {
        DB::member_profile_upsert_query()
            .bind(room_id.to_string())
            .bind(user_id.to_string())
            .bind(Json(profile))
            .execute(txn)
            .await?;
        Ok(())
    }

    /// Stores a state event for a room
    ///
    /// # Errors
    /// This function will return an error if the the query fails
    pub async fn set_room_state<'c>(
        txn: &mut Transaction<'c, DB>,
        room_id: &RoomId,
        event_type: &StateEventType,
        state_key: &str,
        state: Raw<AnySyncStateEvent>,
    ) -> Result<()>
    where
        for<'a> <DB as HasArguments<'a>>::Arguments: IntoArguments<'a, DB>,
        for<'a> &'a mut Transaction<'c, DB>: Executor<'a, Database = DB>,
        String: SqlType<DB>,
        Json<Raw<AnySyncStateEvent>>: SqlType<DB>,
        bool: SqlType<DB>,
    {
        DB::state_upsert_query()
            .bind(room_id.to_string())
            .bind(event_type.to_string())
            .bind(state_key.to_owned())
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
    pub async fn set_stripped_room_state<'c>(
        txn: &mut Transaction<'c, DB>,
        room_id: &RoomId,
        event_type: &StateEventType,
        state_key: &str,
        state: Raw<AnyStrippedStateEvent>,
    ) -> Result<()>
    where
        for<'a> <DB as HasArguments<'a>>::Arguments: IntoArguments<'a, DB>,
        for<'a> &'a mut Transaction<'c, DB>: Executor<'a, Database = DB>,
        String: SqlType<DB>,
        Json<Raw<AnyStrippedStateEvent>>: SqlType<DB>,
        bool: SqlType<DB>,
    {
        DB::state_upsert_query()
            .bind(room_id.to_string())
            .bind(event_type.to_string())
            .bind(state_key.to_owned())
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
    pub async fn set_room_account_data<'c>(
        txn: &mut Transaction<'c, DB>,
        room_id: &RoomId,
        event_type: &RoomAccountDataEventType,
        event_data: Raw<AnyRoomAccountDataEvent>,
    ) -> Result<()>
    where
        for<'a> <DB as HasArguments<'a>>::Arguments: IntoArguments<'a, DB>,
        for<'a> &'a mut Transaction<'c, DB>: Executor<'a, Database = DB>,
        String: SqlType<DB>,
        Json<Raw<AnyRoomAccountDataEvent>>: SqlType<DB>,
        bool: SqlType<DB>,
    {
        DB::account_data_upsert_query()
            .bind(room_id.to_string())
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
    pub async fn set_room_info<'c>(
        txn: &mut Transaction<'c, DB>,
        room_id: &RoomId,
        room_info: RoomInfo,
    ) -> Result<()>
    where
        for<'a> <DB as HasArguments<'a>>::Arguments: IntoArguments<'a, DB>,
        for<'a> &'a mut Transaction<'c, DB>: Executor<'a, Database = DB>,
        String: SqlType<DB>,
        Json<RoomInfo>: SqlType<DB>,
        bool: SqlType<DB>,
    {
        DB::room_upsert_query()
            .bind(room_id.to_string())
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
    pub async fn set_stripped_room_info<'c>(
        txn: &mut Transaction<'c, DB>,
        room_id: &RoomId,
        room_info: RoomInfo,
    ) -> Result<()>
    where
        for<'a> <DB as HasArguments<'a>>::Arguments: IntoArguments<'a, DB>,
        for<'a> &'a mut Transaction<'c, DB>: Executor<'a, Database = DB>,
        String: SqlType<DB>,
        Json<RoomInfo>: SqlType<DB>,
        bool: SqlType<DB>,
    {
        DB::room_upsert_query()
            .bind(room_id.to_string())
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
    pub async fn set_receipt<'c>(
        txn: &mut Transaction<'c, DB>,
        room_id: &RoomId,
        event_id: &EventId,
        receipt_type: &ReceiptType,
        user_id: &UserId,
        receipt: Receipt,
    ) -> Result<()>
    where
        for<'a> <DB as HasArguments<'a>>::Arguments: IntoArguments<'a, DB>,
        for<'a> &'a mut Transaction<'c, DB>: Executor<'a, Database = DB>,
        String: SqlType<DB>,
        Json<Receipt>: SqlType<DB>,
    {
        DB::receipt_upsert_query()
            .bind(room_id.to_string())
            .bind(event_id.to_string())
            .bind(receipt_type.to_string())
            .bind(user_id.to_string())
            .bind(Json(receipt))
            .execute(txn)
            .await?;
        Ok(())
    }

    /// Retrieves a state event in room by event type and state key
    ///
    /// # Errors
    /// This function will return an error if the the query fails
    pub async fn get_state_event(
        &self,
        room_id: &RoomId,
        event_type: StateEventType,
        state_key: &str,
    ) -> Result<Option<Raw<AnySyncStateEvent>>>
    where
        for<'a> <DB as HasArguments<'a>>::Arguments: IntoArguments<'a, DB>,
        for<'c> &'c mut <DB as sqlx::Database>::Connection: Executor<'c, Database = DB>,
        String: SqlType<DB>,
        Json<Raw<AnySyncStateEvent>>: SqlType<DB>,
        for<'a> &'a str: ColumnIndex<<DB as Database>::Row>,
    {
        let row = DB::state_load_query()
            .bind(room_id.to_string())
            .bind(event_type.to_string())
            .bind(state_key.to_owned())
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
    pub async fn get_state_events(
        &self,
        room_id: &RoomId,
        event_type: StateEventType,
    ) -> Result<Vec<Raw<AnySyncStateEvent>>>
    where
        for<'a> <DB as HasArguments<'a>>::Arguments: IntoArguments<'a, DB>,
        for<'c> &'c mut <DB as sqlx::Database>::Connection: Executor<'c, Database = DB>,
        String: SqlType<DB>,
        Json<Raw<AnySyncStateEvent>>: SqlType<DB>,
        for<'a> &'a str: ColumnIndex<<DB as Database>::Row>,
    {
        let mut rows = DB::states_load_query()
            .bind(room_id.to_string())
            .bind(event_type.to_string())
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
    pub async fn get_profile(
        &self,
        room_id: &RoomId,
        user_id: &UserId,
    ) -> Result<Option<MinimalRoomMemberEvent>>
    where
        for<'a> <DB as HasArguments<'a>>::Arguments: IntoArguments<'a, DB>,
        for<'c> &'c mut <DB as sqlx::Database>::Connection: Executor<'c, Database = DB>,
        String: SqlType<DB>,
        Json<MinimalRoomMemberEvent>: SqlType<DB>,
        for<'a> &'a str: ColumnIndex<<DB as Database>::Row>,
    {
        let row = DB::profile_load_query()
            .bind(room_id.to_string())
            .bind(user_id.to_string())
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
    pub async fn get_user_ids(&self, room_id: &RoomId) -> Result<Vec<OwnedUserId>>
    where
        for<'a> <DB as HasArguments<'a>>::Arguments: IntoArguments<'a, DB>,
        for<'c> &'c mut <DB as sqlx::Database>::Connection: Executor<'c, Database = DB>,
        String: SqlType<DB>,
        for<'a> &'a str: ColumnIndex<<DB as Database>::Row>,
    {
        let mut rows = DB::members_load_query()
            .bind(room_id.to_string())
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
    pub async fn get_invited_user_ids(&self, room_id: &RoomId) -> Result<Vec<OwnedUserId>>
    where
        for<'a> <DB as HasArguments<'a>>::Arguments: IntoArguments<'a, DB>,
        for<'c> &'c mut <DB as sqlx::Database>::Connection: Executor<'c, Database = DB>,
        String: SqlType<DB>,
        bool: SqlType<DB>,
        for<'a> &'a str: ColumnIndex<<DB as Database>::Row>,
    {
        let mut rows = DB::members_load_query_with_join_status()
            .bind(room_id.to_string())
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
    pub async fn get_joined_user_ids(&self, room_id: &RoomId) -> Result<Vec<OwnedUserId>>
    where
        for<'a> <DB as HasArguments<'a>>::Arguments: IntoArguments<'a, DB>,
        for<'c> &'c mut <DB as sqlx::Database>::Connection: Executor<'c, Database = DB>,
        String: SqlType<DB>,
        bool: SqlType<DB>,
        for<'a> &'a str: ColumnIndex<<DB as Database>::Row>,
    {
        let mut rows = DB::members_load_query_with_join_status()
            .bind(room_id.to_string())
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
    pub async fn get_member_event(
        &self,
        room_id: &RoomId,
        user_id: &UserId,
    ) -> Result<Option<MemberEvent>>
    where
        for<'a> <DB as HasArguments<'a>>::Arguments: IntoArguments<'a, DB>,
        for<'c> &'c mut <DB as sqlx::Database>::Connection: Executor<'c, Database = DB>,
        String: SqlType<DB>,
        bool: SqlType<DB>,
        Json<StrippedRoomMemberEvent>: SqlType<DB>,
        Json<SyncRoomMemberEvent>: SqlType<DB>,
        for<'a> &'a str: ColumnIndex<<DB as Database>::Row>,
    {
        let row = DB::member_load_query()
            .bind(room_id.to_string())
            .bind(user_id.to_string())
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
    async fn get_room_infos_internal(&self, partial: bool) -> Result<Vec<RoomInfo>>
    where
        for<'a> <DB as HasArguments<'a>>::Arguments: IntoArguments<'a, DB>,
        for<'c> &'c mut <DB as sqlx::Database>::Connection: Executor<'c, Database = DB>,
        Json<RoomInfo>: SqlType<DB>,
        bool: SqlType<DB>,
        for<'a> &'a str: ColumnIndex<<DB as Database>::Row>,
    {
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
    pub async fn get_room_infos(&self) -> Result<Vec<RoomInfo>>
    where
        for<'a> <DB as HasArguments<'a>>::Arguments: IntoArguments<'a, DB>,
        for<'c> &'c mut <DB as sqlx::Database>::Connection: Executor<'c, Database = DB>,
        Json<RoomInfo>: SqlType<DB>,
        bool: SqlType<DB>,
        for<'a> &'a str: ColumnIndex<<DB as Database>::Row>,
    {
        self.get_room_infos_internal(false).await
    }
    /// Get partial room infos
    ///
    /// # Errors
    /// This function will return an error if the the query fails
    pub async fn get_stripped_room_infos(&self) -> Result<Vec<RoomInfo>>
    where
        for<'a> <DB as HasArguments<'a>>::Arguments: IntoArguments<'a, DB>,
        for<'c> &'c mut <DB as sqlx::Database>::Connection: Executor<'c, Database = DB>,
        Json<RoomInfo>: SqlType<DB>,
        bool: SqlType<DB>,
        for<'a> &'a str: ColumnIndex<<DB as Database>::Row>,
    {
        self.get_room_infos_internal(true).await
    }

    /// Get users with display names in room
    ///
    /// # Errors
    /// This function will return an error if the the query fails
    pub async fn get_users_with_display_name(
        &self,
        room_id: &RoomId,
        display_name: &str,
    ) -> Result<BTreeSet<OwnedUserId>>
    where
        for<'a> <DB as HasArguments<'a>>::Arguments: IntoArguments<'a, DB>,
        for<'c> &'c mut <DB as sqlx::Database>::Connection: Executor<'c, Database = DB>,
        String: SqlType<DB>,
        for<'a> &'a str: ColumnIndex<<DB as Database>::Row>,
    {
        let mut rows = DB::users_with_display_name_load_query()
            .bind(room_id.to_string())
            .bind(display_name.to_owned())
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
    pub async fn get_user_room_receipt_event(
        &self,
        room_id: &RoomId,
        receipt_type: ReceiptType,
        user_id: &UserId,
    ) -> Result<Option<(OwnedEventId, Receipt)>>
    where
        for<'a> <DB as HasArguments<'a>>::Arguments: IntoArguments<'a, DB>,
        for<'c> &'c mut <DB as sqlx::Database>::Connection: Executor<'c, Database = DB>,
        String: SqlType<DB>,
        Json<Receipt>: SqlType<DB>,
        for<'a> &'a str: ColumnIndex<<DB as Database>::Row>,
    {
        let row = DB::receipt_load_query()
            .bind(room_id.to_string())
            .bind(receipt_type.to_string())
            .bind(user_id.to_string())
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
    pub async fn get_event_room_receipt_events(
        &self,
        room_id: &RoomId,
        receipt_type: ReceiptType,
        event_id: &EventId,
    ) -> Result<Vec<(OwnedUserId, Receipt)>>
    where
        for<'a> <DB as HasArguments<'a>>::Arguments: IntoArguments<'a, DB>,
        for<'c> &'c mut <DB as sqlx::Database>::Connection: Executor<'c, Database = DB>,
        String: SqlType<DB>,
        Json<Receipt>: SqlType<DB>,
        for<'a> &'a str: ColumnIndex<<DB as Database>::Row>,
    {
        let mut rows = DB::event_receipt_load_query()
            .bind(room_id.to_string())
            .bind(receipt_type.to_string())
            .bind(event_id.to_string())
            .fetch(&*self.db);
        let mut result = Vec::new();
        while let Some(row) = rows.try_next().await? {
            let user_id = row.try_get::<'_, String, _>("user_id")?.try_into()?;
            let receipt = row.try_get::<'_, Json<Receipt>, _>("receipt")?.0;
            result.push((user_id, receipt));
        }
        Ok(result)
    }
}
