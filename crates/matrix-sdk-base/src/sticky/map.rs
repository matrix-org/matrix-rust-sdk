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

//! The in-memory, per-room map of sticky events (the "ephemeral map" of
//! [MSC4354]).
//!
//! [MSC4354]: https://github.com/matrix-org/matrix-spec-proposals/pull/4354

use std::{
    cmp::{Ordering, Reverse},
    collections::{BinaryHeap, HashMap},
};

use matrix_sdk_common::deserialized_responses::TimelineEventKind;
use ruma::{MilliSecondsSinceUnixEpoch, OwnedEventId, UInt};
use tracing::warn;

use super::{RemovalReason, StickyEvent, StickyEventsUpdate, StickyKey};

/// The maximum number of entries (live values and tombstones alike) a single
/// room's map holds.
///
/// Servers are expected to rate limit sticky events, but nothing stops a room
/// member from sending a stream of events with distinct sticky keys. Beyond
/// this many entries, events for *new* keys are dropped; updates to existing
/// keys are still applied. Entries expire after at most one hour, so the map
/// recovers on its own.
pub(super) const MAX_ENTRIES: usize = 500;

/// A sticky event that has been fully resolved (decrypted if needed) and is
/// ready to be applied to the map.
#[derive(Debug)]
pub(crate) struct Candidate {
    /// The map key of the event.
    pub key: StickyKey,
    /// The event ID, used as the tie-breaker of last resort.
    pub event_id: OwnedEventId,
    /// `origin_server_ts + sticky.duration_ms`: the value MSC4354 orders
    /// competing events for the same key by ("last to expire wins"). It is
    /// derived from the event alone, so every client agrees on it.
    pub order_ts: u64,
    /// When this event stops being sticky, in milliseconds since the Unix
    /// epoch, according to our clock and the server's TTL hint.
    pub expires_at: u64,
    /// Whether the content carries nothing but the sticky key, which is how
    /// MSC4354 expresses the removal of a map entry.
    pub is_tombstone: bool,
    /// The event, decrypted if it was encrypted, along with its encryption
    /// info.
    pub kind: TimelineEventKind,
}

/// An entry of the map.
#[derive(Debug)]
struct Entry {
    event_id: OwnedEventId,
    order_ts: u64,
    expires_at: u64,
    is_tombstone: bool,
    kind: TimelineEventKind,
    /// Identifies the insertion this entry stems from, so that stale nodes in
    /// the expiry heap can be told apart from the current one.
    seq: u64,
}

impl Entry {
    /// The conflict-resolution rank of this entry: higher wins.
    fn rank(&self) -> (u64, &OwnedEventId) {
        (self.order_ts, &self.event_id)
    }

    /// Whether this entry is a live value at `now`, i.e. neither expired nor a
    /// tombstone.
    fn is_live(&self, now: u64) -> bool {
        !self.is_tombstone && self.expires_at > now
    }
}

/// A node of the expiry min-heap. Ordered by `expires_at`, then insertion
/// sequence number, so that keys never need to be compared.
#[derive(Debug)]
struct ExpiryNode {
    expires_at: u64,
    seq: u64,
    key: StickyKey,
}

impl PartialEq for ExpiryNode {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for ExpiryNode {}

impl PartialOrd for ExpiryNode {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for ExpiryNode {
    fn cmp(&self, other: &Self) -> Ordering {
        (self.expires_at, self.seq).cmp(&(other.expires_at, other.seq))
    }
}

/// The per-room map of sticky events.
///
/// Each key holds the winning event according to the MSC4354 tie-break: the
/// highest `origin_server_ts + duration_ms` wins, then the highest event ID.
/// A removal (an event whose content only carries the sticky key) is kept as a
/// *tombstone* rather than deleting the key, so that a superseded event that
/// arrives late (which MSC4354 explicitly allows) doesn't resurrect the value.
/// Tombstones are invisible to readers and disappear when they expire.
///
/// Expiry is evaluated against a `now` passed in by the caller, so the map has
/// no notion of time of its own and is trivially testable.
#[derive(Debug, Default)]
pub(crate) struct EphemeralMap {
    entries: HashMap<StickyKey, Entry>,
    /// Min-heap over the expiry of every insertion. Replaced entries leave a
    /// stale node behind, recognised by its `seq` and skipped on pop.
    expiry_heap: BinaryHeap<Reverse<ExpiryNode>>,
    next_seq: u64,
}

impl EphemeralMap {
    /// Apply a batch of candidates, returning what visibly changed.
    pub fn apply(
        &mut self,
        now: u64,
        candidates: impl IntoIterator<Item = Candidate>,
    ) -> StickyEventsUpdate {
        let mut update = StickyEventsUpdate::default();

        for candidate in candidates {
            match self.insert(now, candidate) {
                Some(Change::Added(event)) => update.added.push(event),
                Some(Change::Updated(event)) => update.updated.push(event),
                Some(Change::Removed(key)) => update.removed.push((key, RemovalReason::Replaced)),
                None => {}
            }
        }

        update
    }

    /// Insert a single candidate, returning the visible change, if any.
    fn insert(&mut self, now: u64, candidate: Candidate) -> Option<Change> {
        if candidate.expires_at <= now {
            return None;
        }

        let Candidate { key, event_id, order_ts, expires_at, is_tombstone, kind } = candidate;

        let was_live = match self.entries.get(&key) {
            Some(current) => {
                // The very same event, delivered again (e.g. both in the
                // timeline and in the sticky section, or after the connection
                // restarted).
                if current.event_id == event_id {
                    return None;
                }

                // Last to expire wins, then the highest event ID.
                if (order_ts, &event_id) <= current.rank() {
                    return None;
                }

                current.is_live(now)
            }

            None => {
                if self.entries.len() >= MAX_ENTRIES {
                    // Make room by dropping what has expired before giving up.
                    self.evict_expired(now);

                    if self.entries.len() >= MAX_ENTRIES {
                        warn!(
                            ?key,
                            %event_id,
                            "Dropping a sticky event: the room holds too many sticky events already"
                        );
                        return None;
                    }
                }

                false
            }
        };

        let seq = self.next_seq;
        self.next_seq += 1;

        self.entries
            .insert(key.clone(), Entry { event_id, order_ts, expires_at, is_tombstone, kind, seq });
        self.expiry_heap.push(Reverse(ExpiryNode { expires_at, seq, key: key.clone() }));

        match (was_live, is_tombstone) {
            (true, true) => Some(Change::Removed(key)),
            (true, false) => Some(Change::Updated(self.event(&key)?)),
            (false, false) => Some(Change::Added(self.event(&key)?)),
            // A tombstone replacing nothing visible: nothing to report.
            (false, true) => None,
        }
    }

    /// The live events at `now`, in no particular order.
    pub fn live(&self, now: u64) -> impl Iterator<Item = StickyEvent> + '_ {
        self.entries
            .iter()
            .filter(move |(_, entry)| entry.is_live(now))
            .map(|(key, entry)| Self::event_from_entry(key, entry))
    }

    /// The number of entries, including tombstones and not-yet-evicted expired
    /// ones.
    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// The earliest time an entry may need evicting, if any.
    pub fn next_expiry(&self) -> Option<u64> {
        self.expiry_heap.peek().map(|Reverse(node)| node.expires_at)
    }

    /// Drop every entry that has expired at `now`, returning the keys whose
    /// live value disappeared as a result (expired tombstones are not
    /// reported, they were never visible).
    pub fn evict_expired(&mut self, now: u64) -> Vec<StickyKey> {
        let mut removed = Vec::new();

        while let Some(Reverse(node)) = self.expiry_heap.peek() {
            if node.expires_at > now {
                break;
            }

            let Reverse(node) = self.expiry_heap.pop().expect("the heap was just peeked");

            // Skip stale nodes: the entry has since been replaced by a newer
            // insertion, which has its own node.
            let is_current = self.entries.get(&node.key).is_some_and(|entry| entry.seq == node.seq);

            if is_current
                && let Some(entry) = self.entries.remove(&node.key)
                && !entry.is_tombstone
            {
                removed.push(node.key);
            }
        }

        removed
    }

    /// Drop every entry, returning the keys whose live value disappeared.
    pub fn clear(&mut self, now: u64) -> Vec<StickyKey> {
        self.expiry_heap.clear();
        self.next_seq = 0;

        self.entries.drain().filter(|(_, entry)| entry.is_live(now)).map(|(key, _)| key).collect()
    }

    fn event(&self, key: &StickyKey) -> Option<StickyEvent> {
        self.entries.get(key).map(|entry| Self::event_from_entry(key, entry))
    }

    fn event_from_entry(key: &StickyKey, entry: &Entry) -> StickyEvent {
        StickyEvent {
            key: key.clone(),
            event_id: entry.event_id.clone(),
            kind: entry.kind.clone(),
            expires_at: MilliSecondsSinceUnixEpoch(UInt::new_saturating(entry.expires_at)),
        }
    }
}

/// A single visible change to the map.
enum Change {
    Added(StickyEvent),
    Updated(StickyEvent),
    Removed(StickyKey),
}

#[cfg(test)]
mod tests {
    use assert_matches2::assert_matches;
    use matrix_sdk_common::deserialized_responses::TimelineEventKind;
    use ruma::{
        EventId, MilliSecondsSinceUnixEpoch, events::TimelineEventType, owned_event_id,
        owned_user_id, uint,
    };
    use serde_json::json;

    use super::{Candidate, EphemeralMap, MAX_ENTRIES};
    use crate::sticky::{RemovalReason, StickyKey};

    fn key(sticky_key: &str) -> StickyKey {
        StickyKey {
            sender: owned_user_id!("@alice:localhost"),
            event_type: TimelineEventType::from("m.rtc.member"),
            sticky_key: sticky_key.to_owned(),
        }
    }

    fn kind() -> TimelineEventKind {
        TimelineEventKind::PlainText {
            event: serde_json::from_value(json!({
                "type": "m.rtc.member",
                "sender": "@alice:localhost",
                "event_id": "$event:localhost",
                "origin_server_ts": 1,
                "content": {},
            }))
            .unwrap(),
        }
    }

    fn candidate(key: StickyKey, event_id: &EventId, order_ts: u64, expires_at: u64) -> Candidate {
        Candidate {
            key,
            event_id: event_id.to_owned(),
            order_ts,
            expires_at,
            is_tombstone: false,
            kind: kind(),
        }
    }

    fn tombstone(key: StickyKey, event_id: &EventId, order_ts: u64, expires_at: u64) -> Candidate {
        Candidate { is_tombstone: true, ..candidate(key, event_id, order_ts, expires_at) }
    }

    #[test]
    fn test_added_entries_are_live_until_they_expire() {
        let mut map = EphemeralMap::default();

        let update =
            map.apply(0, [candidate(key("slot"), &owned_event_id!("$a:localhost"), 100, 100)]);
        assert_eq!(update.added.len(), 1);
        assert_eq!(update.added[0].key, key("slot"));
        assert_eq!(update.added[0].expires_at, MilliSecondsSinceUnixEpoch(uint!(100)));

        assert_eq!(map.live(99).count(), 1);
        assert_eq!(map.live(100).count(), 0, "an entry is no longer live at its expiry");
    }

    #[test]
    fn test_already_expired_candidates_are_ignored() {
        let mut map = EphemeralMap::default();

        let update =
            map.apply(100, [candidate(key("slot"), &owned_event_id!("$a:localhost"), 100, 100)]);
        assert!(update.is_empty());
        assert_eq!(map.len(), 0);
    }

    #[test]
    fn test_last_to_expire_wins() {
        let mut map = EphemeralMap::default();
        let k = key("slot");

        map.apply(0, [candidate(k.clone(), &owned_event_id!("$a:localhost"), 100, 1000)]);

        // A later `origin_server_ts + duration_ms` wins…
        let update =
            map.apply(0, [candidate(k.clone(), &owned_event_id!("$b:localhost"), 200, 1000)]);
        assert_eq!(update.updated.len(), 1);
        assert_eq!(update.updated[0].event_id, "$b:localhost");

        // …and an earlier one loses, whatever its local expiry is.
        let update = map.apply(0, [candidate(k, &owned_event_id!("$c:localhost"), 150, 5000)]);
        assert!(update.is_empty());

        let live: Vec<_> = map.live(0).collect();
        assert_eq!(live.len(), 1);
        assert_eq!(live[0].event_id, "$b:localhost");
    }

    #[test]
    fn test_ties_are_broken_by_the_highest_event_id() {
        let mut map = EphemeralMap::default();
        let k = key("slot");

        map.apply(0, [candidate(k.clone(), &owned_event_id!("$b:localhost"), 100, 1000)]);

        assert!(
            map.apply(0, [candidate(k.clone(), &owned_event_id!("$a:localhost"), 100, 1000)])
                .is_empty()
        );
        assert_eq!(
            map.apply(0, [candidate(k, &owned_event_id!("$c:localhost"), 100, 1000)]).updated.len(),
            1
        );
    }

    #[test]
    fn test_redelivering_the_same_event_is_a_no_op() {
        let mut map = EphemeralMap::default();
        let k = key("slot");

        map.apply(0, [candidate(k.clone(), &owned_event_id!("$a:localhost"), 100, 1000)]);

        // The same event, received again a bit later, with a slightly different
        // local expiry: neither an update nor a change of expiry.
        let update = map.apply(0, [candidate(k, &owned_event_id!("$a:localhost"), 100, 1010)]);
        assert!(update.is_empty());

        let live: Vec<_> = map.live(0).collect();
        assert_eq!(live[0].expires_at, MilliSecondsSinceUnixEpoch(uint!(1000)));
    }

    #[test]
    fn test_tombstone_removes_the_live_value() {
        let mut map = EphemeralMap::default();
        let k = key("slot");

        map.apply(0, [candidate(k.clone(), &owned_event_id!("$a:localhost"), 100, 1000)]);

        let update =
            map.apply(0, [tombstone(k.clone(), &owned_event_id!("$b:localhost"), 200, 1000)]);
        assert_eq!(update.removed, vec![(k, RemovalReason::Replaced)]);
        assert_eq!(map.live(0).count(), 0);

        // The tombstone stays in the map until it expires, though.
        assert_eq!(map.len(), 1);
    }

    #[test]
    fn test_late_superseded_event_does_not_resurrect_a_removed_key() {
        let mut map = EphemeralMap::default();
        let k = key("slot");

        // The removal arrives first…
        let update =
            map.apply(0, [tombstone(k.clone(), &owned_event_id!("$b:localhost"), 200, 1000)]);
        assert!(update.is_empty(), "removing nothing visible reports nothing");

        // …then the event it superseded arrives late: it must lose.
        let update =
            map.apply(0, [candidate(k.clone(), &owned_event_id!("$a:localhost"), 100, 1000)]);
        assert!(update.is_empty());
        assert_eq!(map.live(0).count(), 0);

        // A genuinely newer event does win against the tombstone, and shows up
        // as an addition since nothing was visible before.
        let update = map.apply(0, [candidate(k, &owned_event_id!("$c:localhost"), 300, 1000)]);
        assert_eq!(update.added.len(), 1);
        assert_eq!(map.live(0).count(), 1);
    }

    #[test]
    fn test_stale_tombstone_does_not_remove_a_newer_value() {
        let mut map = EphemeralMap::default();
        let k = key("slot");

        map.apply(0, [candidate(k.clone(), &owned_event_id!("$b:localhost"), 200, 1000)]);

        let update = map.apply(0, [tombstone(k, &owned_event_id!("$a:localhost"), 100, 1000)]);
        assert!(update.is_empty());
        assert_eq!(map.live(0).count(), 1);
    }

    #[test]
    fn test_evict_expired_reports_live_values_only() {
        let mut map = EphemeralMap::default();

        map.apply(
            0,
            [
                candidate(key("1"), &owned_event_id!("$a:localhost"), 100, 100),
                candidate(key("2"), &owned_event_id!("$b:localhost"), 200, 200),
                tombstone(key("3"), &owned_event_id!("$c:localhost"), 200, 200),
                candidate(key("4"), &owned_event_id!("$d:localhost"), 300, 300),
            ],
        );

        let mut removed = map.evict_expired(250);
        removed.sort_by(|a, b| a.sticky_key.cmp(&b.sticky_key));
        assert_eq!(removed, vec![key("1"), key("2")]);

        assert_eq!(map.len(), 1);
        assert_eq!(map.next_expiry(), Some(300));
    }

    #[test]
    fn test_evict_expired_skips_stale_heap_nodes() {
        let mut map = EphemeralMap::default();
        let k = key("slot");

        // A short-lived entry, then a replacement that lives much longer.
        map.apply(0, [candidate(k.clone(), &owned_event_id!("$a:localhost"), 100, 100)]);
        map.apply(0, [candidate(k, &owned_event_id!("$b:localhost"), 500, 500)]);

        // The node of the first insertion is stale and must not evict the
        // replacement.
        assert!(map.evict_expired(150).is_empty());
        assert_eq!(map.live(150).count(), 1);
        assert_eq!(map.next_expiry(), Some(500));
    }

    #[test]
    fn test_clear_reports_live_values_only() {
        let mut map = EphemeralMap::default();

        map.apply(
            0,
            [
                candidate(key("1"), &owned_event_id!("$a:localhost"), 100, 100),
                tombstone(key("2"), &owned_event_id!("$b:localhost"), 100, 100),
            ],
        );

        assert_eq!(map.clear(0), vec![key("1")]);
        assert_eq!(map.len(), 0);
        assert_eq!(map.next_expiry(), None);
    }

    #[test]
    fn test_new_keys_are_dropped_beyond_the_cap_but_updates_still_apply() {
        let mut map = EphemeralMap::default();

        map.apply(
            0,
            (0..MAX_ENTRIES).map(|i| {
                candidate(key(&i.to_string()), &owned_event_id!("$a:localhost"), 100, 1000)
            }),
        );
        assert_eq!(map.len(), MAX_ENTRIES);

        // One more key is dropped…
        let update =
            map.apply(0, [candidate(key("extra"), &owned_event_id!("$a:localhost"), 100, 1000)]);
        assert!(update.is_empty());
        assert_eq!(map.len(), MAX_ENTRIES);

        // …but an update to an existing key goes through.
        let update =
            map.apply(0, [candidate(key("0"), &owned_event_id!("$b:localhost"), 200, 1000)]);
        assert_matches!(update.updated.as_slice(), [event]);
        assert_eq!(event.event_id, "$b:localhost");

        // And once entries expire, new keys are accepted again.
        let update = map
            .apply(1000, [candidate(key("extra"), &owned_event_id!("$c:localhost"), 2000, 2000)]);
        assert_eq!(update.added.len(), 1);
    }
}
