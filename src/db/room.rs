//! Room database code
use anyhow::Result;
use ruma::{
    events::{AnyGlobalAccountDataEvent, GlobalAccountDataEventType},
    serde::Raw,
    RoomId,
};
use sqlx::{
    database::HasArguments, types::Json, ColumnIndex, Database, Decode, Encode, Executor,
    IntoArguments, Row, Transaction, Type,
};

use crate::{StateStore, SupportedDatabase};

impl<DB: SupportedDatabase> StateStore<DB> {
    /// Deletes a room from the room store
    ///
    /// # Errors
    /// This function will return an error if the the query fails
    pub async fn remove_room(&self, room_id: &RoomId) -> Result<()>
    where
        for<'a> <DB as HasArguments<'a>>::Arguments: IntoArguments<'a, DB>,
        for<'c> &'c mut <DB as sqlx::Database>::Connection: Executor<'c, Database = DB>,
        for<'q> String: Encode<'q, DB>,
        String: Type<DB>,
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
        for<'q> Option<String>: Encode<'q, DB>,
        for<'q> String: Encode<'q, DB>,
        for<'q> Json<Raw<AnyGlobalAccountDataEvent>>: Encode<'q, DB>,
        Option<String>: Type<DB>,
        String: Type<DB>,
        Json<Raw<AnyGlobalAccountDataEvent>>: Type<DB>,
    {
        DB::account_data_upsert_query()
            .bind(None::<String>)
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
        for<'q> Option<String>: Encode<'q, DB>,
        for<'q> String: Encode<'q, DB>,
        Option<String>: Type<DB>,
        String: Type<DB>,
        Json<Raw<AnyGlobalAccountDataEvent>>: Type<DB>,
        for<'r> Json<Raw<AnyGlobalAccountDataEvent>>: Decode<'r, DB>,
        for<'a> &'a str: ColumnIndex<<DB as Database>::Row>,
    {
        let row = DB::account_data_load_query()
            .bind(None::<String>)
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
}
