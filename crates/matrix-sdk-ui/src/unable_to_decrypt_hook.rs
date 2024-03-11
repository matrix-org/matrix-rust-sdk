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
};

use ruma::OwnedEventId;

/// A generic interface which methods get called whenever we observe a
/// unable-to-decrypt (UTD) event.
pub trait UnableToDecryptHook {
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
#[derive(Clone, Debug, Hash, PartialEq)]
pub struct UnableToDecryptInfo {
    /// The identifier of the event that couldn't get decrypted.
    pub event_id: OwnedEventId,
}

/// A decorator over an existing [`UnableToDecryptHook`] that deduplicates UTDs
/// on similar events, and adds basic consistency checks.
#[derive(Debug)]
pub struct SmartUtdHook {
    /// The parent hook we'll call, when we have found a unique UTD.
    parent: Arc<dyn UnableToDecryptHook>,

    /// The set of events we've already marked as UTDs before.
    known_utds: Arc<Mutex<HashSet<OwnedEventId>>>,
}

impl SmartUtdHook {
    /// Create a new [`SmartUtdHook`] for the given hook.
    pub fn new(parent: Arc<dyn UnableToDecryptHook>) -> Self {
        Self { parent, known_utds: Default::default() }
    }
}

impl UnableToDecryptHook for SmartUtdHook {
    fn on_utd(&self, info: UnableToDecryptInfo) {
        let mut set = self.known_utds.lock().unwrap();

        // Only let the parent hook know if the event wasn't already handled.
        if set.insert(info.event_id.to_owned()) {
            self.parent.on_utd(info);
        }
    }

    fn on_late_decrypt(&self, info: UnableToDecryptInfo) {
        let mut set = self.known_utds.lock().unwrap();

        // Only let the parent hook know if the event was known to be a UTDs.
        if set.remove(&info.event_id) {
            self.parent.on_late_decrypt(info);
        }
    }
}

#[cfg(test)]
mod tests {
    use ruma::{event_id, owned_event_id};

    use super::*;

    #[derive(Default)]
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
}
