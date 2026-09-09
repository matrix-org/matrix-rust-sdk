// Copyright 2025 The Matrix.org Foundation C.I.C.
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

//! Reading the sticky metadata out of raw sync events.
//!
//! Sticky events reach us both through the dedicated sliding sync extension
//! and, deduplicated out of it, through room timelines. Either way they are
//! `Raw<AnySyncTimelineEvent>`s that carry the MSC4354 `sticky` object at the
//! top level, next to the usual `sender`, `event_id` and `origin_server_ts`.
//!
//! The *key* of a sticky event, on the other hand, lives in its content
//! (`type` and `content.sticky_key`), which for an encrypted event is only
//! available after decryption. [`classify`] therefore only reads the outer
//! metadata, and [`resolve`] combines it with the (decrypted) content.

use std::collections::BTreeMap;

use matrix_sdk_common::deserialized_responses::TimelineEventKind;
use ruma::{
    MilliSecondsSinceUnixEpoch, OwnedEventId, OwnedUserId,
    events::{
        AnySyncTimelineEvent, TimelineEventType,
        sticky::{StickyDurationMs, StickyObject},
    },
    serde::Raw,
};
use serde::Deserialize;
use serde_json::value::RawValue as RawJsonValue;

use super::{StickyKey, map::Candidate};

/// The sticky metadata of an event, read from the outer (possibly encrypted)
/// event.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct StickyMeta {
    /// The sender of the event.
    pub sender: OwnedUserId,
    /// The event ID.
    pub event_id: OwnedEventId,
    /// `origin_server_ts + sticky.duration_ms`, the MSC4354 conflict-resolution
    /// value.
    pub order_ts: u64,
    /// When the event stops being sticky, in milliseconds since the Unix epoch.
    pub expires_at: u64,
    /// The part of the event that depends on its content.
    pub payload: Payload,
}

/// What we know about a sticky event's content from the outer event alone.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Payload {
    /// The event is in the clear, so its key is known.
    Plain {
        /// The event type.
        event_type: TimelineEventType,
        /// The `content.sticky_key`.
        sticky_key: String,
        /// Whether the content carries nothing but the sticky key, i.e. this
        /// event removes the entry.
        is_tombstone: bool,
    },
    /// The event is encrypted: its type and sticky key are only known once it
    /// is decrypted.
    Encrypted {
        /// The Megolm session the event was encrypted with, if the content is
        /// well-formed.
        session_id: Option<String>,
    },
}

/// The top-level fields of a sync event we need to tell whether, and until
/// when, it is sticky.
///
/// The field names mirror Ruma's `OriginalSyncMessageLikeEvent::sticky` and
/// `MessageLikeUnsigned::sticky_duration_ttl_ms`; those are only reachable via
/// a typed event content, which we don't have (nor want to deserialize) here.
///
/// A malformed `sticky` object (e.g. an out-of-range duration, which Ruma
/// rejects) fails the whole probe, and the event is then simply not sticky;
/// that is also what Ruma does with `default_on_error`.
#[derive(Deserialize)]
struct EventProbe<'a> {
    sender: OwnedUserId,
    event_id: OwnedEventId,
    origin_server_ts: MilliSecondsSinceUnixEpoch,
    #[serde(rename = "type")]
    event_type: TimelineEventType,
    #[serde(rename = "msc4354_sticky")]
    sticky: Option<StickyObject>,
    #[serde(default)]
    unsigned: UnsignedProbe,
    #[serde(borrow)]
    content: &'a RawJsonValue,
}

#[derive(Default, Deserialize)]
struct UnsignedProbe {
    #[serde(rename = "msc4354_sticky_duration_ttl_ms")]
    sticky_duration_ttl_ms: Option<u64>,
}

/// The unstable name of `content.sticky_key`, as defined by MSC4354.
const UNSTABLE_STICKY_KEY: &str = "msc4354_sticky_key";

/// The stable name of `content.sticky_key`.
const STICKY_KEY: &str = "sticky_key";

/// Read the sticky metadata of a raw sync event, or `None` if it is not
/// sticky, is already expired at `now`, or is in the clear without a sticky
/// key (such events are not tracked in the map).
///
/// `now` is the local time the event was received, in milliseconds since the
/// Unix epoch.
pub(crate) fn classify(now: u64, raw: &Raw<AnySyncTimelineEvent>) -> Option<StickyMeta> {
    let probe: EventProbe<'_> = serde_json::from_str(raw.json().get()).ok()?;

    // Ruma guarantees this is within the range MSC4354 allows.
    let duration_ms = u64::from(probe.sticky?.duration_ms.get());
    let origin_server_ts = u64::from(probe.origin_server_ts.0);

    // MSC4354: the start time is `min(received_ts, origin_server_ts)`, so that
    // an `origin_server_ts` in the future can't extend stickiness, and the end
    // time is `start + duration`. When the server tells us how long the event
    // has left, use that instead, as it removes the clock skew between us and
    // the server.
    let expires_at = match probe.unsigned.sticky_duration_ttl_ms {
        Some(ttl_ms) => now.saturating_add(ttl_ms.min(u64::from(StickyDurationMs::MAX))),
        None => origin_server_ts.min(now).saturating_add(duration_ms),
    };

    if expires_at <= now {
        return None;
    }

    let payload = if probe.event_type == TimelineEventType::RoomEncrypted {
        Payload::Encrypted { session_id: read_session_id(probe.content) }
    } else {
        let (sticky_key, is_tombstone) = read_content(probe.content)?;
        Payload::Plain { event_type: probe.event_type, sticky_key, is_tombstone }
    };

    Some(StickyMeta {
        sender: probe.sender,
        event_id: probe.event_id,
        order_ts: origin_server_ts.saturating_add(duration_ms),
        expires_at,
        payload,
    })
}

/// Turn a sticky event into a map candidate, using the content of `kind` (the
/// event itself if it was in the clear, its decrypted form otherwise).
///
/// Returns `None` if the content carries no sticky key.
pub(crate) fn resolve(meta: StickyMeta, kind: TimelineEventKind) -> Option<Candidate> {
    let (event_type, sticky_key, is_tombstone) = match meta.payload {
        Payload::Plain { event_type, sticky_key, is_tombstone } => {
            (event_type, sticky_key, is_tombstone)
        }
        Payload::Encrypted { .. } => {
            let probe: DecryptedProbe<'_> = serde_json::from_str(kind.raw().json().get()).ok()?;
            let (sticky_key, is_tombstone) = read_content(probe.content)?;
            (probe.event_type, sticky_key, is_tombstone)
        }
    };

    Some(Candidate {
        key: StickyKey { sender: meta.sender, event_type, sticky_key },
        event_id: meta.event_id,
        order_ts: meta.order_ts,
        expires_at: meta.expires_at,
        is_tombstone,
        kind,
    })
}

/// The fields of a decrypted event we need to key it.
#[derive(Deserialize)]
struct DecryptedProbe<'a> {
    #[serde(rename = "type")]
    event_type: TimelineEventType,
    #[serde(borrow)]
    content: &'a RawJsonValue,
}

/// Read `(sticky_key, is_tombstone)` from an event content, or `None` if it
/// has no sticky key.
fn read_content(content: &RawJsonValue) -> Option<(String, bool)> {
    // Only the keys are inspected, the values stay as raw JSON.
    let content: BTreeMap<&str, &RawJsonValue> = serde_json::from_str(content.get()).ok()?;

    let sticky_key = content
        .get(UNSTABLE_STICKY_KEY)
        .or_else(|| content.get(STICKY_KEY))
        .and_then(|value| serde_json::from_str::<String>(value.get()).ok())?;

    // MSC4354: to remove an entry, send an event "with just `content.sticky_key`
    // set, with all the other application-specific fields omitted".
    let is_tombstone = content.keys().all(|key| *key == UNSTABLE_STICKY_KEY || *key == STICKY_KEY);

    Some((sticky_key, is_tombstone))
}

/// Read the Megolm session ID out of an `m.room.encrypted` content.
fn read_session_id(content: &RawJsonValue) -> Option<String> {
    #[derive(Deserialize)]
    struct EncryptedContentProbe {
        session_id: Option<String>,
    }

    serde_json::from_str::<EncryptedContentProbe>(content.get()).ok()?.session_id
}

#[cfg(test)]
mod tests {
    use assert_matches2::assert_matches;
    use matrix_sdk_common::deserialized_responses::TimelineEventKind;
    use ruma::{
        events::{AnySyncTimelineEvent, TimelineEventType},
        serde::Raw,
    };
    use serde_json::{Value, json};

    use super::{Payload, classify, resolve};

    const NOW: u64 = 10_000;

    fn raw(value: Value) -> Raw<AnySyncTimelineEvent> {
        serde_json::from_value(value).unwrap()
    }

    /// A sticky event sent 100ms ago, sticky for 500ms.
    fn sticky_event(content: Value) -> Raw<AnySyncTimelineEvent> {
        raw(json!({
            "type": "m.rtc.member",
            "sender": "@alice:localhost",
            "event_id": "$a:localhost",
            "origin_server_ts": NOW - 100,
            "content": content,
            "msc4354_sticky": { "duration_ms": 500 },
        }))
    }

    #[test]
    fn test_plaintext_event_is_classified_and_resolved() {
        let event = sticky_event(json!({ "msc4354_sticky_key": "slot", "application": "m.call" }));

        let meta = classify(NOW, &event).unwrap();
        assert_eq!(meta.sender, "@alice:localhost");
        assert_eq!(meta.event_id, "$a:localhost");
        // The start time is `min(origin_server_ts, now)`, i.e. the origin.
        assert_eq!(meta.expires_at, NOW + 400);
        assert_eq!(meta.order_ts, NOW + 400);
        assert_matches!(&meta.payload, Payload::Plain { event_type, sticky_key, is_tombstone });
        assert_eq!(*event_type, TimelineEventType::from("m.rtc.member"));
        assert_eq!(sticky_key, "slot");
        assert!(!is_tombstone);

        let candidate = resolve(meta, TimelineEventKind::PlainText { event }).unwrap();
        assert_eq!(candidate.key.sticky_key, "slot");
        assert!(!candidate.is_tombstone);
    }

    #[test]
    fn test_future_origin_server_ts_does_not_extend_stickiness() {
        let event = raw(json!({
            "type": "m.rtc.member",
            "sender": "@alice:localhost",
            "event_id": "$a:localhost",
            "origin_server_ts": NOW + 5000,
            "content": { "msc4354_sticky_key": "slot" },
            "msc4354_sticky": { "duration_ms": 500 },
        }));

        let meta = classify(NOW, &event).unwrap();
        assert_eq!(meta.expires_at, NOW + 500);
        // The ordering value is taken from the event as is, though.
        assert_eq!(meta.order_ts, NOW + 5500);
    }

    #[test]
    fn test_server_ttl_takes_precedence_for_expiry_but_not_for_ordering() {
        let event = raw(json!({
            "type": "m.rtc.member",
            "sender": "@alice:localhost",
            "event_id": "$a:localhost",
            "origin_server_ts": 1000,
            "content": { "msc4354_sticky_key": "slot" },
            "msc4354_sticky": { "duration_ms": 500 },
            "unsigned": { "msc4354_sticky_duration_ttl_ms": 300 },
        }));

        let meta = classify(NOW, &event).unwrap();
        assert_eq!(meta.expires_at, NOW + 300);
        assert_eq!(meta.order_ts, 1500);
    }

    #[test]
    fn test_expired_event_is_not_sticky() {
        let event = sticky_event(json!({ "msc4354_sticky_key": "slot" }));

        // It expires at `NOW + 400`.
        assert!(classify(NOW + 400, &event).is_none());
    }

    #[test]
    fn test_out_of_range_duration_is_not_sticky() {
        let event = raw(json!({
            "type": "m.rtc.member",
            "sender": "@alice:localhost",
            "event_id": "$a:localhost",
            "origin_server_ts": NOW,
            "content": { "msc4354_sticky_key": "slot" },
            "msc4354_sticky": { "duration_ms": 3_600_001 },
        }));

        assert!(classify(NOW, &event).is_none());
    }

    #[test]
    fn test_event_without_sticky_key_is_not_tracked() {
        let event = raw(json!({
            "type": "m.rtc.member",
            "sender": "@alice:localhost",
            "event_id": "$a:localhost",
            "origin_server_ts": NOW,
            "content": { "application": "m.call" },
            "msc4354_sticky": { "duration_ms": 500 },
        }));

        assert!(classify(NOW, &event).is_none());
    }

    #[test]
    fn test_non_sticky_event_is_ignored() {
        let event = raw(json!({
            "type": "m.room.message",
            "sender": "@alice:localhost",
            "event_id": "$a:localhost",
            "origin_server_ts": NOW,
            "content": { "body": "hello", "msgtype": "m.text" },
        }));

        assert!(classify(NOW, &event).is_none());
    }

    #[test]
    fn test_content_with_only_the_sticky_key_is_a_tombstone() {
        for content in [
            json!({ "msc4354_sticky_key": "slot" }),
            json!({ "sticky_key": "slot" }),
            json!({ "msc4354_sticky_key": "slot", "sticky_key": "slot" }),
        ] {
            let event = raw(json!({
                "type": "m.rtc.member",
                "sender": "@alice:localhost",
                "event_id": "$a:localhost",
                "origin_server_ts": NOW,
                "content": content,
                "msc4354_sticky": { "duration_ms": 500 },
            }));

            let meta = classify(NOW, &event).unwrap();
            assert_matches!(meta.payload, Payload::Plain { sticky_key, is_tombstone, .. });
            assert_eq!(sticky_key, "slot");
            assert!(is_tombstone);
        }
    }

    #[test]
    fn test_encrypted_event_is_resolved_from_its_decrypted_form() {
        let event = raw(json!({
            "type": "m.room.encrypted",
            "sender": "@alice:localhost",
            "event_id": "$a:localhost",
            "origin_server_ts": NOW,
            "content": {
                "algorithm": "m.megolm.v1.aes-sha2",
                "ciphertext": "AAAA",
                "session_id": "session",
            },
            "msc4354_sticky": { "duration_ms": 500 },
        }));

        let meta = classify(NOW, &event).unwrap();
        assert_matches!(&meta.payload, Payload::Encrypted { session_id });
        assert_eq!(session_id.as_deref(), Some("session"));

        // Pretend we decrypted it.
        let decrypted = TimelineEventKind::PlainText {
            event: raw(json!({
                "type": "m.rtc.member",
                "sender": "@alice:localhost",
                "event_id": "$a:localhost",
                "origin_server_ts": NOW,
                "content": { "msc4354_sticky_key": "slot", "application": "m.call" },
            })),
        };

        let candidate = resolve(meta, decrypted).unwrap();
        assert_eq!(candidate.key.event_type, TimelineEventType::from("m.rtc.member"));
        assert_eq!(candidate.key.sticky_key, "slot");
        assert!(!candidate.is_tombstone);
    }

    #[test]
    fn test_decrypted_event_without_sticky_key_is_not_tracked() {
        let event = raw(json!({
            "type": "m.room.encrypted",
            "sender": "@alice:localhost",
            "event_id": "$a:localhost",
            "origin_server_ts": NOW,
            "content": { "algorithm": "m.megolm.v1.aes-sha2", "ciphertext": "AAAA" },
            "msc4354_sticky": { "duration_ms": 500 },
        }));

        let meta = classify(NOW, &event).unwrap();

        let decrypted = TimelineEventKind::PlainText {
            event: raw(json!({
                "type": "m.room.message",
                "sender": "@alice:localhost",
                "event_id": "$a:localhost",
                "origin_server_ts": NOW,
                "content": { "body": "hello", "msgtype": "m.text" },
            })),
        };

        assert!(resolve(meta, decrypted).is_none());
    }
}
