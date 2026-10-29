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

//! Types and helpers to remove expired events from the event cache, as
//! defined by [MSC1763] (configurable message retention periods).
//!
//! [MSC1763]: https://github.com/matrix-org/matrix-spec-proposals/pull/1763

use ruma::{MilliSecondsSinceUnixEpoch, serde::Raw};
use serde_json::json;

use super::Event;

/// The event type of the placeholder replacing an expired thread root event,
/// see [`expired_thread_root_placeholder`].
pub const EXPIRED_THREAD_ROOT_PLACEHOLDER_EVENT_TYPE: &str =
    "io.element.msc1763.expired_thread_root";

/// Create a placeholder for an expired thread root event.
///
/// When a thread root event expires but some events of its thread don't, the
/// thread root is replaced by this placeholder, so that the thread can still
/// be displayed. The placeholder has the same event ID, sender and timestamp
/// as the original event, but none of its content.
///
/// Returns `None` if `event` has no event ID.
pub fn expired_thread_root_placeholder(event: &Event) -> Option<Event> {
    let event_id = event.event_id()?;
    let sender = event.sender();
    let timestamp = event.timestamp();

    let raw = Raw::new(&json!({
        "type": EXPIRED_THREAD_ROOT_PLACEHOLDER_EVENT_TYPE,
        "event_id": event_id,
        "sender": sender,
        "origin_server_ts": timestamp,
        "content": {},
    }))
    .ok()?
    .cast_unchecked();

    Some(Event::from_plaintext_with_max_timestamp(
        raw,
        timestamp.unwrap_or_else(MilliSecondsSinceUnixEpoch::now),
    ))
}

/// Whether this event is a placeholder for an expired thread root event, see
/// [`expired_thread_root_placeholder`].
pub fn is_expired_thread_root_placeholder(event: &Event) -> bool {
    event.kind.event_type().as_deref() == Some(EXPIRED_THREAD_ROOT_PLACEHOLDER_EVENT_TYPE)
}

/// Whether this event can expire, i.e. whether it's a non-state event whose
/// `origin_server_ts` is strictly older than `before`.
///
/// Placeholders created by [`expired_thread_root_placeholder`] never expire.
pub fn is_expired(event: &Event, before: MilliSecondsSinceUnixEpoch) -> bool {
    if is_expired_thread_root_placeholder(event) {
        return false;
    }

    // Only non-state events are subject to retention.
    if event.raw().get_field::<serde::de::IgnoredAny>("state_key").ok().flatten().is_some() {
        return false;
    }

    event.timestamp().is_some_and(|timestamp| timestamp < before)
}

#[cfg(test)]
mod tests {
    use matrix_sdk_test::{User, event_factory::EventFactory};
    use ruma::{MilliSecondsSinceUnixEpoch, event_id, room_id, uint};

    use super::{expired_thread_root_placeholder, is_expired, is_expired_thread_root_placeholder};

    #[test]
    fn test_is_expired() {
        let f = EventFactory::new().room(room_id!("!r:x.y")).sender(User::Alice);
        let before = MilliSecondsSinceUnixEpoch(uint!(100));

        let old = f.text_msg("old").server_ts(99).into_event();
        let boundary = f.text_msg("boundary").server_ts(100).into_event();
        let recent = f.text_msg("recent").server_ts(101).into_event();
        let old_state = f.room_name("name").server_ts(1).into_event();

        assert!(is_expired(&old, before));
        assert!(!is_expired(&boundary, before));
        assert!(!is_expired(&recent, before));
        assert!(!is_expired(&old_state, before));
    }

    #[test]
    fn test_expired_thread_root_placeholder() {
        let f = EventFactory::new().room(room_id!("!r:x.y")).sender(User::Alice);
        let event = f.text_msg("root").event_id(event_id!("$root")).server_ts(42).into_event();

        let placeholder = expired_thread_root_placeholder(&event).unwrap();

        assert_eq!(placeholder.event_id(), event.event_id());
        assert_eq!(placeholder.sender(), event.sender());
        assert_eq!(placeholder.timestamp(), event.timestamp());
        assert!(is_expired_thread_root_placeholder(&placeholder));
        assert!(!is_expired_thread_root_placeholder(&event));

        // The placeholder never expires.
        assert!(!is_expired(&placeholder, MilliSecondsSinceUnixEpoch(uint!(1000))));
    }
}
