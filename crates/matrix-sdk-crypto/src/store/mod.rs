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
//! An in-memory only store is provided as well as a SQLite-based one, depending
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
    collections::{BTreeMap, HashMap, HashSet},
    fmt::Debug,
    future,
    ops::Deref,
    sync::{atomic::AtomicBool, Arc},
    time::Duration,
};

use async_std::sync::{Condvar, Mutex as AsyncStdMutex};
use atomic::Ordering;
use dashmap::DashSet;
use futures_core::Stream;
use futures_util::stream::StreamExt;
use ruma::{
    events::secret::request::SecretName, DeviceId, OwnedDeviceId, OwnedRoomId, OwnedUserId, RoomId,
    UserId,
};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::{broadcast, Mutex};
use tokio_stream::wrappers::{errors::BroadcastStreamRecvError, BroadcastStream};
use tracing::{info, warn};
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
    types::{events::room_key_withheld::RoomKeyWithheldEvent, EventEncryptionAlgorithm},
    utilities::encode,
    verification::VerificationMachine,
    CrossSigningStatus,
};

pub mod caches;
mod error;
pub mod locks;
mod memorystore;
mod traits;

#[cfg(any(test, feature = "testing"))]
#[macro_use]
#[allow(missing_docs)]
pub mod integration_tests;

use caches::{SequenceNumber, UsersForKeyQuery};
pub use error::{CryptoStoreError, Result};
use matrix_sdk_common::timeout::timeout;
pub use memorystore::MemoryStore;
pub use traits::{CryptoStore, DynCryptoStore, IntoCryptoStore};

use self::locks::CryptoStoreLock;
pub use crate::gossiping::{GossipRequest, SecretInfo};

/// A wrapper for our CryptoStore trait object.
///
/// This is needed because we want to have a generic interface so we can
/// store/restore objects that we can serialize. Since trait objects and
/// generics don't mix let the CryptoStore store strings and this wrapper
/// adds the generic interface on top.
#[derive(Debug, Clone)]
pub struct Store {
    inner: Arc<StoreInner>,
}

#[derive(Debug)]
struct StoreInner {
    user_id: OwnedUserId,
    identity: Arc<Mutex<PrivateCrossSigningIdentity>>,
    store: Arc<DynCryptoStore>,
    verification_machine: VerificationMachine,
    tracked_users_cache: DashSet<OwnedUserId>,

    /// Record of the users that are waiting for a /keys/query.
    //
    // This uses an async_std::sync::Mutex rather than a
    // matrix_sdk_common::locks::Mutex because it has to match the Condvar (and tokio lacks a
    // working Condvar implementation)
    users_for_key_query: AsyncStdMutex<UsersForKeyQuery>,

    // condition variable that is notified each time an update is received for a user.
    users_for_key_query_condvar: Condvar,

    tracked_user_loading_lock: Mutex<()>,
    tracked_users_loaded: AtomicBool,

    /// The sender side of a broadcast stream that is notified whenever we get
    /// an update to an inbound group session.
    room_keys_received_sender: broadcast::Sender<Vec<RoomKeyInfo>>,
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
    /// Stores when a `m.room_key.withheld` is received
    pub withheld_session_info: BTreeMap<OwnedRoomId, BTreeMap<String, RoomKeyWithheldEvent>>,
    pub room_settings: HashMap<OwnedRoomId, RoomSettings>,
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

/// Result type telling us if a `/keys/query` response was expected for a given
/// user.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum UserKeyQueryResult {
    WasPending,
    WasNotPending,

    /// A query was pending, but we gave up waiting
    TimeoutExpired,
}

/// Room encryption settings which are modified by state events or user options
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct RoomSettings {
    /// The encryption algorithm that should be used in the room.
    pub algorithm: EventEncryptionAlgorithm,
    /// Should untrusted devices receive the room key, or should they be
    /// excluded from the conversation.
    pub only_allow_trusted_devices: bool,
}

impl Default for RoomSettings {
    fn default() -> Self {
        Self {
            algorithm: EventEncryptionAlgorithm::MegolmV1AesSha2,
            only_allow_trusted_devices: false,
        }
    }
}

/// Information on a room key that has been received or imported.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct RoomKeyInfo {
    /// The [messaging algorithm] that this key is used for. Will be one of the
    /// `m.megolm.*` algorithms.
    ///
    /// [messaging algorithm]: https://spec.matrix.org/v1.6/client-server-api/#messaging-algorithms
    pub algorithm: EventEncryptionAlgorithm,

    /// The room where the key is used.
    pub room_id: OwnedRoomId,

    /// The Curve25519 key of the device which initiated the session originally.
    pub sender_key: Curve25519PublicKey,

    /// The ID of the session that the key is for.
    pub session_id: String,
}

impl From<&InboundGroupSession> for RoomKeyInfo {
    fn from(group_session: &InboundGroupSession) -> Self {
        RoomKeyInfo {
            algorithm: group_session.algorithm().clone(),
            room_id: group_session.room_id().to_owned(),
            sender_key: group_session.sender_key(),
            session_id: group_session.session_id().to_owned(),
        }
    }
}

impl Store {
    /// Create a new Store
    pub(crate) fn new(
        user_id: OwnedUserId,
        identity: Arc<Mutex<PrivateCrossSigningIdentity>>,
        store: Arc<DynCryptoStore>,
        verification_machine: VerificationMachine,
    ) -> Self {
        let (room_keys_received_sender, _) = broadcast::channel(10);
        let inner = Arc::new(StoreInner {
            user_id,
            identity,
            store,
            verification_machine,
            tracked_users_cache: DashSet::new(),
            users_for_key_query: AsyncStdMutex::new(UsersForKeyQuery::new()),
            users_for_key_query_condvar: Condvar::new(),
            tracked_users_loaded: AtomicBool::new(false),
            tracked_user_loading_lock: Mutex::new(()),
            room_keys_received_sender,
        });
        Self { inner }
    }

    /// UserId associated with this store
    pub(crate) fn user_id(&self) -> &UserId {
        &self.inner.user_id
    }

    /// DeviceId associated with this store
    pub(crate) fn device_id(&self) -> &DeviceId {
        self.inner.verification_machine.own_device_id()
    }

    /// The Account associated with this store
    pub(crate) fn account(&self) -> &ReadOnlyAccount {
        &self.inner.verification_machine.store.account
    }

    #[cfg(test)]
    /// test helper to reset the cross signing identity
    pub(crate) async fn reset_cross_signing_identity(&self) {
        self.inner.identity.lock().await.reset().await;
    }

    /// PrivateCrossSigningIdentity associated with this store
    pub(crate) fn private_identity(&self) -> Arc<Mutex<PrivateCrossSigningIdentity>> {
        self.inner.identity.clone()
    }

    /// Save the given Sessions to the store
    pub(crate) async fn save_sessions(&self, sessions: &[Session]) -> Result<()> {
        let changes = Changes { sessions: sessions.to_vec(), ..Default::default() };

        self.save_changes(changes).await
    }

    pub(crate) async fn save_changes(&self, changes: Changes) -> Result<()> {
        let room_key_updates: Vec<_> =
            changes.inbound_group_sessions.iter().map(RoomKeyInfo::from).collect();

        self.inner.store.save_changes(changes).await?;

        if !room_key_updates.is_empty() {
            // Ignore the result. It can only fail if there are no listeners.
            let _ = self.inner.room_keys_received_sender.send(room_key_updates);
        }

        Ok(())
    }

    /// Compare the given `InboundGroupSession` with an existing session we have
    /// in the store.
    ///
    /// This method returns `SessionOrdering::Better` if the given session is
    /// better than the one we already have or if we don't have such a
    /// session in the store.
    pub(crate) async fn compare_group_session(
        &self,
        session: &InboundGroupSession,
    ) -> Result<SessionOrdering> {
        let old_session = self
            .inner
            .store
            .get_inbound_group_session(session.room_id(), session.session_id())
            .await?;

        Ok(if let Some(old_session) = old_session {
            session.compare(&old_session).await
        } else {
            SessionOrdering::Better
        })
    }

    #[cfg(test)]
    /// Testing helper to allow to save only a set of devices
    pub(crate) async fn save_devices(&self, devices: &[ReadOnlyDevice]) -> Result<()> {
        let changes = Changes {
            devices: DeviceChanges { changed: devices.to_vec(), ..Default::default() },
            ..Default::default()
        };

        self.save_changes(changes).await
    }

    #[cfg(test)]
    /// Testing helper to allow to save only a set of InboundGroupSession
    pub(crate) async fn save_inbound_group_sessions(
        &self,
        sessions: &[InboundGroupSession],
    ) -> Result<()> {
        let changes = Changes { inbound_group_sessions: sessions.to_vec(), ..Default::default() };

        self.save_changes(changes).await
    }

    /// Get the display name of our own device.
    pub(crate) async fn device_display_name(&self) -> Result<Option<String>, CryptoStoreError> {
        Ok(self
            .inner
            .store
            .get_device(self.user_id(), self.device_id())
            .await?
            .and_then(|d| d.display_name().map(|d| d.to_owned())))
    }

    /// Get the read-only device associated with `device_id` for `user_id`
    pub(crate) async fn get_readonly_device(
        &self,
        user_id: &UserId,
        device_id: &DeviceId,
    ) -> Result<Option<ReadOnlyDevice>> {
        self.inner.store.get_device(user_id, device_id).await
    }

    /// Get the read-only version of all the devices that the given user has.
    ///
    /// *Note*: This doesn't return our own device.
    pub(crate) async fn get_readonly_devices_filtered(
        &self,
        user_id: &UserId,
    ) -> Result<HashMap<OwnedDeviceId, ReadOnlyDevice>> {
        self.inner.store.get_user_devices(user_id).await.map(|mut d| {
            if user_id == self.user_id() {
                d.remove(self.device_id());
            }
            d
        })
    }

    /// Get the read-only version of all the devices that the given user has.
    ///
    /// *Note*: This does also return our own device.
    pub(crate) async fn get_readonly_devices_unfiltered(
        &self,
        user_id: &UserId,
    ) -> Result<HashMap<OwnedDeviceId, ReadOnlyDevice>> {
        self.inner.store.get_user_devices(user_id).await
    }

    /// Get a device for the given user with the given curve25519 key.
    ///
    /// *Note*: This doesn't return our own device.
    pub(crate) async fn get_device_from_curve_key(
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
    pub(crate) async fn get_user_devices_filtered(&self, user_id: &UserId) -> Result<UserDevices> {
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
    pub(crate) async fn get_user_devices(&self, user_id: &UserId) -> Result<UserDevices> {
        let devices = self.get_readonly_devices_unfiltered(user_id).await?;

        let own_identity = self
            .inner
            .store
            .get_user_identity(self.user_id())
            .await?
            .and_then(|i| i.own().cloned());
        let device_owner_identity = self.inner.store.get_user_identity(user_id).await?;

        Ok(UserDevices {
            inner: devices,
            verification_machine: self.inner.verification_machine.clone(),
            own_identity,
            device_owner_identity,
        })
    }

    /// Get a Device copy associated with `device_id` for `user_id`
    pub(crate) async fn get_device(
        &self,
        user_id: &UserId,
        device_id: &DeviceId,
    ) -> Result<Option<Device>> {
        let own_identity = self
            .inner
            .store
            .get_user_identity(self.user_id())
            .await?
            .and_then(|i| i.own().cloned());
        let device_owner_identity = self.inner.store.get_user_identity(user_id).await?;

        Ok(self.inner.store.get_device(user_id, device_id).await?.map(|d| Device {
            inner: d,
            verification_machine: self.inner.verification_machine.clone(),
            own_identity,
            device_owner_identity,
        }))
    }

    ///  Get the Identity of `user_id`
    pub(crate) async fn get_identity(&self, user_id: &UserId) -> Result<Option<UserIdentities>> {
        // let own_identity =
        // self.inner.get_user_identity(self.user_id()).await?.and_then(|i| i.own());
        Ok(if let Some(identity) = self.inner.store.get_user_identity(user_id).await? {
            Some(match identity {
                ReadOnlyUserIdentities::Own(i) => OwnUserIdentity {
                    inner: i,
                    verification_machine: self.inner.verification_machine.clone(),
                }
                .into(),
                ReadOnlyUserIdentities::Other(i) => {
                    let own_identity =
                        self.inner.store.get_user_identity(self.user_id()).await?.and_then(|i| {
                            if let ReadOnlyUserIdentities::Own(i) = i {
                                Some(i)
                            } else {
                                None
                            }
                        });
                    UserIdentity {
                        inner: i,
                        verification_machine: self.inner.verification_machine.clone(),
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
    pub(crate) async fn export_secret(&self, secret_name: &SecretName) -> Option<String> {
        match secret_name {
            SecretName::CrossSigningMasterKey
            | SecretName::CrossSigningUserSigningKey
            | SecretName::CrossSigningSelfSigningKey => {
                self.inner.identity.lock().await.export_secret(secret_name).await
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
    pub(crate) async fn import_cross_signing_keys(
        &self,
        export: CrossSigningKeyExport,
    ) -> Result<CrossSigningStatus, SecretImportError> {
        if let Some(public_identity) =
            self.get_identity(self.user_id()).await?.and_then(|i| i.own())
        {
            let identity = self.inner.identity.lock().await;

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

        Ok(self.inner.identity.lock().await.status().await)
    }

    /// Import the given `secret` named `secret_name` into the keystore.
    pub(crate) async fn import_secret(
        &self,
        secret_name: &SecretName,
        secret: &str,
    ) -> Result<(), SecretImportError> {
        match secret_name {
            SecretName::CrossSigningMasterKey
            | SecretName::CrossSigningUserSigningKey
            | SecretName::CrossSigningSelfSigningKey => {
                if let Some(public_identity) =
                    self.get_identity(self.user_id()).await?.and_then(|i| i.own())
                {
                    let identity = self.inner.identity.lock().await;

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
    pub(crate) async fn mark_user_as_changed(&self, user: &UserId) -> Result<()> {
        self.inner.users_for_key_query.lock().await.insert_user(user);
        self.inner.tracked_users_cache.insert(user.to_owned());

        self.inner.store.save_tracked_users(&[(user, true)]).await
    }

    /// Add entries to the list of users being tracked for device changes
    ///
    /// Any users not already on the list are flagged as awaiting a key query.
    /// Users that were already in the list are unaffected.
    pub(crate) async fn update_tracked_users(
        &self,
        users: impl Iterator<Item = &UserId>,
    ) -> Result<()> {
        self.load_tracked_users().await?;

        let mut store_updates = Vec::new();
        let mut key_query_lock = self.inner.users_for_key_query.lock().await;

        for user_id in users {
            if !self.inner.tracked_users_cache.contains(user_id) {
                self.inner.tracked_users_cache.insert(user_id.to_owned());
                key_query_lock.insert_user(user_id);
                store_updates.push((user_id, true))
            }
        }

        self.inner.store.save_tracked_users(&store_updates).await
    }

    /// Process notifications that users have changed devices.
    ///
    /// This is used to handle the list of device-list updates that is received
    /// from the `/sync` response. Any users *whose device lists we are
    /// tracking* are flagged as needing a key query. Users whose devices we
    /// are not tracking are ignored.
    pub(crate) async fn mark_tracked_users_as_changed(
        &self,
        users: impl Iterator<Item = &UserId>,
    ) -> Result<()> {
        self.load_tracked_users().await?;

        let mut store_updates: Vec<(&UserId, bool)> = Vec::new();
        let mut key_query_lock = self.inner.users_for_key_query.lock().await;

        for user_id in users {
            if self.inner.tracked_users_cache.contains(user_id) {
                key_query_lock.insert_user(user_id);
                store_updates.push((user_id, true));
            }
        }

        self.inner.store.save_tracked_users(&store_updates).await
    }

    /// Flag that the given users devices are now up-to-date.
    ///
    /// This is called after processing the response to a /keys/query request.
    /// Any users whose device lists we are tracking are removed from the
    /// list of those pending a /keys/query.
    pub(crate) async fn mark_tracked_users_as_up_to_date(
        &self,
        users: impl Iterator<Item = &UserId>,
        sequence_number: SequenceNumber,
    ) -> Result<()> {
        let mut store_updates: Vec<(&UserId, bool)> = Vec::new();
        let mut key_query_lock = self.inner.users_for_key_query.lock().await;

        for user_id in users {
            if self.inner.tracked_users_cache.contains(user_id) {
                let clean = key_query_lock.maybe_remove_user(user_id, sequence_number);
                store_updates.push((user_id, !clean));
            }
        }
        self.inner.store.save_tracked_users(&store_updates).await?;
        // wake up any tasks that may have been waiting for updates
        self.inner.users_for_key_query_condvar.notify_all();

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
        if !self.inner.tracked_users_loaded.load(Ordering::SeqCst) {
            let _lock = self.inner.tracked_user_loading_lock.lock().await;

            // Check again if the users have been loaded, in case another call to this
            // method loaded the tracked users between the time we tried to
            // acquire the lock and the time we actually acquired the lock.
            if !self.inner.tracked_users_loaded.load(Ordering::SeqCst) {
                let tracked_users = self.inner.store.load_tracked_users().await?;

                let mut query_users_lock = self.inner.users_for_key_query.lock().await;
                for user in tracked_users {
                    self.inner.tracked_users_cache.insert(user.user_id.to_owned());

                    if user.dirty {
                        query_users_lock.insert_user(&user.user_id);
                    }
                }

                self.inner.tracked_users_loaded.store(true, Ordering::SeqCst);
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
    pub(crate) async fn users_for_key_query(
        &self,
    ) -> Result<(HashSet<OwnedUserId>, SequenceNumber)> {
        self.load_tracked_users().await?;

        Ok(self.inner.users_for_key_query.lock().await.users_for_key_query())
    }

    /// Wait for a `/keys/query` response to be received if one is expected for
    /// the given user.
    ///
    /// If the given timeout elapses, the method will stop waiting and return
    /// `UserKeyQueryResult::TimeoutExpired`
    pub(crate) async fn wait_if_user_key_query_pending(
        &self,
        timeout_duration: Duration,
        user: &UserId,
    ) -> UserKeyQueryResult {
        let mut g = self.inner.users_for_key_query.lock().await;

        let Some(w) = g.maybe_register_waiting_task(user) else {
            return UserKeyQueryResult::WasNotPending;
        };

        let f1 = async {
            while !w.completed.load(Ordering::Relaxed) {
                g = self.inner.users_for_key_query_condvar.wait(g).await;
            }
        };

        match timeout(Box::pin(f1), timeout_duration).await {
            Err(_) => {
                warn!(
                    user_id = ?user,
                    "The user has a pending `/key/query` request which did \
                    not finish yet, some devices might be missing."
                );

                UserKeyQueryResult::TimeoutExpired
            }
            _ => UserKeyQueryResult::WasPending,
        }
    }

    /// See the docs for [`crate::OlmMachine::tracked_users()`].
    pub(crate) async fn tracked_users(&self) -> Result<HashSet<OwnedUserId>> {
        self.load_tracked_users().await?;

        Ok(self.inner.tracked_users_cache.iter().map(|u| u.clone()).collect())
    }

    /// Check whether there is a global flag to only encrypt messages for
    /// trusted devices or for everyone.
    pub async fn get_only_allow_trusted_devices(&self) -> Result<bool> {
        let value = self.get_value("only_allow_trusted_devices").await?.unwrap_or_default();
        Ok(value)
    }

    /// Set global flag whether to encrypt messages for untrusted devices, or
    /// whether they should be excluded from the conversation.
    pub async fn set_only_allow_trusted_devices(
        &self,
        block_untrusted_devices: bool,
    ) -> Result<()> {
        self.set_value("only_allow_trusted_devices", &block_untrusted_devices).await
    }

    /// Get custom stored value associated with a key
    pub async fn get_value<T: DeserializeOwned>(&self, key: &str) -> Result<Option<T>> {
        let Some(value) = self.get_custom_value(key).await? else {
            return Ok(None);
        };
        let deserialized = self.deserialize_value(&value)?;
        Ok(Some(deserialized))
    }

    /// Store custom value associated with a key
    pub async fn set_value(&self, key: &str, value: &impl Serialize) -> Result<()> {
        let serialized = self.serialize_value(value)?;
        self.set_custom_value(key, serialized).await?;
        Ok(())
    }

    fn serialize_value(&self, value: &impl Serialize) -> Result<Vec<u8>> {
        let serialized =
            rmp_serde::to_vec_named(value).map_err(|x| CryptoStoreError::Backend(x.into()))?;
        Ok(serialized)
    }

    fn deserialize_value<T: DeserializeOwned>(&self, value: &[u8]) -> Result<T> {
        let deserialized =
            rmp_serde::from_slice(value).map_err(|e| CryptoStoreError::Backend(e.into()))?;
        Ok(deserialized)
    }

    /// Receive notifications of room keys being received as a [`Stream`].
    ///
    /// Each time a room key is updated in any way, an update will be sent to
    /// the stream. Updates that happen at the same time are batched into a
    /// [`Vec`].
    ///
    /// If the reader of the stream lags too far behind, a warning will be
    /// logged and items will be dropped.
    pub fn room_keys_received_stream(&self) -> impl Stream<Item = Vec<RoomKeyInfo>> {
        let stream = BroadcastStream::new(self.inner.room_keys_received_sender.subscribe());

        // the raw BroadcastStream gives us Results which can fail with
        // BroadcastStreamRecvError if the reader falls behind. That's annoying to work
        // with, so here we just drop the errors.
        stream.filter_map(|result| async move {
            match result {
                Ok(r) => Some(r),
                Err(BroadcastStreamRecvError::Lagged(lag)) => {
                    warn!("room_keys_received_stream missed {lag} updates");
                    None
                }
            }
        })
    }

    /// Receive notifications of a room keys for a specific room being received.
    ///
    /// Same as [`room_keys_received_stream`][Self::room_keys_received_stream],
    /// but filtered by the given room ID.
    pub fn room_keys_for_room_received_stream(
        &self,
        room_id: &RoomId,
    ) -> impl Stream<Item = Vec<RoomKeyInfo>> {
        let room_id = room_id.to_owned();
        self.room_keys_received_stream()
            .map(move |mut vec| {
                vec.retain(|info| info.room_id == room_id);
                vec
            })
            .filter(|vec| future::ready(!vec.is_empty()))
    }

    /// Creates a `CryptoStoreLock` for this store, that will contain the given
    /// key and value when hold.
    pub fn create_store_lock(&self, lock_key: String, lock_value: String) -> CryptoStoreLock {
        CryptoStoreLock::new(self.inner.store.clone(), lock_key, lock_value)
    }
}

impl Deref for Store {
    type Target = DynCryptoStore;

    fn deref(&self) -> &Self::Target {
        self.inner.store.deref()
    }
}
