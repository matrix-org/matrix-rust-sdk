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

use matrix_sdk_base::{deserialized_responses::TimelineEventKind, event_cache::Event};
use ruma::serde::Raw;

/// Strips the bundled relations from a collection of events.
pub(in crate::event_cache) fn strip_relations_from_events(items: &mut [Event]) {
    for ev in items.iter_mut() {
        strip_relations_from_event(ev);
    }
}

/// Strips the bundled relations from an event, if they were present.
pub(in crate::event_cache) fn strip_relations_from_event(ev: &mut Event) {
    match &mut ev.kind {
        TimelineEventKind::Decrypted(decrypted) => {
            // Remove all information about encryption info for
            // the bundled events.
            decrypted.unsigned_encryption_info = None;

            // Remove the `unsigned`/`m.relations` field, if needs be.
            strip_relations_if_present(&mut decrypted.event);
        }

        TimelineEventKind::UnableToDecrypt { event, .. }
        | TimelineEventKind::PlainText { event } => {
            strip_relations_if_present(event);
        }
    }
}

/// Removes the bundled relations from an event, if they were present.
///
/// Only replaces the present if it contained bundled relations.
fn strip_relations_if_present<T>(event: &mut Raw<T>) {
    // We're going to get rid of the `unsigned`/`m.relations` field, if it's
    // present.
    // Use a closure that returns an option so we can quickly short-circuit.
    let mut closure = || -> Option<()> {
        let mut val: serde_json::Value = event.deserialize_as().ok()?;
        let unsigned = val.get_mut("unsigned")?;
        let unsigned_obj = unsigned.as_object_mut()?;
        if unsigned_obj.remove("m.relations").is_some() {
            *event = Raw::new(&val).ok()?.cast_unchecked();
        }
        None
    };
    let _ = closure();
}
