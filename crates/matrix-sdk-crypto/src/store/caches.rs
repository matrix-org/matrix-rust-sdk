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
    collections::{HashMap, HashSet},
    sync::Arc,
};

use dashmap::DashMap;
use matrix_sdk_common::locks::Mutex;
use ruma::{DeviceId, OwnedDeviceId, OwnedRoomId, OwnedUserId, RoomId, UserId};
use tracing::{field::debug, instrument, trace, Span};

use crate::{
    identities::ReadOnlyDevice,
    olm::{InboundGroupSession, Session},
};

/// In-memory store for Olm Sessions.
#[derive(Debug, Default, Clone)]
pub struct SessionStore {
    entries: Arc<DashMap<String, Arc<Mutex<Vec<Session>>>>>,
}

impl SessionStore {
    /// Create a new empty Session store.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a session to the store.
    ///
    /// Returns true if the session was added, false if the session was
    /// already in the store.
    pub async fn add(&self, session: Session) -> bool {
        let sessions_lock = self.entries.entry(session.sender_key.to_base64()).or_default();
        let mut sessions = sessions_lock.lock().await;

        if !sessions.contains(&session) {
            sessions.push(session);
            true
        } else {
            false
        }
    }

    /// Get all the sessions that belong to the given sender key.
    pub fn get(&self, sender_key: &str) -> Option<Arc<Mutex<Vec<Session>>>> {
        self.entries.get(sender_key).map(|s| s.clone())
    }

    /// Add a list of sessions belonging to the sender key.
    pub fn set_for_sender(&self, sender_key: &str, sessions: Vec<Session>) {
        self.entries.insert(sender_key.to_owned(), Arc::new(Mutex::new(sessions)));
    }
}

#[derive(Debug, Default, Clone)]
/// In-memory store that holds inbound group sessions.
pub struct GroupSessionStore {
    entries: Arc<DashMap<OwnedRoomId, HashMap<String, InboundGroupSession>>>,
}

impl GroupSessionStore {
    /// Create a new empty store.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add an inbound group session to the store.
    ///
    /// Returns true if the session was added, false if the session was
    /// already in the store.
    pub fn add(&self, session: InboundGroupSession) -> bool {
        self.entries
            .entry(session.room_id().to_owned())
            .or_default()
            .insert(session.session_id().to_owned(), session)
            .is_none()
    }

    /// Get all the group sessions the store knows about.
    pub fn get_all(&self) -> Vec<InboundGroupSession> {
        self.entries
            .iter()
            .flat_map(|keys| keys.values().cloned().collect::<Vec<InboundGroupSession>>())
            .collect()
    }

    /// Get the number of `InboundGroupSession`s we have.
    pub fn count(&self) -> usize {
        self.entries.iter().map(|d| d.value().len()).sum()
    }

    /// Get a inbound group session from our store.
    ///
    /// # Arguments
    /// * `room_id` - The room id of the room that the session belongs to.
    ///
    /// * `session_id` - The unique id of the session.
    pub fn get(&self, room_id: &RoomId, session_id: &str) -> Option<InboundGroupSession> {
        self.entries.get(room_id)?.get(session_id).cloned()
    }
}

/// In-memory store holding the devices of users.
#[derive(Clone, Debug, Default)]
pub struct DeviceStore {
    entries: Arc<DashMap<OwnedUserId, DashMap<OwnedDeviceId, ReadOnlyDevice>>>,
}

impl DeviceStore {
    /// Create a new empty device store.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a device to the store.
    ///
    /// Returns true if the device was already in the store, false otherwise.
    pub fn add(&self, device: ReadOnlyDevice) -> bool {
        let user_id = device.user_id();
        self.entries
            .entry(user_id.to_owned())
            .or_default()
            .insert(device.device_id().into(), device)
            .is_none()
    }

    /// Get the device with the given device_id and belonging to the given user.
    pub fn get(&self, user_id: &UserId, device_id: &DeviceId) -> Option<ReadOnlyDevice> {
        self.entries.get(user_id).and_then(|m| m.get(device_id).map(|d| d.value().clone()))
    }

    /// Remove the device with the given device_id and belonging to the given
    /// user.
    ///
    /// Returns the device if it was removed, None if it wasn't in the store.
    pub fn remove(&self, user_id: &UserId, device_id: &DeviceId) -> Option<ReadOnlyDevice> {
        self.entries.get(user_id).and_then(|m| m.remove(device_id)).map(|(_, d)| d)
    }

    /// Get a read-only view over all devices of the given user.
    pub fn user_devices(&self, user_id: &UserId) -> HashMap<OwnedDeviceId, ReadOnlyDevice> {
        self.entries
            .entry(user_id.to_owned())
            .or_default()
            .iter()
            .map(|i| (i.key().to_owned(), i.value().clone()))
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
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct SequenceNumber(i64);

impl PartialOrd for SequenceNumber {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.0.wrapping_sub(other.0).cmp(&0))
    }
}

impl Ord for SequenceNumber {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.0.wrapping_sub(other.0).cmp(&0)
    }
}

impl SequenceNumber {
    fn increment(&mut self) {
        self.0 = self.0.wrapping_add(1)
    }

    fn previous(&self) -> Self {
        Self(self.0.wrapping_sub(1))
    }
}

/// Record of the users that are waiting for a /keys/query.
///
/// To avoid races, we maintain a sequence number which is updated each time we
/// receive an invalidation notification. We also record the sequence number at
/// which each user was last invalidated. Then, we attach the current sequence
/// number to each `/keys/query` request, and when we get the response we can
/// tell if any users have been invalidated more recently than that request.
#[derive(Debug)]
pub(super) struct UsersForKeyQuery {
    /// The sequence number we will assign to the next addition to user_map
    next_sequence_number: SequenceNumber,

    /// The users pending a lookup, together with the sequence number at which
    /// they were added to the list
    user_map: HashMap<OwnedUserId, SequenceNumber>,
}

impl UsersForKeyQuery {
    /// Create a new, empty, `UsersForKeyQueryCache`
    pub(super) fn new() -> Self {
        UsersForKeyQuery { next_sequence_number: Default::default(), user_map: Default::default() }
    }

    /// Record a new user that requires a key query
    pub(super) fn insert_user(&mut self, user: &UserId) {
        let seq = self.next_sequence_number;

        trace!(?user, sequence_number = ?seq, "Flagging user for key query");

        self.user_map.insert(user.to_owned(), seq);
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

        if let Some(last_invalidation) = last_invalidation {
            Span::current().record("invalidation_sequence", debug(last_invalidation));

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
}

#[cfg(test)]
mod tests {
    use matrix_sdk_test::async_test;
    use proptest::prelude::*;
    use ruma::room_id;
    use vodozemac::{Curve25519PublicKey, Ed25519PublicKey};

    use super::{DeviceStore, GroupSessionStore, SequenceNumber, SessionStore};
    use crate::{
        identities::device::testing::get_device,
        olm::{tests::get_account_and_session, InboundGroupSession},
    };

    #[async_test]
    async fn test_session_store() {
        let (_, session) = get_account_and_session().await;

        let store = SessionStore::new();

        assert!(store.add(session.clone()).await);
        assert!(!store.add(session.clone()).await);

        let sessions = store.get(&session.sender_key.to_base64()).unwrap();
        let sessions = sessions.lock().await;

        let loaded_session = &sessions[0];

        assert_eq!(&session, loaded_session);
    }

    #[async_test]
    async fn test_session_store_bulk_storing() {
        let (_, session) = get_account_and_session().await;

        let store = SessionStore::new();
        store.set_for_sender(&session.sender_key.to_base64(), vec![session.clone()]);

        let sessions = store.get(&session.sender_key.to_base64()).unwrap();
        let sessions = sessions.lock().await;

        let loaded_session = &sessions[0];

        assert_eq!(&session, loaded_session);
    }

    #[async_test]
    async fn test_group_session_store() {
        let (account, _) = get_account_and_session().await;
        let room_id = room_id!("!test:localhost");
        let curve_key = "Nn0L2hkcCMFKqynTjyGsJbth7QrVmX3lbrksMkrGOAw";

        let (outbound, _) = account.create_group_session_pair_with_defaults(room_id).await;

        assert_eq!(0, outbound.message_index().await);
        assert!(!outbound.shared());
        outbound.mark_as_shared();
        assert!(outbound.shared());

        let inbound = InboundGroupSession::new(
            Curve25519PublicKey::from_base64(curve_key).unwrap(),
            Ed25519PublicKey::from_base64("ee3Ek+J2LkkPmjGPGLhMxiKnhiX//xcqaVL4RP6EypE").unwrap(),
            room_id,
            &outbound.session_key().await,
            outbound.settings().algorithm.to_owned(),
            None,
        )
        .unwrap();

        let store = GroupSessionStore::new();
        store.add(inbound.clone());

        let loaded_session = store.get(room_id, outbound.session_id()).unwrap();
        assert_eq!(inbound, loaded_session);
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
