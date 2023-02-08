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

use std::{collections::HashMap, sync::Arc};

use async_trait::async_trait;
use matrix_sdk_common::{locks::Mutex, AsyncTraitDeps};
use ruma::{DeviceId, OwnedDeviceId, RoomId, TransactionId, UserId};

use super::{BackupKeys, Changes, Result, RoomKeyCounts};
use crate::{
    olm::{
        InboundGroupSession, OlmMessageHash, OutboundGroupSession, PrivateCrossSigningIdentity,
        Session,
    },
    GossipRequest, ReadOnlyAccount, ReadOnlyDevice, ReadOnlyUserIdentities, SecretInfo,
    TrackedUser,
};

/// Represents a store that the `OlmMachine` uses to store E2EE data (such as
/// cryptographic keys).
#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
pub trait CryptoStore: AsyncTraitDeps {
    /// Load an account that was previously stored.
    async fn load_account(&self) -> Result<Option<ReadOnlyAccount>>;

    /// Save the given account in the store.
    ///
    /// # Arguments
    ///
    /// * `account` - The account that should be stored.
    async fn save_account(&self, account: ReadOnlyAccount) -> Result<()>;

    /// Try to load a private cross signing identity, if one is stored.
    async fn load_identity(&self) -> Result<Option<PrivateCrossSigningIdentity>>;

    /// Save the set of changes to the store.
    ///
    /// # Arguments
    ///
    /// * `changes` - The set of changes that should be stored.
    async fn save_changes(&self, changes: Changes) -> Result<()>;

    /// Get all the sessions that belong to the given sender key.
    ///
    /// # Arguments
    ///
    /// * `sender_key` - The sender key that was used to establish the sessions.
    async fn get_sessions(&self, sender_key: &str) -> Result<Option<Arc<Mutex<Vec<Session>>>>>;

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
    ) -> Result<Option<InboundGroupSession>>;

    /// Get all the inbound group sessions we have stored.
    async fn get_inbound_group_sessions(&self) -> Result<Vec<InboundGroupSession>>;

    /// Get the number inbound group sessions we have and how many of them are
    /// backed up.
    async fn inbound_group_session_counts(&self) -> Result<RoomKeyCounts>;

    /// Get all the inbound group sessions we have not backed up yet.
    async fn inbound_group_sessions_for_backup(
        &self,
        limit: usize,
    ) -> Result<Vec<InboundGroupSession>>;

    /// Reset the backup state of all the stored inbound group sessions.
    async fn reset_backup_state(&self) -> Result<()>;

    /// Get the backup keys we have stored.
    async fn load_backup_keys(&self) -> Result<BackupKeys>;

    /// Get the outbound group session we have stored that is used for the
    /// given room.
    async fn get_outbound_group_session(
        &self,
        room_id: &RoomId,
    ) -> Result<Option<OutboundGroupSession>>;

    /// Load the list of users whose devices we are keeping track of.
    async fn load_tracked_users(&self) -> Result<Vec<TrackedUser>>;

    /// Save a list of users and their respective dirty/outdated flags to the
    /// store.
    async fn save_tracked_users(&self, users: &[(&UserId, bool)]) -> Result<()>;

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
    ) -> Result<Option<ReadOnlyDevice>>;

    /// Get all the devices of the given user.
    ///
    /// # Arguments
    ///
    /// * `user_id` - The user for which we should get all the devices.
    async fn get_user_devices(
        &self,
        user_id: &UserId,
    ) -> Result<HashMap<OwnedDeviceId, ReadOnlyDevice>>;

    /// Get the user identity that is attached to the given user id.
    ///
    /// # Arguments
    ///
    /// * `user_id` - The user for which we should get the identity.
    async fn get_user_identity(&self, user_id: &UserId) -> Result<Option<ReadOnlyUserIdentities>>;

    /// Check if a hash for an Olm message stored in the database.
    async fn is_message_known(&self, message_hash: &OlmMessageHash) -> Result<bool>;

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
    ) -> Result<Option<GossipRequest>>;

    /// Get an outgoing key request that we created that matches the given
    /// requested key info.
    ///
    /// # Arguments
    ///
    /// * `key_info` - The key info of an outgoing secret request.
    async fn get_secret_request_by_info(
        &self,
        secret_info: &SecretInfo,
    ) -> Result<Option<GossipRequest>>;

    /// Get all outgoing secret requests that we have in the store.
    async fn get_unsent_secret_requests(&self) -> Result<Vec<GossipRequest>>;

    /// Delete an outgoing key request that we created that matches the given
    /// request id.
    ///
    /// # Arguments
    ///
    /// * `request_id` - The unique request id that identifies this outgoing key
    /// request.
    async fn delete_outgoing_secret_requests(&self, request_id: &TransactionId) -> Result<()>;
}

/// A type that can be type-erased into `Arc<dyn CryptoStore>`.
///
/// This trait is not meant to be implemented directly outside
/// `matrix-sdk-crypto`, but it is automatically implemented for everything that
/// implements `CryptoStore`.
pub trait IntoCryptoStore {
    #[doc(hidden)]
    fn into_crypto_store(self) -> Arc<dyn CryptoStore>;
}

impl<T> IntoCryptoStore for T
where
    T: CryptoStore + 'static,
{
    fn into_crypto_store(self) -> Arc<dyn CryptoStore> {
        Arc::new(self)
    }
}

impl<T> IntoCryptoStore for Arc<T>
where
    T: CryptoStore + 'static,
{
    fn into_crypto_store(self) -> Arc<dyn CryptoStore> {
        self
    }
}

impl IntoCryptoStore for Arc<dyn CryptoStore> {
    fn into_crypto_store(self) -> Arc<dyn CryptoStore> {
        self
    }
}
