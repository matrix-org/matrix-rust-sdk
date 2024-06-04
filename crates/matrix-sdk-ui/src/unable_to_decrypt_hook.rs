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

use matrix_sdk::crypto::types::events::UtdCause;
use ruma::{EventId, OwnedEventId};
use tokio::{spawn, task::JoinHandle, time::sleep};

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
}

impl UtdHookManager {
    /// Create a new [`UtdHookManager`] for the given hook.
    pub fn new(parent: Arc<dyn UnableToDecryptHook>) -> Self {
        Self {
            parent,
            known_utds: Default::default(),
            max_delay: None,
            pending_delayed: Default::default(),
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

        // Construct a closure which will report the UTD to the parent.
        let report_utd = {
            let info =
                UnableToDecryptInfo { event_id: event_id.to_owned(), time_to_decrypt: None, cause };
            let parent = self.parent.clone();
            move || {
                parent.on_utd(info);
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
    }
}

impl Drop for UtdHookManager {
    fn drop(&mut self) {
        // Cancel all the outstanding delayed tasks to report UTDs.
        let mut pending_delayed = self.pending_delayed.lock().unwrap();
        for (_, pending_utd_report) in pending_delayed.drain() {
            pending_utd_report.report_task.abort();
        }
    }
}

#[cfg(test)]
mod tests {
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

    #[test]
    fn test_deduplicates_utds() {
        // If I create a dummy hook,
        let hook = Arc::new(Dummy::default());

        // And I wrap with the UtdHookManager,
        let wrapper = UtdHookManager::new(hook.clone());

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

    #[test]
    fn test_on_late_decrypted_no_effect() {
        // If I create a dummy hook,
        let hook = Arc::new(Dummy::default());

        // And I wrap with the UtdHookManager,
        let wrapper = UtdHookManager::new(hook.clone());

        // And I call the `on_late_decrypt` method before the event had been marked as
        // utd,
        wrapper.on_late_decrypt(event_id!("$1"), UtdCause::Unknown);

        // Then nothing is registered in the parent hook.
        assert!(hook.utds.lock().unwrap().is_empty());
    }

    #[test]
    fn test_on_late_decrypted_after_utd_no_grace_period() {
        // If I create a dummy hook,
        let hook = Arc::new(Dummy::default());

        // And I wrap with the UtdHookManager,
        let wrapper = UtdHookManager::new(hook.clone());

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
        let wrapper = UtdHookManager::new(hook.clone()).with_max_delay(Duration::from_secs(2));

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
        let wrapper = UtdHookManager::new(hook.clone()).with_max_delay(Duration::from_secs(2));

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
