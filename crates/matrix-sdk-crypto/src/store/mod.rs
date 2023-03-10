// Copyright 2020 The Matrix.org Foundation C.I.C.
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

//! Types and traits to implement the storage layer for the [`OlmMachine`]
//!
//! The storage layer for the [`OlmMachine`] can be customized using a trait.
//! Implementing your own [`CryptoStore`]
//!
//! An in-memory only store is provided as well as a Sled based one, depending
//! on your needs and targets a custom store may be implemented, e.g. for
//! `wasm-unknown-unknown` an indexeddb store would be needed
//!
//! ```
//! # use std::sync::Arc;
//! # use matrix_sdk_crypto::{
//! #     OlmMachine,
//! #     store::MemoryStore,
//! # };
//! # use ruma::{device_id, user_id};
//! # let user_id = user_id!("@example:localhost");
//! # let device_id = device_id!("TEST");
//! let store = Arc::new(MemoryStore::new());
//!
//! let machine = OlmMachine::with_store(user_id, device_id, store);
//! ```
//!
//! [`OlmMachine`]: /matrix_sdk_crypto/struct.OlmMachine.html
//! [`CryptoStore`]: trait.Cryptostore.html

use std::{
    collections::{HashMap, HashSet},
    fmt::Debug,
    ops::Deref,
    sync::{atomic::AtomicBool, Arc},
};

use atomic::Ordering;
use dashmap::DashSet;
use matrix_sdk_common::locks::Mutex;
use ruma::{events::secret::request::SecretName, DeviceId, OwnedDeviceId, OwnedUserId, UserId};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tracing::{info, instrument, trace, warn, Span};
use vodozemac::{megolm::SessionOrdering, Curve25519PublicKey};
use zeroize::Zeroize;

use crate::{
    identities::{
        user::{OwnUserIdentity, UserIdentities, UserIdentity},
        Device, ReadOnlyDevice, ReadOnlyUserIdentities, UserDevices,
    },
    olm::{
        InboundGroupSession, OlmMessageHash, OutboundGroupSession, PrivateCrossSigningIdentity,
        ReadOnlyAccount, Session,
    },
    utilities::encode,
    verification::VerificationMachine,
    CrossSigningStatus,
};

pub mod caches;
mod error;
mod memorystore;
mod traits;

#[cfg(any(test, feature = "testing"))]
#[macro_use]
#[allow(missing_docs)]
pub mod integration_tests;

pub use error::{CryptoStoreError, Result};
pub use memorystore::MemoryStore;
pub use traits::{CryptoStore, DynCryptoStore, IntoCryptoStore};

pub use crate::gossiping::{GossipRequest, SecretInfo};

/// A wrapper for our CryptoStore trait object.
///
/// This is needed because we want to have a generic interface so we can
/// store/restore objects that we can serialize. Since trait objects and
/// generics don't mix let the CryptoStore store strings and this wrapper
/// adds the generic interface on top.
#[derive(Debug, Clone)]
pub(crate) struct Store {
    user_id: Arc<UserId>,
    identity: Arc<Mutex<PrivateCrossSigningIdentity>>,
    inner: Arc<DynCryptoStore>,
    verification_machine: VerificationMachine,
    tracked_users_cache: Arc<DashSet<OwnedUserId>>,
    users_for_key_query: Arc<Mutex<UsersForKeyQuery>>,
    tracked_user_loading_lock: Arc<Mutex<()>>,
    tracked_users_loaded: Arc<AtomicBool>,
}

/// Record of the users that are waiting for a /keys/query.
///
/// To avoid races, we maintain a sequence number which is updated each time we
/// receive an invalidation notification. We also record the sequence number at
/// which each user was last invalidated. Then, we attach the current sequence
/// number to each `/keys/query` request, and when we get the response we can
/// tell if any users have been invalidated more recently than that request.
#[derive(Debug)]
struct UsersForKeyQuery {
    /// The sequence number we will assign to the next addition to user_map
    next_sequence_number: InvalidationSequenceNumber,

    /// The users pending a lookup, together with the sequence number at which
    /// they were added to the list
    user_map: HashMap<OwnedUserId, InvalidationSequenceNumber>,
}

// We use wrapping arithmetic for the sequence numbers, to make sure we never
// run out of numbers. (2**64 should be enough for anyone, but it's easy enough
// just to make it wrap.)
//
// We use a *signed* counter so that we can compare values via a subtraction.
// For example, suppose we've just overflowed from i64::MAX to i64::MIN.
// (i64::MAX.wrapping_sub(i64::MIN)) is -1, which tells us that i64::MAX comes
// before i64::MIN in the sequence.
type InvalidationSequenceNumber = i64;

impl UsersForKeyQuery {
    /// Create a new, empty, `UsersForKeyQueryCache`
    fn new() -> Self {
        UsersForKeyQuery { next_sequence_number: 0, user_map: HashMap::new() }
    }

    /// Record a new user that requires a key query
    fn insert_user(&mut self, user: &UserId) {
        let seq = self.next_sequence_number;
        trace!(?user, sequence_number = seq, "Flagging user for key query");
        self.user_map.insert(user.to_owned(), seq);
        self.next_sequence_number = self.next_sequence_number.wrapping_add(1);
    }

    /// Record that a user has received an update with the given sequence
    /// number.
    ///
    /// If the sequence number is newer than the oldest invalidation for this
    /// user, it is removed from the list of those needing an update.
    ///
    /// Returns true if the user is now up-to-date, else false
    #[instrument(level = "trace", skip(self), fields(invalidation_sequence))]
    fn maybe_remove_user(
        &mut self,
        user: &UserId,
        query_sequence: InvalidationSequenceNumber,
    ) -> bool {
        let last_invalidation = self.user_map.get(user);

        if let Some(invalidation_sequence) = last_invalidation {
            Span::current().record("invalidation_sequence", invalidation_sequence);
            if invalidation_sequence.wrapping_sub(query_sequence) > 0 {
                trace!("User invalidated since this query started: still not up-to-date");
                false
            } else {
                trace!("User now up-to-date");
                self.user_map.remove(user);
                true
            }
        } else {
            trace!("User already up-to-date, nothing to do");
            true
        }
    }

    /// Fetch the list of users waiting for a key query, and the current
    /// sequence number
    fn users_for_key_query(&self) -> (HashSet<OwnedUserId>, InvalidationSequenceNumber) {
        // we return the sequence number of the last invalidation
        let sequence_number = self.next_sequence_number.wrapping_sub(1);
        (self.user_map.keys().cloned().collect(), sequence_number)
    }
}

#[derive(Default, Debug)]
#[allow(missing_docs)]
pub struct Changes {
    pub account: Option<ReadOnlyAccount>,
    pub private_identity: Option<PrivateCrossSigningIdentity>,
    pub backup_version: Option<String>,
    pub recovery_key: Option<RecoveryKey>,
    pub sessions: Vec<Session>,
    pub message_hashes: Vec<OlmMessageHash>,
    pub inbound_group_sessions: Vec<InboundGroupSession>,
    pub outbound_group_sessions: Vec<OutboundGroupSession>,
    pub key_requests: Vec<GossipRequest>,
    pub identities: IdentityChanges,
    pub devices: DeviceChanges,
}

/// A user for which we are tracking the list of devices.
#[derive(Debug, Serialize, Deserialize)]
pub struct TrackedUser {
    /// The user ID of the user.
    pub user_id: OwnedUserId,
    /// The outdate/dirty flag of the user, remembers if the list of devices for
    /// the user is considered to be out of date. If the list of devices is
    /// out of date, a `/keys/query` request should be sent out for this
    /// user.
    pub dirty: bool,
}

impl Changes {
    /// Are there any changes stored or is this an empty `Changes` struct
    pub fn is_empty(&self) -> bool {
        self.account.is_none()
            && self.private_identity.is_none()
            && self.sessions.is_empty()
            && self.message_hashes.is_empty()
            && self.inbound_group_sessions.is_empty()
            && self.outbound_group_sessions.is_empty()
            && self.key_requests.is_empty()
            && self.identities.is_empty()
            && self.devices.is_empty()
    }
}

#[derive(Debug, Clone, Default)]
#[allow(missing_docs)]
pub struct IdentityChanges {
    pub new: Vec<ReadOnlyUserIdentities>,
    pub changed: Vec<ReadOnlyUserIdentities>,
}

impl IdentityChanges {
    fn is_empty(&self) -> bool {
        self.new.is_empty() && self.changed.is_empty()
    }
}

#[derive(Debug, Clone, Default)]
#[allow(missing_docs)]
pub struct DeviceChanges {
    pub new: Vec<ReadOnlyDevice>,
    pub changed: Vec<ReadOnlyDevice>,
    pub deleted: Vec<ReadOnlyDevice>,
}

/// The private part of a backup key.
#[derive(Zeroize, Deserialize, Serialize)]
#[zeroize(drop)]
#[serde(transparent)]
pub struct RecoveryKey {
    pub(crate) inner: Box<[u8; RecoveryKey::KEY_SIZE]>,
}

impl RecoveryKey {
    /// The number of bytes the recovery key will hold.
    pub const KEY_SIZE: usize = 32;

    /// Create a new random recovery key.
    pub fn new() -> Result<Self, rand::Error> {
        let mut rng = rand::thread_rng();

        let mut key = Box::new([0u8; Self::KEY_SIZE]);
        rand::Fill::try_fill(key.as_mut_slice(), &mut rng)?;

        Ok(Self { inner: key })
    }

    /// Export the `RecoveryKey` as a base64 encoded string.
    pub fn to_base64(&self) -> String {
        encode(self.inner.as_slice())
    }
}

#[cfg(not(tarpaulin_include))]
impl Debug for RecoveryKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RecoveryKey").finish()
    }
}

impl DeviceChanges {
    /// Merge the given `DeviceChanges` into this instance of `DeviceChanges`.
    pub fn extend(&mut self, other: DeviceChanges) {
        self.new.extend(other.new);
        self.changed.extend(other.changed);
        self.deleted.extend(other.deleted);
    }

    fn is_empty(&self) -> bool {
        self.new.is_empty() && self.changed.is_empty() && self.deleted.is_empty()
    }
}

/// Struct holding info about how many room keys the store has.
#[derive(Debug, Clone, Default)]
pub struct RoomKeyCounts {
    /// The total number of room keys the store has.
    pub total: usize,
    /// The number of backed up room keys the store has.
    pub backed_up: usize,
}

/// Stored versions of the backup keys.
#[derive(Default, Debug)]
pub struct BackupKeys {
    /// The recovery key, the one used to decrypt backed up room keys.
    pub recovery_key: Option<RecoveryKey>,
    /// The version that we are using for backups.
    pub backup_version: Option<String>,
}

/// A struct containing private cross signing keys that can be backed up or
/// uploaded to the secret store.
#[derive(Zeroize)]
#[zeroize(drop)]
pub struct CrossSigningKeyExport {
    /// The seed of the master key encoded as unpadded base64.
    pub master_key: Option<String>,
    /// The seed of the self signing key encoded as unpadded base64.
    pub self_signing_key: Option<String>,
    /// The seed of the user signing key encoded as unpadded base64.
    pub user_signing_key: Option<String>,
}

#[cfg(not(tarpaulin_include))]
impl Debug for CrossSigningKeyExport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CrossSigningKeyExport")
            .field("master_key", &self.master_key.is_some())
            .field("self_signing_key", &self.self_signing_key.is_some())
            .field("user_signing_key", &self.user_signing_key.is_some())
            .finish_non_exhaustive()
    }
}

/// Error describing what went wrong when importing private cross signing keys
/// or the key backup key.
#[derive(Debug, Error)]
pub enum SecretImportError {
    /// The key that we tried to import was invalid.
    #[error(transparent)]
    Key(#[from] vodozemac::KeyError),
    /// The public key of the imported private key doesn't match to the public
    /// key that was uploaded to the server.
    #[error(
        "The public key of the imported private key doesn't match to the \
            public key that was uploaded to the server"
    )]
    MismatchedPublicKeys,
    /// The new version of the identity couldn't be stored.
    #[error(transparent)]
    Store(#[from] CryptoStoreError),
}

impl Store {
    /// Create a new Store
    pub fn new(
        user_id: Arc<UserId>,
        identity: Arc<Mutex<PrivateCrossSigningIdentity>>,
        store: Arc<DynCryptoStore>,
        verification_machine: VerificationMachine,
    ) -> Self {
        Self {
            user_id,
            identity,
            inner: store,
            verification_machine,
            tracked_users_cache: DashSet::new().into(),
            users_for_key_query: Mutex::new(UsersForKeyQuery::new()).into(),
            tracked_users_loaded: AtomicBool::new(false).into(),
            tracked_user_loading_lock: Mutex::new(()).into(),
        }
    }

    /// UserId associated with this store
    pub fn user_id(&self) -> &UserId {
        &self.user_id
    }

    /// DeviceId associated with this store
    pub fn device_id(&self) -> &DeviceId {
        self.verification_machine.own_device_id()
    }

    /// The Account associated with this store
    pub fn account(&self) -> &ReadOnlyAccount {
        &self.verification_machine.store.account
    }

    #[cfg(test)]
    /// test helper to reset the cross signing identity
    pub async fn reset_cross_signing_identity(&self) {
        self.identity.lock().await.reset().await;
    }

    ///  PrivateCrossSigningIdentity associated with this store
    pub fn private_identity(&self) -> Arc<Mutex<PrivateCrossSigningIdentity>> {
        self.identity.clone()
    }

    /// Save the given Sessions to the store
    pub async fn save_sessions(&self, sessions: &[Session]) -> Result<()> {
        let changes = Changes { sessions: sessions.to_vec(), ..Default::default() };

        self.save_changes(changes).await
    }

    /// Compare the given `InboundGroupSession` with an existing session we have
    /// in the store.
    ///
    /// This method returns `SessionOrdering::Better` if the given session is
    /// better than the one we already have or if we don't have such a
    /// session in the store.
    pub async fn compare_group_session(
        &self,
        session: &InboundGroupSession,
    ) -> Result<SessionOrdering> {
        let old_session =
            self.inner.get_inbound_group_session(session.room_id(), session.session_id()).await?;

        Ok(if let Some(old_session) = old_session {
            session.compare(&old_session).await
        } else {
            SessionOrdering::Better
        })
    }

    #[cfg(test)]
    /// Testing helper to allow to save only a set of devices
    pub async fn save_devices(&self, devices: &[ReadOnlyDevice]) -> Result<()> {
        let changes = Changes {
            devices: DeviceChanges { changed: devices.to_vec(), ..Default::default() },
            ..Default::default()
        };

        self.save_changes(changes).await
    }

    #[cfg(test)]
    /// Testing helper to allo to save only a set of InboundGroupSession
    pub async fn save_inbound_group_sessions(
        &self,
        sessions: &[InboundGroupSession],
    ) -> Result<()> {
        let changes = Changes { inbound_group_sessions: sessions.to_vec(), ..Default::default() };

        self.save_changes(changes).await
    }

    /// Get the display name of our own device.
    pub async fn device_display_name(&self) -> Result<Option<String>, CryptoStoreError> {
        Ok(self
            .inner
            .get_device(self.user_id(), self.device_id())
            .await?
            .and_then(|d| d.display_name().map(|d| d.to_owned())))
    }

    /// Get the read-only device associated with `device_id` for `user_id`
    pub async fn get_readonly_device(
        &self,
        user_id: &UserId,
        device_id: &DeviceId,
    ) -> Result<Option<ReadOnlyDevice>> {
        self.inner.get_device(user_id, device_id).await
    }

    /// Get the read-only version of all the devices that the given user has.
    ///
    /// *Note*: This doesn't return our own device.
    pub async fn get_readonly_devices_filtered(
        &self,
        user_id: &UserId,
    ) -> Result<HashMap<OwnedDeviceId, ReadOnlyDevice>> {
        self.inner.get_user_devices(user_id).await.map(|mut d| {
            if user_id == self.user_id() {
                d.remove(self.device_id());
            }
            d
        })
    }

    /// Get the read-only version of all the devices that the given user has.
    ///
    /// *Note*: This does also return our own device.
    pub async fn get_readonly_devices_unfiltered(
        &self,
        user_id: &UserId,
    ) -> Result<HashMap<OwnedDeviceId, ReadOnlyDevice>> {
        self.inner.get_user_devices(user_id).await
    }

    /// Get a device for the given user with the given curve25519 key.
    ///
    /// *Note*: This doesn't return our own device.
    pub async fn get_device_from_curve_key(
        &self,
        user_id: &UserId,
        curve_key: Curve25519PublicKey,
    ) -> Result<Option<Device>> {
        self.get_user_devices(user_id)
            .await
            .map(|d| d.devices().find(|d| d.curve25519_key() == Some(curve_key)))
    }

    /// Get all devices associated with the given `user_id`
    ///
    /// *Note*: This doesn't return our own device.
    pub async fn get_user_devices_filtered(&self, user_id: &UserId) -> Result<UserDevices> {
        self.get_user_devices(user_id).await.map(|mut d| {
            if user_id == self.user_id() {
                d.inner.remove(self.device_id());
            }
            d
        })
    }

    /// Get all devices associated with the given `user_id`
    ///
    /// *Note*: This does also return our own device.
    pub async fn get_user_devices(&self, user_id: &UserId) -> Result<UserDevices> {
        let devices = self.get_readonly_devices_unfiltered(user_id).await?;

        let own_identity =
            self.inner.get_user_identity(&self.user_id).await?.and_then(|i| i.own().cloned());
        let device_owner_identity = self.inner.get_user_identity(user_id).await?;

        Ok(UserDevices {
            inner: devices,
            verification_machine: self.verification_machine.clone(),
            own_identity,
            device_owner_identity,
        })
    }

    /// Get a Device copy associated with `device_id` for `user_id`
    pub async fn get_device(
        &self,
        user_id: &UserId,
        device_id: &DeviceId,
    ) -> Result<Option<Device>> {
        let own_identity =
            self.inner.get_user_identity(&self.user_id).await?.and_then(|i| i.own().cloned());
        let device_owner_identity = self.inner.get_user_identity(user_id).await?;

        Ok(self.inner.get_device(user_id, device_id).await?.map(|d| Device {
            inner: d,
            verification_machine: self.verification_machine.clone(),
            own_identity,
            device_owner_identity,
        }))
    }

    ///  Get the Identity of `user_id`
    pub async fn get_identity(&self, user_id: &UserId) -> Result<Option<UserIdentities>> {
        // let own_identity =
        // self.inner.get_user_identity(self.user_id()).await?.and_then(|i| i.own());
        Ok(if let Some(identity) = self.inner.get_user_identity(user_id).await? {
            Some(match identity {
                ReadOnlyUserIdentities::Own(i) => OwnUserIdentity {
                    inner: i,
                    verification_machine: self.verification_machine.clone(),
                }
                .into(),
                ReadOnlyUserIdentities::Other(i) => {
                    let own_identity =
                        self.inner.get_user_identity(self.user_id()).await?.and_then(|i| {
                            if let ReadOnlyUserIdentities::Own(i) = i {
                                Some(i)
                            } else {
                                None
                            }
                        });
                    UserIdentity {
                        inner: i,
                        verification_machine: self.verification_machine.clone(),
                        own_identity,
                    }
                    .into()
                }
            })
        } else {
            None
        })
    }

    /// Try to export the secret with the given secret name.
    ///
    /// The exported secret will be encoded as unpadded base64. Returns `Null`
    /// if the secret can't be found.
    ///
    /// # Arguments
    ///
    /// * `secret_name` - The name of the secret that should be exported.
    pub async fn export_secret(&self, secret_name: &SecretName) -> Option<String> {
        match secret_name {
            SecretName::CrossSigningMasterKey
            | SecretName::CrossSigningUserSigningKey
            | SecretName::CrossSigningSelfSigningKey => {
                self.identity.lock().await.export_secret(secret_name).await
            }
            SecretName::RecoveryKey => {
                #[cfg(feature = "backups_v1")]
                if let Some(key) = self.load_backup_keys().await.unwrap().recovery_key {
                    let exported = key.to_base64();
                    Some(exported)
                } else {
                    None
                }

                #[cfg(not(feature = "backups_v1"))]
                None
            }
            name => {
                warn!(secret = ?name, "Unknown secret was requested");
                None
            }
        }
    }

    /// Import the Cross Signing Keys
    pub async fn import_cross_signing_keys(
        &self,
        export: CrossSigningKeyExport,
    ) -> Result<CrossSigningStatus, SecretImportError> {
        if let Some(public_identity) = self.get_identity(&self.user_id).await?.and_then(|i| i.own())
        {
            let identity = self.identity.lock().await;

            identity
                .import_secrets(
                    public_identity,
                    export.master_key.as_deref(),
                    export.self_signing_key.as_deref(),
                    export.user_signing_key.as_deref(),
                )
                .await?;

            let status = identity.status().await;
            info!(?status, "Successfully imported the private cross signing keys");

            let changes =
                Changes { private_identity: Some(identity.clone()), ..Default::default() };

            self.save_changes(changes).await?;
        }

        Ok(self.identity.lock().await.status().await)
    }

    /// Import the given `secret` named `secret_name` into the keystore.
    pub async fn import_secret(
        &self,
        secret_name: &SecretName,
        secret: &str,
    ) -> Result<(), SecretImportError> {
        match secret_name {
            SecretName::CrossSigningMasterKey
            | SecretName::CrossSigningUserSigningKey
            | SecretName::CrossSigningSelfSigningKey => {
                if let Some(public_identity) =
                    self.get_identity(&self.user_id).await?.and_then(|i| i.own())
                {
                    let identity = self.identity.lock().await;

                    identity.import_secret(public_identity, secret_name, secret).await?;
                    info!(
                        secret_name = secret_name.as_ref(),
                        "Successfully imported a private cross signing key"
                    );

                    let changes =
                        Changes { private_identity: Some(identity.clone()), ..Default::default() };

                    self.save_changes(changes).await?;
                }
            }
            SecretName::RecoveryKey => {
                // We don't import the recovery key here since we'll want to
                // check if the public key matches to the latest version on the
                // server. We instead leave the key in the event and let the
                // user import it later.
            }
            name => {
                warn!(secret = ?name, "Tried to import an unknown secret");
            }
        }

        Ok(())
    }

    /// Mark the given user as being tracked for device lists, and mark that it
    /// has an outdated device list.
    ///
    /// This means that the user will be considered for a `/keys/query` request
    /// next time [`Store::users_for_key_query()`] is called.
    pub async fn mark_user_as_changed(&self, user: &UserId) -> Result<()> {
        self.users_for_key_query.lock().await.insert_user(user);
        self.tracked_users_cache.insert(user.to_owned());

        self.inner.save_tracked_users(&[(user, true)]).await
    }

    /// Add entries to the list of users being tracked for device changes
    ///
    /// Any users not already on the list are flagged as awaiting a key query.
    /// Users that were already in the list are unaffected.
    pub async fn update_tracked_users(&self, users: impl Iterator<Item = &UserId>) -> Result<()> {
        self.load_tracked_users().await?;

        let mut store_updates = Vec::new();
        let mut key_query_lock = self.users_for_key_query.lock().await;

        for user_id in users {
            if !self.tracked_users_cache.contains(user_id) {
                self.tracked_users_cache.insert(user_id.to_owned());
                key_query_lock.insert_user(user_id);
                store_updates.push((user_id, true))
            }
        }

        self.inner.save_tracked_users(&store_updates).await
    }

    /// Process notifications that users have changed devices.
    ///
    /// This is used to handle the list of device-list updates that is received
    /// from the `/sync` response. Any users *whose device lists we are
    /// tracking* are flagged as needing a key query. Users whose devices we
    /// are not tracking are ignored.
    pub async fn mark_tracked_users_as_changed(
        &self,
        users: impl Iterator<Item = &UserId>,
    ) -> Result<()> {
        self.load_tracked_users().await?;
        let mut store_updates: Vec<(&UserId, bool)> = Vec::new();

        let mut key_query_lock = self.users_for_key_query.lock().await;
        for user_id in users {
            if self.tracked_users_cache.contains(user_id) {
                key_query_lock.insert_user(user_id);
                store_updates.push((user_id, true));
            }
        }
        self.inner.save_tracked_users(&store_updates).await?;
        Ok(())
    }

    /// Flag that the given users devices are now up-to-date.
    ///
    /// This is called after processing the response to a /keys/query request.
    /// Any users whose device lists we are tracking are removed from the
    /// list of those pending a /keys/query.
    pub async fn mark_tracked_users_as_up_to_date(
        &self,
        users: impl Iterator<Item = &UserId>,
        sequence_number: InvalidationSequenceNumber,
    ) -> Result<()> {
        let mut store_updates: Vec<(&UserId, bool)> = Vec::new();
        let mut key_query_lock = self.users_for_key_query.lock().await;
        for user_id in users {
            if self.tracked_users_cache.contains(user_id) {
                let clean = key_query_lock.maybe_remove_user(user_id, sequence_number);
                store_updates.push((user_id, !clean));
            }
        }
        self.inner.save_tracked_users(&store_updates).await?;

        Ok(())
    }

    /// Load the list of users for whom we are tracking their device lists and
    /// fill out our caches.
    ///
    /// This method ensures that we're only going to load the users from the
    /// actual [`CryptoStore`] once, it will also make sure that any
    /// concurrent calls to this method get deduplicated.
    async fn load_tracked_users(&self) -> Result<()> {
        // If the users are loaded do nothing, otherwise acquire a lock.
        if !self.tracked_users_loaded.load(Ordering::SeqCst) {
            let _lock = self.tracked_user_loading_lock.lock().await;

            // Check again if the users have been loaded, in case another call to this
            // method loaded the tracked users between the time we tried to
            // acquire the lock and the time we actually acquired the lock.
            if !self.tracked_users_loaded.load(Ordering::SeqCst) {
                let tracked_users = self.inner.load_tracked_users().await?;

                let mut query_users_lock = self.users_for_key_query.lock().await;
                for user in tracked_users {
                    self.tracked_users_cache.insert(user.user_id.to_owned());

                    if user.dirty {
                        query_users_lock.insert_user(&user.user_id);
                    }
                }

                self.tracked_users_loaded.store(true, Ordering::SeqCst);
            }
        }

        Ok(())
    }

    /// Get the set of users that has the outdate/dirty flag set for their list
    /// of devices.
    ///
    /// This set should be included in a `/keys/query` request which will update
    /// the device list.
    ///
    /// # Returns
    ///
    /// A pair `(users, sequence_number)`, where `users` is the list of users to
    /// be queried, and `sequence_number` is the current sequence number,
    /// which should be returned in `mark_tracked_users_as_up_to_date`.
    pub async fn users_for_key_query(
        &self,
    ) -> Result<(HashSet<OwnedUserId>, InvalidationSequenceNumber)> {
        self.load_tracked_users().await?;

        Ok(self.users_for_key_query.lock().await.users_for_key_query())
    }

    /// See the docs for [`crate::OlmMachine::tracked_users()`].
    pub async fn tracked_users(&self) -> Result<HashSet<OwnedUserId>> {
        self.load_tracked_users().await?;

        Ok(self.tracked_users_cache.iter().map(|u| u.clone()).collect())
    }
}

impl Deref for Store {
    type Target = DynCryptoStore;

    fn deref(&self) -> &Self::Target {
        self.inner.deref()
    }
}
