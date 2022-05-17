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
use helpers::SqlType;
#[cfg(feature = "e2e-encryption")]
use matrix_sdk_store_encryption::StoreCipher;

pub mod helpers;
pub use helpers::SupportedDatabase;
#[cfg(feature = "e2e-encryption")]
use sqlx::{database::HasArguments, ColumnIndex, Executor, IntoArguments};
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

    /// Returns a reference to the cipher
    ///
    /// # Errors
    /// This function will return an error if the database has not been unlocked
    #[cfg(feature = "e2e-encryption")]
    pub(crate) fn ensure_cipher(&self) -> Result<&StoreCipher> {
        Ok(&self.ensure_e2e()?.cipher)
    }

    /// Returns a refcounted reference to the cipher
    ///
    /// # Errors
    /// This function will return an error if the database has not been unlocked
    #[cfg(feature = "e2e-encryption")]
    pub(crate) fn ensure_cipher_arc(&self) -> Result<Arc<StoreCipher>> {
        Ok(Arc::clone(&self.ensure_e2e()?.cipher))
    }

    /// Unlocks the e2e encryption database
    /// # Errors
    /// This function will fail if the passphrase is not `hunter2`
    #[cfg(feature = "e2e-encryption")]
    pub async fn unlock(&mut self) -> Result<()>
    where
        for<'a> <DB as HasArguments<'a>>::Arguments: IntoArguments<'a, DB>,
        for<'c> &'c mut <DB as sqlx::Database>::Connection: Executor<'c, Database = DB>,
        Vec<u8>: SqlType<DB>,
        for<'a> &'a str: ColumnIndex<<DB as Database>::Row>,
    {
        self.unlock_with_passphrase("hunter2").await
    }

    /// Unlocks the e2e encryption database with password
    /// # Errors
    /// This function will fail if the passphrase is wrong
    #[cfg(feature = "e2e-encryption")]
    pub async fn unlock_with_passphrase(&mut self, passphrase: &str) -> Result<()>
    where
        for<'a> <DB as HasArguments<'a>>::Arguments: IntoArguments<'a, DB>,
        for<'c> &'c mut <DB as sqlx::Database>::Connection: Executor<'c, Database = DB>,
        Vec<u8>: SqlType<DB>,
        for<'a> &'a str: ColumnIndex<<DB as Database>::Row>,
    {
        // Try to read the store cipher
        let cipher_export = self.get_kv(b"cipher".to_vec()).await?;
        if let Some(cipher) = cipher_export {
            self.cryptostore = Some(CryptostoreData::new(StoreCipher::import(
                passphrase, &cipher,
            )?));
        } else {
            // Store the cipher in the database
            let cipher = StoreCipher::new()?;
            self.insert_kv(b"cipher".to_vec(), cipher.export(passphrase)?)
                .await?;
            self.cryptostore = Some(CryptostoreData::new(cipher));
        }
        Ok(())
    }
}
