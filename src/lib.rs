//! SQL State Storage for matrix-sdk
//!
//! ## Usage
//!
//! ```rust,ignore
//!
//! let sql_pool: Arc<sqlx::Pool<DB>> = /* ... */;
//! // Create the state store, applying migrations if necessary
//! let state_store = StateStore::new(&sql_pool).await?;
//!
//! ```
//!
//! After that you can pass it into your client builder as follows:
//!
//! ```rust,ignore
//! let store_config = StoreConfig::new().state_store(Box::new(state_store));
//!
//! let client_builder = Client::builder()
//!                     /* ... */
//!                      .store_config(store_config)
//! ```
//!
//! ## About Trait bounds
//!
//! The list of trait bounds may seem daunting, however every implementation of [`SupportedDatabase`] matches the trait bounds specified.

use std::sync::Arc;

use anyhow::Result;

#[cfg(feature = "e2e-encryption")]
use cryptostore::CryptostoreData;
#[cfg(feature = "e2e-encryption")]
use helpers::{BorrowedSqlType, SqlType};
#[cfg(feature = "e2e-encryption")]
use matrix_sdk_store_encryption::StoreCipher;

pub mod helpers;
pub use helpers::SupportedDatabase;
#[cfg(feature = "e2e-encryption")]
use matrix_sdk_base::{deserialized_responses::MemberEvent, MinimalRoomMemberEvent, RoomInfo};
#[cfg(feature = "e2e-encryption")]
use ruma::{
    events::{
        presence::PresenceEvent,
        receipt::Receipt,
        room::member::{StrippedRoomMemberEvent, SyncRoomMemberEvent},
        AnyGlobalAccountDataEvent, AnyRoomAccountDataEvent, AnyStrippedStateEvent,
        AnySyncStateEvent,
    },
    serde::Raw,
};
#[cfg(feature = "e2e-encryption")]
use sqlx::{
    database::HasArguments, types::Json, ColumnIndex, Executor, IntoArguments, Transaction,
};
use sqlx::{migrate::Migrate, Database, Pool};

#[cfg(feature = "e2e-encryption")]
mod cryptostore;
mod statestore;

/// SQL State Storage for matrix-sdk
#[allow(single_use_lifetimes)]
#[derive(Debug)]
pub struct StateStore<DB: SupportedDatabase> {
    /// The database connection
    db: Arc<Pool<DB>>,
    #[cfg(feature = "e2e-encryption")]
    /// Extra cryptostore data
    cryptostore: Option<CryptostoreData>,
}

#[allow(single_use_lifetimes)]
impl<DB: SupportedDatabase> StateStore<DB> {
    /// Create a new State Store and automtaically performs migrations
    ///
    /// # Errors
    /// This function will return an error if the migration cannot be applied
    pub async fn new(db: &Arc<Pool<DB>>) -> Result<Self>
    where
        <DB as Database>::Connection: Migrate,
    {
        let db = Arc::clone(db);
        let migrator = DB::get_migrator();
        migrator.run(&*db).await?;
        #[cfg(not(feature = "e2e-encryption"))]
        {
            Ok(Self { db })
        }
        #[cfg(feature = "e2e-encryption")]
        {
            Ok(Self {
                db,
                cryptostore: None,
            })
        }
    }

    /// Returns a reference to the cryptostore specific data if the store has been unlocked
    ///
    /// # Errors
    /// This function will return an error if the database has not been unlocked
    #[cfg(feature = "e2e-encryption")]
    pub(crate) fn ensure_e2e(&self) -> Result<&CryptostoreData> {
        self.cryptostore
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Not unlocked"))
    }

    /// Unlocks the e2e encryption database
    /// # Errors
    /// This function will fail if the database could not be unlocked
    #[cfg(feature = "e2e-encryption")]
    pub async fn unlock(&mut self) -> Result<()>
    where
        for<'a> <DB as HasArguments<'a>>::Arguments: IntoArguments<'a, DB>,
        for<'c> &'c mut <DB as sqlx::Database>::Connection: Executor<'c, Database = DB>,
        for<'c, 'a> &'a mut Transaction<'c, DB>: Executor<'a, Database = DB>,
        for<'a> &'a [u8]: BorrowedSqlType<'a, DB>,
        for<'a> &'a str: BorrowedSqlType<'a, DB>,
        Vec<u8>: SqlType<DB>,
        String: SqlType<DB>,
        bool: SqlType<DB>,
        Vec<u8>: SqlType<DB>,
        Option<String>: SqlType<DB>,
        Json<Raw<AnyGlobalAccountDataEvent>>: SqlType<DB>,
        Json<Raw<PresenceEvent>>: SqlType<DB>,
        Json<SyncRoomMemberEvent>: SqlType<DB>,
        Json<MinimalRoomMemberEvent>: SqlType<DB>,
        Json<Raw<AnySyncStateEvent>>: SqlType<DB>,
        Json<Raw<AnyRoomAccountDataEvent>>: SqlType<DB>,
        Json<RoomInfo>: SqlType<DB>,
        Json<Receipt>: SqlType<DB>,
        Json<Raw<AnyStrippedStateEvent>>: SqlType<DB>,
        Json<StrippedRoomMemberEvent>: SqlType<DB>,
        Json<MemberEvent>: SqlType<DB>,
        for<'a> &'a str: ColumnIndex<<DB as Database>::Row>,
    {
        self.cryptostore = Some(CryptostoreData::new_unencrypted());
        self.load_tracked_users().await?;
        Ok(())
    }

    /// Unlocks the e2e encryption database with password
    /// # Errors
    /// This function will fail if the passphrase is wrong
    #[cfg(feature = "e2e-encryption")]
    pub async fn unlock_with_passphrase(&mut self, passphrase: &str) -> Result<()>
    where
        for<'a> <DB as HasArguments<'a>>::Arguments: IntoArguments<'a, DB>,
        for<'c> &'c mut <DB as sqlx::Database>::Connection: Executor<'c, Database = DB>,
        for<'c, 'a> &'a mut Transaction<'c, DB>: Executor<'a, Database = DB>,
        for<'a> &'a [u8]: BorrowedSqlType<'a, DB>,
        for<'a> &'a str: BorrowedSqlType<'a, DB>,
        Vec<u8>: SqlType<DB>,
        String: SqlType<DB>,
        bool: SqlType<DB>,
        Vec<u8>: SqlType<DB>,
        Option<String>: SqlType<DB>,
        Json<Raw<AnyGlobalAccountDataEvent>>: SqlType<DB>,
        Json<Raw<PresenceEvent>>: SqlType<DB>,
        Json<SyncRoomMemberEvent>: SqlType<DB>,
        Json<MinimalRoomMemberEvent>: SqlType<DB>,
        Json<Raw<AnySyncStateEvent>>: SqlType<DB>,
        Json<Raw<AnyRoomAccountDataEvent>>: SqlType<DB>,
        Json<RoomInfo>: SqlType<DB>,
        Json<Receipt>: SqlType<DB>,
        Json<Raw<AnyStrippedStateEvent>>: SqlType<DB>,
        Json<StrippedRoomMemberEvent>: SqlType<DB>,
        Json<MemberEvent>: SqlType<DB>,
        for<'a> &'a str: ColumnIndex<<DB as Database>::Row>,
    {
        // Try to read the store cipher

        let cipher_export = self.get_kv(b"cipher").await?;
        if let Some(cipher) = cipher_export {
            self.cryptostore = Some(CryptostoreData::new(StoreCipher::import(
                passphrase, &cipher,
            )?));
        } else {
            // Store the cipher in the database
            let cipher = StoreCipher::new()?;
            self.insert_kv(b"cipher", &cipher.export(passphrase)?)
                .await?;
            self.cryptostore = Some(CryptostoreData::new(cipher));
        }
        self.load_tracked_users().await?;
        Ok(())
    }
}
