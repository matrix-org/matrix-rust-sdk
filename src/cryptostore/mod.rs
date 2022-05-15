//! Crypto store implementation

use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use anyhow::Result;
use async_trait::async_trait;
use educe::Educe;
use matrix_sdk_base::locks::{Mutex, RwLock};
use matrix_sdk_crypto::{
    olm::{
        IdentityKeys, InboundGroupSession, OlmMessageHash, OutboundGroupSession,
        PrivateCrossSigningIdentity, Session,
    },
    store::{
        caches::{DeviceStore, GroupSessionStore, SessionStore},
        BackupKeys, Changes, CryptoStore, RecoveryKey, RoomKeyCounts,
    },
    CryptoStoreError, GossipRequest, ReadOnlyAccount, ReadOnlyDevice, ReadOnlyUserIdentities,
    SecretInfo,
};
use matrix_sdk_store_encryption::StoreCipher;
use ruma::{DeviceId, OwnedDeviceId, OwnedUserId, RoomId, TransactionId, UserId};
use sqlx::{database::HasArguments, ColumnIndex, Database, Executor, IntoArguments, Transaction};

use crate::{helpers::SqlType, StateStore, SupportedDatabase};

/// Store Result type
type StoreResult<T> = Result<T, CryptoStoreError>;

/// Cryptostore data
#[derive(Educe)]
#[educe(Debug)]
#[allow(clippy::redundant_pub_crate)]
pub(crate) struct CryptostoreData {
    /// Encryption cipher
    #[educe(Debug(ignore))]
    pub(crate) cipher: StoreCipher,
    /// Account info
    pub(crate) account: RwLock<Option<AccountInfo>>,
    /// In-Memory session store
    pub(crate) sessions: SessionStore,
    /// In-Memory group session store
    pub(crate) group_sessions: GroupSessionStore,
    /// In-Memory device store
    pub(crate) devices: DeviceStore,
}

impl CryptostoreData {
    /// Create a new cryptostore data
    pub(crate) fn new(cipher: StoreCipher) -> Self {
        Self {
            cipher,
            account: RwLock::new(None),
            sessions: SessionStore::new(),
            group_sessions: GroupSessionStore::new(),
            devices: DeviceStore::new(),
        }
    }
}
/// Account information
#[derive(Clone, Debug)]
#[allow(clippy::redundant_pub_crate)]
pub(crate) struct AccountInfo {
    user_id: Arc<UserId>,
    device_id: Arc<DeviceId>,
    identity_keys: Arc<IdentityKeys>,
}

impl<DB: SupportedDatabase> StateStore<DB> {
    /// Loads a previously stored account
    ///
    /// # Errors
    /// This function will return an error if the database has not been unlocked,
    /// or if the query fails.
    pub(crate) async fn load_account(&self) -> Result<Option<ReadOnlyAccount>>
    where
        for<'a> <DB as HasArguments<'a>>::Arguments: IntoArguments<'a, DB>,
        for<'c> &'c mut <DB as sqlx::Database>::Connection: Executor<'c, Database = DB>,
        Vec<u8>: SqlType<DB>,
        for<'a> &'a str: ColumnIndex<<DB as Database>::Row>,
    {
        let cipher = self.ensure_cipher()?;
        let account = match self.get_kv(b"e2e_account".to_vec()).await? {
            Some(account) => {
                let account = cipher.decrypt_value(&account)?;
                let account = ReadOnlyAccount::from_pickle(account)?;

                let account_info = AccountInfo {
                    user_id: Arc::clone(&account.user_id),
                    device_id: Arc::clone(&account.device_id),
                    identity_keys: Arc::clone(&account.identity_keys),
                };
                *(self.ensure_e2e()?.account.write().await) = Some(account_info);

                Some(account)
            }
            None => None,
        };
        Ok(account)
    }

    /// Stores an account
    ///
    /// # Errors
    /// This function will return an error if the database has not been unlocked,
    /// or if the query fails.
    pub(crate) async fn save_account(&self, account: ReadOnlyAccount) -> Result<()>
    where
        for<'a> <DB as HasArguments<'a>>::Arguments: IntoArguments<'a, DB>,
        for<'c, 'a> &'a mut Transaction<'c, DB>: Executor<'a, Database = DB>,
        Vec<u8>: SqlType<DB>,
    {
        let mut txn = self.db.begin().await?;
        self.save_account_txn(&mut txn, account).await?;
        txn.commit().await?;

        Ok(())
    }

    /// Stores an account in a transaction
    ///
    /// # Errors
    /// This function will return an error if the database has not been unlocked,
    /// or if the query fails.
    pub(crate) async fn save_account_txn<'c>(
        &self,
        txn: &mut Transaction<'c, DB>,
        account: ReadOnlyAccount,
    ) -> Result<()>
    where
        for<'a> <DB as HasArguments<'a>>::Arguments: IntoArguments<'a, DB>,
        for<'a> &'a mut Transaction<'c, DB>: Executor<'a, Database = DB>,
        Vec<u8>: SqlType<DB>,
    {
        let cipher = self.ensure_cipher()?;
        let account_info = AccountInfo {
            user_id: Arc::clone(&account.user_id),
            device_id: Arc::clone(&account.device_id),
            identity_keys: Arc::clone(&account.identity_keys),
        };
        *(self.ensure_e2e()?.account.write().await) = Some(account_info);
        Self::insert_kv_txn(
            txn,
            b"e2e_account".to_vec(),
            cipher.encrypt_value(&account.pickle().await)?,
        )
        .await?;
        Ok(())
    }

    /// Loads the cross-signing identity
    ///
    /// # Errors
    /// This function will return an error if the database has not been unlocked,
    /// or if the query fails.
    pub(crate) async fn load_identity(&self) -> Result<Option<PrivateCrossSigningIdentity>>
    where
        for<'a> <DB as HasArguments<'a>>::Arguments: IntoArguments<'a, DB>,
        for<'c> &'c mut <DB as sqlx::Database>::Connection: Executor<'c, Database = DB>,
        Vec<u8>: SqlType<DB>,
        for<'a> &'a str: ColumnIndex<<DB as Database>::Row>,
    {
        let cipher = self.ensure_cipher()?;
        let private_identity = match self.get_kv(b"private_identity".to_vec()).await? {
            Some(account) => {
                let private_identity = cipher.decrypt_value(&account)?;
                let private_identity =
                    PrivateCrossSigningIdentity::from_pickle(private_identity).await?;
                Some(private_identity)
            }
            None => None,
        };
        Ok(private_identity)
    }

    /// Stores the cross-signing identity
    ///
    /// # Errors
    /// This function will return an error if the database has not been unlocked,
    /// or if the query fails.
    pub(crate) async fn store_identity<'c>(
        &self,
        txn: &mut Transaction<'c, DB>,
        identity: PrivateCrossSigningIdentity,
    ) -> Result<()>
    where
        for<'a> <DB as HasArguments<'a>>::Arguments: IntoArguments<'a, DB>,
        for<'a> &'a mut Transaction<'c, DB>: Executor<'a, Database = DB>,
        Vec<u8>: SqlType<DB>,
    {
        let cipher = self.ensure_cipher()?;
        Self::insert_kv_txn(
            txn,
            b"private_identity".to_vec(),
            cipher.encrypt_value(&identity.pickle().await?)?,
        )
        .await?;
        Ok(())
    }

    /// Stores the backup version
    ///
    /// # Errors
    /// This function will return an error if the database has not been unlocked,
    /// or if the query fails.
    pub(crate) async fn store_backup_version<'c>(
        &self,
        txn: &mut Transaction<'c, DB>,
        backup_version: String,
    ) -> Result<()>
    where
        for<'a> <DB as HasArguments<'a>>::Arguments: IntoArguments<'a, DB>,
        for<'a> &'a mut Transaction<'c, DB>: Executor<'a, Database = DB>,
        Vec<u8>: SqlType<DB>,
    {
        let cipher = self.ensure_cipher()?;
        Self::insert_kv_txn(
            txn,
            b"backup_version".to_vec(),
            cipher.encrypt_value(&backup_version)?,
        )
        .await?;
        Ok(())
    }

    /// Stores the recovery key
    ///
    /// # Errors
    /// This function will return an error if the database has not been unlocked,
    /// or if the query fails.
    pub(crate) async fn store_recovery_key<'c>(
        &self,
        txn: &mut Transaction<'c, DB>,
        recovery_key: RecoveryKey,
    ) -> Result<()>
    where
        for<'a> <DB as HasArguments<'a>>::Arguments: IntoArguments<'a, DB>,
        for<'a> &'a mut Transaction<'c, DB>: Executor<'a, Database = DB>,
        Vec<u8>: SqlType<DB>,
    {
        let cipher = self.ensure_cipher()?;
        Self::insert_kv_txn(
            txn,
            b"recovery_key".to_vec(),
            cipher.encrypt_value(&recovery_key)?,
        )
        .await?;
        Ok(())
    }

    /// Saves an olm session to database
    ///
    /// # Errors
    /// This function will return an error if the database has not been unlocked,
    /// or if the query fails.
    pub(crate) async fn save_session<'c>(
        &self,
        txn: &mut Transaction<'c, DB>,
        session: Session,
    ) -> Result<()>
    where
        for<'a> <DB as HasArguments<'a>>::Arguments: IntoArguments<'a, DB>,
        for<'a> &'a mut Transaction<'c, DB>: Executor<'a, Database = DB>,
        [u8; 32]: SqlType<DB>,
        Vec<u8>: SqlType<DB>,
    {
        let cipher = self.ensure_cipher()?;
        let user_id = cipher.hash_key("statestore_session:user_id", session.user_id.as_bytes());
        let device_id =
            cipher.hash_key("statestore_session:device_id", session.device_id.as_bytes());
        DB::session_store_query()
            .bind(user_id)
            .bind(device_id)
            .bind(cipher.encrypt_value(&session.pickle().await)?)
            .execute(txn)
            .await?;
        self.ensure_e2e()?.sessions.add(session).await;
        Ok(())
    }

    /// Saves an olm message hash
    ///
    /// # Errors
    /// This function will return an error if the query fails
    pub(crate) async fn save_message_hash<'c>(
        txn: &mut Transaction<'c, DB>,
        message_hash: OlmMessageHash,
    ) -> Result<()>
    where
        for<'a> <DB as HasArguments<'a>>::Arguments: IntoArguments<'a, DB>,
        for<'a> &'a mut Transaction<'c, DB>: Executor<'a, Database = DB>,
        String: SqlType<DB>,
    {
        DB::olm_message_hash_store_query()
            .bind(message_hash.sender_key)
            .bind(message_hash.hash)
            .execute(txn)
            .await?;
        Ok(())
    }

    /// Saves an inbound group session
    ///
    /// # Errors
    /// This function will return an error if the database has not been unlocked,
    /// or if the query fails.
    pub(crate) async fn save_inbound_group_session<'c>(
        &self,
        txn: &mut Transaction<'c, DB>,
        session: InboundGroupSession,
    ) -> Result<()>
    where
        for<'a> <DB as HasArguments<'a>>::Arguments: IntoArguments<'a, DB>,
        for<'a> &'a mut Transaction<'c, DB>: Executor<'a, Database = DB>,
        [u8; 32]: SqlType<DB>,
        Vec<u8>: SqlType<DB>,
    {
        let cipher = self.ensure_cipher()?;
        let session_id = cipher.hash_key(
            "statestore_inbound_group_session:session_id",
            session.session_id.as_bytes(),
        );
        DB::inbound_group_session_store_query()
            .bind(session_id)
            .bind(cipher.encrypt_value(&session.pickle().await)?)
            .execute(txn)
            .await?;
        self.ensure_e2e()?.group_sessions.add(session);
        Ok(())
    }

    /// Saves an outbound group session
    ///
    /// # Errors
    /// This function will return an error if the database has not been unlocked,
    /// or if the query fails.
    pub(crate) async fn save_outbound_group_session<'c>(
        &self,
        txn: &mut Transaction<'c, DB>,
        session: OutboundGroupSession,
    ) -> Result<()>
    where
        for<'a> <DB as HasArguments<'a>>::Arguments: IntoArguments<'a, DB>,
        for<'a> &'a mut Transaction<'c, DB>: Executor<'a, Database = DB>,
        [u8; 32]: SqlType<DB>,
        Vec<u8>: SqlType<DB>,
    {
        let cipher = self.ensure_cipher()?;
        let session_id = cipher.hash_key(
            "statestore_inbound_group_session:session_id",
            session.session_id().as_bytes(),
        );
        DB::inbound_group_session_store_query()
            .bind(session_id)
            .bind(cipher.encrypt_value(&session.pickle().await)?)
            .execute(txn)
            .await?;
        Ok(())
    }

    /// Saves a gossip request
    ///
    /// # Errors
    /// This function will return an error if the database has not been unlocked,
    /// or if the query fails.
    pub(crate) async fn save_gossip_request<'c>(
        &self,
        txn: &mut Transaction<'c, DB>,
        request: GossipRequest,
    ) -> Result<()>
    where
        for<'a> <DB as HasArguments<'a>>::Arguments: IntoArguments<'a, DB>,
        for<'a> &'a mut Transaction<'c, DB>: Executor<'a, Database = DB>,
        [u8; 32]: SqlType<DB>,
        Vec<u8>: SqlType<DB>,
        bool: SqlType<DB>,
    {
        let cipher = self.ensure_cipher()?;
        let recipient_id = cipher.hash_key(
            "statestore_gossip_request:recipient_id",
            request.request_recipient.as_bytes(),
        );
        let request_id = cipher.hash_key(
            "statestore_gossip_request:request_id",
            request.request_id.as_bytes(),
        );
        let info_key = cipher.hash_key(
            "statestore_gossip_request:info_key",
            request.info.as_key().as_bytes(),
        );
        DB::gossip_request_store_query()
            .bind(recipient_id)
            .bind(request_id)
            .bind(info_key)
            .bind(request.sent_out)
            .bind(cipher.encrypt_value(&request)?)
            .execute(txn)
            .await?;
        Ok(())
    }

    /// Saves a cryptographic identity
    ///
    /// # Errors
    /// This function will return an error if the database has not been unlocked,
    /// or if the query fails.
    pub(crate) async fn save_crypto_identity<'c>(
        &self,
        txn: &mut Transaction<'c, DB>,
        identity: ReadOnlyUserIdentities,
    ) -> Result<()>
    where
        for<'a> <DB as HasArguments<'a>>::Arguments: IntoArguments<'a, DB>,
        for<'a> &'a mut Transaction<'c, DB>: Executor<'a, Database = DB>,
        [u8; 32]: SqlType<DB>,
        Vec<u8>: SqlType<DB>,
    {
        let cipher = self.ensure_cipher()?;
        let user_id = cipher.hash_key(
            "statestore_crypto_identity:user_id",
            identity.user_id().as_bytes(),
        );
        DB::identity_upsert_query()
            .bind(user_id)
            .bind(cipher.encrypt_value(&identity)?)
            .execute(txn)
            .await?;
        Ok(())
    }

    /// Saves a device
    ///
    /// # Errors
    /// This function will return an error if the database has not been unlocked,
    /// or if the query fails.
    pub(crate) async fn save_device<'c>(
        &self,
        txn: &mut Transaction<'c, DB>,
        device: ReadOnlyDevice,
    ) -> Result<()>
    where
        for<'a> <DB as HasArguments<'a>>::Arguments: IntoArguments<'a, DB>,
        for<'a> &'a mut Transaction<'c, DB>: Executor<'a, Database = DB>,
        [u8; 32]: SqlType<DB>,
        Vec<u8>: SqlType<DB>,
    {
        let cipher = self.ensure_cipher()?;
        let user_id = cipher.hash_key("statestore_device:user_id", device.user_id().as_bytes());
        let device_id =
            cipher.hash_key("statestore_device:device_id", device.device_id().as_bytes());
        DB::device_upsert_query()
            .bind(user_id)
            .bind(device_id)
            .bind(cipher.encrypt_value(&device)?)
            .execute(txn)
            .await?;
        self.ensure_e2e()?.devices.add(device);
        Ok(())
    }

    /// Deletes a device
    ///
    /// # Errors
    /// This function will return an error if the database has not been unlocked,
    /// or if the query fails.
    pub(crate) async fn delete_device<'c>(
        &self,
        txn: &mut Transaction<'c, DB>,
        device: ReadOnlyDevice,
    ) -> Result<()>
    where
        for<'a> <DB as HasArguments<'a>>::Arguments: IntoArguments<'a, DB>,
        for<'a> &'a mut Transaction<'c, DB>: Executor<'a, Database = DB>,
        [u8; 32]: SqlType<DB>,
        Vec<u8>: SqlType<DB>,
    {
        let cipher = self.ensure_cipher()?;
        let user_id = cipher.hash_key("statestore_device:user_id", device.user_id().as_bytes());
        let device_id =
            cipher.hash_key("statestore_device:device_id", device.device_id().as_bytes());
        DB::device_delete_query()
            .bind(user_id)
            .bind(device_id)
            .execute(txn)
            .await?;
        self.ensure_e2e()?
            .devices
            .remove(device.user_id(), device.device_id());
        Ok(())
    }

    /// Applies cryptostore changes to the database in a transaction
    ///
    /// # Errors
    /// This function will return an error if the database has not been unlocked,
    /// or if the query fails.
    pub(crate) async fn save_changes_txn<'c>(
        &self,
        txn: &mut Transaction<'c, DB>,
        changes: Changes,
    ) -> Result<()>
    where
        for<'a> <DB as HasArguments<'a>>::Arguments: IntoArguments<'a, DB>,
        for<'a> &'a mut Transaction<'c, DB>: Executor<'a, Database = DB>,
        [u8; 32]: SqlType<DB>,
        Vec<u8>: SqlType<DB>,
        String: SqlType<DB>,
        bool: SqlType<DB>,
    {
        if let Some(account) = changes.account {
            self.save_account_txn(txn, account).await?;
        }
        if let Some(identity) = changes.private_identity {
            self.store_identity(txn, identity).await?;
        }
        if let Some(backup_version) = changes.backup_version {
            self.store_backup_version(txn, backup_version).await?;
        }
        if let Some(recovery_key) = changes.recovery_key {
            self.store_recovery_key(txn, recovery_key).await?;
        }
        for session in changes.sessions {
            self.save_session(txn, session).await?;
        }
        for message_hash in changes.message_hashes {
            Self::save_message_hash(txn, message_hash).await?;
        }
        for session in changes.inbound_group_sessions {
            self.save_inbound_group_session(txn, session).await?;
        }
        for session in changes.outbound_group_sessions {
            self.save_outbound_group_session(txn, session).await?;
        }
        for request in changes.key_requests {
            self.save_gossip_request(txn, request).await?;
        }
        for identity_change in changes
            .identities
            .changed
            .into_iter()
            .chain(changes.identities.new.into_iter())
        {
            self.save_crypto_identity(txn, identity_change).await?;
        }

        for device in changes
            .devices
            .changed
            .into_iter()
            .chain(changes.devices.new.into_iter())
        {
            self.save_device(txn, device).await?;
        }

        for device in changes.devices.deleted {
            self.delete_device(txn, device).await?;
        }

        Ok(())
    }

    /// Applies cryptostore changes to the database
    ///
    /// # Errors
    /// This function will return an error if the database has not been unlocked,
    /// or if the query fails.
    pub(crate) async fn save_changes(&self, changes: Changes) -> Result<()>
    where
        for<'a> <DB as HasArguments<'a>>::Arguments: IntoArguments<'a, DB>,
        for<'c, 'a> &'a mut Transaction<'c, DB>: Executor<'a, Database = DB>,
        [u8; 32]: SqlType<DB>,
        Vec<u8>: SqlType<DB>,
        String: SqlType<DB>,
        bool: SqlType<DB>,
    {
        let mut txn = self.db.begin().await?;
        self.save_changes_txn(&mut txn, changes).await?;
        txn.commit().await?;
        Ok(())
    }
}

#[async_trait]
impl<DB: SupportedDatabase> CryptoStore for StateStore<DB>
where
    for<'a> <DB as HasArguments<'a>>::Arguments: IntoArguments<'a, DB>,
    for<'c> &'c mut <DB as sqlx::Database>::Connection: Executor<'c, Database = DB>,
    for<'c, 'a> &'a mut Transaction<'c, DB>: Executor<'a, Database = DB>,
    [u8; 32]: SqlType<DB>,
    Vec<u8>: SqlType<DB>,
    String: SqlType<DB>,
    bool: SqlType<DB>,
    for<'a> &'a str: ColumnIndex<<DB as Database>::Row>,
{
    async fn load_account(&self) -> StoreResult<Option<ReadOnlyAccount>> {
        self.load_account()
            .await
            .map_err(|e| CryptoStoreError::Backend(e.into()))
    }
    async fn save_account(&self, account: ReadOnlyAccount) -> StoreResult<()> {
        self.save_account(account)
            .await
            .map_err(|e| CryptoStoreError::Backend(e.into()))
    }
    async fn load_identity(&self) -> StoreResult<Option<PrivateCrossSigningIdentity>> {
        self.load_identity()
            .await
            .map_err(|e| CryptoStoreError::Backend(e.into()))
    }
    async fn save_changes(&self, changes: Changes) -> StoreResult<()> {
        self.save_changes(changes)
            .await
            .map_err(|e| CryptoStoreError::Backend(e.into()))
    }
    async fn get_sessions(
        &self,
        sender_key: &str,
    ) -> StoreResult<Option<Arc<Mutex<Vec<Session>>>>> {
        todo!();
    }
    async fn get_inbound_group_session(
        &self,
        room_id: &RoomId,
        sender_key: &str,
        session_id: &str,
    ) -> StoreResult<Option<InboundGroupSession>> {
        todo!();
    }
    async fn get_inbound_group_sessions(&self) -> StoreResult<Vec<InboundGroupSession>> {
        todo!();
    }
    async fn inbound_group_session_counts(&self) -> StoreResult<RoomKeyCounts> {
        todo!();
    }
    async fn inbound_group_sessions_for_backup(
        &self,
        limit: usize,
    ) -> StoreResult<Vec<InboundGroupSession>> {
        todo!();
    }
    async fn reset_backup_state(&self) -> StoreResult<()> {
        todo!();
    }
    async fn load_backup_keys(&self) -> StoreResult<BackupKeys> {
        todo!();
    }
    async fn get_outbound_group_sessions(
        &self,
        room_id: &RoomId,
    ) -> StoreResult<Option<OutboundGroupSession>> {
        todo!();
    }
    fn is_user_tracked(&self, user_id: &UserId) -> bool {
        todo!();
    }
    fn has_users_for_key_query(&self) -> bool {
        todo!();
    }
    fn users_for_key_query(&self) -> HashSet<OwnedUserId> {
        todo!();
    }
    fn tracked_users(&self) -> HashSet<OwnedUserId> {
        todo!();
    }
    async fn update_tracked_user(&self, user: &UserId, dirty: bool) -> StoreResult<bool> {
        todo!();
    }

    async fn get_device(
        &self,
        user_id: &UserId,
        device_id: &DeviceId,
    ) -> StoreResult<Option<ReadOnlyDevice>> {
        todo!();
    }
    async fn get_user_devices(
        &self,
        user_id: &UserId,
    ) -> StoreResult<HashMap<OwnedDeviceId, ReadOnlyDevice>> {
        todo!();
    }
    async fn get_user_identity(
        &self,
        user_id: &UserId,
    ) -> StoreResult<Option<ReadOnlyUserIdentities>> {
        todo!();
    }
    async fn is_message_known(&self, message_hash: &OlmMessageHash) -> StoreResult<bool> {
        todo!();
    }
    async fn get_outgoing_secret_requests(
        &self,
        request_id: &TransactionId,
    ) -> StoreResult<Option<GossipRequest>> {
        todo!();
    }
    async fn get_secret_request_by_info(
        &self,
        secret_info: &SecretInfo,
    ) -> StoreResult<Option<GossipRequest>> {
        todo!();
    }
    async fn get_unsent_secret_requests(&self) -> StoreResult<Vec<GossipRequest>> {
        todo!();
    }
    async fn delete_outgoing_secret_requests(&self, request_id: &TransactionId) -> StoreResult<()> {
        todo!();
    }
}
