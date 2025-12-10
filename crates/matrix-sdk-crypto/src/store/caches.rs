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

//! Collection of small in-memory stores that can be used to cache Olm objects.
//!
//! Note: You'll only be interested in these if you are implementing a custom
//! `CryptoStore`.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    fmt::Display,
    ops::Deref,
    sync::{
        Arc, Weak,
        atomic::{AtomicBool, Ordering},
    },
};

use matrix_sdk_common::locks::RwLock as StdRwLock;
use ruma::{DeviceId, OwnedDeviceId, OwnedUserId, UserId};
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, MutexGuard, OwnedRwLockReadGuard, RwLock};
use tracing::{Span, field::display, instrument, trace};

use super::{CryptoStoreError, CryptoStoreWrapper};
use crate::{Account, identities::DeviceData, olm::Session};

/// In-memory store for Olm Sessions.
#[derive(Debug, Default, Clone)]
pub struct SessionStore {
    #[allow(clippy::type_complexity)]
    pub(crate) entries: Arc<RwLock<BTreeMap<String, Arc<Mutex<Vec<Session>>>>>>,
}

impl SessionStore {
    /// Create a new empty Session store.
    pub fn new() -> Self {
        Self::default()
    }

    /// Clear all entries in the session store.
    ///
    /// This is intended to be used when regenerating olm machines.
    pub async fn clear(&self) {
        self.entries.write().await.clear()
    }

    /// Add a session to the store.
    ///
    /// Returns true if the session was added, false if the session was
    /// already in the store.
    pub async fn add(&self, session: Session) -> bool {
        let sessions_lock =
            self.entries.write().await.entry(session.sender_key.to_base64()).or_default().clone();

        let mut sessions = sessions_lock.lock().await;

        if !sessions.contains(&session) {
            sessions.push(session);
            true
        } else {
            false
        }
    }

    /// Get all the sessions that belong to the given sender key.
    pub async fn get(&self, sender_key: &str) -> Option<Arc<Mutex<Vec<Session>>>> {
        self.entries.read().await.get(sender_key).cloned()
    }

    /// Add a list of sessions belonging to the sender key.
    pub async fn set_for_sender(&self, sender_key: &str, sessions: Vec<Session>) {
        self.entries.write().await.insert(sender_key.to_owned(), Arc::new(Mutex::new(sessions)));
    }
}

/// In-memory store holding the devices of users.
#[derive(Debug, Default)]
pub struct DeviceStore {
    entries: StdRwLock<BTreeMap<OwnedUserId, BTreeMap<OwnedDeviceId, DeviceData>>>,
}

impl DeviceStore {
    /// Create a new empty device store.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a device to the store.
    ///
    /// Returns true if the device was already in the store, false otherwise.
    pub fn add(&self, device: DeviceData) -> bool {
        let user_id = device.user_id();
        self.entries
            .write()
            .entry(user_id.to_owned())
            .or_default()
            .insert(device.device_id().into(), device)
            .is_none()
    }

    /// Get the device with the given device_id and belonging to the given user.
    pub fn get(&self, user_id: &UserId, device_id: &DeviceId) -> Option<DeviceData> {
        Some(self.entries.read().get(user_id)?.get(device_id)?.clone())
    }

    /// Remove the device with the given device_id and belonging to the given
    /// user.
    ///
    /// Returns the device if it was removed, None if it wasn't in the store.
    pub fn remove(&self, user_id: &UserId, device_id: &DeviceId) -> Option<DeviceData> {
        self.entries.write().get_mut(user_id)?.remove(device_id)
    }

    /// Get a read-only view over all devices of the given user.
    pub fn user_devices(&self, user_id: &UserId) -> HashMap<OwnedDeviceId, DeviceData> {
        self.entries
            .write()
            .entry(user_id.to_owned())
            .or_default()
            .iter()
            .map(|(key, value)| (key.to_owned(), value.clone()))
            .collect()
    }
}

/// A numeric type that can represent an infinite ordered sequence.
///
/// It uses wrapping arithmetic to make sure we never run out of numbers. (2**64
/// should be enough for anyone, but it's easy enough just to make it wrap.)
//
/// Internally it uses a *signed* counter so that we can compare values via a
/// subtraction. For example, suppose we've just overflowed from i64::MAX to
/// i64::MIN. (i64::MAX.wrapping_sub(i64::MIN)) is -1, which tells us that
/// i64::MAX comes before i64::MIN in the sequence.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(transparent)]
pub struct SequenceNumber(i64);

impl Display for SequenceNumber {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl PartialOrd for SequenceNumber {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for SequenceNumber {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.0.wrapping_sub(other.0).cmp(&0)
    }
}

impl SequenceNumber {
    pub(crate) fn increment(&mut self) {
        self.0 = self.0.wrapping_add(1)
    }

    fn previous(&self) -> Self {
        Self(self.0.wrapping_sub(1))
    }
}

/// Information on a task which is waiting for a `/keys/query` to complete.
#[derive(Debug)]
pub(super) struct KeysQueryWaiter {
    /// The user that we are waiting for
    user: OwnedUserId,

    /// The sequence number of the last invalidation of the users's device list
    /// when we started waiting (ie, any `/keys/query` result with the same or
    /// greater sequence number will satisfy this waiter)
    sequence_number: SequenceNumber,

    /// Whether the `/keys/query` has completed.
    ///
    /// This is only modified whilst holding the mutex on `users_for_key_query`.
    pub(super) completed: AtomicBool,
}

/// Record of the users that are waiting for a /keys/query.
///
/// To avoid races, we maintain a sequence number which is updated each time we
/// receive an invalidation notification. We also record the sequence number at
/// which each user was last invalidated. Then, we attach the current sequence
/// number to each `/keys/query` request, and when we get the response we can
/// tell if any users have been invalidated more recently than that request.
#[derive(Debug, Default)]
pub(super) struct UsersForKeyQuery {
    /// The sequence number we will assign to the next addition to user_map
    next_sequence_number: SequenceNumber,

    /// The users pending a lookup, together with the sequence number at which
    /// they were added to the list
    user_map: HashMap<OwnedUserId, SequenceNumber>,

    /// A list of tasks waiting for key queries to complete.
    ///
    /// We expect this list to remain fairly short, so don't bother partitioning
    /// by user.
    tasks_awaiting_key_query: Vec<Weak<KeysQueryWaiter>>,
}

impl UsersForKeyQuery {
    /// Record a new user that requires a key query
    pub(super) fn insert_user(&mut self, user: &UserId) {
        let sequence_number = self.next_sequence_number;

        trace!(?user, %sequence_number, "Flagging user for key query");

        self.user_map.insert(user.to_owned(), sequence_number);
        self.next_sequence_number.increment();
    }

    /// Record that a user has received an update with the given sequence
    /// number.
    ///
    /// If the sequence number is newer than the oldest invalidation for this
    /// user, it is removed from the list of those needing an update.
    ///
    /// Returns true if the user is now up-to-date, else false
    #[instrument(level = "trace", skip(self), fields(invalidation_sequence))]
    pub(super) fn maybe_remove_user(
        &mut self,
        user: &UserId,
        query_sequence: SequenceNumber,
    ) -> bool {
        let last_invalidation = self.user_map.get(user).copied();

        // If there were any jobs waiting for this key query to complete, we can flag
        // them as completed and remove them from our list. We also clear out any tasks
        // that have been cancelled.
        self.tasks_awaiting_key_query.retain(|waiter| {
            let Some(waiter) = waiter.upgrade() else {
                // the TaskAwaitingKeyQuery has been dropped, so it probably timed out and the
                // caller went away. We can remove it from our list whether or not it's for this
                // user.
                trace!("removing expired waiting task");

                return false;
            };

            if waiter.user == user && waiter.sequence_number <= query_sequence {
                trace!(
                    ?user,
                    %query_sequence,
                    waiter_sequence = %waiter.sequence_number,
                    "Removing completed waiting task"
                );

                waiter.completed.store(true, Ordering::Relaxed);

                false
            } else {
                trace!(
                    ?user,
                    %query_sequence,
                    waiter_user = ?waiter.user,
                    waiter_sequence= %waiter.sequence_number,
                    "Retaining still-waiting task"
                );

                true
            }
        });

        if let Some(last_invalidation) = last_invalidation {
            Span::current().record("invalidation_sequence", display(last_invalidation));

            if last_invalidation > query_sequence {
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
    pub(super) fn users_for_key_query(&self) -> (HashSet<OwnedUserId>, SequenceNumber) {
        // we return the sequence number of the last invalidation
        let sequence_number = self.next_sequence_number.previous();
        (self.user_map.keys().cloned().collect(), sequence_number)
    }

    /// Check if a key query is pending for a user, and register for a wakeup if
    /// so.
    ///
    /// If no key query is currently pending, returns `None`. Otherwise, returns
    /// (an `Arc` to) a `KeysQueryWaiter`, whose `completed` flag will
    /// be set once the lookup completes.
    pub(super) fn maybe_register_waiting_task(
        &mut self,
        user: &UserId,
    ) -> Option<Arc<KeysQueryWaiter>> {
        self.user_map.get(user).map(|&sequence_number| {
            trace!(?user, %sequence_number, "Registering new waiting task");

            let waiter = Arc::new(KeysQueryWaiter {
                sequence_number,
                user: user.to_owned(),
                completed: AtomicBool::new(false),
            });

            self.tasks_awaiting_key_query.push(Arc::downgrade(&waiter));

            waiter
        })
    }
}

#[derive(Debug)]
pub(crate) struct StoreCache {
    pub(super) store: Arc<CryptoStoreWrapper>,
    pub(super) tracked_users: StdRwLock<BTreeSet<OwnedUserId>>,
    pub(super) loaded_tracked_users: RwLock<bool>,
    pub(super) account: Mutex<Option<Account>>,
}

impl StoreCache {
    pub(crate) fn store_wrapper(&self) -> &CryptoStoreWrapper {
        self.store.as_ref()
    }

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
    pub(super) async fn account(&self) -> super::Result<impl Deref<Target = Account> + '_> {
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
    pub(super) cache: OwnedRwLockReadGuard<StoreCache>,
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
    pub async fn account(&self) -> super::Result<impl Deref<Target = Account> + '_> {
        self.cache.account().await
    }
}

impl Deref for StoreCacheGuard {
    type Target = StoreCache;

    fn deref(&self) -> &Self::Target {
        &self.cache
    }
}

#[cfg(test)]
mod tests {
    use matrix_sdk_test::async_test;
    use proptest::prelude::*;

    use super::{DeviceStore, SequenceNumber, SessionStore};
    use crate::{
        identities::device::testing::get_device, olm::tests::get_account_and_session_test_helper,
    };

    #[async_test]
    async fn test_session_store() {
        let (_, session) = get_account_and_session_test_helper();

        let store = SessionStore::new();

        assert!(store.add(session.clone()).await);
        assert!(!store.add(session.clone()).await);

        let sessions = store.get(&session.sender_key.to_base64()).await.unwrap();
        let sessions = sessions.lock().await;

        let loaded_session = &sessions[0];

        assert_eq!(&session, loaded_session);
    }

    #[async_test]
    async fn test_session_store_bulk_storing() {
        let (_, session) = get_account_and_session_test_helper();

        let store = SessionStore::new();
        store.set_for_sender(&session.sender_key.to_base64(), vec![session.clone()]).await;

        let sessions = store.get(&session.sender_key.to_base64()).await.unwrap();
        let sessions = sessions.lock().await;

        let loaded_session = &sessions[0];

        assert_eq!(&session, loaded_session);
    }

    #[async_test]
    async fn test_device_store() {
        let device = get_device();
        let store = DeviceStore::new();

        assert!(store.add(device.clone()));
        assert!(!store.add(device.clone()));

        let loaded_device = store.get(device.user_id(), device.device_id()).unwrap();

        assert_eq!(device, loaded_device);

        let user_devices = store.user_devices(device.user_id());

        assert_eq!(&**user_devices.keys().next().unwrap(), device.device_id());
        assert_eq!(user_devices.values().next().unwrap(), &device);

        let loaded_device = user_devices.get(device.device_id()).unwrap();

        assert_eq!(&device, loaded_device);

        store.remove(device.user_id(), device.device_id());

        let loaded_device = store.get(device.user_id(), device.device_id());
        assert!(loaded_device.is_none());
    }

    #[test]
    fn sequence_at_boundary() {
        let first = SequenceNumber(i64::MAX);
        let second = SequenceNumber(first.0.wrapping_add(1));
        let third = SequenceNumber(first.0.wrapping_sub(1));

        assert!(second > first);
        assert!(first < second);
        assert!(third < first);
        assert!(first > third);
        assert!(second > third);
        assert!(third < second);
    }

    proptest! {
        #[test]
        fn partial_eq_sequence_number(sequence in i64::MIN..i64::MAX) {
            let first = SequenceNumber(sequence);
            let second = SequenceNumber(first.0.wrapping_add(1));
            let third = SequenceNumber(first.0.wrapping_sub(1));

            assert!(second > first);
            assert!(first < second);
            assert!(third < first);
            assert!(first > third);
            assert!(second > third);
            assert!(third < second);
        }
    }
}
