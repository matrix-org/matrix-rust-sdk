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

//! Background fetching of the global profiles of users we know about, but
//! haven't received a global profile for through sync, using [MSC4536].
//!
//! [MSC4536]: https://github.com/matrix-org/matrix-spec-proposals/pull/4536

use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    sync::{Arc, Mutex as StdMutex},
};

use matrix_sdk_base::task_monitor::BackgroundTaskHandle;
use ruma::{
    OwnedUserId,
    api::client::profile::query_profile,
    profile::{UserProfileChanges, UserProfileUpdate},
};
use tokio::sync::Notify;
use tracing::{debug, instrument, trace, warn};

use crate::{Client, Result, client::WeakClient};

/// The maximum number of users the homeserver accepts in a single request.
const MAX_USERS_PER_REQUEST: usize = 100;

/// The users the fetcher is tracking.
#[derive(Debug, Default)]
struct FetcherState {
    /// Users waiting to be fetched, in the order they were requested.
    pending_users: VecDeque<OwnedUserId>,

    /// Users in the batch currently being fetched.
    in_flight_users: BTreeSet<OwnedUserId>,

    /// Users the homeserver didn't return a profile for, e.g. remote users.
    ///
    /// TBC: This is only kept in memory so these users are fetched at most
    /// once per run. Whether they should be persisted (or retried) is still
    /// to be decided.
    unavailable_users: BTreeSet<OwnedUserId>,
}

impl FetcherState {
    /// Whether the given user is already queued, being fetched, or known to be
    /// unavailable.
    fn is_tracked(&self, user_id: &OwnedUserId) -> bool {
        self.in_flight_users.contains(user_id)
            || self.unavailable_users.contains(user_id)
            || self.pending_users.contains(user_id)
    }

    /// Queue the given users, if they aren't tracked already.
    ///
    /// Returns the number of users that were queued.
    fn enqueue(&mut self, user_ids: impl IntoIterator<Item = OwnedUserId>) -> usize {
        let mut queued = 0;

        for user_id in user_ids {
            if !self.is_tracked(&user_id) {
                self.pending_users.push_back(user_id);
                queued += 1;
            }
        }

        queued
    }

    /// Whether there are users waiting to be fetched.
    fn has_pending_users(&self) -> bool {
        !self.pending_users.is_empty()
    }

    /// Take up to `max` pending users, in the order they were queued, and mark
    /// them as being fetched.
    fn take_pending_users(&mut self, max: usize) -> Vec<OwnedUserId> {
        let size = self.pending_users.len().min(max);
        let users: Vec<_> = self.pending_users.drain(..size).collect();
        self.in_flight_users.extend(users.iter().cloned());
        users
    }

    /// Record the given users as not having a profile on the homeserver.
    fn record_unavailable_users(&mut self, user_ids: impl IntoIterator<Item = OwnedUserId>) {
        self.unavailable_users.extend(user_ids);
    }

    /// The current batch is done, successfully or not.
    fn finish_batch(&mut self) {
        self.in_flight_users.clear();
    }
}

/// Fetches the global profiles of users in batches, in the background.
///
/// Users that already have a global profile in the store, or are already
/// tracked by the fetcher, are ignored so that the same profile is only
/// fetched once.
pub(crate) struct GlobalProfileFetcher {
    /// The users being tracked.
    state: Arc<StdMutex<FetcherState>>,

    /// Notified when users are queued, to wake up the task.
    users_queued: Arc<Notify>,

    /// The task fetching the profiles in the background.
    _task: BackgroundTaskHandle,
}

impl GlobalProfileFetcher {
    pub(crate) fn new(client: &Client) -> Self {
        let state = Arc::new(StdMutex::new(FetcherState::default()));
        let users_queued = Arc::new(Notify::new());

        let task = client
            .task_monitor()
            .spawn_infinite_task(
                "client::global_profile_fetcher",
                Self::fetch_task(client.downgrade(), state.clone(), users_queued.clone()),
            )
            .abort_on_drop();

        Self { state, users_queued, _task: task }
    }

    /// Queue the given users to have their global profiles fetched, if they
    /// aren't tracked already.
    ///
    /// Users that already have a stored global profile are skipped when their
    /// batch is built.
    pub(crate) fn enqueue(&self, user_ids: impl IntoIterator<Item = OwnedUserId>) {
        let queued = self.state.lock().unwrap().enqueue(user_ids);

        if queued > 0 {
            trace!(queued, "Queued users for a global profile fetch");
            self.users_queued.notify_one();
        }
    }

    /// The background task fetching the pending users in batches, until there
    /// are none left, and then waiting for more to be queued.
    async fn fetch_task(
        client: WeakClient,
        state: Arc<StdMutex<FetcherState>>,
        users_queued: Arc<Notify>,
    ) {
        loop {
            if !state.lock().unwrap().has_pending_users() {
                trace!("Waiting for users to fetch global profiles for");
                users_queued.notified().await;
                continue;
            }

            let Some(client) = client.get() else {
                // The client is shutting down.
                return;
            };

            if let Err(error) = Self::fetch_next_batch(&client, &state).await {
                warn!(?error, "Failed to fetch a batch of global profiles");
            }

            state.lock().unwrap().finish_batch();
        }
    }

    /// Fetch and save the global profiles of the next batch of pending users.
    #[instrument(skip_all)]
    async fn fetch_next_batch(client: &Client, state: &StdMutex<FetcherState>) -> Result<()> {
        let ticket = client.base_client().begin_global_profile_fetch();
        let mut users = Vec::new();

        // Take pending users until there are enough without a stored profile
        // to fill a request.
        while users.len() < MAX_USERS_PER_REQUEST {
            let candidates =
                state.lock().unwrap().take_pending_users(MAX_USERS_PER_REQUEST - users.len());

            if candidates.is_empty() {
                break;
            }

            // Track the users before checking the store: any update saved
            // before this point is visible in the store, and any saved after
            // it invalidates the fetched profile, so none can be missed.
            ticket.track(candidates.iter().cloned());

            let stored_users: BTreeSet<OwnedUserId> = client
                .state_store()
                .get_global_profiles(&candidates)
                .await?
                .into_keys()
                .map(ToOwned::to_owned)
                .collect();
            users.extend(candidates.into_iter().filter(|user_id| !stored_users.contains(user_id)));
        }

        if users.is_empty() {
            return Ok(());
        }

        let request = query_profile::unstable::Request::new(users.clone());
        let mut response = client.send(request).await?;

        let mut profiles = BTreeMap::new();
        let mut unavailable = Vec::new();

        for user_id in users {
            let Some(profile) = response.profiles.remove(&user_id) else {
                unavailable.push(user_id);
                continue;
            };

            let mut changes = UserProfileChanges::new();
            changes.updated.extend(profile.into_iter().map(|(field, value)| (field.into(), value)));
            profiles.insert(user_id, UserProfileUpdate::Updated(changes));
        }

        debug!(
            fetched = profiles.len(),
            unavailable = unavailable.len(),
            "Fetched global profiles"
        );

        state.lock().unwrap().record_unavailable_users(unavailable);
        client.base_client().finish_global_profile_fetch(ticket, profiles).await?;

        Ok(())
    }
}

#[cfg(all(test, not(target_family = "wasm")))]
mod tests {
    use std::{collections::BTreeSet, time::Duration};

    use matrix_sdk_base::RequestedRequiredStates;
    use matrix_sdk_test::async_test;
    use ruma::{
        OwnedUserId, UserId,
        api::client::sync::sync_events::v5,
        owned_user_id,
        profile::{ProfileFieldName, ProfileFieldValue, UserProfileChanges, UserProfileUpdate},
    };
    use serde_json::json;
    use tokio::time::{sleep, timeout};
    use wiremock::ResponseTemplate;

    use super::{FetcherState, GlobalProfileFetcher};
    use crate::{Client, test_utils::mocks::MatrixMockServer};

    #[test]
    fn test_queue_takes_pending_users_in_order() {
        let alice = owned_user_id!("@alice:example.org");
        let bob = owned_user_id!("@bob:example.org");
        let carol = owned_user_id!("@carol:example.org");
        let mut state = FetcherState::default();

        // Given three queued users.
        assert_eq!(state.enqueue([alice.clone(), bob.clone(), carol.clone()]), 3);

        // Then they are taken in the order they were queued, up to the maximum.
        assert_eq!(state.take_pending_users(2), vec![alice, bob]);
        assert_eq!(state.take_pending_users(2), vec![carol]);
        assert!(!state.has_pending_users());
    }

    #[test]
    fn test_queue_skips_tracked_users() {
        let alice = owned_user_id!("@alice:example.org");
        let bob = owned_user_id!("@bob:example.org");
        let carol = owned_user_id!("@carol:example.org");
        let mut state = FetcherState::default();

        // Duplicates are only queued once.
        assert_eq!(state.enqueue([alice.clone(), bob.clone(), alice.clone()]), 2);

        // Users being fetched (Alice) or pending (Bob) aren't queued again.
        assert_eq!(state.take_pending_users(1), vec![alice.clone()]);
        assert_eq!(state.enqueue([alice.clone(), bob, carol]), 1);

        // Users without a profile on the homeserver aren't queued again either,
        // even once their batch is finished.
        state.record_unavailable_users([alice.clone()]);
        state.finish_batch();
        assert_eq!(state.enqueue([alice]), 0);
    }

    #[async_test]
    async fn test_users_are_fetched_in_batches() {
        let server = MatrixMockServer::new().await;
        let client = client_with_query_profiles(&server).await;

        // Given 250 users without a stored global profile.
        let users: Vec<OwnedUserId> =
            (0..250).map(|i| UserId::parse(format!("@user{i}:example.org")).unwrap()).collect();

        // Then they are fetched in batches of 100, in the order they were
        // queued.
        for batch in users.chunks(100) {
            server
                .mock_query_profiles()
                .expect_users(batch)
                .ok_with_profiles(vec![])
                .mock_once()
                .mount()
                .await;
        }

        // When queuing them all.
        let fetcher = client.global_profile_fetcher();
        fetcher.enqueue(users.clone());
        wait_until_idle(fetcher).await;

        // And as none were returned, they are all marked as unavailable.
        let state = fetcher.state.lock().unwrap();
        assert_eq!(state.unavailable_users, users.into_iter().collect::<BTreeSet<_>>());
    }

    #[async_test]
    async fn test_fetched_profiles_are_stored() {
        let server = MatrixMockServer::new().await;
        let client = client_with_query_profiles(&server).await;
        let alice = owned_user_id!("@alice:example.org");
        let bob = owned_user_id!("@bob:example.org");
        let mut profile_updates = client.subscribe_to_global_profile_updates();

        // Given a homeserver that only returns a profile for Alice.
        server
            .mock_query_profiles()
            .expect_users(&[alice.clone(), bob.clone()])
            .ok_with_profiles(vec![(
                alice.clone(),
                vec![ProfileFieldValue::DisplayName("Alice".to_owned())],
            )])
            .mock_once()
            .mount()
            .await;

        // When fetching the profiles of Alice and Bob.
        let fetcher = client.global_profile_fetcher();
        fetcher.enqueue([alice.clone(), bob.clone()]);
        wait_until_idle(fetcher).await;

        // Then Alice's profile is stored, and subscribers are notified.
        let profile = client.state_store().get_global_profile(&alice).await.unwrap().unwrap();
        assert_eq!(profile.get(ProfileFieldName::DisplayName.as_str()), Some(&json!("Alice")));
        assert_eq!(profile_updates.recv().await.unwrap(), BTreeSet::from([alice]));

        // And Bob is marked as unavailable, without a stored profile.
        assert!(client.state_store().get_global_profile(&bob).await.unwrap().is_none());
        assert!(fetcher.state.lock().unwrap().unavailable_users.contains(&bob));
    }

    #[async_test]
    async fn test_users_are_only_fetched_once() {
        let server = MatrixMockServer::new().await;
        let client = client_with_query_profiles(&server).await;
        let alice = owned_user_id!("@alice:example.org");
        let bob = owned_user_id!("@bob:example.org");

        // Given a homeserver that only returns a profile for Alice, and expects
        // a single request.
        server
            .mock_query_profiles()
            .expect_users(&[alice.clone(), bob.clone()])
            .ok_with_profiles(vec![(
                alice.clone(),
                vec![ProfileFieldValue::DisplayName("Alice".to_owned())],
            )])
            .mock_once()
            .mount()
            .await;
        server.mock_query_profiles().ok_with_profiles(vec![]).never().mount().await;

        // When queuing the same users several times, e.g. from different rooms.
        let fetcher = client.global_profile_fetcher();
        fetcher.enqueue([alice.clone(), bob.clone()]);
        fetcher.enqueue([bob.clone(), alice.clone()]);
        wait_until_idle(fetcher).await;

        // And again once Alice is stored and Bob is known to be unavailable.
        fetcher.enqueue([alice, bob]);
        wait_until_idle(fetcher).await;

        // Then no other request is sent (verified by the mocks on drop).
    }

    #[async_test]
    async fn test_batches_are_filled_with_missing_users() {
        let server = MatrixMockServer::new().await;
        let client = client_with_query_profiles(&server).await;

        // Given 250 users, of which every other one up to the 200th already
        // has a stored global profile.
        let users: Vec<OwnedUserId> =
            (0..250).map(|i| UserId::parse(format!("@user{i}:example.org")).unwrap()).collect();
        let (stored, missing): (Vec<_>, Vec<_>) =
            users.iter().cloned().enumerate().partition(|(i, _)| *i < 200 && i % 2 == 0);
        let missing: Vec<_> = missing.into_iter().map(|(_, user_id)| user_id).collect();

        let mut response = v5::Response::new("0".to_owned());
        for (_, user_id) in stored {
            let mut changes = UserProfileChanges::new();
            changes.updated.insert(ProfileFieldName::DisplayName, json!("Stored"));
            response.extensions.profiles.users.insert(user_id, UserProfileUpdate::Updated(changes));
        }
        client
            .process_sliding_sync_test_helper(&response, &RequestedRequiredStates::default())
            .await
            .unwrap();

        // Then the 150 missing users are fetched in full batches of 100 and 50.
        for batch in missing.chunks(100) {
            server
                .mock_query_profiles()
                .expect_users(batch)
                .ok_with_profiles(vec![])
                .mock_once()
                .mount()
                .await;
        }

        // When queuing them all.
        let fetcher = client.global_profile_fetcher();
        fetcher.enqueue(users);
        wait_until_idle(fetcher).await;
    }

    #[async_test]
    async fn test_stored_users_are_skipped() {
        let server = MatrixMockServer::new().await;
        let client = client_with_query_profiles(&server).await;
        let alice = owned_user_id!("@alice:example.org");
        let bob = owned_user_id!("@bob:example.org");

        // Given that Alice already has a stored global profile.
        let mut changes = UserProfileChanges::new();
        changes.updated.insert(ProfileFieldName::DisplayName, json!("Alice"));
        sync_profile_update(&client, &alice, UserProfileUpdate::Updated(changes)).await;

        // Then only Bob is fetched.
        server
            .mock_query_profiles()
            .expect_users(std::slice::from_ref(&bob))
            .ok_with_profiles(vec![])
            .mock_once()
            .mount()
            .await;

        // When queuing Alice and Bob.
        let fetcher = client.global_profile_fetcher();
        fetcher.enqueue([alice, bob]);
        wait_until_idle(fetcher).await;
    }

    #[async_test]
    async fn test_fetched_profile_does_not_overwrite_a_sync_update() {
        let server = MatrixMockServer::new().await;
        let client = client_with_query_profiles(&server).await;
        let alice = owned_user_id!("@alice:example.org");

        // Given a slow request for Alice's profile, that is in flight.
        mock_slow_query(&server, &alice, "Old Alice").await;
        let fetcher = client.global_profile_fetcher();
        fetcher.enqueue([alice.clone()]);
        sleep(Duration::from_millis(100)).await;
        assert!(fetcher.state.lock().unwrap().in_flight_users.contains(&alice));

        // When a sync updates Alice's profile before the response arrives.
        let mut changes = UserProfileChanges::new();
        changes.updated.insert(ProfileFieldName::DisplayName, json!("New Alice"));
        sync_profile_update(&client, &alice, UserProfileUpdate::Updated(changes)).await;
        wait_until_idle(fetcher).await;

        // Then the more recent profile from the sync is kept.
        let profile = client.state_store().get_global_profile(&alice).await.unwrap().unwrap();
        assert_eq!(profile.get(ProfileFieldName::DisplayName.as_str()), Some(&json!("New Alice")));
    }

    #[async_test]
    async fn test_fetched_profile_does_not_restore_a_dropped_profile() {
        let server = MatrixMockServer::new().await;
        let client = client_with_query_profiles(&server).await;
        let alice = owned_user_id!("@alice:example.org");

        // Given a slow request for Alice's profile, that is in flight.
        mock_slow_query(&server, &alice, "Alice").await;
        let fetcher = client.global_profile_fetcher();
        fetcher.enqueue([alice.clone()]);
        sleep(Duration::from_millis(100)).await;
        assert!(fetcher.state.lock().unwrap().in_flight_users.contains(&alice));

        // When a sync drops Alice's profile before the response arrives, e.g.
        // because we no longer share a room.
        sync_profile_update(&client, &alice, UserProfileUpdate::Dropped).await;
        wait_until_idle(fetcher).await;

        // Then Alice's profile isn't stored again.
        assert!(client.state_store().get_global_profile(&alice).await.unwrap().is_none());
    }

    /* Helpers */

    async fn client_with_query_profiles(server: &MatrixMockServer) -> Client {
        server
            .mock_versions()
            .with_versions(vec!["v1.16"])
            .with_profiles_sliding_sync_extension()
            .with_query_profiles()
            .ok()
            .named("versions")
            .mount()
            .await;
        server.client_builder().no_server_versions().build().await
    }

    /// Wait until the fetcher has processed all the queued users.
    async fn wait_until_idle(fetcher: &GlobalProfileFetcher) {
        timeout(Duration::from_secs(5), async {
            loop {
                {
                    let state = fetcher.state.lock().unwrap();
                    if state.pending_users.is_empty() && state.in_flight_users.is_empty() {
                        return;
                    }
                }
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the fetcher should become idle");
    }

    /// Mock a query for Alice's profile that responds with the given display
    /// name, after a delay, so that a sync can happen while it is in flight.
    async fn mock_slow_query(server: &MatrixMockServer, alice: &OwnedUserId, display_name: &str) {
        let profiles = json!({ alice.as_str(): { "displayname": display_name } });
        server
            .mock_query_profiles()
            .expect_users(std::slice::from_ref(alice))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(profiles)
                    .set_delay(Duration::from_millis(300)),
            )
            .mock_once()
            .mount()
            .await;
    }

    /// Process a sliding sync response carrying the given profile update.
    async fn sync_profile_update(
        client: &Client,
        user_id: &OwnedUserId,
        update: UserProfileUpdate,
    ) {
        let mut response = v5::Response::new("0".to_owned());
        response.extensions.profiles.users.insert(user_id.clone(), update);
        client
            .process_sliding_sync_test_helper(&response, &RequestedRequiredStates::default())
            .await
            .unwrap();
    }
}
