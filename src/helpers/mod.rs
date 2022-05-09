//! Various helper functionality

use sqlx::{database::HasArguments, migrate::Migrator, query::Query, Database};

use self::private::Sealed;

/// Private module for the [`Sealed`] trait.
mod private {

    /// A trait for “sealing” a trait
    #[allow(unreachable_pub)]
    pub trait Sealed {}

    #[cfg(feature = "postgres")]
    impl Sealed for sqlx::postgres::Postgres {}
    #[cfg(feature = "mysql")]
    impl Sealed for sqlx::mysql::MySql {}
    #[cfg(feature = "sqlite")]
    impl Sealed for sqlx::sqlite::Sqlite {}
}

/// Supported Database trait
///
/// It contains many methods that try to generate queries for the supported databases.
#[allow(single_use_lifetimes)]
pub trait SupportedDatabase: Database + Sealed {
    /// Returns the migrator for the current database type
    fn get_migrator() -> &'static Migrator;

    /// Returns a query for upserting into the `statestore_kv` table
    ///
    /// # Arguments
    /// * `$1` - The key to insert
    /// * `$2` - The value to insert
    fn kv_upsert_query() -> Query<'static, Self, <Self as HasArguments<'static>>::Arguments> {
        sqlx::query(
            r#"
                INSERT INTO statestore_kv (kv_key, kv_value)
                VALUES ($1, $2)
                ON CONFLICT (kv_key) DO UPDATE SET kv_value = $2
            "#,
        )
    }
}

#[cfg(feature = "postgres")]
impl SupportedDatabase for sqlx::postgres::Postgres {
    fn get_migrator() -> &'static Migrator {
        &sqlx::migrate!("./migrations/postgres")
    }
}

#[cfg(feature = "mysql")]
impl SupportedDatabase for sqlx::mysql::MySql {
    fn get_migrator() -> &'static Migrator {
        &sqlx::migrate!("./migrations/mysql")
    }
    fn kv_upsert_query() -> Query<'static, Self, <Self as HasArguments<'static>>::Arguments> {
        sqlx::query(
            r#"
                INSERT INTO statestore_kv (kv_key, kv_value)
                VALUES ($1, $2)
                ON DUPLICATE KEY UPDATE kv_value = $2
            "#,
        )
    }
}

#[cfg(feature = "sqlite")]
impl SupportedDatabase for sqlx::sqlite::Sqlite {
    fn get_migrator() -> &'static Migrator {
        &sqlx::migrate!("./migrations/sqlite")
    }
}
