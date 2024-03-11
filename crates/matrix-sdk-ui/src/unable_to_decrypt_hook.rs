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
    collections::HashSet,
    sync::{Arc, Mutex},
    time::Duration,
};

use ruma::OwnedEventId;
use tokio::{spawn, task::JoinHandle, time::sleep};

/// A generic interface which methods get called whenever we observe a
/// unable-to-decrypt (UTD) event.
pub trait UnableToDecryptHook: std::fmt::Debug + Send + Sync {
    /// Called every time the hook observes an encrypted event that couldn't be
    /// decrypted.
    fn on_utd(&self, info: UnableToDecryptInfo);

    /// Called every time we successfully decrypted an event that before,
    /// couldn't be decrypted initially.
    ///
    /// This happens whenever the key to decrypt an event comes in after we saw
    /// the encrypted event. The cause might be that the network sent the
    /// key slower than the event itself, or that there was any failure in
    /// the decryption process, or that the key to decrypt was received late
    /// (e.g. from a backup, etc.).
    fn on_late_decrypt(&self, info: UnableToDecryptInfo);
}

/// Information about an event we couldn't decrypt.
#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct UnableToDecryptInfo {
    /// The identifier of the event that couldn't get decrypted.
    pub event_id: OwnedEventId,
}

/// A decorator over an existing [`UnableToDecryptHook`] that deduplicates UTDs
/// on similar events, and adds basic consistency checks.
///
/// It can also implement a grace period before reporting an event as a UTD, if
/// configured with [`Self::with_delay`]. Instead of immediately reporting the
/// UTD, the reporting will be delayed by the grace period; the reporting of the
/// late decryption via [`UnableToDecryptHook::on_late_decrypt`] still happens
/// in all the cases, though.
#[derive(Debug)]
pub struct SmartUtdHook {
    /// The parent hook we'll call, when we have found a unique UTD.
    parent: Arc<dyn UnableToDecryptHook>,

    /// The set of events we've already marked as UTDs before.
    ///
    /// Note: this is unbounded, because we have absolutely no idea how long it
    /// will take for a UTD to resolve, or if it will even resolve at any
    /// point.
    known_utds: Arc<Mutex<HashSet<OwnedEventId>>>,

    /// An optional delay before marking the event as UTD.
    delay: Option<Duration>,

    /// The set of outstanding tasks to report deferred UTDs.
    pending_delayed: Arc<Mutex<Vec<(OwnedEventId, JoinHandle<()>)>>>,
}

impl SmartUtdHook {
    /// Create a new [`SmartUtdHook`] for the given hook.
    pub fn new(parent: Arc<dyn UnableToDecryptHook>) -> Self {
        Self {
            parent,
            known_utds: Default::default(),
            delay: None,
            pending_delayed: Default::default(),
        }
    }

    /// Reports UTDs with the given delay.
    ///
    /// Note: late decryptions are always reported, even if there was a grace
    /// period set for the reporting of the UTD.
    pub fn with_delay(mut self, delay: Duration) -> Self {
        self.delay = Some(delay);
        self
    }
}

impl Drop for SmartUtdHook {
    fn drop(&mut self) {
        // Cancel all the outstanding delayed tasks to report UTDs.
        let mut pending_delayed = self.pending_delayed.lock().unwrap();
        for (_, task) in pending_delayed.drain(..) {
            task.abort();
        }
    }
}

impl UnableToDecryptHook for SmartUtdHook {
    fn on_utd(&self, info: UnableToDecryptInfo) {
        // Only let the parent hook know if the event wasn't already handled.
        if !self.known_utds.lock().unwrap().insert(info.event_id.to_owned()) {
            return;
        }

        let Some(delay) = self.delay.clone() else {
            // No delay: immediately report the event to the parent hook.
            self.parent.on_utd(info);
            return;
        };

        let event_id = info.event_id.clone();

        // Clone Arc'd pointers shared with the task below.
        let known_utds = self.known_utds.clone();
        let pending_delayed = self.pending_delayed.clone();
        let parent = self.parent.clone();

        // Spawn a task that will wait for the given delay, and maybe call the parent
        // hook then.
        let handle = spawn(async move {
            // Wait for the given delay.
            sleep(delay.clone()).await;

            // In any case, remove the task from the outstanding set.
            pending_delayed.lock().unwrap().retain(|(event_id, _)| *event_id != info.event_id);

            // Check if the event is still in the set: if not, it's been decrypted since
            // then!
            if known_utds.lock().unwrap().contains(&info.event_id) {
                parent.on_utd(info);
            }
        });

        // Add the task to the set of pending tasks.
        self.pending_delayed.lock().unwrap().push((event_id, handle));
    }

    fn on_late_decrypt(&self, info: UnableToDecryptInfo) {
        // Only let the parent hook know if the event was known to be a UTDs.
        if self.known_utds.lock().unwrap().remove(&info.event_id) {
            self.parent.on_late_decrypt(info);
        }
    }
}

#[cfg(test)]
mod tests {
    use matrix_sdk_test::async_test;
    use ruma::{event_id, owned_event_id};

    use super::*;

    #[derive(Debug, Default)]
    struct Dummy {
        utds: Mutex<Vec<UnableToDecryptInfo>>,
        late_decrypted: Mutex<Vec<UnableToDecryptInfo>>,
    }

    impl UnableToDecryptHook for Dummy {
        fn on_utd(&self, info: UnableToDecryptInfo) {
            self.utds.lock().unwrap().push(info);
        }
        fn on_late_decrypt(&self, info: UnableToDecryptInfo) {
            self.late_decrypted.lock().unwrap().push(info);
        }
    }

    #[test]
    fn test_deduplicates_utds() {
        // If I create a dummy hook,
        let hook = Arc::new(Dummy::default());

        // And I wrap with the SmartUtdHook decorator,
        let wrapper = SmartUtdHook::new(hook.clone());

        // And I call the `on_utd` method multiple times, sometimes on the same event,
        wrapper.on_utd(UnableToDecryptInfo { event_id: owned_event_id!("$1") });
        wrapper.on_utd(UnableToDecryptInfo { event_id: owned_event_id!("$1") });
        wrapper.on_utd(UnableToDecryptInfo { event_id: owned_event_id!("$2") });
        wrapper.on_utd(UnableToDecryptInfo { event_id: owned_event_id!("$1") });
        wrapper.on_utd(UnableToDecryptInfo { event_id: owned_event_id!("$2") });
        wrapper.on_utd(UnableToDecryptInfo { event_id: owned_event_id!("$3") });

        // Then the event ids have been deduplicated,
        {
            let utds = hook.utds.lock().unwrap();
            assert_eq!(utds.len(), 3);
            assert_eq!(utds[0].event_id, event_id!("$1"));
            assert_eq!(utds[1].event_id, event_id!("$2"));
            assert_eq!(utds[2].event_id, event_id!("$3"));
        }

        // And no late decryption has been reported.
        assert!(hook.late_decrypted.lock().unwrap().is_empty());
    }

    #[test]
    fn test_on_late_decrypted_no_effect() {
        // If I create a dummy hook,
        let hook = Arc::new(Dummy::default());

        // And I wrap with the SmartUtdHook decorator,
        let wrapper = SmartUtdHook::new(hook.clone());

        // And I call the `on_late_decrypt` method before the event had been marked as
        // utd,
        wrapper.on_late_decrypt(UnableToDecryptInfo { event_id: owned_event_id!("$1") });

        // Then nothing is registered in the parent hook.
        assert!(hook.utds.lock().unwrap().is_empty());
        assert!(hook.late_decrypted.lock().unwrap().is_empty());
    }

    #[test]
    fn test_on_late_decrypted_after_utd() {
        // If I create a dummy hook,
        let hook = Arc::new(Dummy::default());

        // And I wrap with the SmartUtdHook decorator,
        let wrapper = SmartUtdHook::new(hook.clone());

        // And I call the `on_utd` method for an event,
        wrapper.on_utd(UnableToDecryptInfo { event_id: owned_event_id!("$1") });

        // Then the UTD has been notified, but not as late-decrypted event.
        {
            let utds = hook.utds.lock().unwrap();
            assert_eq!(utds.len(), 1);
            assert_eq!(utds[0].event_id, event_id!("$1"));
        }
        assert!(hook.late_decrypted.lock().unwrap().is_empty());

        // And when I call the `on_late_decrypt` method before the event had been marked
        // as utd,
        wrapper.on_late_decrypt(UnableToDecryptInfo { event_id: owned_event_id!("$1") });

        // Then the event is now known as a late-decrypted event too.
        {
            let late_decrypted = hook.late_decrypted.lock().unwrap();
            assert_eq!(late_decrypted.len(), 1);
            assert_eq!(late_decrypted[0].event_id, event_id!("$1"));
        }

        // (Sanity check: The reported UTD hasn't been touched.)
        {
            let utds = hook.utds.lock().unwrap();
            assert_eq!(utds.len(), 1);
            assert_eq!(utds[0].event_id, event_id!("$1"));
        }
    }

    #[cfg(not(target_arch = "wasm32"))] // wasm32 has no time for that
    #[async_test]
    async fn test_delayed_reporting() {
        // If I create a dummy hook,
        let hook = Arc::new(Dummy::default());

        // And I wrap with the SmartUtdHook decorator, configured to delay reporting
        // after 2 seconds.
        let wrapper = SmartUtdHook::new(hook.clone()).with_delay(Duration::from_secs(2));

        // And I call the `on_utd` method for an event,
        wrapper.on_utd(UnableToDecryptInfo { event_id: owned_event_id!("$1") });

        // Then the UTD has not been notified quite yet.
        assert!(hook.utds.lock().unwrap().is_empty());
        assert!(hook.late_decrypted.lock().unwrap().is_empty());

        // If I wait for 1 second, then it's still not been notified yet.
        sleep(Duration::from_secs(1)).await;

        assert!(hook.utds.lock().unwrap().is_empty());
        assert!(hook.late_decrypted.lock().unwrap().is_empty());

        // But if I wait just a bit more, then it's been notified \o/
        sleep(Duration::from_millis(1500)).await;

        {
            let utds = hook.utds.lock().unwrap();
            assert_eq!(utds.len(), 1);
            assert_eq!(utds[0].event_id, event_id!("$1"));
        }
    }

    #[cfg(not(target_arch = "wasm32"))] // wasm32 has no time for that
    #[async_test]
    async fn test_delayed_not_reporting() {
        // If I create a dummy hook,
        let hook = Arc::new(Dummy::default());

        // And I wrap with the SmartUtdHook decorator, configured to delay reporting
        // after 2 seconds.
        let wrapper = SmartUtdHook::new(hook.clone()).with_delay(Duration::from_secs(2));

        // And I call the `on_utd` method for an event,
        wrapper.on_utd(UnableToDecryptInfo { event_id: owned_event_id!("$1") });

        // Then the UTD has not been notified quite yet.
        assert!(hook.utds.lock().unwrap().is_empty());
        assert!(hook.late_decrypted.lock().unwrap().is_empty());
        assert_eq!(wrapper.pending_delayed.lock().unwrap().len(), 1);

        // If I wait for 1 second, and mark the event as late-decrypted,
        sleep(Duration::from_secs(1)).await;

        wrapper.on_late_decrypt(UnableToDecryptInfo { event_id: owned_event_id!("$1") });

        // Then it's still not reported as a UTD (but it's reported as a late-decrypted
        // event).
        assert!(hook.utds.lock().unwrap().is_empty());
        {
            let late = hook.late_decrypted.lock().unwrap();
            assert_eq!(late.len(), 1);
            assert_eq!(late[0].event_id, event_id!("$1"));
        }

        // And I get the same result after the grace period ended for sure.
        sleep(Duration::from_millis(1500)).await;

        assert!(hook.utds.lock().unwrap().is_empty());
        {
            let late = hook.late_decrypted.lock().unwrap();
            assert_eq!(late.len(), 1);
            assert_eq!(late[0].event_id, event_id!("$1"));
        }

        // And there aren't any pending delayed reports anymore.
        assert!(wrapper.pending_delayed.lock().unwrap().is_empty());
    }
}
