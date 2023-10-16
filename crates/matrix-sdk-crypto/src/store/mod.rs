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
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    fmt::Debug,
    ops::Deref,
    sync::{atomic::Ordering, Arc, RwLock as StdRwLock},
    time::Duration,
};

use as_variant::as_variant;
use async_std::sync::{Condvar, Mutex as AsyncStdMutex};
use futures_core::Stream;
use futures_util::StreamExt;
use ruma::{
    events::secret::request::SecretName, DeviceId, OwnedDeviceId, OwnedRoomId, OwnedUserId, UserId,
};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::{Mutex, MutexGuard, OwnedRwLockReadGuard, OwnedRwLockWriteGuard, RwLock};
use tracing::{info, warn};
use vodozemac::{base64_encode, megolm::SessionOrdering, Curve25519PublicKey};
use zeroize::Zeroize;

use crate::{
    gossiping::GossippedSecret,
    identities::{
        user::UserIdentities, Device, ReadOnlyDevice, ReadOnlyUserIdentities, UserDevices,
    },
    olm::{
        Account, InboundGroupSession, OlmMessageHash, OutboundGroupSession,
        PrivateCrossSigningIdentity, Session, StaticAccountData,
    },
    types::{events::room_key_withheld::RoomKeyWithheldEvent, EventEncryptionAlgorithm},
    verification::VerificationMachine,
    CrossSigningStatus, ReadOnlyOwnUserIdentity,
};

pub mod caches;
mod crypto_store_wrapper;
mod error;
mod memorystore;
mod traits;

#[cfg(any(test, feature = "testing"))]
#[macro_use]
#[allow(missing_docs)]
pub mod integration_tests;

use caches::{SequenceNumber, UsersForKeyQuery};
pub(crate) use crypto_store_wrapper::CryptoStoreWrapper;
pub use error::{CryptoStoreError, Result};
use matrix_sdk_common::{store_locks::CrossProcessStoreLock, timeout::timeout};
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
pub struct Store {
    inner: Arc<StoreInner>,
}

#[derive(Debug)]
pub(crate) struct StoreCache {
    store: Arc<CryptoStoreWrapper>,

    tracked_users: StdRwLock<BTreeSet<OwnedUserId>>,
    tracked_user_loading_lock: RwLock<bool>,
    account: Mutex<Option<Account>>,
}

impl StoreCache {
    /// Returns a reference to the `Account.`
    ///
    /// Either load the account from the cache, or the store, if missing from
    /// the cache.
    ///
    /// Note there should always be an account stored at least in the store, so
    /// this doesn't return an `Option`.
    pub async fn account(&self) -> Result<impl Deref<Target = Account> + '_> {
        let mut guard = self.account.lock().await;
        if guard.is_some() {
            Ok(MutexGuard::map(guard, |acc| acc.as_mut().unwrap()))
        } else {
            match self.store.load_account().await? {
                Some(account) => {
                    *guard = Some(account);
                    Ok(MutexGuard::map(guard, |acc| acc.as_mut().unwrap()))
                }
                None => Err(CryptoStoreError::AccountUnset),
            }
        }
    }
}

impl StoreCache {
    /// Load the list of users for whom we are tracking their device lists and
    /// fill out our caches.
    ///
    /// This method ensures that we're only going to load the users from the
    /// actual [`CryptoStore`] once, it will also make sure that any
    /// concurrent calls to this method get deduplicated.
    async fn ensure_sync_tracked_users(&self, store: &Store) -> Result<()> {
        // Check if the users are loaded, and in that case do nothing.
        let loaded = self.tracked_user_loading_lock.read().await;
        if *loaded {
            return Ok(());
        }

        // Otherwise, we may load the users.
        drop(loaded);
        let mut loaded = self.tracked_user_loading_lock.write().await;

        // Check again if the users have been loaded, in case another call to this
        // method loaded the tracked users between the time we tried to
        // acquire the lock and the time we actually acquired the lock.
        if *loaded {
            return Ok(());
        }

        let tracked_users = store.inner.store.load_tracked_users().await?;

        let mut query_users_lock = store.inner.users_for_key_query.lock().await;
        let mut tracked_users_cache = self.tracked_users.write().unwrap();
        for user in tracked_users {
            tracked_users_cache.insert(user.user_id.to_owned());

            if user.dirty {
                query_users_lock.insert_user(&user.user_id);
            }
        }

        *loaded = true;

        Ok(())
    }

    /// Process notifications that users have changed devices.
    ///
    /// This is used to handle the list of device-list updates that is received
    /// from the `/sync` response. Any users *whose device lists we are
    /// tracking* are flagged as needing a key query. Users whose devices we
    /// are not tracking are ignored.
    pub(crate) async fn mark_tracked_users_as_changed(
        &self,
        store: &Store,
        users: impl Iterator<Item = &UserId>,
    ) -> Result<()> {
        let mut store_updates: Vec<(&UserId, bool)> = Vec::new();
        let mut key_query_lock = store.inner.users_for_key_query.lock().await;

        {
            let tracked_users = &self.tracked_users.read().unwrap();
            for user_id in users {
                if tracked_users.contains(user_id) {
                    key_query_lock.insert_user(user_id);
                    store_updates.push((user_id, true));
                }
            }
        }

        store.inner.store.save_tracked_users(&store_updates).await
    }

    /// Mark the given user as being tracked for device lists, and mark that it
    /// has an outdated device list.
    ///
    /// This means that the user will be considered for a `/keys/query` request
    /// next time [`Store::users_for_key_query()`] is called.
    pub(crate) async fn mark_user_as_changed(&self, store: &Store, user: &UserId) -> Result<()> {
        store.inner.users_for_key_query.lock().await.insert_user(user);
        self.tracked_users.write().unwrap().insert(user.to_owned());

        store.inner.store.save_tracked_users(&[(user, true)]).await
    }
}

pub(crate) struct StoreCacheGuard {
    cache: OwnedRwLockReadGuard<StoreCache>,
    // TODO: (bnjbvr, #2624) add cross-process lock guard here.
}

impl Deref for StoreCacheGuard {
    type Target = StoreCache;

    fn deref(&self) -> &Self::Target {
        &self.cache
    }
}

/// A temporary transaction (that implies a write) to the underlying store.
#[allow(missing_debug_implementations)]
pub struct StoreTransaction {
    store: Store,
    changes: PendingChanges,
    // TODO hold onto the cross-process crypto store lock + cache.
    cache: OwnedRwLockWriteGuard<StoreCache>,
}

impl StoreTransaction {
    /// Starts a new `StoreTransaction`.
    pub async fn new(store: Store) -> Result<Self> {
        let cache = store.inner.cache.clone();

        Ok(Self {
            store,
            changes: PendingChanges::default(),
            cache: cache.clone().write_owned().await,
        })
    }

    pub(crate) fn cache(&self) -> &StoreCache {
        &self.cache
    }

    /// Returns a reference to the current `Store`.
    pub fn store(&self) -> &Store {
        &self.store
    }

    /// Gets a `Account` for update.
    pub async fn account(&mut self) -> Result<&mut Account> {
        if self.changes.account.is_none() {
            // Make sure the cache loaded the account.
            let _ = self.cache.account().await?;
            self.changes.account = self.cache.account.lock().await.take();
        }
        Ok(self.changes.account.as_mut().unwrap())
    }

    /// Commits all dirty fields to the store, and maintains the cache so it
    /// reflects the current state of the database.
    pub async fn commit(self) -> Result<()> {
        // Save changes in the database.
        self.store.save_pending_changes(self.changes).await?;

        // Make the cache coherent with the database.
        // for changes.account: nothing to do, it's the same underlying shared account.

        Ok(())
    }
}

#[derive(Debug)]
struct StoreInner {
    identity: Arc<Mutex<PrivateCrossSigningIdentity>>,
    store: Arc<CryptoStoreWrapper>,

    /// In-memory cache for the current crypto store.
    ///
    /// ⚠ Must remain private.
    cache: Arc<RwLock<StoreCache>>,

    verification_machine: VerificationMachine,

    /// Record of the users that are waiting for a /keys/query.
    //
    // This uses an async_std::sync::Mutex rather than a
    // matrix_sdk_common::locks::Mutex because it has to match the Condvar (and tokio lacks a
    // working Condvar implementation)
    users_for_key_query: AsyncStdMutex<UsersForKeyQuery>,

    // condition variable that is notified each time an update is received for a user.
    users_for_key_query_condvar: Condvar,

    /// Static account data that never changes (and thus can be loaded once and
    /// for all when creating the store).
    static_account: StaticAccountData,
}

/// Aggregated changes to be saved in the database.
///
/// This is an update version of `Changes` that will replace it as #2624
/// progresses.
// If you ever add a field here, make sure to update `Changes::is_empty` too.
#[derive(Default, Debug)]
#[allow(missing_docs)]
pub struct PendingChanges {
    pub account: Option<Account>,
}

impl PendingChanges {
    /// Are there any changes stored or is this an empty `Changes` struct?
    pub fn is_empty(&self) -> bool {
        self.account.is_none()
    }
}

/// Aggregated changes to be saved in the database.
// If you ever add a field here, make sure to update `Changes::is_empty` too.
#[derive(Default, Debug)]
#[allow(missing_docs)]
pub struct Changes {
    pub private_identity: Option<PrivateCrossSigningIdentity>,
    pub backup_version: Option<String>,
    pub backup_decryption_key: Option<BackupDecryptionKey>,
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
    pub secrets: Vec<GossippedSecret>,
    pub next_batch_token: Option<String>,
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
    /// Are there any changes stored or is this an empty `Changes` struct?
    pub fn is_empty(&self) -> bool {
        self.private_identity.is_none()
            && self.backup_version.is_none()
            && self.backup_decryption_key.is_none()
            && self.sessions.is_empty()
            && self.message_hashes.is_empty()
            && self.inbound_group_sessions.is_empty()
            && self.outbound_group_sessions.is_empty()
            && self.key_requests.is_empty()
            && self.identities.is_empty()
            && self.devices.is_empty()
            && self.withheld_session_info.is_empty()
            && self.room_settings.is_empty()
            && self.secrets.is_empty()
            && self.next_batch_token.is_none()
    }
}

/// This struct is used to remember whether an identity has undergone a change
/// or remains the same as the one we already know about.
///
/// When the homeserver informs us of a potential change in a user's identity or
/// device during a `/sync` response, it triggers a `/keys/query` request from
/// our side. In response to this query, the server provides a comprehensive
/// snapshot of all the user's devices and identities.
///
/// Our responsibility is to discern whether a device or identity is new,
/// changed, or unchanged.
#[derive(Debug, Clone, Default)]
#[allow(missing_docs)]
pub struct IdentityChanges {
    pub new: Vec<ReadOnlyUserIdentities>,
    pub changed: Vec<ReadOnlyUserIdentities>,
    pub unchanged: Vec<ReadOnlyUserIdentities>,
}

impl IdentityChanges {
    fn is_empty(&self) -> bool {
        self.new.is_empty() && self.changed.is_empty()
    }

    /// Convert the vectors contained in the [`IdentityChanges`] into
    /// three maps from user id to user identity (new, updated, unchanged).
    fn into_maps(
        self,
    ) -> (
        BTreeMap<OwnedUserId, ReadOnlyUserIdentities>,
        BTreeMap<OwnedUserId, ReadOnlyUserIdentities>,
        BTreeMap<OwnedUserId, ReadOnlyUserIdentities>,
    ) {
        let new: BTreeMap<_, _> = self
            .new
            .into_iter()
            .map(|identity| (identity.user_id().to_owned(), identity))
            .collect();

        let changed: BTreeMap<_, _> = self
            .changed
            .into_iter()
            .map(|identity| (identity.user_id().to_owned(), identity))
            .collect();

        let unchanged: BTreeMap<_, _> = self
            .unchanged
            .into_iter()
            .map(|identity| (identity.user_id().to_owned(), identity))
            .collect();

        (new, changed, unchanged)
    }
}

#[derive(Debug, Clone, Default)]
#[allow(missing_docs)]
pub struct DeviceChanges {
    pub new: Vec<ReadOnlyDevice>,
    pub changed: Vec<ReadOnlyDevice>,
    pub deleted: Vec<ReadOnlyDevice>,
}

/// Convert the devices and vectors contained in the [`DeviceChanges`] into
/// a [`DeviceUpdates`] struct.
///
/// The [`DeviceChanges`] will contain vectors of [`ReadOnlyDevice`]s which
/// we want to convert to a [`Device`].
fn collect_device_updates(
    verification_machine: VerificationMachine,
    own_identity: Option<ReadOnlyOwnUserIdentity>,
    identities: IdentityChanges,
    devices: DeviceChanges,
) -> DeviceUpdates {
    let mut new: BTreeMap<_, BTreeMap<_, _>> = BTreeMap::new();
    let mut changed: BTreeMap<_, BTreeMap<_, _>> = BTreeMap::new();

    let (new_identities, changed_identities, unchanged_identities) = identities.into_maps();

    let map_device = |device: ReadOnlyDevice| {
        let device_owner_identity = new_identities
            .get(device.user_id())
            .or_else(|| changed_identities.get(device.user_id()))
            .or_else(|| unchanged_identities.get(device.user_id()))
            .cloned();

        Device {
            inner: device,
            verification_machine: verification_machine.to_owned(),
            own_identity: own_identity.to_owned(),
            device_owner_identity,
        }
    };

    for device in devices.new {
        let device = map_device(device);

        new.entry(device.user_id().to_owned())
            .or_default()
            .insert(device.device_id().to_owned(), device);
    }

    for device in devices.changed {
        let device = map_device(device);

        changed
            .entry(device.user_id().to_owned())
            .or_default()
            .insert(device.device_id().to_owned(), device.to_owned());
    }

    DeviceUpdates { new, changed }
}

/// Updates about [`Device`]s which got received over the `/keys/query`
/// endpoint.
#[derive(Clone, Debug, Default)]
pub struct DeviceUpdates {
    /// The list of newly discovered devices.
    ///
    /// A device being in this list does not necessarily mean that the device
    /// was just created, it just means that it's the first time we're
    /// seeing this device.
    pub new: BTreeMap<OwnedUserId, BTreeMap<OwnedDeviceId, Device>>,
    /// The list of changed devices.
    pub changed: BTreeMap<OwnedUserId, BTreeMap<OwnedDeviceId, Device>>,
}

/// Updates about [`UserIdentities`] which got received over the `/keys/query`
/// endpoint.
#[derive(Clone, Debug, Default)]
pub struct IdentityUpdates {
    /// The list of newly discovered user identities .
    ///
    /// A identity being in this list does not necessarily mean that the
    /// identity was just created, it just means that it's the first time
    /// we're seeing this identity.
    pub new: BTreeMap<OwnedUserId, UserIdentities>,
    /// The list of changed identities.
    pub changed: BTreeMap<OwnedUserId, UserIdentities>,
    /// The list of unchanged identities.
    pub unchanged: BTreeMap<OwnedUserId, UserIdentities>,
}

/// The private part of a backup key.
///
/// The private part of the key is not used on a regular basis. Rather, it is
/// used only when we need to *recover* the backup.
///
/// Typically, this private key is itself encrypted and stored in server-side
/// secret storage (SSSS), whence it can be retrieved when it is needed for a
/// recovery operation. Alternatively, the key can be "gossiped" between devices
/// via "secret sharing".
#[derive(Clone, Zeroize, Deserialize, Serialize)]
#[zeroize(drop)]
#[serde(transparent)]
pub struct BackupDecryptionKey {
    pub(crate) inner: Box<[u8; BackupDecryptionKey::KEY_SIZE]>,
}

impl BackupDecryptionKey {
    /// The number of bytes the decryption key will hold.
    pub const KEY_SIZE: usize = 32;

    /// Create a new random decryption key.
    pub fn new() -> Result<Self, rand::Error> {
        let mut rng = rand::thread_rng();

        let mut key = Box::new([0u8; Self::KEY_SIZE]);
        rand::Fill::try_fill(key.as_mut_slice(), &mut rng)?;

        Ok(Self { inner: key })
    }

    /// Export the [`BackupDecryptionKey`] as a base64 encoded string.
    pub fn to_base64(&self) -> String {
        base64_encode(self.inner.as_slice())
    }
}

#[cfg(not(tarpaulin_include))]
impl Debug for BackupDecryptionKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BackupDecryptionKey").finish()
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
#[derive(Default, Clone, Debug)]
pub struct BackupKeys {
    /// The key used to decrypt backed up room keys.
    pub decryption_key: Option<BackupDecryptionKey>,
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
    /// Create a new Store.
    pub(crate) fn new(
        account: StaticAccountData,
        identity: Arc<Mutex<PrivateCrossSigningIdentity>>,
        store: Arc<CryptoStoreWrapper>,
        verification_machine: VerificationMachine,
    ) -> Self {
        Self {
            inner: Arc::new(StoreInner {
                static_account: account,
                identity,
                store: store.clone(),
                verification_machine,
                users_for_key_query: AsyncStdMutex::new(UsersForKeyQuery::new()),
                users_for_key_query_condvar: Condvar::new(),
                cache: Arc::new(RwLock::new(StoreCache {
                    store,
                    tracked_users: Default::default(),
                    tracked_user_loading_lock: Default::default(),
                    account: Default::default(),
                })),
            }),
        }
    }

    /// UserId associated with this store
    pub(crate) fn user_id(&self) -> &UserId {
        &self.inner.static_account.user_id
    }

    /// DeviceId associated with this store
    pub(crate) fn device_id(&self) -> &DeviceId {
        self.inner.verification_machine.own_device_id()
    }

    /// The static data for the account associated with this store.
    pub(crate) fn static_account(&self) -> &StaticAccountData {
        &self.inner.static_account
    }

    pub(crate) async fn cache(&self) -> Result<StoreCacheGuard> {
        // TODO: (bnjbvr, #2624) If configured with a cross-process lock:
        // - try to take the lock,
        // - if acquired, look if another process touched the underlying storage,
        // - if yes, reload everything; if no, return current cache

        let cache = StoreCacheGuard { cache: self.inner.cache.clone().read_owned().await };

        // Make sure tracked users are always up to date.
        cache.ensure_sync_tracked_users(self).await?;

        Ok(cache)
    }

    pub(crate) async fn transaction(&self) -> Result<StoreTransaction> {
        StoreTransaction::new(self.clone()).await
    }

    // Note: bnjbvr lost against borrowck here. Ideally, the `F` parameter would
    // take a `&StoreTransaction`, but callers didn't quite like that.
    pub(crate) async fn with_transaction<
        T,
        Fut: futures_core::Future<Output = Result<(StoreTransaction, T), crate::OlmError>>,
        F: FnOnce(StoreTransaction) -> Fut,
    >(
        &self,
        func: F,
    ) -> Result<T, crate::OlmError> {
        let tr = self.transaction().await?;
        let (tr, res) = func(tr).await?;
        tr.commit().await?;
        Ok(res)
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
        self.inner.store.save_changes(changes).await
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

    /// Convenience helper to persist an array of [`InboundGroupSession`]s.
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
        let own_identity = self
            .inner
            .store
            .get_user_identity(self.user_id())
            .await?
            .and_then(as_variant!(ReadOnlyUserIdentities::Own));

        Ok(self.inner.store.get_user_identity(user_id).await?.map(|i| {
            UserIdentities::new(
                self.clone(),
                i,
                self.inner.verification_machine.to_owned(),
                own_identity,
            )
        }))
    }

    /// Try to export the secret with the given secret name.
    ///
    /// The exported secret will be encoded as unpadded base64. Returns `Null`
    /// if the secret can't be found.
    ///
    /// # Arguments
    ///
    /// * `secret_name` - The name of the secret that should be exported.
    pub async fn export_secret(
        &self,
        secret_name: &SecretName,
    ) -> Result<Option<String>, CryptoStoreError> {
        Ok(match secret_name {
            SecretName::CrossSigningMasterKey
            | SecretName::CrossSigningUserSigningKey
            | SecretName::CrossSigningSelfSigningKey => {
                self.inner.identity.lock().await.export_secret(secret_name).await
            }
            SecretName::RecoveryKey => {
                #[cfg(feature = "backups_v1")]
                if let Some(key) = self.load_backup_keys().await?.decryption_key {
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
        })
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
                    public_identity.to_owned(),
                    export.master_key.as_deref(),
                    export.self_signing_key.as_deref(),
                    export.user_signing_key.as_deref(),
                )
                .await?;

            let status = identity.status().await;

            let diff = identity.get_public_identity_diff(&public_identity.inner).await;

            let mut changes =
                Changes { private_identity: Some(identity.clone()), ..Default::default() };

            if diff.none_differ() {
                public_identity.mark_as_verified();
                changes.identities.changed.push(ReadOnlyUserIdentities::Own(public_identity.inner));
            }

            info!(?status, "Successfully imported the private cross-signing keys");

            self.save_changes(changes).await?;
        }

        Ok(self.inner.identity.lock().await.status().await)
    }

    /// Import the given `secret` named `secret_name` into the keystore.
    pub async fn import_secret(&self, secret: &GossippedSecret) -> Result<(), SecretImportError> {
        match &secret.secret_name {
            SecretName::CrossSigningMasterKey
            | SecretName::CrossSigningUserSigningKey
            | SecretName::CrossSigningSelfSigningKey => {
                if let Some(public_identity) =
                    self.get_identity(self.user_id()).await?.and_then(|i| i.own())
                {
                    let identity = self.inner.identity.lock().await;

                    identity
                        .import_secret(
                            public_identity,
                            &secret.secret_name,
                            &secret.event.content.secret,
                        )
                        .await?;
                    info!(
                        secret_name = ?secret.secret_name,
                        "Successfully imported a private cross signing key"
                    );

                    let changes =
                        Changes { private_identity: Some(identity.clone()), ..Default::default() };

                    self.save_changes(changes).await?;
                }
            }
            SecretName::RecoveryKey => {
                // We don't import the decryption key here since we'll want to
                // check if the public key matches to the latest version on the
                // server. We instead put the secret into a secret inbox where
                // it will stay until it either gets overwritten
                // or the user accepts the secret.
            }
            name => {
                warn!(secret = ?name, "Tried to import an unknown secret");
            }
        }

        Ok(())
    }

    /// Add entries to the list of users being tracked for device changes
    ///
    /// Any users not already on the list are flagged as awaiting a key query.
    /// Users that were already in the list are unaffected.
    pub(crate) async fn update_tracked_users(
        &self,
        users: impl Iterator<Item = &UserId>,
    ) -> Result<()> {
        let cache = self.cache().await?;

        let mut store_updates = Vec::new();
        let mut key_query_lock = self.inner.users_for_key_query.lock().await;

        {
            let mut tracked_users = cache.tracked_users.write().unwrap();
            for user_id in users {
                if tracked_users.insert(user_id.to_owned()) {
                    key_query_lock.insert_user(user_id);
                    store_updates.push((user_id, true))
                }
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

        {
            let cache = self.cache().await?;
            let tracked_users = cache.tracked_users.read().unwrap();
            for user_id in users {
                if tracked_users.contains(user_id) {
                    let clean = key_query_lock.maybe_remove_user(user_id, sequence_number);
                    store_updates.push((user_id, !clean));
                }
            }
        }
        self.inner.store.save_tracked_users(&store_updates).await?;
        // wake up any tasks that may have been waiting for updates
        self.inner.users_for_key_query_condvar.notify_all();

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
        // Make sure the tracked users set is up to date.
        let _cache = self.cache().await?;

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
        let mut users_for_key_query = self.inner.users_for_key_query.lock().await;

        let Some(waiter) = users_for_key_query.maybe_register_waiting_task(user) else {
            return UserKeyQueryResult::WasNotPending;
        };

        let wait_for_completion = async {
            while !waiter.completed.load(Ordering::Relaxed) {
                users_for_key_query =
                    self.inner.users_for_key_query_condvar.wait(users_for_key_query).await;
            }
        };

        match timeout(Box::pin(wait_for_completion), timeout_duration).await {
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
        Ok(self.cache().await?.tracked_users.read().unwrap().iter().cloned().collect())
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
    ///
    /// The stream will terminate once all references to the underlying
    /// `CryptoStoreWrapper` are dropped.
    pub fn room_keys_received_stream(&self) -> impl Stream<Item = Vec<RoomKeyInfo>> {
        self.inner.store.room_keys_received_stream()
    }

    /// Returns a stream of user identity updates, allowing users to listen for
    /// notifications about new or changed user identities.
    ///
    /// The stream produced by this method emits updates whenever a new user
    /// identity is discovered or when an existing identities information is
    /// changed. Users can subscribe to this stream and receive updates in
    /// real-time.
    ///
    /// Caution: the returned stream will never terminate, and it holds a
    /// reference to the [`CryptoStore`]. Listeners should be careful to avoid
    /// resource leaks.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use matrix_sdk_crypto::OlmMachine;
    /// # use ruma::{device_id, user_id};
    /// # use futures_util::{pin_mut, StreamExt};
    /// # let machine: OlmMachine = unimplemented!();
    /// # futures_executor::block_on(async {
    /// let identities_stream = machine.store().user_identities_stream();
    /// pin_mut!(identities_stream);
    ///
    /// for identity_updates in identities_stream.next().await {
    ///     for (_, identity) in identity_updates.new {
    ///         println!("A new identity has been added {}", identity.user_id());
    ///     }
    /// }
    /// # });
    /// ```
    pub fn user_identities_stream(&self) -> impl Stream<Item = IdentityUpdates> {
        let verification_machine = self.inner.verification_machine.to_owned();

        let this = self.clone();
        self.inner.store.identities_stream().map(move |(own_identity, identities, _)| {
            let (new_identities, changed_identities, unchanged_identities) = identities.into_maps();

            let map_identity = |(user_id, identity)| {
                (
                    user_id,
                    UserIdentities::new(
                        this.clone(),
                        identity,
                        verification_machine.to_owned(),
                        own_identity.to_owned(),
                    ),
                )
            };

            let new = new_identities.into_iter().map(map_identity).collect();
            let changed = changed_identities.into_iter().map(map_identity).collect();
            let unchanged = unchanged_identities.into_iter().map(map_identity).collect();

            IdentityUpdates { new, changed, unchanged }
        })
    }

    /// Returns a stream of device updates, allowing users to listen for
    /// notifications about new or changed devices.
    ///
    /// The stream produced by this method emits updates whenever a new device
    /// is discovered or when an existing device's information is changed. Users
    /// can subscribe to this stream and receive updates in real-time.
    ///
    /// Caution: the returned stream will never terminate, and it holds a
    /// reference to the [`CryptoStore`]. Listeners should be careful to avoid
    /// resource leaks.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use matrix_sdk_crypto::OlmMachine;
    /// # use ruma::{device_id, user_id};
    /// # use futures_util::{pin_mut, StreamExt};
    /// # let machine: OlmMachine = unimplemented!();
    /// # futures_executor::block_on(async {
    /// let devices_stream = machine.store().devices_stream();
    /// pin_mut!(devices_stream);
    ///
    /// for device_updates in devices_stream.next().await {
    ///     if let Some(user_devices) = device_updates.new.get(machine.user_id()) {
    ///         for device in user_devices.values() {
    ///             println!("A new device has been added {}", device.device_id());
    ///         }
    ///     }
    /// }
    /// # });
    /// ```
    pub fn devices_stream(&self) -> impl Stream<Item = DeviceUpdates> {
        let verification_machine = self.inner.verification_machine.to_owned();

        self.inner.store.identities_stream().map(move |(own_identity, identities, devices)| {
            collect_device_updates(
                verification_machine.to_owned(),
                own_identity,
                identities,
                devices,
            )
        })
    }

    /// Returns a [`Stream`] of user identity and device updates
    ///
    /// The stream returned by this method returns the same data as
    /// [`Store::user_identities_stream`] and [`Store::devices_stream`] but does
    /// not include references to the `VerificationMachine`. It is therefore a
    /// lower-level view on that data.
    ///
    /// The stream will terminate once all references to the underlying
    /// `CryptoStoreWrapper` are dropped.
    pub fn identities_stream_raw(&self) -> impl Stream<Item = (IdentityChanges, DeviceChanges)> {
        self.inner.store.identities_stream().map(|(_, identities, devices)| (identities, devices))
    }

    /// Creates a `CrossProcessStoreLock` for this store, that will contain the
    /// given key and value when hold.
    pub fn create_store_lock(
        &self,
        lock_key: String,
        lock_value: String,
    ) -> CrossProcessStoreLock<LockableCryptoStore> {
        self.inner.store.create_store_lock(lock_key, lock_value)
    }

    /// Receive notifications of gossipped secrets being received and stored in
    /// the secret inbox as a [`Stream`].
    ///
    /// The gossipped secrets are received using the `m.secret.send` event type
    /// and are guaranteed to have been received over a 1-to-1 Olm
    /// [`Session`] from a verified [`Device`].
    ///
    /// The [`GossippedSecret`] can also be later found in the secret inbox and
    /// retrieved using the [`CryptoStore::get_secrets_from_inbox()`] method.
    ///
    /// After a suitable secret of a certain type has been found it can be
    /// removed from the store
    /// using the [`CryptoStore::delete_secrets_from_inbox()`] method.
    ///
    /// The only secret this will currently broadcast is the
    /// `m.megolm_backup.v1`.
    ///
    /// If the reader of the stream lags too far behind, a warning will be
    /// logged and items will be dropped.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use matrix_sdk_crypto::OlmMachine;
    /// # use ruma::{device_id, user_id};
    /// # use futures_util::{pin_mut, StreamExt};
    /// # let alice = user_id!("@alice:example.org").to_owned();
    /// # futures_executor::block_on(async {
    /// # let machine = OlmMachine::new(&alice, device_id!("DEVICEID")).await;
    ///
    /// let secret_stream = machine.store().secrets_stream();
    /// pin_mut!(secret_stream);
    ///
    /// for secret in secret_stream.next().await {
    ///     // Accept the secret if it's valid, then delete all the secrets of this type.
    ///     machine.store().delete_secrets_from_inbox(&secret.secret_name);
    /// }
    /// # });
    /// ```
    pub fn secrets_stream(&self) -> impl Stream<Item = GossippedSecret> {
        self.inner.store.secrets_stream()
    }
}

impl Deref for Store {
    type Target = DynCryptoStore;

    fn deref(&self) -> &Self::Target {
        self.inner.store.deref().deref()
    }
}

/// A crypto store that implements primitives for cross-process locking.
#[derive(Clone, Debug)]
pub struct LockableCryptoStore(Arc<dyn CryptoStore<Error = CryptoStoreError>>);

#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
impl matrix_sdk_common::store_locks::BackingStore for LockableCryptoStore {
    type Error = CryptoStoreError;

    async fn try_lock(
        &self,
        lease_duration_ms: u32,
        key: &str,
        holder: &str,
    ) -> std::result::Result<bool, Self::Error> {
        self.0.try_take_leased_lock(lease_duration_ms, key, holder).await
    }
}
