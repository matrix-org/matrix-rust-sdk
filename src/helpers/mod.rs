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

    /// Deletes a room given its ID
    ///
    /// # Arguments
    /// * `$1` - The room ID
    fn room_remove_query() -> Query<'static, Self, <Self as HasArguments<'static>>::Arguments> {
        sqlx::query(
            r#"
                DELETE FROM statestore_rooms
                WHERE room_id = $1
            "#,
        )
    }

    /// Upserts account data
    ///
    /// # Arguments
    /// * `$1` - The room ID for the account data, or null
    /// * `$2` - The account data event type
    /// * `$3` - The account data event content
    fn account_data_upsert_query(
    ) -> Query<'static, Self, <Self as HasArguments<'static>>::Arguments> {
        sqlx::query(
            r#"
                INSERT INTO statestore_accountdata
                    (room_id, event_type, account_data)
                VALUES ($1, $2, $3)
                ON CONFLICT(room_id, event_type) DO UPDATE SET account_data = $3
            "#,
        )
    }

    /// Retrieves account data
    ///
    /// # Arguments
    /// * `$1` - The room ID for the account data, or null
    /// * `$2` - The account data event type
    fn account_data_load_query() -> Query<'static, Self, <Self as HasArguments<'static>>::Arguments>
    {
        sqlx::query(
            r#"
                SELECT account_data FROM statestore_accountdata
                WHERE room_id = $1 AND event_type = $2
            "#,
        )
    }

    /// Upserts user presence data
    ///
    /// # Arguments
    /// * `$1` - The user ID
    /// * `$2` - The presence data
    fn presence_upsert_query() -> Query<'static, Self, <Self as HasArguments<'static>>::Arguments> {
        sqlx::query(
            r#"
                INSERT INTO statestore_presence
                    (user_id, presence)
                VALUES ($1, $2)
                ON CONFLICT(user_id) DO UPDATE SET presence = $2
            "#,
        )
    }

    /// Retrieves user presence data
    ///
    /// # Arguments
    /// * `$1` - The user ID
    fn presence_load_query() -> Query<'static, Self, <Self as HasArguments<'static>>::Arguments> {
        sqlx::query(
            r#"
                SELECT presence FROM statestore_presence
                WHERE user_id = $1
            "#,
        )
    }

    /// Upserts room membership information
    ///
    /// # Arguments
    /// * `$1` - The room ID
    /// * `$2` - The user ID
    /// * `$3` - Whether or not the membership event is stripped
    /// * `$4` - The membership event content
    /// * `$5` - The display name of the user
    fn member_upsert_query() -> Query<'static, Self, <Self as HasArguments<'static>>::Arguments> {
        sqlx::query(
            r#"
                INSERT INTO statestore_memberships
                    (room_id, user_id, is_stripped, member_event, displayname)
                VALUES ($1, $2, $3, $4, $5)
                ON CONFLICT(room_id, user_id) DO UPDATE SET is_stripped = $3, member_event = $4, displayname = $5
            "#,
        )
    }

    /// Upserts user profile information
    ///
    /// # Arguments
    /// * `$1` - The room ID
    /// * `$2` - The user ID
    /// * `$3` - The profile event content
    fn member_profile_upsert_query(
    ) -> Query<'static, Self, <Self as HasArguments<'static>>::Arguments> {
        sqlx::query(
            r#"
                INSERT INTO statestore_profiles
                    (room_id, user_id, is_partial, profile_event)
                VALUES ($1, $2, 0, $3)
                ON CONFLICT(room_id, user_id) DO UPDATE SET profile_event = $3
            "#,
        )
    }

    /// Upserts a state event
    ///
    /// # Arguments
    /// * `$1` - The room ID
    /// * `$2` - The event type
    /// * `$3` - The state key
    /// * `$4` - Whether or not the state is partial
    /// * `$5` - The event content
    fn state_upsert_query() -> Query<'static, Self, <Self as HasArguments<'static>>::Arguments> {
        sqlx::query(
            r#"
                INSERT INTO statestore_state
                    (room_id, event_type, state_key, is_partial, state_event)
                VALUES ($1, $2, $3, $4, $5)
                ON CONFLICT(room_id, event_type, state_key) DO UPDATE SET is_partial = $4, state_event = $5
            "#,
        )
    }

    /// Upserts room information
    ///
    /// # Arguments
    /// * `$1` - The room ID
    /// * `$2` - Whether or not the state is partial
    /// * `$3` - The room info
    fn room_upsert_query() -> Query<'static, Self, <Self as HasArguments<'static>>::Arguments> {
        sqlx::query(
            r#"
                INSERT INTO statestore_rooms
                    (room_id, is_partial, room_info)
                VALUES ($1, $2, $3)
                ON CONFLICT(room_id) DO UPDATE SET is_partial = $2, room_info = $3
            "#,
        )
    }

    /// Upserts an event receipt
    ///
    /// # Arguments
    /// * `$1` - The room ID
    /// * `$2` - The event ID
    /// * `$3` - The receipt type
    /// * `$4` - The user id
    /// * `$5` - The receipt content
    fn receipt_upsert_query() -> Query<'static, Self, <Self as HasArguments<'static>>::Arguments> {
        sqlx::query(
            r#"
                INSERT INTO statestore_receipts
                    (room_id, event_id, receipt_type, user_id, receipt)
                VALUES ($1, $2, $3, $4, $5)
                ON CONFLICT(room_id, user_id) DO UPDATE SET event_id = $2, receipt_type = $3, receipt = $5
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
