//! Various helper functionality

use sqlx::{
    database::HasArguments, migrate::Migrator, query::Query, Database, Decode, Encode, Type,
};

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

/// Helper trait that marks an SQL-Compatible type
pub trait SqlType<DB: Database>:
    for<'a> Encode<'a, DB> + for<'a> Decode<'a, DB> + Type<DB>
{
}
impl<DB: Database, T> SqlType<DB> for T where
    T: for<'a> Encode<'a, DB> + for<'a> Decode<'a, DB> + Type<DB>
{
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
                DELETE FROM statestore_rooms WHERE room_id = $1;
                DELETE FROM statestore_accountdata WHERE room_id = $1;
                DELETE FROM statestore_members WHERE room_id = $1;
                DELETE FROM statestore_state WHERE room_id = $1;
                DELETE FROM statestore_receipts WHERE room_id = $1;
            "#,
        )
    }

    /// Upserts account data
    ///
    /// # Arguments
    /// * `$1` - The room ID for the account data
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
    /// * `$1` - The room ID for the account data
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
    /// * `$6` - Whether or not the user has joined
    fn member_upsert_query() -> Query<'static, Self, <Self as HasArguments<'static>>::Arguments> {
        sqlx::query(
            r#"
                INSERT INTO statestore_members
                    (room_id, user_id, is_partial, member_event, displayname, joined)
                VALUES ($1, $2, $3, $4, $5, $6)
                ON CONFLICT(room_id, user_id) DO UPDATE SET is_partial = $3, member_event = $4, displayname = $5, joined = $6
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
                INSERT INTO statestore_members
                    (room_id, user_id, is_partial, user_profile)
                VALUES ($1, $2, 0, $3)
                ON CONFLICT(room_id, user_id) DO UPDATE SET user_profile = $3
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
                ON CONFLICT(room_id, receipt_type, user_id) DO UPDATE SET event_id = $2, receipt_type = $3, receipt = $5
            "#,
        )
    }

    /// Retrieves a state event
    ///
    /// # Arguments
    /// * `$1` - The room ID
    /// * `$2` - The event type
    /// * `$3` - The state key
    fn state_load_query() -> Query<'static, Self, <Self as HasArguments<'static>>::Arguments> {
        sqlx::query(
            r#"
                SELECT state_event FROM statestore_state
                WHERE room_id = $1 AND event_type = $2 AND state_key = $3 AND is_partial = '0'
            "#,
        )
    }

    /// Retrieves all state events by type in room
    ///
    /// # Arguments
    /// * `$1` - The room ID
    /// * `$2` - The event type
    fn states_load_query() -> Query<'static, Self, <Self as HasArguments<'static>>::Arguments> {
        sqlx::query(
            r#"
                SELECT state_event FROM statestore_state
                WHERE room_id = $1 AND event_type = $2 AND is_partial = '0'
            "#,
        )
    }

    /// Retrieves the user profile event for a user in a room
    ///
    /// # Arguments
    /// * `$1` - The room ID
    /// * `$2` - The user ID
    fn profile_load_query() -> Query<'static, Self, <Self as HasArguments<'static>>::Arguments> {
        sqlx::query(
            r#"
                SELECT user_profile FROM statestore_members
                WHERE room_id = $1 AND user_id = $2 AND user_profile IS NOT NULL
            "#,
        )
    }

    /// Removes a member from a room
    ///
    /// # Arguments
    /// * `$1` - The room ID
    /// * `$2` - The user ID
    fn member_remove_query() -> Query<'static, Self, <Self as HasArguments<'static>>::Arguments> {
        sqlx::query(
            r#"
                DELETE FROM statestore_memberships
                WHERE room_id = $1 AND user_id = $2
            "#,
        )
    }

    /// List all users in a room
    ///
    /// # Arguments
    /// * `$1` - The room ID
    fn members_load_query() -> Query<'static, Self, <Self as HasArguments<'static>>::Arguments> {
        sqlx::query(
            r#"
                SELECT user_id FROM statestore_members
                WHERE room_id = $1
            "#,
        )
    }

    /// List all users in a room
    ///
    /// # Arguments
    /// * `$1` - The room ID
    /// * `$2` - Whether or not the user has joined
    fn members_load_query_with_join_status(
    ) -> Query<'static, Self, <Self as HasArguments<'static>>::Arguments> {
        sqlx::query(
            r#"
                SELECT user_id FROM statestore_members
                WHERE room_id = $1 AND joined = $2
            "#,
        )
    }

    /// Get specific member event
    ///
    /// # Arguments
    /// * `$1` - The room ID
    /// * `$2` - The user ID
    fn member_load_query() -> Query<'static, Self, <Self as HasArguments<'static>>::Arguments> {
        sqlx::query(
            r#"
                SELECT is_partial, member_event FROM statestore_members
                WHERE room_id = $1 AND user_id = $2 AND member_event IS NOT NULL
            "#,
        )
    }

    /// Get room infos
    ///
    /// # Arguments
    /// * `$1` - Whether or not the info is partial
    fn room_info_load_query() -> Query<'static, Self, <Self as HasArguments<'static>>::Arguments> {
        sqlx::query(
            r#"
                SELECT room_info FROM statestore_rooms
                WHERE is_partial = $1
            "#,
        )
    }

    /// Get users with display name in room
    ///
    /// # Arguments
    /// * `$1` - The room ID
    /// * `$2` - The display name
    fn users_with_display_name_load_query(
    ) -> Query<'static, Self, <Self as HasArguments<'static>>::Arguments> {
        sqlx::query(
            r#"
                SELECT user_id FROM statestore_members
                WHERE room_id = $1 AND displayname = $2
            "#,
        )
    }

    /// Get latest receipt for user in room
    ///
    /// # Arguments
    /// * `$1` - The room ID
    /// * `$2` - The receipt type
    /// * `$3` - The user ID
    fn receipt_load_query() -> Query<'static, Self, <Self as HasArguments<'static>>::Arguments> {
        sqlx::query(
            r#"
                SELECT event_id, receipt FROM statestore_receipts
                WHERE room_id = $1 AND receipt_type = $2 AND user_id = $3
            "#,
        )
    }

    /// Get all receipts for event in room
    ///
    /// # Arguments
    /// * `$1` - The room ID
    /// * `$2` - The receipt type
    /// * `$3` - The event ID
    fn event_receipt_load_query() -> Query<'static, Self, <Self as HasArguments<'static>>::Arguments>
    {
        sqlx::query(
            r#"
                SELECT user_id, receipt FROM statestore_receipts
                WHERE room_id = $1 AND receipt_type = $2 AND event_id = $3
            "#,
        )
    }

    /// Stores a cryptostore session
    ///
    /// # Arguments
    /// * `$1` - The hashed user ID
    /// * `$2` - The hashed device ID
    /// * `$3` - The encrypted session data
    #[cfg(feature = "e2e-encryption")]
    fn session_store_query() -> Query<'static, Self, <Self as HasArguments<'static>>::Arguments> {
        sqlx::query(
            r#"
                INSERT INTO statestore_session (user_id, device_id, session_data)
                VALUES ($1, $2, $3)
            "#,
        )
    }

    /// Stores an Olm message hash
    ///
    /// # Arguments
    /// * `$1` - The sender key
    /// * `$2` - The message hash
    #[cfg(feature = "e2e-encryption")]
    fn olm_message_hash_store_query(
    ) -> Query<'static, Self, <Self as HasArguments<'static>>::Arguments> {
        sqlx::query(
            r#"
                INSERT INTO statestore_olm_message_hash (sender_key, message_hash)
                VALUES ($1, $2)
            "#,
        )
    }

    /// Stores an inbound group session
    ///
    /// # Arguments
    /// * `$1` - The hashed session id
    /// * `$2` - The encrypted session data
    #[cfg(feature = "e2e-encryption")]
    fn inbound_group_session_store_query(
    ) -> Query<'static, Self, <Self as HasArguments<'static>>::Arguments> {
        sqlx::query(
            r#"
                INSERT INTO statestore_inbound_group_session (session_id, session_data)
                VALUES ($1, $2)
            "#,
        )
    }

    /// Stores an outbound group session
    ///
    /// # Arguments
    /// * `$1` - The hashed session id
    /// * `$2` - The encrypted session data
    #[cfg(feature = "e2e-encryption")]
    fn outbound_group_session_store_query(
    ) -> Query<'static, Self, <Self as HasArguments<'static>>::Arguments> {
        sqlx::query(
            r#"
                INSERT INTO statestore_outbound_group_session (session_id, session_data)
                VALUES ($1, $2)
            "#,
        )
    }

    /// Stores a gossip request
    ///
    /// # Arguments
    /// * `$1` - The hashed recipient ID
    /// * `$2` - The hashed request ID
    /// * `$3` - The hashed secret request info
    /// * `$4` - Whether or not the request has been sent
    /// * `$5` - The encrypted request data
    #[cfg(feature = "e2e-encryption")]
    fn gossip_request_store_query(
    ) -> Query<'static, Self, <Self as HasArguments<'static>>::Arguments> {
        sqlx::query(
            r#"
                INSERT INTO statestore_gossip_request (recipient_id, request_id, secret_request_info, sent, request_data)
                VALUES ($1, $2, $3, $4, $5)
            "#,
        )
    }

    /// Upserts a cryptographic identity
    ///
    /// # Arguments
    /// * `$1` - The hashed user ID
    /// * `$2` - The encrypted identity data
    #[cfg(feature = "e2e-encryption")]
    fn identity_upsert_query() -> Query<'static, Self, <Self as HasArguments<'static>>::Arguments> {
        sqlx::query(
            r#"
                INSERT INTO statestore_identity (user_id, identity_data)
                VALUES ($1, $2)
                ON CONFLICT (user_id) DO UPDATE SET identity_data = $2
            "#,
        )
    }

    /// Upserts a device
    ///
    /// # Arguments
    /// * `$1` - The hashed user ID
    /// * `$2` - The hashed device ID
    /// * `$3` - The encrypted device data
    #[cfg(feature = "e2e-encryption")]
    fn device_upsert_query() -> Query<'static, Self, <Self as HasArguments<'static>>::Arguments> {
        sqlx::query(
            r#"
                INSERT INTO statestore_device (user_id, device_id, device_data)
                VALUES ($1, $2, $3)
                ON CONFLICT (user_id, device_id) DO UPDATE SET device_data = $3
            "#,
        )
    }

    /// Deletes a device
    ///
    /// # Arguments
    /// * `$1` - The hashed user ID
    /// * `$2` - The hashed device ID
    #[cfg(feature = "e2e-encryption")]
    fn device_delete_query() -> Query<'static, Self, <Self as HasArguments<'static>>::Arguments> {
        sqlx::query(
            r#"
                DELETE FROM statestore_device
                WHERE user_id = $1 AND device_id = $2
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
