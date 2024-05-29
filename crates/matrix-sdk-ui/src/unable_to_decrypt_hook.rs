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
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use growable_bloom_filter::{GrowableBloom, GrowableBloomBuilder};
use matrix_sdk::{crypto::types::events::UtdCause, Client};
use matrix_sdk_base::{StateStoreDataKey, StateStoreDataValue};
use ruma::{EventId, OwnedEventId};
use tokio::{spawn, task::JoinHandle, time::sleep};
use tracing::{debug, error, trace, warn};

/// A generic interface which methods get called whenever we observe a
/// unable-to-decrypt (UTD) event.
pub trait UnableToDecryptHook: std::fmt::Debug + Send + Sync {
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
}

/// Data about a UTD event which we are waiting to report to the parent hook.
#[derive(Debug)]
struct PendingUtdReport {
    /// The time that we received the UTD report from the timeline code.
    marked_utd_at: Instant,

    /// The task that will report this UTD to the parent hook.
    report_task: JoinHandle<()>,
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
    /// The parent hook we'll call, when we have found a unique UTD.
    parent: Arc<dyn UnableToDecryptHook>,

    /// The set of events we've marked as UTDs.
    ///
    /// Events are added to this set when they are first flagged as UTDs. If
    /// they are subsequently successfully decrypted, they are removed from
    /// this set. (In other words, this is a superset of the events in
    /// [`Self::pending_delayed`].
    ///
    /// Note: this is unbounded, because we have absolutely no idea how long it
    /// will take for a UTD to resolve, or if it will even resolve at any
    /// point.
    known_utds: Arc<Mutex<HashSet<OwnedEventId>>>,

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

    /// An object which keeps track of the set of UTDs that have already been
    /// reported to the parent hook.
    reported_utds: ReportedUtdsManager,
}

impl UtdHookManager {
    /// Create a new [`UtdHookManager`] for the given hook.
    ///
    /// A [`Client`] must also be provided; this provides a link to the
    /// [`matrix_sdk_base::StateStore`] which is used to load and store the
    /// persistent data.
    pub fn new(parent: Arc<dyn UnableToDecryptHook>, client: Client) -> Self {
        Self {
            parent,
            known_utds: Default::default(),
            max_delay: None,
            pending_delayed: Default::default(),
            reported_utds: ReportedUtdsManager::new(client),
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

    /// The function to call whenever a UTD is seen for the first time.
    ///
    /// Pipe in any information that needs to be included in the final report.
    pub(crate) fn on_utd(&self, event_id: &EventId, cause: UtdCause) {
        // First of all, check if we already have a task to handle this UTD. If so, our
        // work is done
        let mut pending_delayed_lock = self.pending_delayed.lock().unwrap();
        if pending_delayed_lock.contains_key(event_id) {
            return;
        }

        // Keep track of UTDs we have already seen.
        {
            let mut known_utds = self.known_utds.lock().unwrap();
            if !known_utds.insert(event_id.to_owned()) {
                return;
            }
        }

        // A previous instance of UtdHookManager may already have reported this UTD, so
        // check that too.
        if self.reported_utds.contains(event_id) {
            return;
        }

        // Construct a closure which will report the UTD to the parent.
        let report_utd = {
            let info =
                UnableToDecryptInfo { event_id: event_id.to_owned(), time_to_decrypt: None, cause };
            let parent = self.parent.clone();
            let reported_utds = self.reported_utds.clone();
            let event_id = event_id.to_owned();
            move || {
                parent.on_utd(info);
                reported_utds.insert(event_id);
            }
        };

        let Some(max_delay) = self.max_delay else {
            // No delay: immediately report the event to the parent hook.
            report_utd();
            return;
        };

        // Clone data shared with the task below.
        let pending_delayed = self.pending_delayed.clone();
        let target_event_id = event_id.to_owned();

        // Spawn a task that will wait for the given delay, and maybe call the parent
        // hook then.
        let handle = spawn(async move {
            // Wait for the given delay.
            sleep(max_delay).await;

            // Remove the task from the outstanding set. But if it's already been removed,
            // it's been decrypted since the task was added!
            if pending_delayed.lock().unwrap().remove(&target_event_id).is_some() {
                report_utd();
            }
        });

        // Add the task to the set of pending tasks.
        pending_delayed_lock.insert(
            event_id.to_owned(),
            PendingUtdReport { marked_utd_at: Instant::now(), report_task: handle },
        );
    }

    /// The function to call whenever an event that was marked as a UTD has
    /// eventually been decrypted.
    ///
    /// Note: if this is called for an event that was never marked as a UTD
    /// before, it has no effect.
    pub(crate) fn on_late_decrypt(&self, event_id: &EventId, cause: UtdCause) {
        let mut pending_delayed_lock = self.pending_delayed.lock().unwrap();
        self.known_utds.lock().unwrap().remove(event_id);

        // Only let the parent hook know about the late decryption if the event is
        // a pending UTD. If so, remove the event from the pending list —
        // doing so will cause the reporting task to no-op if it runs.
        let Some(pending_utd_report) = pending_delayed_lock.remove(event_id) else {
            return;
        };

        // We can also cancel the reporting task.
        pending_utd_report.report_task.abort();

        // Now we can report the late decryption.
        let info = UnableToDecryptInfo {
            event_id: event_id.to_owned(),
            time_to_decrypt: Some(pending_utd_report.marked_utd_at.elapsed()),
            cause,
        };
        self.parent.on_utd(info);
        self.reported_utds.insert(event_id.to_owned());
    }
}

impl Drop for UtdHookManager {
    fn drop(&mut self) {
        // Cancel all the outstanding delayed tasks to report UTDs.
        let mut pending_delayed = self.pending_delayed.lock().unwrap();
        for (_, pending_utd_report) in pending_delayed.drain() {
            pending_utd_report.report_task.abort();
        }

        // If there is a pending flush task, abort it. We'll lose data, but
        // there's not much we can do at this point. It's better than hanging onto a
        // reference to the Client forever.
        self.reported_utds.cancel_flush_task();
    }
}

/// A manager which keeps track of the UTD events which have been reported to
/// the parent hook, and takes care of persisting the data to the store
/// periodically.
///
/// It is based on a Bloom filter, which is a space-efficient probabilistic data
/// structure.
#[derive(Debug, Clone)]
struct ReportedUtdsManager {
    /// A Client associated with the UTD hook. This is used to access the store
    /// which we persist our data to.
    client: Client,

    /// The actual data about which events have been reported.
    data: Arc<Mutex<ReportedUtdsData>>,

    /// Mutex which is used to ensure that only one flush job runs at a time.
    /// See the notes in [`ReportedUtdsManager::flush`].
    flush_mutex: Arc<tokio::sync::Mutex<()>>,
}

/// Data store object for [`ReportedUtdsManager::data`].
#[derive(Debug)]
struct ReportedUtdsData {
    /// Bloom filter containing the event IDs of events which have been reported
    /// as UTDs
    pub bloom_filter: GrowableBloom,

    /// A pending task to flush the data to the store.
    ///
    /// If this is `None`, there is no pending job.
    pub flush_task: Option<JoinHandle<()>>,
}

impl ReportedUtdsManager {
    /// Create a new [`ReportedUtdsManager`] with an empty data set.
    pub fn new(client: Client) -> Self {
        let bloom_filter = {
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
            GrowableBloomBuilder::new().estimated_insertions(1000).desired_error_ratio(0.01).build()
        };
        Self {
            client,
            data: Arc::new(Mutex::new(ReportedUtdsData { bloom_filter, flush_task: None })),
            flush_mutex: Arc::new(tokio::sync::Mutex::new(())),
        }
    }

    /// Insert a new entry into the bloom filter.
    ///
    /// A task will be started to persist the state of the bloom filter to the
    /// store, if one was not already pending.
    pub fn insert(&self, event_id: OwnedEventId) {
        let mut data = self.data.lock().unwrap();

        data.bloom_filter.insert(event_id);

        // If there isn't already an active job to flush the data to the store, kick one
        // off.
        if data.flush_task.is_none() {
            trace!("Scheduling UtdHookManager flush task");
            let this = self.clone();
            data.flush_task = Some(spawn(async move {
                this.flush().await.unwrap_or_else(|e| {
                    error!("unable to flush UTD report data: {}", e);
                });
            }));
        }
    }

    /// Test if the event id is in the bloom filter.
    ///
    /// If `true` is returned, this event ID was *probably* previously reported.
    /// If `false` is returned, it has definitely *not* been reported.
    pub fn contains(&self, event_id: &EventId) -> bool {
        self.data.lock().unwrap().bloom_filter.contains(event_id)
    }

    /// If there is a pending flush task, cancel it.
    pub fn cancel_flush_task(&self) {
        if let Some(flush_task) = self.data.lock().unwrap().flush_task.take() {
            warn!("Cancelling pending UTD report flush task");
            flush_task.abort();
        }
    }

    async fn flush(&self) -> Result<(), Box<dyn std::error::Error>> {
        // Make sure that only one flush task runs at a time.
        // Otherwise, if one flush task was particularly slow, it could be overtaken
        // by the next task; then when the first task completed it would overwrite the
        // newer data.
        //
        // (Annoyingly, we can't use the mutex on `data` for this, because that is
        // a regular `std::sync::Mutex`, which isn't `Send`, so we can't hold over an
        // `await` point. Conversely, we can't use `flush_mutex` to protect `data`
        // elsewhere because locking a `tokio::sync::Mutex` is an async
        // operation, and we require synchronous access to `data`.)
        let _flush_lock = self.flush_mutex.lock().await;

        // Atomically clear the `flush_task` and serialise the current state of the
        // bloom filter.
        let serialized = {
            let mut data = self.data.lock().unwrap();
            data.flush_task.take();
            StateStoreDataValue::UtdHookManagerData(rmp_serde::to_vec(&data.bloom_filter)?)
        };

        self.client.store().set_kv_data(StateStoreDataKey::UtdHookManagerData, serialized).await?;
        debug!("Flushed UTD report data to store");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use matrix_sdk::test_utils::test_client_builder;
    use matrix_sdk_test::async_test;
    use ruma::event_id;

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

    async fn build_test_client() -> Client {
        test_client_builder(None).build().await.unwrap()
    }

    #[async_test]
    async fn test_deduplicates_utds() {
        // If I create a dummy hook,
        let hook = Arc::new(Dummy::default());

        // And I wrap with the UtdHookManager,
        let wrapper = UtdHookManager::new(hook.clone(), build_test_client().await);

        // And I call the `on_utd` method multiple times, sometimes on the same event,
        wrapper.on_utd(event_id!("$1"), UtdCause::Unknown);
        wrapper.on_utd(event_id!("$1"), UtdCause::Unknown);
        wrapper.on_utd(event_id!("$2"), UtdCause::Unknown);
        wrapper.on_utd(event_id!("$1"), UtdCause::Unknown);
        wrapper.on_utd(event_id!("$2"), UtdCause::Unknown);
        wrapper.on_utd(event_id!("$3"), UtdCause::Unknown);

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
        }
    }

    #[async_test]
    async fn test_on_late_decrypted_no_effect() {
        // If I create a dummy hook,
        let hook = Arc::new(Dummy::default());

        // And I wrap with the UtdHookManager,
        let wrapper = UtdHookManager::new(hook.clone(), build_test_client().await);

        // And I call the `on_late_decrypt` method before the event had been marked as
        // utd,
        wrapper.on_late_decrypt(event_id!("$1"), UtdCause::Unknown);

        // Then nothing is registered in the parent hook.
        assert!(hook.utds.lock().unwrap().is_empty());
    }

    #[async_test]
    async fn test_on_late_decrypted_after_utd_no_grace_period() {
        // If I create a dummy hook,
        let hook = Arc::new(Dummy::default());

        // And I wrap with the UtdHookManager,
        let wrapper = UtdHookManager::new(hook.clone(), build_test_client().await);

        // And I call the `on_utd` method for an event,
        wrapper.on_utd(event_id!("$1"), UtdCause::Unknown);

        // Then the UTD has been notified, but not as late-decrypted event.
        {
            let utds = hook.utds.lock().unwrap();
            assert_eq!(utds.len(), 1);
            assert_eq!(utds[0].event_id, event_id!("$1"));
            assert!(utds[0].time_to_decrypt.is_none());
        }

        // And when I call the `on_late_decrypt` method,
        wrapper.on_late_decrypt(event_id!("$1"), UtdCause::Unknown);

        // Then the event is not reported again as a late-decryption.
        {
            let utds = hook.utds.lock().unwrap();
            assert_eq!(utds.len(), 1);

            // The previous report is still there. (There was no grace period.)
            assert_eq!(utds[0].event_id, event_id!("$1"));
            assert!(utds[0].time_to_decrypt.is_none());
        }
    }

    #[cfg(not(target_arch = "wasm32"))] // wasm32 has no time for that
    #[async_test]
    async fn test_delayed_utd() {
        // If I create a dummy hook,
        let hook = Arc::new(Dummy::default());

        // And I wrap with the UtdHookManager, configured to delay reporting after 2
        // seconds.
        let wrapper = UtdHookManager::new(hook.clone(), build_test_client().await)
            .with_max_delay(Duration::from_secs(2));

        // And I call the `on_utd` method for an event,
        wrapper.on_utd(event_id!("$1"), UtdCause::Unknown);

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

    #[cfg(not(target_arch = "wasm32"))] // wasm32 has no time for that
    #[async_test]
    async fn test_delayed_late_decryption() {
        // If I create a dummy hook,
        let hook = Arc::new(Dummy::default());

        // And I wrap with the UtdHookManager, configured to delay reporting after 2
        // seconds.
        let wrapper = UtdHookManager::new(hook.clone(), build_test_client().await)
            .with_max_delay(Duration::from_secs(2));

        // And I call the `on_utd` method for an event,
        wrapper.on_utd(event_id!("$1"), UtdCause::Unknown);

        // Then the UTD has not been notified quite yet.
        assert!(hook.utds.lock().unwrap().is_empty());
        assert_eq!(wrapper.pending_delayed.lock().unwrap().len(), 1);

        // If I wait for 1 second, and mark the event as late-decrypted,
        sleep(Duration::from_secs(1)).await;

        wrapper.on_late_decrypt(event_id!("$1"), UtdCause::Unknown);

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
