// Copyright 2024 The Matrix.org Foundation C.I.C.
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

//! This module provides a generic interface to subscribe to unable-to-decrypt
//! events, and notable updates to such events.
//!
//! This provides a general trait that a consumer may implement, as well as
//! utilities to simplify usage of this trait.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use growable_bloom_filter::{GrowableBloom, GrowableBloomBuilder};
use matrix_sdk::{
    Client,
    executor::{JoinHandle, spawn},
    sleep::sleep,
};
use matrix_sdk_base::{
    SendOutsideWasm, StateStoreDataKey, StateStoreDataValue, StoreError, SyncOutsideWasm,
    crypto::types::events::UtdCause,
};
use ruma::{
    EventId, MilliSecondsSinceUnixEpoch, OwnedEventId, OwnedServerName, UserId,
    time::{Duration, Instant},
};
use tokio::sync::{Mutex as AsyncMutex, MutexGuard};
use tracing::{error, trace};

/// A generic interface which methods get called whenever we observe a
/// unable-to-decrypt (UTD) event.
pub trait UnableToDecryptHook: std::fmt::Debug + SendOutsideWasm + SyncOutsideWasm {
    /// Called every time the hook observes an encrypted event that couldn't be
    /// decrypted.
    ///
    /// If the hook manager was configured with a max delay, this could also
    /// contain extra information for late-decrypted events. See details in
    /// [`UnableToDecryptInfo::time_to_decrypt`].
    fn on_utd(&self, info: UnableToDecryptInfo);
}

/// Information about an event we were unable to decrypt (UTD).
#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct UnableToDecryptInfo {
    /// The identifier of the event that couldn't get decrypted.
    pub event_id: OwnedEventId,

    /// If the event could be decrypted late (that is, the event was encrypted
    /// at first, but could be decrypted later on), then this indicates the
    /// time it took to decrypt the event. If it is not set, this is
    /// considered a definite UTD.
    pub time_to_decrypt: Option<Duration>,

    /// What we know about what caused this UTD. E.g. was this event sent when
    /// we were not a member of this room?
    pub cause: UtdCause,

    /// The difference between the event creation time (`origin_server_ts`) and
    /// the time our device was created. If negative, this event was sent
    /// *before* our device was created.
    pub event_local_age_millis: i64,

    /// Whether the user had verified their own identity at the point they
    /// received the UTD event.
    pub user_trusts_own_identity: bool,

    /// The homeserver of the user that sent the undecryptable event.
    pub sender_homeserver: OwnedServerName,

    /// Our local user's own homeserver, or `None` if the client is not logged
    /// in.
    pub own_homeserver: Option<OwnedServerName>,
}

/// Data about a UTD event which we are waiting to report to the parent hook.
#[derive(Debug)]
struct PendingUtdReport {
    /// The time that we received the UTD report from the timeline code.
    marked_utd_at: Instant,

    /// The task that will report this UTD to the parent hook.
    report_task: JoinHandle<()>,

    /// The UnableToDecryptInfo structure for this UTD event.
    utd_info: UnableToDecryptInfo,
}

/// A manager over an existing [`UnableToDecryptHook`] that deduplicates UTDs
/// on similar events, and adds basic consistency checks.
///
/// It can also implement a grace period before reporting an event as a UTD, if
/// configured with [`Self::with_max_delay`]. Instead of immediately reporting
/// the UTD, the reporting will be delayed by the max delay at most; if the
/// event could eventually get decrypted, it may be reported before the end of
/// that delay.
#[derive(Debug)]
pub struct UtdHookManager {
    /// A Client associated with the UTD hook. This is used to access the store
    /// which we persist our data to.
    client: Client,

    /// The parent hook we'll call, when we have found a unique UTD.
    parent: Arc<dyn UnableToDecryptHook>,

    /// An optional delay before marking the event as UTD ("grace period").
    max_delay: Option<Duration>,

    /// A mapping of events we're going to report as UTDs, to the tasks to do
    /// so.
    ///
    /// Note: this is empty if no [`Self::max_delay`] is set.
    ///
    /// Note: this is theoretically unbounded in size, although this set of
    /// tasks will degrow over time, as tasks expire after the max delay.
    pending_delayed: Arc<Mutex<HashMap<OwnedEventId, PendingUtdReport>>>,

    /// Bloom filter containing the event IDs of events which have been reported
    /// as UTDs
    reported_utds: Arc<AsyncMutex<GrowableBloom>>,
}

impl UtdHookManager {
    /// Create a new [`UtdHookManager`] for the given hook.
    ///
    /// A [`Client`] must also be provided; this provides a link to the
    /// [`matrix_sdk_base::StateStore`] which is used to load and store the
    /// persistent data.
    pub fn new(parent: Arc<dyn UnableToDecryptHook>, client: Client) -> Self {
        let bloom_filter =
            // Some slightly arbitrarily-chosen parameters here. We specify that, after 1000
            // UTDs, we want to have a false-positive rate of 1%.
            //
            // The GrowableBloomFilter is based on a series of (partitioned) Bloom filters;
            // once the first starts getting full (the expected false-positive
            // rate gets too high), it adds another Bloom filter. Each new entry
            // is recorded in the most recent Bloom filter; when querying, if
            // *any* of the component filters show a match, that shows
            // an overall match.
            //
            // The first component filter is created based on the parameters we give. For
            // reasons derived in the paper [1], a partitioned Bloom filter with
            // target false-positive rate `P` after `n` insertions requires a
            // number of slices `k` given by:
            //
            // k = log2(1/P) = -ln(P) / ln(2)
            //
            // ... where each slice has a number of bits `m` given by
            //
            // m = n / ln(2)
            //
            // We have to have a whole number of slices and bits, so the total number of
            // bits M is:
            //
            // M = ceil(k) * ceil(m)
            //   = ceil(-ln(P) / ln(2)) * ceil(n / ln(2))
            //
            // In other words, our FP rate of 1% after 1000 insertions requires:
            //
            // M = ceil(-ln(0.01) / ln(2)) * ceil(1000 / ln(2))
            //   = 7 * 1443 = 10101 bits
            //
            // So our filter starts off with 1263 bytes of data (plus a little overhead).
            // Once we hit 1000 UTDs, we add a second component filter with a capacity
            // double that of the original and target error rate 85% of the
            // original (another 2526 bytes), which then lasts us until a total
            // of 3000 UTDs.
            //
            // [1]: https://gsd.di.uminho.pt/members/cbm/ps/dbloom.pdf
            GrowableBloomBuilder::new().estimated_insertions(1000).desired_error_ratio(0.01).build();

        Self {
            client,
            parent,
            max_delay: None,
            pending_delayed: Default::default(),
            reported_utds: Arc::new(AsyncMutex::new(bloom_filter)),
        }
    }

    /// Reports UTDs with the given max delay.
    ///
    /// Note: late decryptions are always reported, even if there was a grace
    /// period set for the reporting of the UTD.
    pub fn with_max_delay(mut self, delay: Duration) -> Self {
        self.max_delay = Some(delay);
        self
    }

    /// Load the persistent data for the UTD hook from the store.
    ///
    /// If the client previously used a UtdHookManager, and UTDs were
    /// encountered, the data on the reported UTDs is loaded from the store.
    /// Otherwise, there is no effect.
    pub async fn reload_from_store(&mut self) -> Result<(), StoreError> {
        let existing_data =
            self.client.state_store().get_kv_data(StateStoreDataKey::UtdHookManagerData).await?;

        if let Some(existing_data) = existing_data {
            let bloom_filter = existing_data
                .into_utd_hook_manager_data()
                .expect("StateStore::get_kv_data should return data of the right type");
            self.reported_utds = Arc::new(AsyncMutex::new(bloom_filter));
        }
        Ok(())
    }

    /// The function to call whenever a UTD is seen for the first time.
    ///
    /// Pipe in any information that needs to be included in the final report.
    ///
    /// # Arguments
    ///  * `event_id` - The ID of the event that could not be decrypted.
    ///  * `cause` - Our best guess at the reason why the event can't be
    ///    decrypted.
    ///  * `event_timestamp` - The event's `origin_server_ts` field (or creation
    ///    time for local echo).
    ///  * `sender_user_id` - The Matrix user ID of the user that sent the
    ///    undecryptable message.
    pub(crate) async fn on_utd(
        &self,
        event_id: &EventId,
        cause: UtdCause,
        event_timestamp: MilliSecondsSinceUnixEpoch,
        sender_user_id: &UserId,
    ) {
        trace!(%event_id, "UtdHookManager: Observed UTD");
        // Hold the lock on `reported_utds` throughout, to avoid races with other
        // threads.
        let mut reported_utds_lock = self.reported_utds.lock().await;

        // Check if this, or a previous instance of UtdHookManager, has already reported
        // this UTD, and bail out if not.
        if reported_utds_lock.contains(event_id) {
            return;
        }

        // Otherwise, check if we already have a task to handle this UTD.
        if self.pending_delayed.lock().unwrap().contains_key(event_id) {
            return;
        }

        let event_local_age_millis = i64::from(event_timestamp.get()).saturating_sub_unsigned(
            self.client.encryption().device_creation_timestamp().await.get().into(),
        );

        let own_user_id = self.client.user_id();
        let user_trusts_own_identity = if let Some(own_user_id) = own_user_id {
            if let Ok(Some(own_id)) = self.client.encryption().get_user_identity(own_user_id).await
            {
                own_id.is_verified()
            } else {
                false
            }
        } else {
            false
        };

        let own_homeserver = own_user_id.map(|id| id.server_name().to_owned());
        let sender_homeserver = sender_user_id.server_name().to_owned();

        let info = UnableToDecryptInfo {
            event_id: event_id.to_owned(),
            time_to_decrypt: None,
            cause,
            event_local_age_millis,
            user_trusts_own_identity,
            own_homeserver,
            sender_homeserver,
        };

        let Some(max_delay) = self.max_delay else {
            // No delay: immediately report the event to the parent hook.
            Self::report_utd(info, &self.parent, &self.client, &mut reported_utds_lock).await;
            return;
        };

        // Clone data shared with the task below.
        let pending_delayed = self.pending_delayed.clone();
        let reported_utds = self.reported_utds.clone();
        let parent = self.parent.clone();
        let client = self.client.clone();
        let owned_event_id = event_id.to_owned();

        // Spawn a task that will wait for the given delay, and maybe call the parent
        // hook then.
        let handle = spawn(async move {
            // Wait for the given delay.
            sleep(max_delay).await;

            // Make sure we take out the lock on `reported_utds` before removing the entry
            // from `pending_delayed`, to ensure we don't race against another call to
            // `on_utd` (which could otherwise see that the entry has been
            // removed from `pending_delayed` but not yet added to
            // `reported_utds`).
            let mut reported_utds_lock = reported_utds.lock().await;

            // Remove the task from the outstanding set. But if it's already been removed,
            // it's been decrypted since the task was added!
            let pending_report = pending_delayed.lock().unwrap().remove(&owned_event_id);
            if let Some(pending_report) = pending_report {
                Self::report_utd(
                    pending_report.utd_info,
                    &parent,
                    &client,
                    &mut reported_utds_lock,
                )
                .await;
            }
        });

        // Add the task to the set of pending tasks.
        self.pending_delayed.lock().unwrap().insert(
            event_id.to_owned(),
            PendingUtdReport { marked_utd_at: Instant::now(), report_task: handle, utd_info: info },
        );
    }

    /// The function to call whenever an event that was marked as a UTD has
    /// eventually been decrypted.
    ///
    /// Note: if this is called for an event that was never marked as a UTD
    /// before, it has no effect.
    pub(crate) async fn on_late_decrypt(&self, event_id: &EventId) {
        trace!(%event_id, "UtdHookManager: On late decrypt");
        // Hold the lock on `reported_utds` throughout, to avoid races with other
        // threads.
        let mut reported_utds_lock = self.reported_utds.lock().await;

        // Only let the parent hook know about the late decryption if the event is
        // a pending UTD. If so, remove the event from the pending list —
        // doing so will cause the reporting task to no-op if it runs.
        let Some(pending_utd_report) = self.pending_delayed.lock().unwrap().remove(event_id) else {
            trace!(%event_id, "UtdHookManager: received a late decrypt report for an unknown utd");
            return;
        };

        // We can also cancel the reporting task.
        pending_utd_report.report_task.abort();

        // Update the UTD Info struct with new data, then report it
        let mut info = pending_utd_report.utd_info;
        info.time_to_decrypt = Some(pending_utd_report.marked_utd_at.elapsed());
        Self::report_utd(info, &self.parent, &self.client, &mut reported_utds_lock).await;
    }

    /// Helper for [`UtdHookManager::on_utd`] and
    /// [`UtdHookManager.on_late_decrypt`]: reports the UTD to the parent,
    /// and records the event as reported.
    ///
    /// Must be called with the lock held on [`UtdHookManager::reported_utds`],
    /// and takes a `MutexGuard` to enforce that.
    async fn report_utd(
        info: UnableToDecryptInfo,
        parent_hook: &Arc<dyn UnableToDecryptHook>,
        client: &Client,
        reported_utds_lock: &mut MutexGuard<'_, GrowableBloom>,
    ) {
        let event_id = info.event_id.clone();
        parent_hook.on_utd(info);
        reported_utds_lock.insert(event_id);
        if let Err(e) = client
            .state_store()
            .set_kv_data(
                StateStoreDataKey::UtdHookManagerData,
                StateStoreDataValue::UtdHookManagerData(reported_utds_lock.clone()),
            )
            .await
        {
            error!("Unable to persist UTD report data: {}", e);
        }
    }
}

impl Drop for UtdHookManager {
    fn drop(&mut self) {
        // Cancel all the outstanding delayed tasks to report UTDs.
        //
        // Here, we don't take the lock on `reported_utd`s (indeed, we can't, since
        // `reported_utds` has an async mutex, and `drop` has to be sync), but
        // that's ok. We can't race against `on_utd` or `on_late_decrypt`, since
        // they both have `&self` references which mean `drop` can't be called.
        // We *could* race against one of the actual tasks to report
        // UTDs, but that's ok too: either the report task will bail out when it sees
        // the entry has been removed from `pending_delayed` (which is fine), or the
        // report task will successfully report the UTD (which is fine).
        let mut pending_delayed = self.pending_delayed.lock().unwrap();
        for (_, pending_utd_report) in pending_delayed.drain() {
            pending_utd_report.report_task.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use matrix_sdk::test_utils::{logged_in_client, no_retry_test_client};
    use matrix_sdk_test::async_test;
    use ruma::{event_id, server_name, user_id};

    use super::*;

    #[derive(Debug, Default)]
    struct Dummy {
        utds: Mutex<Vec<UnableToDecryptInfo>>,
    }

    impl UnableToDecryptHook for Dummy {
        fn on_utd(&self, info: UnableToDecryptInfo) {
            self.utds.lock().unwrap().push(info);
        }
    }

    #[async_test]
    async fn test_deduplicates_utds() {
        // If I create a dummy hook,
        let hook = Arc::new(Dummy::default());

        // And I wrap with the UtdHookManager,
        let wrapper = UtdHookManager::new(hook.clone(), logged_in_client(None).await);

        // And I call the `on_utd` method multiple times, sometimes on the same event,
        let event_timestamp = MilliSecondsSinceUnixEpoch::now();
        let sender_user = user_id!("@example2:localhost");
        let federated_user = user_id!("@example2:example.com");
        wrapper.on_utd(event_id!("$1"), UtdCause::Unknown, event_timestamp, sender_user).await;
        wrapper.on_utd(event_id!("$1"), UtdCause::Unknown, event_timestamp, sender_user).await;
        wrapper.on_utd(event_id!("$2"), UtdCause::Unknown, event_timestamp, federated_user).await;
        wrapper.on_utd(event_id!("$1"), UtdCause::Unknown, event_timestamp, sender_user).await;
        wrapper.on_utd(event_id!("$2"), UtdCause::Unknown, event_timestamp, federated_user).await;
        wrapper.on_utd(event_id!("$3"), UtdCause::Unknown, event_timestamp, sender_user).await;

        // Then the event ids have been deduplicated,
        {
            let utds = hook.utds.lock().unwrap();
            assert_eq!(utds.len(), 3);
            assert_eq!(utds[0].event_id, event_id!("$1"));
            assert_eq!(utds[1].event_id, event_id!("$2"));
            assert_eq!(utds[2].event_id, event_id!("$3"));

            // No event is a late-decryption event.
            assert!(utds[0].time_to_decrypt.is_none());
            assert!(utds[1].time_to_decrypt.is_none());
            assert!(utds[2].time_to_decrypt.is_none());

            // event_local_age_millis should be a small positive number, because the
            // timestamp we used was after we created the device
            let utd_local_age = utds[0].event_local_age_millis;
            assert!(utd_local_age >= 0);
            assert!(utd_local_age <= 1000);

            assert_eq!(utds[0].sender_homeserver, server_name!("localhost"));
            assert_eq!(utds[0].own_homeserver, Some(server_name!("localhost").to_owned()));

            assert_eq!(utds[1].sender_homeserver, server_name!("example.com"));
            assert_eq!(utds[1].own_homeserver, Some(server_name!("localhost").to_owned()));
        }
    }

    #[async_test]
    async fn test_deduplicates_utds_from_previous_session() {
        // Use a single client for both hooks, so that both hooks are backed by the same
        // memorystore.
        let client = no_retry_test_client(None).await;

        // Dummy hook 1, with the first UtdHookManager
        {
            let hook = Arc::new(Dummy::default());
            let wrapper = UtdHookManager::new(hook.clone(), client.clone());

            // I call it a couple of times with different events
            wrapper
                .on_utd(
                    event_id!("$1"),
                    UtdCause::Unknown,
                    MilliSecondsSinceUnixEpoch::now(),
                    user_id!("@a:b"),
                )
                .await;
            wrapper
                .on_utd(
                    event_id!("$2"),
                    UtdCause::Unknown,
                    MilliSecondsSinceUnixEpoch::now(),
                    user_id!("@a:b"),
                )
                .await;

            // Sanity-check the reported event IDs
            {
                let utds = hook.utds.lock().unwrap();
                assert_eq!(utds.len(), 2);
                assert_eq!(utds[0].event_id, event_id!("$1"));
                assert!(utds[0].time_to_decrypt.is_none());
                assert_eq!(utds[1].event_id, event_id!("$2"));
                assert!(utds[1].time_to_decrypt.is_none());
            }
        }

        // Now, create a *new* hook, with a *new* UtdHookManager
        {
            let hook = Arc::new(Dummy::default());
            let mut wrapper = UtdHookManager::new(hook.clone(), client.clone());
            wrapper.reload_from_store().await.unwrap();

            // Call it with more events, some of which match the previous instance
            wrapper
                .on_utd(
                    event_id!("$1"),
                    UtdCause::Unknown,
                    MilliSecondsSinceUnixEpoch::now(),
                    user_id!("@a:b"),
                )
                .await;
            wrapper
                .on_utd(
                    event_id!("$3"),
                    UtdCause::Unknown,
                    MilliSecondsSinceUnixEpoch::now(),
                    user_id!("@a:b"),
                )
                .await;

            // Only the *new* ones should be reported
            let utds = hook.utds.lock().unwrap();
            assert_eq!(utds.len(), 1);
            assert_eq!(utds[0].event_id, event_id!("$3"));
        }
    }

    /// Test that UTD events which had not yet been reported in a previous
    /// session, are reported in the next session.
    #[async_test]
    async fn test_does_not_deduplicate_late_utds_from_previous_session() {
        // Use a single client for both hooks, so that both hooks are backed by the same
        // memorystore.
        let client = no_retry_test_client(None).await;

        // Dummy hook 1, with the first UtdHookManager
        {
            let hook = Arc::new(Dummy::default());
            let wrapper = UtdHookManager::new(hook.clone(), client.clone())
                .with_max_delay(Duration::from_secs(2));

            // a UTD event
            wrapper
                .on_utd(
                    event_id!("$1"),
                    UtdCause::Unknown,
                    MilliSecondsSinceUnixEpoch::now(),
                    user_id!("@a:b"),
                )
                .await;

            // The event ID should not yet have been reported.
            {
                let utds = hook.utds.lock().unwrap();
                assert_eq!(utds.len(), 0);
            }
        }

        // Now, create a *new* hook, with a *new* UtdHookManager
        {
            let hook = Arc::new(Dummy::default());
            let mut wrapper = UtdHookManager::new(hook.clone(), client.clone());
            wrapper.reload_from_store().await.unwrap();

            // Call the new hook with the same event
            wrapper
                .on_utd(
                    event_id!("$1"),
                    UtdCause::Unknown,
                    MilliSecondsSinceUnixEpoch::now(),
                    user_id!("@a:b"),
                )
                .await;

            // And it should be reported.
            sleep(Duration::from_millis(2500)).await;

            let utds = hook.utds.lock().unwrap();
            assert_eq!(utds.len(), 1);
            assert_eq!(utds[0].event_id, event_id!("$1"));
        }
    }

    #[async_test]
    async fn test_on_late_decrypted_no_effect() {
        // If I create a dummy hook,
        let hook = Arc::new(Dummy::default());

        // And I wrap with the UtdHookManager,
        let wrapper = UtdHookManager::new(hook.clone(), no_retry_test_client(None).await);

        // And I call the `on_late_decrypt` method before the event had been marked as
        // utd,
        wrapper.on_late_decrypt(event_id!("$1")).await;

        // Then nothing is registered in the parent hook.
        assert!(hook.utds.lock().unwrap().is_empty());
    }

    #[async_test]
    async fn test_on_late_decrypted_after_utd_no_grace_period() {
        // If I create a dummy hook,
        let hook = Arc::new(Dummy::default());

        // And I wrap with the UtdHookManager,
        let wrapper = UtdHookManager::new(hook.clone(), no_retry_test_client(None).await);

        // And I call the `on_utd` method for an event,
        wrapper
            .on_utd(
                event_id!("$1"),
                UtdCause::Unknown,
                MilliSecondsSinceUnixEpoch::now(),
                user_id!("@a:b"),
            )
            .await;

        // Then the UTD has been notified, but not as late-decrypted event.
        {
            let utds = hook.utds.lock().unwrap();
            assert_eq!(utds.len(), 1);
            assert_eq!(utds[0].event_id, event_id!("$1"));
            assert!(utds[0].time_to_decrypt.is_none());
        }

        // And when I call the `on_late_decrypt` method,
        wrapper.on_late_decrypt(event_id!("$1")).await;

        // Then the event is not reported again as a late-decryption.
        {
            let utds = hook.utds.lock().unwrap();
            assert_eq!(utds.len(), 1);

            // The previous report is still there. (There was no grace period.)
            assert_eq!(utds[0].event_id, event_id!("$1"));
            assert!(utds[0].time_to_decrypt.is_none());
        }
    }

    #[cfg(not(target_family = "wasm"))] // wasm32 has no time for that
    #[async_test]
    async fn test_delayed_utd() {
        // If I create a dummy hook,
        let hook = Arc::new(Dummy::default());

        // And I wrap with the UtdHookManager, configured to delay reporting after 2
        // seconds.
        let wrapper = UtdHookManager::new(hook.clone(), no_retry_test_client(None).await)
            .with_max_delay(Duration::from_secs(2));

        // And I call the `on_utd` method for an event,
        wrapper
            .on_utd(
                event_id!("$1"),
                UtdCause::Unknown,
                MilliSecondsSinceUnixEpoch::now(),
                user_id!("@a:b"),
            )
            .await;

        // Then the UTD is not being reported immediately.
        assert!(hook.utds.lock().unwrap().is_empty());
        assert_eq!(wrapper.pending_delayed.lock().unwrap().len(), 1);

        // If I wait for 1 second, then it's still not been notified yet.
        sleep(Duration::from_secs(1)).await;

        assert!(hook.utds.lock().unwrap().is_empty());
        assert_eq!(wrapper.pending_delayed.lock().unwrap().len(), 1);

        // But if I wait just a bit more, then it's getting notified as a definite UTD.
        sleep(Duration::from_millis(1500)).await;

        {
            let utds = hook.utds.lock().unwrap();
            assert_eq!(utds.len(), 1);
            assert_eq!(utds[0].event_id, event_id!("$1"));
            assert!(utds[0].time_to_decrypt.is_none());
        }

        assert!(wrapper.pending_delayed.lock().unwrap().is_empty());
    }

    #[cfg(not(target_family = "wasm"))] // wasm32 has no time for that
    #[async_test]
    async fn test_delayed_late_decryption() {
        // If I create a dummy hook,
        let hook = Arc::new(Dummy::default());

        // And I wrap with the UtdHookManager, configured to delay reporting after 2
        // seconds.
        let wrapper = UtdHookManager::new(hook.clone(), no_retry_test_client(None).await)
            .with_max_delay(Duration::from_secs(2));

        // And I call the `on_utd` method for an event,
        wrapper
            .on_utd(
                event_id!("$1"),
                UtdCause::Unknown,
                MilliSecondsSinceUnixEpoch::now(),
                user_id!("@a:b"),
            )
            .await;

        // Then the UTD has not been notified quite yet.
        assert!(hook.utds.lock().unwrap().is_empty());
        assert_eq!(wrapper.pending_delayed.lock().unwrap().len(), 1);

        // If I wait for 1 second, and mark the event as late-decrypted,
        sleep(Duration::from_secs(1)).await;

        wrapper.on_late_decrypt(event_id!("$1")).await;

        // Then it's being immediately reported as a late-decryption UTD.
        {
            let utds = hook.utds.lock().unwrap();
            assert_eq!(utds.len(), 1);
            assert_eq!(utds[0].event_id, event_id!("$1"));
            assert!(utds[0].time_to_decrypt.is_some());
        }

        // And there aren't any pending delayed reports anymore.
        assert!(wrapper.pending_delayed.lock().unwrap().is_empty());
    }
}
