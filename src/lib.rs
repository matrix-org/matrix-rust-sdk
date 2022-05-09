//! SQL State Storage for matrix-sdk

use std::sync::Arc;

use anyhow::Result;

pub mod helpers;
pub use helpers::SupportedDatabase;
use sqlx::{migrate::Migrate, Database, Pool};
mod db;

/// SQL State Storage for matrix-sdk
#[derive(Clone, Debug)]
#[allow(single_use_lifetimes)]
pub struct StateStore<DB: SupportedDatabase> {
    /// The database connection
    db: Arc<Pool<DB>>,
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
        Ok(Self { db })
    }
}
