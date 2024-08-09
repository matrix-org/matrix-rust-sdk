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
//! let machine = OlmMachine::with_store(user_id, device_id, store, None);
//! ```
//!
//! [`OlmMachine`]: /matrix_sdk_crypto/struct.OlmMachine.html
//! [`CryptoStore`]: trait.Cryptostore.html

use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    fmt::Debug,
    ops::Deref,
    pin::pin,
    sync::{atomic::Ordering, Arc, RwLock as StdRwLock},
    time::Duration,
};

use as_variant::as_variant;
use futures_core::Stream;
use futures_util::StreamExt;
use ruma::{
    encryption::KeyUsage, events::secret::request::SecretName, DeviceId, OwnedDeviceId,
    OwnedRoomId, OwnedUserId, UserId,
};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::{Mutex, MutexGuard, Notify, OwnedRwLockReadGuard, OwnedRwLockWriteGuard, RwLock};
use tracing::{info, warn};
use vodozemac::{base64_encode, megolm::SessionOrdering, Curve25519PublicKey};
use zeroize::{Zeroize, ZeroizeOnDrop};

#[cfg(doc)]
use crate::{backups::BackupMachine, identities::OwnUserIdentity};
use crate::{
    gossiping::GossippedSecret,
    identities::{user::UserIdentities, Device, DeviceData, UserDevices, UserIdentityData},
    olm::{
        Account, ExportedRoomKey, InboundGroupSession, OlmMessageHash, OutboundGroupSession,
        PrivateCrossSigningIdentity, Session, StaticAccountData,
    },
    types::{
        events::room_key_withheld::RoomKeyWithheldEvent, BackupSecrets, CrossSigningSecrets,
        EventEncryptionAlgorithm, MegolmBackupV1Curve25519AesSha2Secrets, SecretsBundle,
    },
    verification::VerificationMachine,
    CrossSigningStatus, OwnUserIdentityData, RoomKeyImportResult,
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

#[derive(Debug, Default)]
pub(crate) struct KeyQueryManager {
    /// Record of the users that are waiting for a /keys/query.
    users_for_key_query: Mutex<UsersForKeyQuery>,

    /// Notifier that is triggered each time an update is received for a user.
    users_for_key_query_notify: Notify,
}

impl KeyQueryManager {
    pub async fn synced<'a>(&'a self, cache: &'a StoreCache) -> Result<SyncedKeyQueryManager<'a>> {
        self.ensure_sync_tracked_users(cache).await?;
        Ok(SyncedKeyQueryManager { cache, manager: self })
    }

    /// Load the list of users for whom we are tracking their device lists and
    /// fill out our caches.
    ///
    /// This method ensures that we're only going to load the users from the
    /// actual [`CryptoStore`] once, it will also make sure that any
    /// concurrent calls to this method get deduplicated.
    async fn ensure_sync_tracked_users(&self, cache: &StoreCache) -> Result<()> {
        // Check if the users are loaded, and in that case do nothing.
        let loaded = cache.loaded_tracked_users.read().await;
        if *loaded {
            return Ok(());
        }

        // Otherwise, we may load the users.
        drop(loaded);
        let mut loaded = cache.loaded_tracked_users.write().await;

        // Check again if the users have been loaded, in case another call to this
        // method loaded the tracked users between the time we tried to
        // acquire the lock and the time we actually acquired the lock.
        if *loaded {
            return Ok(());
        }

        let tracked_users = cache.store.load_tracked_users().await?;

        let mut query_users_lock = self.users_for_key_query.lock().await;
        let mut tracked_users_cache = cache.tracked_users.write().unwrap();
        for user in tracked_users {
            tracked_users_cache.insert(user.user_id.to_owned());

            if user.dirty {
                query_users_lock.insert_user(&user.user_id);
            }
        }

        *loaded = true;

        Ok(())
    }

    /// Wait for a `/keys/query` response to be received if one is expected for
    /// the given user.
    ///
    /// If the given timeout elapses, the method will stop waiting and return
    /// `UserKeyQueryResult::TimeoutExpired`.
    ///
    /// Requires a [`StoreCacheGuard`] to make sure the users for which a key
    /// query is pending are up to date, but doesn't hold on to it
    /// thereafter: the lock is short-lived in this case.
    pub async fn wait_if_user_key_query_pending(
        &self,
        cache: StoreCacheGuard,
        timeout_duration: Duration,
        user: &UserId,
    ) -> Result<UserKeyQueryResult> {
        {
            // Drop the cache early, so we don't keep it while waiting (since writing the
            // results requires to write in the cache, thus take another lock).
            self.ensure_sync_tracked_users(&cache).await?;
            drop(cache);
        }

        let mut users_for_key_query = self.users_for_key_query.lock().await;
        let Some(waiter) = users_for_key_query.maybe_register_waiting_task(user) else {
            return Ok(UserKeyQueryResult::WasNotPending);
        };

        let wait_for_completion = async {
            while !waiter.completed.load(Ordering::Relaxed) {
                // Register for being notified before releasing the mutex, so
                // it's impossible to miss a wakeup between the last check for
                // whether we should wait, and starting to wait.
                let mut notified = pin!(self.users_for_key_query_notify.notified());
                notified.as_mut().enable();
                drop(users_for_key_query);

                // Wait for a notification
                notified.await;

                // Reclaim the lock before checking the flag to avoid races
                // when two notifications happen right after each other and the
                // second one sets the flag we want to wait for.
                users_for_key_query = self.users_for_key_query.lock().await;
            }
        };

        match timeout(Box::pin(wait_for_completion), timeout_duration).await {
            Err(_) => {
                warn!(
                    user_id = ?user,
                    "The user has a pending `/key/query` request which did \
                    not finish yet, some devices might be missing."
                );

                Ok(UserKeyQueryResult::TimeoutExpired)
            }
            _ => Ok(UserKeyQueryResult::WasPending),
        }
    }
}

pub(crate) struct SyncedKeyQueryManager<'a> {
    cache: &'a StoreCache,
    manager: &'a KeyQueryManager,
}

impl<'a> SyncedKeyQueryManager<'a> {
    /// Add entries to the list of users being tracked for device changes
    ///
    /// Any users not already on the list are flagged as awaiting a key query.
    /// Users that were already in the list are unaffected.
    pub async fn update_tracked_users(&self, users: impl Iterator<Item = &UserId>) -> Result<()> {
        let mut store_updates = Vec::new();
        let mut key_query_lock = self.manager.users_for_key_query.lock().await;

        {
            let mut tracked_users = self.cache.tracked_users.write().unwrap();
            for user_id in users {
                if tracked_users.insert(user_id.to_owned()) {
                    key_query_lock.insert_user(user_id);
                    store_updates.push((user_id, true))
                }
            }
        }

        self.cache.store.save_tracked_users(&store_updates).await
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
        let mut store_updates: Vec<(&UserId, bool)> = Vec::new();
        let mut key_query_lock = self.manager.users_for_key_query.lock().await;

        {
            let tracked_users = &self.cache.tracked_users.read().unwrap();
            for user_id in users {
                if tracked_users.contains(user_id) {
                    key_query_lock.insert_user(user_id);
                    store_updates.push((user_id, true));
                }
            }
        }

        self.cache.store.save_tracked_users(&store_updates).await
    }

    /// Flag that the given users devices are now up-to-date.
    ///
    /// This is called after processing the response to a /keys/query request.
    /// Any users whose device lists we are tracking are removed from the
    /// list of those pending a /keys/query.
    pub async fn mark_tracked_users_as_up_to_date(
        &self,
        users: impl Iterator<Item = &UserId>,
        sequence_number: SequenceNumber,
    ) -> Result<()> {
        let mut store_updates: Vec<(&UserId, bool)> = Vec::new();
        let mut key_query_lock = self.manager.users_for_key_query.lock().await;

        {
            let tracked_users = self.cache.tracked_users.read().unwrap();
            for user_id in users {
                if tracked_users.contains(user_id) {
                    let clean = key_query_lock.maybe_remove_user(user_id, sequence_number);
                    store_updates.push((user_id, !clean));
                }
            }
        }

        self.cache.store.save_tracked_users(&store_updates).await?;
        // wake up any tasks that may have been waiting for updates
        self.manager.users_for_key_query_notify.notify_waiters();

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
    pub async fn users_for_key_query(&self) -> (HashSet<OwnedUserId>, SequenceNumber) {
        self.manager.users_for_key_query.lock().await.users_for_key_query()
    }

    /// See the docs for [`crate::OlmMachine::tracked_users()`].
    pub fn tracked_users(&self) -> HashSet<OwnedUserId> {
        self.cache.tracked_users.read().unwrap().iter().cloned().collect()
    }

    /// Mark the given user as being tracked for device lists, and mark that it
    /// has an outdated device list.
    ///
    /// This means that the user will be considered for a `/keys/query` request
    /// next time [`Store::users_for_key_query()`] is called.
    pub async fn mark_user_as_changed(&self, user: &UserId) -> Result<()> {
        self.manager.users_for_key_query.lock().await.insert_user(user);
        self.cache.tracked_users.write().unwrap().insert(user.to_owned());

        self.cache.store.save_tracked_users(&[(user, true)]).await
    }
}

#[derive(Debug)]
pub(crate) struct StoreCache {
    store: Arc<CryptoStoreWrapper>,
    tracked_users: StdRwLock<BTreeSet<OwnedUserId>>,
    loaded_tracked_users: RwLock<bool>,
    account: Mutex<Option<Account>>,
}

impl StoreCache {
    /// Returns a reference to the `Account`.
    ///
    /// Either load the account from the cache, or the store if missing from
    /// the cache.
    ///
    /// Note there should always be an account stored at least in the store, so
    /// this doesn't return an `Option`.
    ///
    /// Note: this method should remain private, otherwise it's possible to ask
    /// for a `StoreTransaction`, then get the `StoreTransaction::cache()`
    /// and thus have two different live copies of the `Account` at once.
    async fn account(&self) -> Result<impl Deref<Target = Account> + '_> {
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

/// Read-only store cache guard.
///
/// This type should hold all the methods that are available when the cache is
/// borrowed in read-only mode, while all the write operations on those fields
/// should happen as part of a `StoreTransaction`.
pub(crate) struct StoreCacheGuard {
    cache: OwnedRwLockReadGuard<StoreCache>,
    // TODO: (bnjbvr, #2624) add cross-process lock guard here.
}

impl StoreCacheGuard {
    /// Returns a reference to the `Account`.
    ///
    /// Either load the account from the cache, or the store if missing from
    /// the cache.
    ///
    /// Note there should always be an account stored at least in the store, so
    /// this doesn't return an `Option`.
    pub async fn account(&self) -> Result<impl Deref<Target = Account> + '_> {
        self.cache.account().await
    }
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
    async fn new(store: Store) -> Self {
        let cache = store.inner.cache.clone();

        Self { store, changes: PendingChanges::default(), cache: cache.clone().write_owned().await }
    }

    pub(crate) fn cache(&self) -> &StoreCache {
        &self.cache
    }

    /// Returns a reference to the current `Store`.
    pub fn store(&self) -> &Store {
        &self.store
    }

    /// Gets a `Account` for update.
    ///
    /// Note: since it's guaranteed that one can't have both a
    /// `StoreTransaction` and a `StoreCacheGuard` at runtime (since the
    /// underlying `StoreCache` is guarded by a `RwLock` mutex), this ensures
    /// that we can't have two copies of an `Account` alive at the same time.
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
        if self.changes.is_empty() {
            return Ok(());
        }

        // Save changes in the database.
        let account = self.changes.account.as_ref().map(|acc| acc.deep_clone());

        self.store.save_pending_changes(self.changes).await?;

        // Make the cache coherent with the database.
        if let Some(account) = account {
            *self.cache.account.lock().await = Some(account);
        }

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
#[derive(Clone, Debug, Serialize, Deserialize)]
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
    pub new: Vec<UserIdentityData>,
    pub changed: Vec<UserIdentityData>,
    pub unchanged: Vec<UserIdentityData>,
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
        BTreeMap<OwnedUserId, UserIdentityData>,
        BTreeMap<OwnedUserId, UserIdentityData>,
        BTreeMap<OwnedUserId, UserIdentityData>,
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
    pub new: Vec<DeviceData>,
    pub changed: Vec<DeviceData>,
    pub deleted: Vec<DeviceData>,
}

/// Convert the devices and vectors contained in the [`DeviceChanges`] into
/// a [`DeviceUpdates`] struct.
///
/// The [`DeviceChanges`] will contain vectors of [`DeviceData`]s which
/// we want to convert to a [`Device`].
fn collect_device_updates(
    verification_machine: VerificationMachine,
    own_identity: Option<OwnUserIdentityData>,
    identities: IdentityChanges,
    devices: DeviceChanges,
) -> DeviceUpdates {
    let mut new: BTreeMap<_, BTreeMap<_, _>> = BTreeMap::new();
    let mut changed: BTreeMap<_, BTreeMap<_, _>> = BTreeMap::new();

    let (new_identities, changed_identities, unchanged_identities) = identities.into_maps();

    let map_device = |device: DeviceData| {
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
#[derive(Clone, Zeroize, ZeroizeOnDrop, Deserialize, Serialize)]
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
        f.debug_tuple("BackupDecryptionKey").field(&"...").finish()
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
#[derive(Default, Zeroize, ZeroizeOnDrop)]
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

/// Error describing what went wrong when exporting a [`SecretsBundle`].
///
/// The [`SecretsBundle`] can only be exported if we have all cross-signing
/// private keys in the store.
#[derive(Debug, Error)]
pub enum SecretsBundleExportError {
    /// The store itself had an error.
    #[error(transparent)]
    Store(#[from] CryptoStoreError),
    /// We're missing one or multiple cross-signing keys.
    #[error("The store is missing one or multiple cross-signing keys")]
    MissingCrossSigningKey(KeyUsage),
    /// We're missing all cross-signing keys.
    #[error("The store doesn't contain any cross-signing keys")]
    MissingCrossSigningKeys,
    /// We have a backup key stored, but we don't know the version of the
    /// backup.
    #[error("The store contains a backup key, but no backup version")]
    MissingBackupVersion,
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

    /// The maximum time an encryption session should be used for, before it is
    /// rotated.
    pub session_rotation_period: Option<Duration>,

    /// The maximum number of messages an encryption session should be used for,
    /// before it is rotated.
    pub session_rotation_period_messages: Option<usize>,
}

impl Default for RoomSettings {
    fn default() -> Self {
        Self {
            algorithm: EventEncryptionAlgorithm::MegolmV1AesSha2,
            only_allow_trusted_devices: false,
            session_rotation_period: None,
            session_rotation_period_messages: None,
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

/// Information on a room key that has been withheld
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct RoomKeyWithheldInfo {
    /// The room where the key is used.
    pub room_id: OwnedRoomId,

    /// The ID of the session that the key is for.
    pub session_id: String,

    /// The `m.room_key.withheld` event that notified us that the key is being
    /// withheld.
    pub withheld_event: RoomKeyWithheldEvent,
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
                cache: Arc::new(RwLock::new(StoreCache {
                    store,
                    tracked_users: Default::default(),
                    loaded_tracked_users: Default::default(),
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
        Ok(StoreCacheGuard { cache: self.inner.cache.clone().read_owned().await })
    }

    pub(crate) async fn transaction(&self) -> StoreTransaction {
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
        let tr = self.transaction().await;
        let (tr, res) = func(tr).await?;
        tr.commit().await?;
        Ok(res)
    }

    #[cfg(test)]
    /// test helper to reset the cross signing identity
    pub(crate) async fn reset_cross_signing_identity(&self) {
        self.inner.identity.lock().await.reset();
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

    pub(crate) async fn get_sessions(
        &self,
        sender_key: &str,
    ) -> Result<Option<Arc<Mutex<Vec<Session>>>>> {
        self.inner.store.get_sessions(sender_key).await
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
    pub(crate) async fn save_device_data(&self, devices: &[DeviceData]) -> Result<()> {
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

    /// Get the device data for the given [`UserId`] and [`DeviceId`].
    ///
    /// *Note*: This method will include our own device which is always present
    /// in the store.
    pub(crate) async fn get_device_data(
        &self,
        user_id: &UserId,
        device_id: &DeviceId,
    ) -> Result<Option<DeviceData>> {
        self.inner.store.get_device(user_id, device_id).await
    }

    /// Get the device data for the given [`UserId`] and [`DeviceId`].
    ///
    /// *Note*: This method will **not** include our own device.
    ///
    /// Use this method if you need a list of recipients for a given user, since
    /// we don't want to encrypt for our own device, otherwise take a look at
    /// the [`Store::get_device_data_for_user`] method.
    pub(crate) async fn get_device_data_for_user_filtered(
        &self,
        user_id: &UserId,
    ) -> Result<HashMap<OwnedDeviceId, DeviceData>> {
        self.inner.store.get_user_devices(user_id).await.map(|mut d| {
            if user_id == self.user_id() {
                d.remove(self.device_id());
            }
            d
        })
    }

    /// Get the [`DeviceData`] for all the devices a user has.
    ///
    /// *Note*: This method will include our own device which is always present
    /// in the store.
    ///
    /// Use this method if you need to operate on or update all devices of a
    /// user, otherwise take a look at the
    /// [`Store::get_device_data_for_user_filtered`] method.
    pub(crate) async fn get_device_data_for_user(
        &self,
        user_id: &UserId,
    ) -> Result<HashMap<OwnedDeviceId, DeviceData>> {
        self.inner.store.get_user_devices(user_id).await
    }

    /// Get a [`Device`] for the given user with the given
    /// [`Curve25519PublicKey`] key.
    ///
    /// *Note*: This method will include our own device which is always present
    /// in the store.
    pub(crate) async fn get_device_from_curve_key(
        &self,
        user_id: &UserId,
        curve_key: Curve25519PublicKey,
    ) -> Result<Option<Device>> {
        self.get_user_devices(user_id)
            .await
            .map(|d| d.devices().find(|d| d.curve25519_key() == Some(curve_key)))
    }

    /// Get all devices associated with the given [`UserId`].
    ///
    /// This method is more expensive than the
    /// [`Store::get_device_data_for_user`] method, since a [`Device`]
    /// requires the [`OwnUserIdentityData`] and the [`UserIdentityData`] of the
    /// device owner to be fetched from the store as well.
    ///
    /// *Note*: This method will include our own device which is always present
    /// in the store.
    pub(crate) async fn get_user_devices(&self, user_id: &UserId) -> Result<UserDevices> {
        let devices = self.get_device_data_for_user(user_id).await?;

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

    /// Get a [`Device`] for the given user with the given [`DeviceId`].
    ///
    /// This method is more expensive than the [`Store::get_device_data`] method
    /// since a [`Device`] requires the [`OwnUserIdentityData`] and the
    /// [`UserIdentityData`] of the device owner to be fetched from the
    /// store as well.
    ///
    /// *Note*: This method will include our own device which is always present
    /// in the store.
    pub(crate) async fn get_device(
        &self,
        user_id: &UserId,
        device_id: &DeviceId,
    ) -> Result<Option<Device>> {
        if let Some(device_data) = self.inner.store.get_device(user_id, device_id).await? {
            Ok(Some(self.wrap_device_data(device_data).await?))
        } else {
            Ok(None)
        }
    }

    /// Create a new device using the supplied [`DeviceData`]. Normally we would
    /// call [`Self::get_device`] to find an existing device inside this
    /// store. Only call this if you have some existing DeviceData and want
    /// to wrap it with the extra information provided by a [`Device`].
    pub(crate) async fn wrap_device_data(&self, device_data: DeviceData) -> Result<Device> {
        let own_identity = self
            .inner
            .store
            .get_user_identity(self.user_id())
            .await?
            .and_then(|i| i.own().cloned());

        let device_owner_identity =
            self.inner.store.get_user_identity(device_data.user_id()).await?;

        Ok(Device {
            inner: device_data,
            verification_machine: self.inner.verification_machine.clone(),
            own_identity,
            device_owner_identity,
        })
    }

    ///  Get the Identity of `user_id`
    pub(crate) async fn get_identity(&self, user_id: &UserId) -> Result<Option<UserIdentities>> {
        let own_identity = self
            .inner
            .store
            .get_user_identity(self.user_id())
            .await?
            .and_then(as_variant!(UserIdentityData::Own));

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
                if let Some(key) = self.load_backup_keys().await?.decryption_key {
                    let exported = key.to_base64();
                    Some(exported)
                } else {
                    None
                }
            }
            name => {
                warn!(secret = ?name, "Unknown secret was requested");
                None
            }
        })
    }

    /// Export all the private cross signing keys we have.
    ///
    /// The export will contain the seed for the ed25519 keys as a unpadded
    /// base64 encoded string.
    ///
    /// This method returns `None` if we don't have any private cross signing
    /// keys.
    pub async fn export_cross_signing_keys(
        &self,
    ) -> Result<Option<CrossSigningKeyExport>, CryptoStoreError> {
        let master_key = self.export_secret(&SecretName::CrossSigningMasterKey).await?;
        let self_signing_key = self.export_secret(&SecretName::CrossSigningSelfSigningKey).await?;
        let user_signing_key = self.export_secret(&SecretName::CrossSigningUserSigningKey).await?;

        Ok(if master_key.is_none() && self_signing_key.is_none() && user_signing_key.is_none() {
            None
        } else {
            Some(CrossSigningKeyExport { master_key, self_signing_key, user_signing_key })
        })
    }

    /// Import our private cross signing keys.
    ///
    /// The export needs to contain the seed for the Ed25519 keys as an unpadded
    /// base64 encoded string.
    pub async fn import_cross_signing_keys(
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
                changes.identities.changed.push(UserIdentityData::Own(public_identity.inner));
            }

            info!(?status, "Successfully imported the private cross-signing keys");

            self.save_changes(changes).await?;
        } else {
            warn!("No public identity found while importing cross-signing keys, a /keys/query needs to be done");
        }

        Ok(self.inner.identity.lock().await.status().await)
    }

    /// Export all the secrets we have in the store into a [`SecretsBundle`].
    ///
    /// This method will export all the private cross-signing keys and, if
    /// available, the private part of a backup key and its accompanying
    /// version.
    ///
    /// The method will fail if we don't have all three private cross-signing
    /// keys available.
    ///
    /// **Warning**: Only export this and share it with a trusted recipient,
    /// i.e. if an existing device is sharing this with a new device.
    pub async fn export_secrets_bundle(&self) -> Result<SecretsBundle, SecretsBundleExportError> {
        let Some(cross_signing) = self.export_cross_signing_keys().await? else {
            return Err(SecretsBundleExportError::MissingCrossSigningKeys);
        };

        let Some(master_key) = cross_signing.master_key.clone() else {
            return Err(SecretsBundleExportError::MissingCrossSigningKey(KeyUsage::Master));
        };

        let Some(user_signing_key) = cross_signing.user_signing_key.clone() else {
            return Err(SecretsBundleExportError::MissingCrossSigningKey(KeyUsage::UserSigning));
        };

        let Some(self_signing_key) = cross_signing.self_signing_key.clone() else {
            return Err(SecretsBundleExportError::MissingCrossSigningKey(KeyUsage::SelfSigning));
        };

        let backup_keys = self.load_backup_keys().await?;

        let backup = if let Some(key) = backup_keys.decryption_key {
            if let Some(backup_version) = backup_keys.backup_version {
                Some(BackupSecrets::MegolmBackupV1Curve25519AesSha2(
                    MegolmBackupV1Curve25519AesSha2Secrets { key, backup_version },
                ))
            } else {
                return Err(SecretsBundleExportError::MissingBackupVersion);
            }
        } else {
            None
        };

        Ok(SecretsBundle {
            cross_signing: CrossSigningSecrets { master_key, user_signing_key, self_signing_key },
            backup,
        })
    }

    /// Import and persists secrets from a [`SecretsBundle`].
    ///
    /// This method will import all the private cross-signing keys and, if
    /// available, the private part of a backup key and its accompanying
    /// version into the store.
    ///
    /// **Warning**: Only import this from a trusted source, i.e. if an existing
    /// device is sharing this with a new device. The imported cross-signing
    /// keys will create a [`OwnUserIdentity`] and mark it as verified.
    ///
    /// The backup key will be persisted in the store and can be enabled using
    /// the [`BackupMachine`].
    pub async fn import_secrets_bundle(
        &self,
        bundle: &SecretsBundle,
    ) -> Result<(), SecretImportError> {
        let mut changes = Changes::default();

        if let Some(backup_bundle) = &bundle.backup {
            match backup_bundle {
                BackupSecrets::MegolmBackupV1Curve25519AesSha2(bundle) => {
                    changes.backup_decryption_key = Some(bundle.key.clone());
                    changes.backup_version = Some(bundle.backup_version.clone());
                }
            }
        }

        let identity = self.inner.identity.lock().await;

        identity
            .import_secrets_unchecked(
                Some(&bundle.cross_signing.master_key),
                Some(&bundle.cross_signing.self_signing_key),
                Some(&bundle.cross_signing.user_signing_key),
            )
            .await?;

        let public_identity = identity.to_public_identity().await.expect(
            "We should be able to create a new public identity since we just imported \
             all the private cross-signing keys",
        );

        changes.private_identity = Some(identity.clone());
        changes.identities.new.push(UserIdentityData::Own(public_identity));

        Ok(self.save_changes(changes).await?)
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

    /// Receive notifications of received `m.room_key.withheld` messages.
    ///
    /// Each time an `m.room_key.withheld` is received and stored, an update
    /// will be sent to the stream. Updates that happen at the same time are
    /// batched into a [`Vec`].
    ///
    /// If the reader of the stream lags too far behind, a warning will be
    /// logged and items will be dropped.
    pub fn room_keys_withheld_received_stream(
        &self,
    ) -> impl Stream<Item = Vec<RoomKeyWithheldInfo>> {
        self.inner.store.room_keys_withheld_received_stream()
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

    /// Import the given room keys into the store.
    ///
    /// # Arguments
    ///
    /// * `exported_keys` - The keys to be imported.
    /// * `from_backup_version` - If the keys came from key backup, the key
    ///   backup version. This will cause the keys to be marked as already
    ///   backed up, and therefore not requiring another backup.
    /// * `progress_listener` - Callback which will be called after each key is
    ///   processed. Called with arguments `(processed, total)` where
    ///   `processed` is the number of keys processed so far, and `total` is the
    ///   total number of keys (i.e., `exported_keys.len()`).
    pub async fn import_room_keys(
        &self,
        exported_keys: Vec<ExportedRoomKey>,
        from_backup_version: Option<&str>,
        progress_listener: impl Fn(usize, usize),
    ) -> Result<RoomKeyImportResult> {
        let mut sessions = Vec::new();

        async fn new_session_better(
            session: &InboundGroupSession,
            old_session: Option<InboundGroupSession>,
        ) -> bool {
            if let Some(old_session) = &old_session {
                session.compare(old_session).await == SessionOrdering::Better
            } else {
                true
            }
        }

        let total_count = exported_keys.len();
        let mut keys = BTreeMap::new();

        for (i, key) in exported_keys.into_iter().enumerate() {
            match InboundGroupSession::from_export(&key) {
                Ok(session) => {
                    let old_session = self
                        .inner
                        .store
                        .get_inbound_group_session(session.room_id(), session.session_id())
                        .await?;

                    // Only import the session if we didn't have this session or
                    // if it's a better version of the same session.
                    if new_session_better(&session, old_session).await {
                        if from_backup_version.is_some() {
                            session.mark_as_backed_up();
                        }

                        keys.entry(session.room_id().to_owned())
                            .or_insert_with(BTreeMap::new)
                            .entry(session.sender_key().to_base64())
                            .or_insert_with(BTreeSet::new)
                            .insert(session.session_id().to_owned());

                        sessions.push(session);
                    }
                }
                Err(e) => {
                    warn!(
                        sender_key= key.sender_key.to_base64(),
                        room_id = ?key.room_id,
                        session_id = key.session_id,
                        error = ?e,
                        "Couldn't import a room key from a file export."
                    );
                }
            }

            progress_listener(i, total_count);
        }

        let imported_count = sessions.len();

        self.inner.store.save_inbound_group_sessions(sessions, from_backup_version).await?;

        info!(total_count, imported_count, room_keys = ?keys, "Successfully imported room keys");

        Ok(RoomKeyImportResult::new(imported_count, total_count, keys))
    }

    /// Import the given room keys into our store.
    ///
    /// # Arguments
    ///
    /// * `exported_keys` - A list of previously exported keys that should be
    ///   imported into our store. If we already have a better version of a key
    ///   the key will *not* be imported.
    ///
    /// Returns a tuple of numbers that represent the number of sessions that
    /// were imported and the total number of sessions that were found in the
    /// key export.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use std::io::Cursor;
    /// # use matrix_sdk_crypto::{OlmMachine, decrypt_room_key_export};
    /// # use ruma::{device_id, user_id};
    /// # let alice = user_id!("@alice:example.org");
    /// # async {
    /// # let machine = OlmMachine::new(&alice, device_id!("DEVICEID")).await;
    /// # let export = Cursor::new("".to_owned());
    /// let exported_keys = decrypt_room_key_export(export, "1234").unwrap();
    /// machine.store().import_exported_room_keys(exported_keys, |_, _| {}).await.unwrap();
    /// # };
    /// ```
    pub async fn import_exported_room_keys(
        &self,
        exported_keys: Vec<ExportedRoomKey>,
        progress_listener: impl Fn(usize, usize),
    ) -> Result<RoomKeyImportResult> {
        self.import_room_keys(exported_keys, None, progress_listener).await
    }

    pub(crate) fn crypto_store(&self) -> Arc<CryptoStoreWrapper> {
        self.inner.store.clone()
    }

    /// Export the keys that match the given predicate.
    ///
    /// # Arguments
    ///
    /// * `predicate` - A closure that will be called for every known
    ///   `InboundGroupSession`, which represents a room key. If the closure
    ///   returns `true` the `InboundGroupSession` will be included in the
    ///   export, if the closure returns `false` it will not be included.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use matrix_sdk_crypto::{OlmMachine, encrypt_room_key_export};
    /// # use ruma::{device_id, user_id, room_id};
    /// # let alice = user_id!("@alice:example.org");
    /// # async {
    /// # let machine = OlmMachine::new(&alice, device_id!("DEVICEID")).await;
    /// let room_id = room_id!("!test:localhost");
    /// let exported_keys = machine.store().export_room_keys(|s| s.room_id() == room_id).await.unwrap();
    /// let encrypted_export = encrypt_room_key_export(&exported_keys, "1234", 1);
    /// # };
    /// ```
    pub async fn export_room_keys(
        &self,
        predicate: impl FnMut(&InboundGroupSession) -> bool,
    ) -> Result<Vec<ExportedRoomKey>> {
        let mut exported = Vec::new();

        let mut sessions = self.get_inbound_group_sessions().await?;
        sessions.retain(predicate);

        for session in sessions {
            let export = session.export().await;
            exported.push(export);
        }

        Ok(exported)
    }

    /// Export room keys matching a predicate, providing them as an async
    /// `Stream`.
    ///
    /// # Arguments
    ///
    /// * `predicate` - A closure that will be called for every known
    ///   `InboundGroupSession`, which represents a room key. If the closure
    ///   returns `true` the `InboundGroupSession` will be included in the
    ///   export, if the closure returns `false` it will not be included.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use std::pin::pin;
    ///
    /// use matrix_sdk_crypto::{olm::ExportedRoomKey, OlmMachine};
    /// use ruma::{device_id, room_id, user_id};
    /// use tokio_stream::StreamExt;
    /// # async {
    /// let alice = user_id!("@alice:example.org");
    /// let machine = OlmMachine::new(&alice, device_id!("DEVICEID")).await;
    /// let room_id = room_id!("!test:localhost");
    /// let mut keys = pin!(machine
    ///     .store()
    ///     .export_room_keys_stream(|s| s.room_id() == room_id)
    ///     .await
    ///     .unwrap());
    /// while let Some(key) = keys.next().await {
    ///     println!("{}", key.room_id);
    /// }
    /// # };
    /// ```
    pub async fn export_room_keys_stream(
        &self,
        predicate: impl FnMut(&InboundGroupSession) -> bool,
    ) -> Result<impl Stream<Item = ExportedRoomKey>> {
        // TODO: if/when there is a get_inbound_group_sessions_stream, use that here.
        let sessions = self.get_inbound_group_sessions().await?;
        Ok(futures_util::stream::iter(sessions.into_iter().filter(predicate))
            .then(|session| async move { session.export().await }))
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

#[cfg(test)]
mod tests {
    use std::pin::pin;

    use futures_util::StreamExt;
    use matrix_sdk_test::async_test;
    use ruma::{room_id, user_id};

    use crate::{machine::test_helpers::get_machine_pair, types::EventEncryptionAlgorithm};

    #[async_test]
    async fn import_room_keys_notifies_stream() {
        use futures_util::FutureExt;

        let (alice, bob, _) =
            get_machine_pair(user_id!("@a:s.co"), user_id!("@b:s.co"), false).await;

        let room1_id = room_id!("!room1:localhost");
        alice.create_outbound_group_session_with_defaults_test_helper(room1_id).await.unwrap();
        let exported_sessions = alice.store().export_room_keys(|_| true).await.unwrap();

        let mut room_keys_received_stream = Box::pin(bob.store().room_keys_received_stream());
        bob.store().import_room_keys(exported_sessions, None, |_, _| {}).await.unwrap();

        let room_keys = room_keys_received_stream
            .next()
            .now_or_never()
            .flatten()
            .expect("We should have received an update of room key infos");
        assert_eq!(room_keys.len(), 1);
        assert_eq!(room_keys[0].room_id, "!room1:localhost");
    }

    #[async_test]
    async fn export_room_keys_provides_selected_keys() {
        // Given an OlmMachine with room keys in it
        let (alice, _, _) = get_machine_pair(user_id!("@a:s.co"), user_id!("@b:s.co"), false).await;
        let room1_id = room_id!("!room1:localhost");
        let room2_id = room_id!("!room2:localhost");
        let room3_id = room_id!("!room3:localhost");
        alice.create_outbound_group_session_with_defaults_test_helper(room1_id).await.unwrap();
        alice.create_outbound_group_session_with_defaults_test_helper(room2_id).await.unwrap();
        alice.create_outbound_group_session_with_defaults_test_helper(room3_id).await.unwrap();

        // When I export some of the keys
        let keys = alice
            .store()
            .export_room_keys(|s| s.room_id() == room2_id || s.room_id() == room3_id)
            .await
            .unwrap();

        // Then the requested keys were provided
        assert_eq!(keys.len(), 2);
        assert_eq!(keys[0].algorithm, EventEncryptionAlgorithm::MegolmV1AesSha2);
        assert_eq!(keys[1].algorithm, EventEncryptionAlgorithm::MegolmV1AesSha2);
        assert_eq!(keys[0].room_id, "!room2:localhost");
        assert_eq!(keys[1].room_id, "!room3:localhost");
        assert_eq!(keys[0].session_key.to_base64().len(), 220);
        assert_eq!(keys[1].session_key.to_base64().len(), 220);
    }

    #[async_test]
    async fn export_room_keys_stream_can_provide_all_keys() {
        // Given an OlmMachine with room keys in it
        let (alice, _, _) = get_machine_pair(user_id!("@a:s.co"), user_id!("@b:s.co"), false).await;
        let room1_id = room_id!("!room1:localhost");
        let room2_id = room_id!("!room2:localhost");
        alice.create_outbound_group_session_with_defaults_test_helper(room1_id).await.unwrap();
        alice.create_outbound_group_session_with_defaults_test_helper(room2_id).await.unwrap();

        // When I export the keys as a stream
        let mut keys = pin!(alice.store().export_room_keys_stream(|_| true).await.unwrap());

        // And collect them
        let mut collected = vec![];
        while let Some(key) = keys.next().await {
            collected.push(key);
        }

        // Then all the keys were provided
        assert_eq!(collected.len(), 2);
        assert_eq!(collected[0].algorithm, EventEncryptionAlgorithm::MegolmV1AesSha2);
        assert_eq!(collected[1].algorithm, EventEncryptionAlgorithm::MegolmV1AesSha2);
        assert_eq!(collected[0].room_id, "!room1:localhost");
        assert_eq!(collected[1].room_id, "!room2:localhost");
        assert_eq!(collected[0].session_key.to_base64().len(), 220);
        assert_eq!(collected[1].session_key.to_base64().len(), 220);
    }

    #[async_test]
    async fn export_room_keys_stream_can_provide_a_subset_of_keys() {
        // Given an OlmMachine with room keys in it
        let (alice, _, _) = get_machine_pair(user_id!("@a:s.co"), user_id!("@b:s.co"), false).await;
        let room1_id = room_id!("!room1:localhost");
        let room2_id = room_id!("!room2:localhost");
        alice.create_outbound_group_session_with_defaults_test_helper(room1_id).await.unwrap();
        alice.create_outbound_group_session_with_defaults_test_helper(room2_id).await.unwrap();

        // When I export the keys as a stream
        let mut keys =
            pin!(alice.store().export_room_keys_stream(|s| s.room_id() == room1_id).await.unwrap());

        // And collect them
        let mut collected = vec![];
        while let Some(key) = keys.next().await {
            collected.push(key);
        }

        // Then all the keys matching our predicate were provided, and no others
        assert_eq!(collected.len(), 1);
        assert_eq!(collected[0].algorithm, EventEncryptionAlgorithm::MegolmV1AesSha2);
        assert_eq!(collected[0].room_id, "!room1:localhost");
        assert_eq!(collected[0].session_key.to_base64().len(), 220);
    }

    #[async_test]
    async fn export_secrets_bundle() {
        let user_id = user_id!("@alice:example.com");
        let (first, second, _) = get_machine_pair(user_id, user_id, false).await;

        let _ = first
            .bootstrap_cross_signing(false)
            .await
            .expect("We should be able to bootstrap cross-signing");

        let bundle = first.store().export_secrets_bundle().await.expect(
            "We should be able to export the secrets bundle, now that we \
             have the cross-signing keys",
        );

        assert!(bundle.backup.is_none(), "The bundle should not contain a backup key");

        second
            .store()
            .import_secrets_bundle(&bundle)
            .await
            .expect("We should be able to import the secrets bundle");

        let status = second.cross_signing_status().await;
        let identity = second.get_identity(user_id, None).await.unwrap().unwrap().own().unwrap();

        assert!(identity.is_verified(), "The public identity should be marked as verified.");

        assert!(status.is_complete(), "We should have imported all the cross-signing keys");
    }
}
