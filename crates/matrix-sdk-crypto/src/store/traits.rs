// Copyright 2023 The Matrix.org Foundation C.I.C.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::{collections::HashMap, fmt, sync::Arc};

use async_trait::async_trait;
use matrix_sdk_common::AsyncTraitDeps;
use ruma::{
    events::secret::request::SecretName, DeviceId, OwnedDeviceId, RoomId, TransactionId, UserId,
};
use vodozemac::Curve25519PublicKey;

use super::{
    BackupKeys, Changes, CryptoStoreError, PendingChanges, Result, RoomKeyCounts, RoomSettings,
};
#[cfg(doc)]
use crate::olm::SenderData;
use crate::{
    olm::{
        InboundGroupSession, OlmMessageHash, OutboundGroupSession, PrivateCrossSigningIdentity,
        SenderDataType, Session,
    },
    types::events::room_key_withheld::RoomKeyWithheldEvent,
    Account, DeviceData, GossipRequest, GossippedSecret, SecretInfo, TrackedUser, UserIdentityData,
};

/// Represents a store that the `OlmMachine` uses to store E2EE data (such as
/// cryptographic keys).
#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
pub trait CryptoStore: AsyncTraitDeps {
    /// The error type used by this crypto store.
    type Error: fmt::Debug + Into<CryptoStoreError>;

    /// Load an account that was previously stored.
    async fn load_account(&self) -> Result<Option<Account>, Self::Error>;

    /// Try to load a private cross signing identity, if one is stored.
    async fn load_identity(&self) -> Result<Option<PrivateCrossSigningIdentity>, Self::Error>;

    /// Save the set of changes to the store.
    ///
    /// # Arguments
    ///
    /// * `changes` - The set of changes that should be stored.
    async fn save_changes(&self, changes: Changes) -> Result<(), Self::Error>;

    /// Save the set of changes to the store.
    ///
    /// This is an updated version of `save_changes` that will replace it as
    /// #2624 makes progress.
    ///
    /// # Arguments
    ///
    /// * `changes` - The set of changes that should be stored.
    async fn save_pending_changes(&self, changes: PendingChanges) -> Result<(), Self::Error>;

    /// Save a list of inbound group sessions to the store.
    ///
    /// # Arguments
    ///
    /// * `sessions` - The sessions to be saved.
    /// * `backed_up_to_version` - If the keys should be marked as having been
    ///   backed up, the version of the backup.
    ///
    /// Note: some implementations ignore `backup_version` and assume the
    /// current backup version, which is normally the same.
    async fn save_inbound_group_sessions(
        &self,
        sessions: Vec<InboundGroupSession>,
        backed_up_to_version: Option<&str>,
    ) -> Result<(), Self::Error>;

    /// Get all the sessions that belong to the given sender key.
    ///
    /// # Arguments
    ///
    /// * `sender_key` - The sender key that was used to establish the sessions.
    async fn get_sessions(&self, sender_key: &str) -> Result<Option<Vec<Session>>, Self::Error>;

    /// Get the inbound group session from our store.
    ///
    /// # Arguments
    /// * `room_id` - The room id of the room that the session belongs to.
    ///
    /// * `sender_key` - The sender key that sent us the session.
    ///
    /// * `session_id` - The unique id of the session.
    async fn get_inbound_group_session(
        &self,
        room_id: &RoomId,
        session_id: &str,
    ) -> Result<Option<InboundGroupSession>, Self::Error>;

    /// Get withheld info for this key.
    /// Allows to know if the session was not sent on purpose.
    /// This only returns withheld info sent by the owner of the group session,
    /// not the one you can get from a response to a key request from
    /// another of your device.
    async fn get_withheld_info(
        &self,
        room_id: &RoomId,
        session_id: &str,
    ) -> Result<Option<RoomKeyWithheldEvent>, Self::Error>;

    /// Get all the inbound group sessions we have stored.
    async fn get_inbound_group_sessions(&self) -> Result<Vec<InboundGroupSession>, Self::Error>;

    /// Get the number inbound group sessions we have and how many of them are
    /// backed up.
    async fn inbound_group_session_counts(
        &self,
        backup_version: Option<&str>,
    ) -> Result<RoomKeyCounts, Self::Error>;

    /// Get a batch of inbound group sessions for the device with the supplied
    /// curve key, whose sender data is of the supplied type.
    ///
    /// Sessions are not necessarily returned in any specific order, but the
    /// returned batches are consistent: if this function is called repeatedly
    /// with `after_session_id` set to the session ID from the last result
    /// from the previous call, until an empty result is returned, then
    /// eventually all matching sessions are returned. (New sessions that are
    /// added in the course of iteration may or may not be returned.)
    ///
    /// This function is used when the device information is updated via a
    /// `/keys/query` response and we want to update the sender data based
    /// on the new information.
    ///
    /// # Arguments
    ///
    /// * `curve_key` - only return sessions created by the device with this
    ///   curve key.
    ///
    /// * `sender_data_type` - only return sessions whose [`SenderData`] record
    ///   is in this state.
    ///
    /// * `after_session_id` - return the sessions after this id, or start at
    ///   the earliest if this is None.
    ///
    /// * `limit` - return a maximum of this many sessions.
    async fn get_inbound_group_sessions_for_device_batch(
        &self,
        curve_key: Curve25519PublicKey,
        sender_data_type: SenderDataType,
        after_session_id: Option<String>,
        limit: usize,
    ) -> Result<Vec<InboundGroupSession>, Self::Error>;

    /// Return a batch of ['InboundGroupSession'] ("room keys") that have not
    /// yet been backed up in the supplied backup version.
    ///
    /// The size of the returned `Vec` is <= `limit`.
    ///
    /// Note: some implementations ignore `backup_version` and assume the
    /// current backup version, which is normally the same.
    async fn inbound_group_sessions_for_backup(
        &self,
        backup_version: &str,
        limit: usize,
    ) -> Result<Vec<InboundGroupSession>, Self::Error>;

    /// Store the fact that the supplied sessions were backed up into the backup
    /// with version `backup_version`.
    ///
    /// Note: some implementations ignore `backup_version` and assume the
    /// current backup version, which is normally the same.
    async fn mark_inbound_group_sessions_as_backed_up(
        &self,
        backup_version: &str,
        room_and_session_ids: &[(&RoomId, &str)],
    ) -> Result<(), Self::Error>;

    /// Reset the backup state of all the stored inbound group sessions.
    ///
    /// Note: this is mostly implemented by stores that ignore the
    /// `backup_version` argument on `inbound_group_sessions_for_backup` and
    /// `mark_inbound_group_sessions_as_backed_up`. Implementations that
    /// pay attention to the supplied backup version probably don't need to
    /// update their storage when the current backup version changes, so have
    /// empty implementations of this method.
    async fn reset_backup_state(&self) -> Result<(), Self::Error>;

    /// Get the backup keys we have stored.
    async fn load_backup_keys(&self) -> Result<BackupKeys, Self::Error>;

    /// Get the outbound group session we have stored that is used for the
    /// given room.
    async fn get_outbound_group_session(
        &self,
        room_id: &RoomId,
    ) -> Result<Option<OutboundGroupSession>, Self::Error>;

    /// Provide the list of users whose devices we are keeping track of, and
    /// whether they are considered dirty/outdated.
    async fn load_tracked_users(&self) -> Result<Vec<TrackedUser>, Self::Error>;

    /// Update the list of users whose devices we are keeping track of, and
    /// whether they are considered dirty/outdated.
    ///
    /// Replaces any existing entry with a matching user ID.
    async fn save_tracked_users(&self, users: &[(&UserId, bool)]) -> Result<(), Self::Error>;

    /// Get the device for the given user with the given device ID.
    ///
    /// # Arguments
    ///
    /// * `user_id` - The user that the device belongs to.
    ///
    /// * `device_id` - The unique id of the device.
    async fn get_device(
        &self,
        user_id: &UserId,
        device_id: &DeviceId,
    ) -> Result<Option<DeviceData>, Self::Error>;

    /// Get all the devices of the given user.
    ///
    /// # Arguments
    ///
    /// * `user_id` - The user for which we should get all the devices.
    async fn get_user_devices(
        &self,
        user_id: &UserId,
    ) -> Result<HashMap<OwnedDeviceId, DeviceData>, Self::Error>;

    /// Get the device for the current client.
    ///
    /// Since our own device is set when the store is created, this will always
    /// return a device (unless there is an error).
    async fn get_own_device(&self) -> Result<DeviceData, Self::Error>;

    /// Get the user identity that is attached to the given user id.
    ///
    /// # Arguments
    ///
    /// * `user_id` - The user for which we should get the identity.
    async fn get_user_identity(
        &self,
        user_id: &UserId,
    ) -> Result<Option<UserIdentityData>, Self::Error>;

    /// Check if a hash for an Olm message stored in the database.
    async fn is_message_known(&self, message_hash: &OlmMessageHash) -> Result<bool, Self::Error>;

    /// Get an outgoing secret request that we created that matches the given
    /// request id.
    ///
    /// # Arguments
    ///
    /// * `request_id` - The unique request id that identifies this outgoing
    /// secret request.
    async fn get_outgoing_secret_requests(
        &self,
        request_id: &TransactionId,
    ) -> Result<Option<GossipRequest>, Self::Error>;

    /// Get an outgoing key request that we created that matches the given
    /// requested key info.
    ///
    /// # Arguments
    ///
    /// * `key_info` - The key info of an outgoing secret request.
    async fn get_secret_request_by_info(
        &self,
        secret_info: &SecretInfo,
    ) -> Result<Option<GossipRequest>, Self::Error>;

    /// Get all outgoing secret requests that we have in the store.
    async fn get_unsent_secret_requests(&self) -> Result<Vec<GossipRequest>, Self::Error>;

    /// Delete an outgoing key request that we created that matches the given
    /// request id.
    ///
    /// # Arguments
    ///
    /// * `request_id` - The unique request id that identifies this outgoing key
    /// request.
    async fn delete_outgoing_secret_requests(
        &self,
        request_id: &TransactionId,
    ) -> Result<(), Self::Error>;

    /// Get all the secrets with the given [`SecretName`] we have currently
    /// stored.
    async fn get_secrets_from_inbox(
        &self,
        secret_name: &SecretName,
    ) -> Result<Vec<GossippedSecret>, Self::Error>;

    /// Delete all the secrets with the given [`SecretName`] we have currently
    /// stored.
    async fn delete_secrets_from_inbox(&self, secret_name: &SecretName) -> Result<(), Self::Error>;

    /// Get the room settings, such as the encryption algorithm or whether to
    /// encrypt only for trusted devices.
    ///
    /// # Arguments
    ///
    /// * `room_id` - The room id of the room
    async fn get_room_settings(
        &self,
        room_id: &RoomId,
    ) -> Result<Option<RoomSettings>, Self::Error>;

    /// Get arbitrary data from the store
    ///
    /// # Arguments
    ///
    /// * `key` - The key to fetch data for
    async fn get_custom_value(&self, key: &str) -> Result<Option<Vec<u8>>, Self::Error>;

    /// Put arbitrary data into the store
    ///
    /// # Arguments
    ///
    /// * `key` - The key to insert data into
    ///
    /// * `value` - The value to insert
    async fn set_custom_value(&self, key: &str, value: Vec<u8>) -> Result<(), Self::Error>;

    /// Remove arbitrary data into the store
    ///
    /// # Arguments
    ///
    /// * `key` - The key to insert data into
    async fn remove_custom_value(&self, key: &str) -> Result<(), Self::Error>;

    /// Try to take a leased lock.
    ///
    /// This attempts to take a lock for the given lease duration.
    ///
    /// - If we already had the lease, this will extend the lease.
    /// - If we didn't, but the previous lease has expired, we will acquire the
    ///   lock.
    /// - If there was no previous lease, we will acquire the lock.
    /// - Otherwise, we don't get the lock.
    ///
    /// Returns whether taking the lock succeeded.
    async fn try_take_leased_lock(
        &self,
        lease_duration_ms: u32,
        key: &str,
        holder: &str,
    ) -> Result<bool, Self::Error>;

    /// Load the next-batch token for a to-device query, if any.
    async fn next_batch_token(&self) -> Result<Option<String>, Self::Error>;
}

#[repr(transparent)]
struct EraseCryptoStoreError<T>(T);

#[cfg(not(tarpaulin_include))]
impl<T: fmt::Debug> fmt::Debug for EraseCryptoStoreError<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
impl<T: CryptoStore> CryptoStore for EraseCryptoStoreError<T> {
    type Error = CryptoStoreError;

    async fn load_account(&self) -> Result<Option<Account>> {
        self.0.load_account().await.map_err(Into::into)
    }

    async fn load_identity(&self) -> Result<Option<PrivateCrossSigningIdentity>> {
        self.0.load_identity().await.map_err(Into::into)
    }

    async fn save_changes(&self, changes: Changes) -> Result<()> {
        self.0.save_changes(changes).await.map_err(Into::into)
    }

    async fn save_pending_changes(&self, changes: PendingChanges) -> Result<()> {
        self.0.save_pending_changes(changes).await.map_err(Into::into)
    }

    async fn save_inbound_group_sessions(
        &self,
        sessions: Vec<InboundGroupSession>,
        backed_up_to_version: Option<&str>,
    ) -> Result<()> {
        self.0.save_inbound_group_sessions(sessions, backed_up_to_version).await.map_err(Into::into)
    }

    async fn get_sessions(&self, sender_key: &str) -> Result<Option<Vec<Session>>> {
        self.0.get_sessions(sender_key).await.map_err(Into::into)
    }

    async fn get_inbound_group_session(
        &self,
        room_id: &RoomId,
        session_id: &str,
    ) -> Result<Option<InboundGroupSession>> {
        self.0.get_inbound_group_session(room_id, session_id).await.map_err(Into::into)
    }

    async fn get_inbound_group_sessions(&self) -> Result<Vec<InboundGroupSession>> {
        self.0.get_inbound_group_sessions().await.map_err(Into::into)
    }

    async fn get_inbound_group_sessions_for_device_batch(
        &self,
        curve_key: Curve25519PublicKey,
        sender_data_type: SenderDataType,
        after_session_id: Option<String>,
        limit: usize,
    ) -> Result<Vec<InboundGroupSession>> {
        self.0
            .get_inbound_group_sessions_for_device_batch(
                curve_key,
                sender_data_type,
                after_session_id,
                limit,
            )
            .await
            .map_err(Into::into)
    }

    async fn inbound_group_session_counts(
        &self,
        backup_version: Option<&str>,
    ) -> Result<RoomKeyCounts> {
        self.0.inbound_group_session_counts(backup_version).await.map_err(Into::into)
    }
    async fn inbound_group_sessions_for_backup(
        &self,
        backup_version: &str,
        limit: usize,
    ) -> Result<Vec<InboundGroupSession>> {
        self.0.inbound_group_sessions_for_backup(backup_version, limit).await.map_err(Into::into)
    }

    async fn mark_inbound_group_sessions_as_backed_up(
        &self,
        backup_version: &str,
        room_and_session_ids: &[(&RoomId, &str)],
    ) -> Result<()> {
        self.0
            .mark_inbound_group_sessions_as_backed_up(backup_version, room_and_session_ids)
            .await
            .map_err(Into::into)
    }

    async fn reset_backup_state(&self) -> Result<()> {
        self.0.reset_backup_state().await.map_err(Into::into)
    }

    async fn load_backup_keys(&self) -> Result<BackupKeys> {
        self.0.load_backup_keys().await.map_err(Into::into)
    }

    async fn get_outbound_group_session(
        &self,
        room_id: &RoomId,
    ) -> Result<Option<OutboundGroupSession>> {
        self.0.get_outbound_group_session(room_id).await.map_err(Into::into)
    }

    async fn load_tracked_users(&self) -> Result<Vec<TrackedUser>> {
        self.0.load_tracked_users().await.map_err(Into::into)
    }

    async fn save_tracked_users(&self, users: &[(&UserId, bool)]) -> Result<()> {
        self.0.save_tracked_users(users).await.map_err(Into::into)
    }

    async fn get_device(
        &self,
        user_id: &UserId,
        device_id: &DeviceId,
    ) -> Result<Option<DeviceData>> {
        self.0.get_device(user_id, device_id).await.map_err(Into::into)
    }

    async fn get_user_devices(
        &self,
        user_id: &UserId,
    ) -> Result<HashMap<OwnedDeviceId, DeviceData>> {
        self.0.get_user_devices(user_id).await.map_err(Into::into)
    }

    async fn get_own_device(&self) -> Result<DeviceData> {
        self.0.get_own_device().await.map_err(Into::into)
    }

    async fn get_user_identity(&self, user_id: &UserId) -> Result<Option<UserIdentityData>> {
        self.0.get_user_identity(user_id).await.map_err(Into::into)
    }

    async fn is_message_known(&self, message_hash: &OlmMessageHash) -> Result<bool> {
        self.0.is_message_known(message_hash).await.map_err(Into::into)
    }

    async fn get_outgoing_secret_requests(
        &self,
        request_id: &TransactionId,
    ) -> Result<Option<GossipRequest>> {
        self.0.get_outgoing_secret_requests(request_id).await.map_err(Into::into)
    }

    async fn get_secret_request_by_info(
        &self,
        secret_info: &SecretInfo,
    ) -> Result<Option<GossipRequest>> {
        self.0.get_secret_request_by_info(secret_info).await.map_err(Into::into)
    }

    async fn get_unsent_secret_requests(&self) -> Result<Vec<GossipRequest>> {
        self.0.get_unsent_secret_requests().await.map_err(Into::into)
    }

    async fn delete_outgoing_secret_requests(&self, request_id: &TransactionId) -> Result<()> {
        self.0.delete_outgoing_secret_requests(request_id).await.map_err(Into::into)
    }

    async fn get_secrets_from_inbox(
        &self,
        secret_name: &SecretName,
    ) -> Result<Vec<GossippedSecret>> {
        self.0.get_secrets_from_inbox(secret_name).await.map_err(Into::into)
    }

    async fn delete_secrets_from_inbox(&self, secret_name: &SecretName) -> Result<()> {
        self.0.delete_secrets_from_inbox(secret_name).await.map_err(Into::into)
    }

    async fn get_withheld_info(
        &self,
        room_id: &RoomId,
        session_id: &str,
    ) -> Result<Option<RoomKeyWithheldEvent>, Self::Error> {
        self.0.get_withheld_info(room_id, session_id).await.map_err(Into::into)
    }

    async fn get_room_settings(&self, room_id: &RoomId) -> Result<Option<RoomSettings>> {
        self.0.get_room_settings(room_id).await.map_err(Into::into)
    }

    async fn get_custom_value(&self, key: &str) -> Result<Option<Vec<u8>>, Self::Error> {
        self.0.get_custom_value(key).await.map_err(Into::into)
    }

    async fn set_custom_value(&self, key: &str, value: Vec<u8>) -> Result<(), Self::Error> {
        self.0.set_custom_value(key, value).await.map_err(Into::into)
    }

    async fn remove_custom_value(&self, key: &str) -> Result<(), Self::Error> {
        self.0.remove_custom_value(key).await.map_err(Into::into)
    }

    async fn try_take_leased_lock(
        &self,
        lease_duration_ms: u32,
        key: &str,
        holder: &str,
    ) -> Result<bool, Self::Error> {
        self.0.try_take_leased_lock(lease_duration_ms, key, holder).await.map_err(Into::into)
    }

    async fn next_batch_token(&self) -> Result<Option<String>, Self::Error> {
        self.0.next_batch_token().await.map_err(Into::into)
    }
}

/// A type-erased [`CryptoStore`].
pub type DynCryptoStore = dyn CryptoStore<Error = CryptoStoreError>;

/// A type that can be type-erased into `Arc<DynCryptoStore>`.
///
/// This trait is not meant to be implemented directly outside
/// `matrix-sdk-crypto`, but it is automatically implemented for everything that
/// implements `CryptoStore`.
pub trait IntoCryptoStore {
    #[doc(hidden)]
    fn into_crypto_store(self) -> Arc<DynCryptoStore>;
}

impl<T> IntoCryptoStore for T
where
    T: CryptoStore + 'static,
{
    fn into_crypto_store(self) -> Arc<DynCryptoStore> {
        Arc::new(EraseCryptoStoreError(self))
    }
}

// Turns a given `Arc<T>` into `Arc<DynCryptoStore>` by attaching the
// CryptoStore impl vtable of `EraseCryptoStoreError<T>`.
impl<T> IntoCryptoStore for Arc<T>
where
    T: CryptoStore + 'static,
{
    fn into_crypto_store(self) -> Arc<DynCryptoStore> {
        let ptr: *const T = Arc::into_raw(self);
        let ptr_erased = ptr as *const EraseCryptoStoreError<T>;
        // SAFETY: EraseCryptoStoreError is repr(transparent) so T and
        //         EraseCryptoStoreError<T> have the same layout and ABI
        unsafe { Arc::from_raw(ptr_erased) }
    }
}

impl IntoCryptoStore for Arc<DynCryptoStore> {
    fn into_crypto_store(self) -> Arc<DynCryptoStore> {
        self
    }
}
