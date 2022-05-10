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

    /// Returns a query for loading from the `statestore_kv` table
    ///
    /// # Arguments
    /// * `$1` - The key to load
    fn kv_load_query() -> Query<'static, Self, <Self as HasArguments<'static>>::Arguments> {
        sqlx::query(
            r#"
                SELECT kv_value FROM statestore_kv WHERE kv_key = $1
            "#,
        )
    }

    /// Returns a query for loading from the `statestore_media` table
    ///
    /// # Arguments
    /// * `$1` - The key to load
    fn media_load_query() -> Query<'static, Self, <Self as HasArguments<'static>>::Arguments> {
        sqlx::query(
            r#"
                UPDATE statestore_media
                SET last_access = NOW()
                WHERE media_url = $1
                RETURNING media_data
            "#,
        )
    }

    /// Returns the first query for storing into the `statestore_media` table
    ///
    /// # Arguments
    /// * `$1` - The key to insert
    /// * `$2` - The value to insert
    fn media_insert_query_1() -> Query<'static, Self, <Self as HasArguments<'static>>::Arguments> {
        sqlx::query(
            r#"
                INSERT INTO statestore_media (media_url, media_data, last_access)
                VALUES ($1, $2, NOW())
                ON CONFLICT (media_url) DO NOTHING
            "#,
        )
    }

    /// Returns the second query for storing into the `statestore_media` table
    fn media_insert_query_2() -> Query<'static, Self, <Self as HasArguments<'static>>::Arguments> {
        sqlx::query(
            r#"
                DELETE FROM statestore_media
                WHERE media_url NOT IN
                    (SELECT media_url FROM statestore_media
                     ORDER BY last_access DESC
                     LIMIT 100)
            "#,
        )
    }

    /// Deletes the media with the mxc URL
    ///
    /// # Arguments
    /// * `$1` - The mxc URL
    fn media_delete_query() -> Query<'static, Self, <Self as HasArguments<'static>>::Arguments> {
        sqlx::query(
            r#"
                DELETE FROM statestore_media
                WHERE media_url = $1
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

// Fucking mysql
#[cfg(feature = "mysql")]
impl SupportedDatabase for sqlx::mysql::MySql {
    fn get_migrator() -> &'static Migrator {
        &sqlx::migrate!("./migrations/mysql")
    }
    fn kv_upsert_query() -> Query<'static, Self, <Self as HasArguments<'static>>::Arguments> {
        sqlx::query(
            r#"
                INSERT INTO statestore_kv (kv_key, kv_value)
                VALUES (?, ?) AS excluded
                ON DUPLICATE KEY UPDATE kv_value = excluded.kv_value
            "#,
        )
    }
    fn kv_load_query() -> Query<'static, Self, <Self as HasArguments<'static>>::Arguments> {
        sqlx::query(
            r#"
                SELECT kv_value FROM statestore_kv WHERE kv_key = ?
            "#,
        )
    }
    fn media_load_query() -> Query<'static, Self, <Self as HasArguments<'static>>::Arguments> {
        sqlx::query(
            r#"
                SET @media_url := ?;
                UPDATE statestore_media
                SET last_access = NOW()
                WHERE media_url = @media_url;
                SELECT media_data FROM statestore_media WHERE media_url = @media_url;
            "#,
        )
    }
    fn media_insert_query_1() -> Query<'static, Self, <Self as HasArguments<'static>>::Arguments> {
        sqlx::query(
            r#"
                INSERT INTO statestore_media (media_url, media_data, last_access)
                VALUES (?, ?, NOW())
                ON DUPLICATE KEY UPDATE media_url = media_url
            "#,
        )
    }
    // Thank you oracle
    fn media_insert_query_2() -> Query<'static, Self, <Self as HasArguments<'static>>::Arguments> {
        sqlx::query(
            r#"
                DELETE FROM statestore_media
                WHERE last_access < DATE_SUB(NOW(), INTERVAL 5 MINUTE)
            "#,
        )
    }
    fn media_delete_query() -> Query<'static, Self, <Self as HasArguments<'static>>::Arguments> {
        sqlx::query(
            r#"
                DELETE FROM statestore_media
                WHERE media_url = ?
            "#,
        )
    }
}

#[cfg(feature = "sqlite")]
impl SupportedDatabase for sqlx::sqlite::Sqlite {
    fn get_migrator() -> &'static Migrator {
        &sqlx::migrate!("./migrations/sqlite")
    }

    fn media_load_query() -> Query<'static, Self, <Self as HasArguments<'static>>::Arguments> {
        sqlx::query(
            r#"
                UPDATE statestore_media
                SET last_access = datetime(CURRENT_TIMESTAMP, 'localtime')
                WHERE media_url = $1
                RETURNING media_data
            "#,
        )
    }

    fn media_insert_query_1() -> Query<'static, Self, <Self as HasArguments<'static>>::Arguments> {
        sqlx::query(
            r#"
                INSERT INTO statestore_media (media_url, media_data, last_access)
                VALUES ($1, $2, datetime(CURRENT_TIMESTAMP, 'localtime'))
                ON CONFLICT (media_url) DO NOTHING
            "#,
        )
    }
}
