// Copyright 2026 The Matrix.org Foundation C.I.C.
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

//! Tracking of the global profiles fetched outside of a sync, e.g. via MSC4536.
//!
//! The sync keeps running while a fetch is in flight, so by the time the
//! response arrives some of the fetched profiles may have been updated or
//! dropped by the sync. To avoid overwriting these with stale data, every write
//! of global profiles invalidates the written users in the fetches tracking
//! them, and
//! [`BaseClient::finish_global_profile_fetch`] skips these users when saving.
//!
//! [`BaseClient::finish_global_profile_fetch`]: crate::BaseClient::finish_global_profile_fetch

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex},
};

use ruma::OwnedUserId;
use tokio::sync::MutexGuard;

/// The registry of the global profile fetches in flight.
///
/// Each fetch is registered for as long as its [`GlobalProfileFetchTicket`]
/// exists.
#[derive(Clone, Debug, Default)]
pub(crate) struct GlobalProfileFetches {
    inner: Arc<Mutex<FetchesInner>>,
}

#[derive(Debug, Default)]
struct FetchesInner {
    /// The ID to use for the next fetch.
    next_id: u64,

    /// The fetches in flight, by ID.
    fetches: BTreeMap<u64, Fetch>,
}

#[derive(Debug, Default)]
struct Fetch {
    /// The users whose profiles are being fetched.
    users: BTreeSet<OwnedUserId>,

    /// The users whose fetched profiles have been invalidated by an update
    /// since the fetch started.
    invalidated_users: BTreeSet<OwnedUserId>,
}

impl GlobalProfileFetches {
    /// Register a new fetch, initially for no users.
    pub(crate) fn begin(&self) -> GlobalProfileFetchTicket {
        let mut inner = self.inner.lock().unwrap();
        let id = inner.next_id;
        inner.next_id += 1;
        inner.fetches.insert(id, Fetch::default());

        GlobalProfileFetchTicket { id, fetches: self.clone() }
    }

    /// Invalidate the profiles of the given users in any fetch in flight, as
    /// they have just been updated. Called for every write of global profiles,
    /// e.g. by the sync.
    pub(crate) fn invalidate_users(
        &self,
        user_ids: &BTreeSet<OwnedUserId>,
        _state_store_guard: &MutexGuard<'_, ()>,
    ) {
        let mut inner = self.inner.lock().unwrap();
        for fetch in inner.fetches.values_mut() {
            fetch.invalidated_users.extend(fetch.users.intersection(user_ids).cloned());
        }
    }
}

/// A fetch of global profiles in flight, created with
/// [`BaseClient::begin_global_profile_fetch`].
///
/// Pass it to [`BaseClient::finish_global_profile_fetch`] along with the
/// fetched profiles. Dropping it cancels the fetch.
///
/// [`BaseClient::begin_global_profile_fetch`]: crate::BaseClient::begin_global_profile_fetch
/// [`BaseClient::finish_global_profile_fetch`]: crate::BaseClient::finish_global_profile_fetch
#[derive(Debug)]
pub struct GlobalProfileFetchTicket {
    /// The ID of this fetch in the registry.
    id: u64,

    /// The registry this fetch is registered in.
    fetches: GlobalProfileFetches,
}

impl GlobalProfileFetchTicket {
    /// Add the given users to this fetch.
    ///
    /// Any update to their global profiles from now on invalidates the fetched
    /// profiles. Call this before checking the store for which of these users'
    /// profiles are missing, so that no update can be missed.
    pub fn track(&self, users: impl IntoIterator<Item = OwnedUserId>) {
        let mut inner = self.fetches.inner.lock().unwrap();
        if let Some(fetch) = inner.fetches.get_mut(&self.id) {
            fetch.users.extend(users);
        }
    }

    /// Finish the fetch, returning the users whose fetched profiles have been
    /// invalidated since it started.
    pub(crate) fn finish(self, _state_store_guard: &MutexGuard<'_, ()>) -> BTreeSet<OwnedUserId> {
        let mut inner = self.fetches.inner.lock().unwrap();
        inner
            .fetches
            .get_mut(&self.id)
            .map(|fetch| std::mem::take(&mut fetch.invalidated_users))
            .unwrap_or_default()
    }
}

impl Drop for GlobalProfileFetchTicket {
    fn drop(&mut self) {
        // Unregister the fetch, whether it was finished or cancelled.
        self.fetches.inner.lock().unwrap().fetches.remove(&self.id);
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use ruma::owned_user_id;
    use tokio::sync::Mutex;

    use super::GlobalProfileFetches;

    #[test]
    fn test_only_updated_users_of_a_fetch_are_invalidated() {
        let state_store_lock = Mutex::new(());
        let state_store_guard = state_store_lock.try_lock().unwrap();
        let fetches = GlobalProfileFetches::default();
        let alice = owned_user_id!("@alice:example.org");
        let bob = owned_user_id!("@bob:example.org");
        let carol = owned_user_id!("@carol:example.org");

        // Given two fetches in flight.
        let first = fetches.begin();
        first.track([alice.clone(), bob]);
        let second = fetches.begin();
        second.track([carol.clone()]);

        // When Alice and Carol are updated.
        fetches
            .invalidate_users(&BTreeSet::from([alice.clone(), carol.clone()]), &state_store_guard);

        // Then each fetch only reports its own updated users.
        let first_invalidated_users = first.finish(&state_store_guard);
        assert_eq!(first_invalidated_users, BTreeSet::from([alice]));
        let second_invalidated_users = second.finish(&state_store_guard);
        assert_eq!(second_invalidated_users, BTreeSet::from([carol]));
    }

    #[test]
    fn test_finished_or_dropped_fetches_are_unregistered() {
        let state_store_lock = Mutex::new(());
        let state_store_guard = state_store_lock.try_lock().unwrap();
        let fetches = GlobalProfileFetches::default();

        // Given a fetch that is dropped, and one that is finished.
        drop(fetches.begin());
        fetches.begin().finish(&state_store_guard);

        // Then neither is registered anymore.
        assert!(fetches.inner.lock().unwrap().fetches.is_empty());
    }
}
