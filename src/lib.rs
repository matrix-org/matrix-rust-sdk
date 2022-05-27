//! SQL State Storage for matrix-sdk
//!
//! ## Usage
//!
//! ```rust,ignore
//!
//! let sql_pool: Arc<sqlx::Pool<DB>> = /* ... */;
//! // Create the  store config
//! let store_config = matrix_sdk_sql::store_config(sql_pool, Some(std::env::var("MYAPP_SECRET_KEY")?)).await?;
//!
//! ```
//!
//! After that you can pass it into your client builder as follows:
//!
//! ```rust,ignore
//! let client_builder = Client::builder()
//!                     /* ... */
//!                      .store_config(store_config)
//! ```
//!
//! ### [`CryptoStore`]
//!
//! Enabling the `e2e-encryption` feature enables cryptostore functionality. To protect encryption session information, the contents of the tables are encrypted in the same manner as in `matrix-sdk-sled`.
//!
//! Before you can use cryptostore functionality, you need to unlock the cryptostore:
//!
//! ```rust,ignore
//! let mut state_store = /* as above */;
//!
//! state_store.unlock_with_passphrase(std::env::var("MYAPP_SECRET_KEY")?).await?;
//! ```
//!
//! If you are using the `store_config` function, the store will be automatically unlocked for you.
//!
//! ## About Trait bounds
//!
//! The list of trait bounds may seem daunting, however all enabled database backends are supported.

use std::sync::Arc;

#[cfg(feature = "e2e-encryption")]
use cryptostore::CryptostoreData;
use helpers::{BorrowedSqlType, SqlType};
use matrix_sdk_base::store::StoreConfig;
#[cfg(feature = "e2e-encryption")]
use matrix_sdk_store_encryption::StoreCipher;

mod helpers;
pub use helpers::SupportedDatabase;
use matrix_sdk_base::{deserialized_responses::MemberEvent, MinimalRoomMemberEvent, RoomInfo};
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
use sqlx::{
    database::HasArguments, migrate::Migrate, types::Json, ColumnIndex, Database, Executor,
    IntoArguments, Pool, Transaction,
};
use thiserror::Error;

#[cfg(feature = "e2e-encryption")]
mod cryptostore;
mod statestore;

/// Errors that can occur in the SQL Store
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum SQLStoreError {
    /// Database error
    #[error("Database error: {0}")]
    Database(#[from] sqlx::Error),
    /// Migration failed
    #[error("Migration for database failed: {0}")]
    Migration(#[from] sqlx::migrate::MigrateError),
    /// Database is still locked
    #[cfg(feature = "e2e-encryption")]
    #[error("A cryptostore access ocurred without the database being unlocked")]
    DatabaseLocked,
    /// An UTF-8 string has failed to decode
    #[error("Data failed to decode: UTF-8 error {0}")]
    DecodeUtf8(#[from] std::string::FromUtf8Error),
    #[error("Data failed to decode: ID decoding error {0}")]
    /// An ID failed to decode
    DecodeId(#[from] ruma::IdParseError),
    #[cfg(feature = "e2e-encryption")]
    /// Data failed to encrypt/decrypt
    #[error("Failed to encrypt/decrypt data: {0}")]
    Crypto(#[from] matrix_sdk_store_encryption::Error),
    /// Failed to encode/decode data as bincode
    #[cfg(feature = "e2e-encryption")]
    #[error("Failed to encode/decode data as bincode: {0}")]
    Bincode(#[from] bincode::Error),
    /// Failed to decode a JSON value
    #[cfg(feature = "e2e-encryption")]
    #[error("Failed to encode/decode data as json: {0}")]
    Json(#[from] serde_json::Error),
    /// Failed to pickle data
    #[cfg(feature = "e2e-encryption")]
    #[error("Failed to pickle data: {0}")]
    Pickle(#[from] vodozemac::PickleError),
    /// Failed to verify data
    #[cfg(feature = "e2e-encryption")]
    #[error("Failed to verify data: {0}")]
    Sign(Box<dyn std::error::Error + Send + Sync>),
    /// Account info was not found
    #[cfg(feature = "e2e-encryption")]
    #[error("Account info was not found")]
    MissingAccountInfo,
}

/// Result type returned by SQL Store functions
pub type Result<T, E = SQLStoreError> = std::result::Result<T, E>;

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
            .ok_or(SQLStoreError::DatabaseLocked)
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

/// Creates a new store confiig
///
/// # Errors
/// This function will return an error if the migration cannot be applied,
/// or if the passphrase is incorrect
pub async fn store_config<DB: SupportedDatabase>(
    db: &Arc<Pool<DB>>,
    passphrase: Option<&str>,
) -> Result<StoreConfig>
where
    <DB as Database>::Connection: Migrate,
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
    #[cfg(not(feature = "e2e-encryption"))]
    {
        let _ = passphrase;
        let state_store = StateStore::new(db).await?;
        Ok(StoreConfig::new().state_store(Box::new(state_store)))
    }
    #[cfg(feature = "e2e-encryption")]
    {
        let state_store = StateStore::new(db).await?;
        let mut crypto_store = StateStore::new(db).await?;
        if let Some(passphrase) = passphrase {
            crypto_store.unlock_with_passphrase(passphrase).await?;
        } else {
            crypto_store.unlock().await?;
        }
        Ok(StoreConfig::new()
            .state_store(Box::new(state_store))
            .crypto_store(Box::new(crypto_store)))
    }
}
