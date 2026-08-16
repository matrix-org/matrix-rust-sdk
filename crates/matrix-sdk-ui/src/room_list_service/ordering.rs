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

//! Ordering of the room list.
//!
//! This module orders the room list with the following total order:
//!
//! 1. Rooms whose latest event is an unsent local event come first.
//! 2. Rooms are then ordered by recency, most recent first:
//!    - a room with a computed latest event is ordered by that event's
//!      timestamp;
//!    - a room without a computed latest event is slotted in by its bump stamp
//!      (`RoomInfo::recency_stamp`, the server-side `bump_stamp` of MSC4186),
//!      relative to the bump stamps of the other rooms. Concretely, it borrows
//!      the smallest timestamp among the _anchors_ (rooms having both a
//!      timestamp and a bump stamp) whose bump stamp is at or above its own,
//!      and is then sub-ordered by its own bump stamp. This guarantees that a
//!      room without a preview never ranks above a room the server considers
//!      more recent that has one: a single anchor with an inconsistent (fresh
//!      timestamp, stale bump stamp) pair - a just-sent local message before
//!      its sync echo, say - cannot drag its bump-stamp neighbours to the top
//!      of the list. A room whose bump stamp is above every anchor's borrows
//!      the top anchor's timestamp and sits just above it, so a genuinely new
//!      event in a room with no computable preview still surfaces;
//!    - before any latest event has been computed (e.g. at startup), this
//!      degenerates into a pure bump-stamp order, which is the server's view of
//!      recency, so the initial order is immediately meaningful.
//! 3. Ties are broken by display name, then by room ID, so the order is fully
//!    deterministic.
//!
//! Why is this not implemented as a [`Sorter`](super::sorters::Sorter)? A
//! pairwise comparison cannot implement the slotting rule as a total order:
//! comparing a timestamp-less room with a timestamp-ful room must go through
//! bump stamps, and whenever the timestamp order and the bump-stamp order
//! disagree, transitivity breaks (`sort_by` requires a total order and is
//! allowed to panic otherwise). The slotting rule needs a global view of all
//! rooms, so this module computes the order for the whole list at once, once
//! per incoming batch of updates.
//!
//! Working batch-at-a-time has a second benefit: the ordering is updated
//! _atomically_. All the position changes caused by one batch of updates (for
//! example a set of freshly computed latest events) are emitted as a single
//! batch of [`VectorDiff`]s, computed as a minimal set of moves (via a longest
//! increasing subsequence), instead of a trickle of individual moves. This
//! avoids the room list visibly churning while previews are computed.

use std::{
    cmp::Ordering,
    collections::{HashMap, HashSet},
};

use async_stream::stream;
use eyeball_im::{Vector, VectorDiff};
use futures_util::{Stream, StreamExt as _, pin_mut};
use ruma::{OwnedRoomId, RoomId};

use super::RoomListItem;

/// Order the given room list by recency. See the module documentation for the
/// exact semantics of the order.
///
/// This is a stream adapter: it consumes a [`Vector`] of initial values plus a
/// stream of batches of [`VectorDiff`]s, and produces the sorted initial
/// values plus a stream of batches of [`VectorDiff`]s applying to the sorted
/// list.
pub(super) fn sorted_by_recency<S>(
    initial_values: Vector<RoomListItem>,
    input_stream: S,
) -> (Vector<RoomListItem>, impl Stream<Item = Vec<VectorDiff<RoomListItem>>>)
where
    S: Stream<Item = Vec<VectorDiff<RoomListItem>>>,
{
    let mut state = OrderingState::new(initial_values);
    let sorted_values = state.sorted_values();

    let stream = stream! {
        pin_mut!(input_stream);

        while let Some(batch) = input_stream.next().await {
            let output = state.process(batch);

            if !output.is_empty() {
                yield output;
            }
        }
    };

    (sorted_values, stream)
}

/// The recency score of a room: primarily the timestamp of the latest event,
/// possibly borrowed from the nearest room by bump stamp, then the room's own
/// bump stamp as a sub-order. Compared in reverse (largest, i.e. most recent,
/// first).
type Recency = (u64, u64);

/// The inputs from which a room's position in the order is computed. This is
/// kept per room to detect whether an update can possibly change the order
/// (see [`OrderingState::process`]'s fast path).
#[derive(Clone, PartialEq, Eq)]
struct KeyInputs {
    unsent: bool,
    timestamp: Option<u64>,
    bump_stamp: Option<u64>,
    name: Option<String>,
}

impl KeyInputs {
    fn of(room: &RoomListItem) -> Self {
        Self {
            unsent: room.cached_latest_event_is_unsent,
            timestamp: room.cached_latest_event_timestamp.map(|ts| ts.get().into()),
            bump_stamp: room.cached_recency_stamp.map(Into::into),
            name: room.cached_display_name.clone(),
        }
    }
}

/// Compute the [`Recency`] of every room in `items`.
///
/// Rooms with a latest event timestamp use it directly. Rooms without one
/// borrow a timestamp from the _anchors_ (rooms having both a timestamp and a
/// bump stamp) via [`borrowed_timestamp`], and are sub-ordered by their own
/// bump stamp. Rooms with neither a timestamp nor a bump stamp have no
/// recency and sort last.
fn compute_recencies(items: &[&RoomListItem]) -> Vec<Option<Recency>> {
    // The anchors, sorted by bump stamp, with each timestamp replaced by the
    // minimum timestamp of all anchors at or above that bump stamp (a suffix
    // minimum). This makes the bump-stamp -> borrowed-timestamp mapping
    // monotone, so one anchor with an inconsistently fresh timestamp cannot
    // promote its neighbours.
    let mut anchors: Vec<(u64, u64)> = items
        .iter()
        .filter_map(|room| match (room.cached_latest_event_timestamp, room.cached_recency_stamp) {
            (Some(timestamp), Some(bump_stamp)) => {
                Some((bump_stamp.into(), timestamp.get().into()))
            }
            _ => None,
        })
        .collect();
    anchors.sort_unstable();

    let top_anchor_timestamp = anchors.last().map(|&(_, timestamp)| timestamp);

    let mut suffix_min = u64::MAX;
    for (_, timestamp) in anchors.iter_mut().rev() {
        suffix_min = suffix_min.min(*timestamp);
        *timestamp = suffix_min;
    }

    items
        .iter()
        .map(|room| {
            let timestamp = room.cached_latest_event_timestamp.map(|ts| u64::from(ts.get()));
            let bump_stamp = room.cached_recency_stamp.map(u64::from);

            match (timestamp, bump_stamp) {
                (Some(timestamp), Some(bump_stamp)) => Some((timestamp, bump_stamp)),
                // No bump stamp: sub-order below rooms with the same timestamp.
                (Some(timestamp), None) => Some((timestamp, 0)),
                (None, Some(bump_stamp)) => Some((
                    borrowed_timestamp(&anchors, top_anchor_timestamp, bump_stamp),
                    bump_stamp,
                )),
                (None, None) => None,
            }
        })
        .collect()
}

/// The timestamp a room without a latest event borrows, given its bump stamp:
/// the smallest timestamp among the anchors whose bump stamp is at or above
/// `bump_stamp` (`anchors` already carries suffix minima, so this is one
/// lookup). The result can only be exceeded by out-ranking every one of those
/// anchors' own timestamps, so a previewless room never sorts above a room
/// the server considers more recent that has a visible message.
///
/// A bump stamp above every anchor's borrows the top anchor's timestamp (and
/// the caller's bump-stamp sub-order places it just above that anchor). No
/// anchors at all returns 0 (the oldest possible timestamp), degenerating
/// into a pure bump-stamp order.
fn borrowed_timestamp(
    anchors: &[(u64, u64)],
    top_anchor_timestamp: Option<u64>,
    bump_stamp: u64,
) -> u64 {
    let index = anchors.partition_point(|&(anchor_bump_stamp, _)| anchor_bump_stamp < bump_stamp);

    match anchors.get(index) {
        Some(&(_, suffix_min_timestamp)) => suffix_min_timestamp,
        None => top_anchor_timestamp.unwrap_or(0),
    }
}

/// Compare two rooms given their pre-computed [`Recency`]. This is a total
/// order (the final room ID comparison guarantees antisymmetry).
fn cmp_rooms(
    left: &RoomListItem,
    left_recency: Option<Recency>,
    right: &RoomListItem,
    right_recency: Option<Recency>,
) -> Ordering {
    // Rooms with an unsent local latest event come first.
    match (left.cached_latest_event_is_unsent, right.cached_latest_event_is_unsent) {
        (true, false) => return Ordering::Less,
        (false, true) => return Ordering::Greater,
        _ => {}
    }

    // Most recent first; a room with a recency comes before a room without.
    match (left_recency, right_recency) {
        (Some(left_recency), Some(right_recency)) => {
            match left_recency.cmp(&right_recency).reverse() {
                Ordering::Equal => {}
                ordering => return ordering,
            }
        }
        (Some(_), None) => return Ordering::Less,
        (None, Some(_)) => return Ordering::Greater,
        (None, None) => {}
    }

    // Tie-break by name, then by room ID for full determinism.
    left.cached_display_name
        .cmp(&right.cached_display_name)
        .then_with(|| left.room_id().cmp(right.room_id()))
}

/// Compute the sorted order of `mirror` as a list of room IDs.
fn sort_rooms(mirror: &Vector<RoomListItem>) -> Vec<OwnedRoomId> {
    let items: Vec<&RoomListItem> = mirror.iter().collect();
    let recencies = compute_recencies(&items);

    let mut indices: Vec<usize> = (0..items.len()).collect();
    indices.sort_unstable_by(|&left, &right| {
        cmp_rooms(items[left], recencies[left], items[right], recencies[right])
    });

    indices.into_iter().map(|index| items[index].room_id().to_owned()).collect()
}

/// Compute the minimal set of moves transforming the `old` order into the
/// `new` order, via a longest increasing subsequence: the rooms on the LIS
/// keep their relative order and don't move at all.
///
/// Returns `(kept, removes, inserts)` where `kept` is the set of rooms that
/// do not move, `removes` is the list of `old` indices to remove in
/// descending order, and `inserts` is the list of `new` indices to insert in
/// ascending order. Applying the removes then the inserts (with values from
/// `new`) transforms `old` into `new`.
fn diff_orders(
    old: &[OwnedRoomId],
    new: &[OwnedRoomId],
) -> (HashSet<OwnedRoomId>, Vec<usize>, Vec<usize>) {
    let old_positions: HashMap<&RoomId, usize> =
        old.iter().enumerate().map(|(index, room_id)| (room_id.as_ref(), index)).collect();

    // For each room in the new order, its position in the old order (if any).
    let sequence: Vec<Option<usize>> =
        new.iter().map(|room_id| old_positions.get(&**room_id).copied()).collect();

    // Longest increasing subsequence over `sequence` (patience sorting).
    // `tails[length - 1]` is the index into `new` of the smallest possible
    // tail of an increasing subsequence of that length.
    let mut tails: Vec<usize> = Vec::new();
    let mut parents: Vec<Option<usize>> = vec![None; new.len()];

    for (new_index, maybe_old_position) in sequence.iter().enumerate() {
        let Some(old_position) = maybe_old_position else { continue };

        let length = tails.partition_point(|&tail| sequence[tail].unwrap() < *old_position);

        if length == tails.len() {
            tails.push(new_index);
        } else {
            tails[length] = new_index;
        }

        parents[new_index] = (length > 0).then(|| tails[length - 1]);
    }

    // Reconstruct the LIS, i.e. the set of rooms that don't move.
    let mut kept_new_indices = HashSet::new();
    let mut cursor = tails.last().copied();

    while let Some(new_index) = cursor {
        kept_new_indices.insert(new_index);
        cursor = parents[new_index];
    }

    let kept: HashSet<OwnedRoomId> =
        kept_new_indices.iter().map(|&new_index| new[new_index].clone()).collect();

    let removes = (0..old.len()).rev().filter(|&index| !kept.contains(&old[index])).collect();
    let inserts = (0..new.len()).filter(|&index| !kept_new_indices.contains(&index)).collect();

    (kept, removes, inserts)
}

/// The state of the [`sorted_by_recency`] adapter.
struct OrderingState {
    /// The input values, in input (unsorted) order. Input diff indices apply
    /// to this.
    mirror: Vector<RoomListItem>,

    /// The current output order.
    sorted: Vec<OwnedRoomId>,

    /// The position of every room in `sorted`.
    positions: HashMap<OwnedRoomId, usize>,

    /// The last seen ordering inputs of every room, to detect updates that
    /// cannot change the order.
    key_inputs: HashMap<OwnedRoomId, KeyInputs>,
}

impl OrderingState {
    fn new(initial_values: Vector<RoomListItem>) -> Self {
        let mut this = Self {
            mirror: initial_values,
            sorted: Vec::new(),
            positions: HashMap::new(),
            key_inputs: HashMap::new(),
        };
        this.rebuild();

        this
    }

    /// Recompute `sorted`, `positions` and `key_inputs` from `mirror`.
    fn rebuild(&mut self) {
        self.sorted = sort_rooms(&self.mirror);
        self.positions = self
            .sorted
            .iter()
            .enumerate()
            .map(|(index, room_id)| (room_id.clone(), index))
            .collect();
        self.key_inputs = self
            .mirror
            .iter()
            .map(|room| (room.room_id().to_owned(), KeyInputs::of(room)))
            .collect();
    }

    /// The current values, in sorted order.
    fn sorted_values(&self) -> Vector<RoomListItem> {
        let by_id: HashMap<&RoomId, &RoomListItem> =
            self.mirror.iter().map(|room| (room.room_id(), room)).collect();

        self.sorted.iter().map(|room_id| by_id[&**room_id].clone()).collect()
    }

    /// Process one batch of input diffs, and return the batch of output diffs
    /// applying to the sorted order.
    fn process(&mut self, batch: Vec<VectorDiff<RoomListItem>>) -> Vec<VectorDiff<RoomListItem>> {
        // Scan the batch: collect the updated values, and classify the diffs.
        let mut touched: Vec<RoomListItem> = Vec::new();
        let mut structural = false;
        let mut wholesale = false;

        for diff in &batch {
            match diff {
                VectorDiff::Set { value, .. } => {
                    touched.push(value.clone());
                }

                VectorDiff::Insert { value, .. }
                | VectorDiff::PushBack { value }
                | VectorDiff::PushFront { value } => {
                    structural = true;
                    touched.push(value.clone());
                }

                VectorDiff::Append { values } => {
                    structural = true;
                    touched.extend(values.iter().cloned());
                }

                VectorDiff::Remove { .. }
                | VectorDiff::Truncate { .. }
                | VectorDiff::PopFront
                | VectorDiff::PopBack => {
                    structural = true;
                }

                VectorDiff::Reset { .. } | VectorDiff::Clear => {
                    wholesale = true;
                }
            }
        }

        // Apply the batch to the mirror.
        for diff in batch {
            diff.apply(&mut self.mirror);
        }

        // A `Reset` or `Clear` anywhere in the batch: rebuild everything and
        // emit a single wholesale diff.
        if wholesale {
            self.rebuild();

            return if self.mirror.is_empty() {
                vec![VectorDiff::Clear]
            } else {
                vec![VectorDiff::Reset { values: self.sorted_values() }]
            };
        }

        // Fast path: only `Set`s whose ordering inputs are unchanged. The
        // order cannot change; forward the value updates at their sorted
        // positions.
        let key_changed = touched
            .iter()
            .any(|value| self.key_inputs.get(value.room_id()) != Some(&KeyInputs::of(value)));

        if !structural && !key_changed {
            return touched
                .into_iter()
                .filter_map(|value| {
                    let index = *self.positions.get(value.room_id())?;
                    Some(VectorDiff::Set { index, value })
                })
                .collect();
        }

        // Full path: recompute the order and emit the minimal set of moves,
        // atomically in one batch.
        let new_sorted = sort_rooms(&self.mirror);
        let (kept, removes, inserts) = diff_orders(&self.sorted, &new_sorted);

        let by_id: HashMap<&RoomId, &RoomListItem> =
            self.mirror.iter().map(|room| (room.room_id(), room)).collect();

        let new_positions: HashMap<OwnedRoomId, usize> = new_sorted
            .iter()
            .enumerate()
            .map(|(index, room_id)| (room_id.clone(), index))
            .collect();

        let mut output = Vec::with_capacity(removes.len() + inserts.len());

        for index in removes {
            output.push(VectorDiff::Remove { index });
        }

        for index in inserts {
            let room_id: &RoomId = &new_sorted[index];
            output.push(VectorDiff::Insert { index, value: by_id[room_id].clone() });
        }

        // Value updates for rooms that did not move.
        for value in touched {
            if kept.contains(value.room_id()) {
                let index = new_positions[value.room_id()];
                output.push(VectorDiff::Set { index, value });
            }
        }

        // Update the state. `key_inputs` is refreshed for the rooms still
        // present; rooms that disappeared are dropped.
        self.sorted = new_sorted;
        self.key_inputs.retain(|room_id, _| new_positions.contains_key(room_id));
        for room_id in self.sorted.iter() {
            if let Some(room) = by_id.get(&**room_id) {
                self.key_inputs.insert(room_id.clone(), KeyInputs::of(room));
            }
        }
        self.positions = new_positions;

        output
    }
}

#[cfg(test)]
mod tests {
    use matrix_sdk::{
        RoomRecencyStamp,
        latest_events::{LatestEventValue, RemoteLatestEventValue},
        test_utils::mocks::MatrixMockServer,
    };
    use matrix_sdk_base::RoomInfoNotableUpdateReasons;
    use matrix_sdk_test::async_test;
    use ruma::{events::room::message::RoomMessageEventContent, room_id, serde::Raw};
    use serde_json::json;
    use stream_assert::assert_pending;
    use tokio_stream::wrappers::ReceiverStream;

    use super::{super::filters::new_rooms, *};

    fn remote(origin_server_ts: u64) -> LatestEventValue {
        LatestEventValue::Remote(RemoteLatestEventValue::from_plaintext(
            Raw::from_json_string(
                json!({
                    "content": RoomMessageEventContent::text_plain("raclette"),
                    "type": "m.room.message",
                    "event_id": "$ev0",
                    "room_id": "!r0",
                    "origin_server_ts": origin_server_ts,
                    "sender": "@mnt_io:matrix.org",
                })
                .to_string(),
            )
            .unwrap(),
        ))
    }

    async fn set_latest_event(room: &mut RoomListItem, value: LatestEventValue) {
        room.update_room_info(|mut info| {
            info.set_latest_event(value);
            (info, RoomInfoNotableUpdateReasons::LATEST_EVENT)
        })
        .await;
        room.refresh_cached_data();
    }

    async fn set_bump_stamp(room: &mut RoomListItem, stamp: RoomRecencyStamp) {
        room.update_room_info(|mut info| {
            info.update_recency_stamp(stamp);
            (info, RoomInfoNotableUpdateReasons::RECENCY_STAMP)
        })
        .await;
        room.refresh_cached_data();
    }

    #[test]
    fn test_borrowed_timestamp() {
        // No anchors: 0, i.e. pure bump-stamp order.
        assert_eq!(borrowed_timestamp(&[], None, 42), 0);

        // Consistent anchors (timestamps increase with bump stamps), already
        // carrying their suffix minima (which leave them unchanged here).
        let anchors = [(10, 100), (20, 200), (40, 400)];

        // At or below an anchor's bump stamp: that anchor's timestamp.
        assert_eq!(borrowed_timestamp(&anchors, Some(400), 20), 200);
        assert_eq!(borrowed_timestamp(&anchors, Some(400), 12), 200);
        assert_eq!(borrowed_timestamp(&anchors, Some(400), 5), 100);
        assert_eq!(borrowed_timestamp(&anchors, Some(400), 30), 400);
        // Above every anchor: the top anchor's timestamp.
        assert_eq!(borrowed_timestamp(&anchors, Some(400), 100), 400);

        // An inconsistent anchor (fresh timestamp, stale bump stamp at bump
        // 10): the suffix minima flatten it out for everything above it.
        let anchors = [(10, 200), (20, 200), (40, 400)];

        // Rooms at/below the poisoned anchor's bump stamp cannot borrow its
        // fresh timestamp beyond the smallest above them.
        assert_eq!(borrowed_timestamp(&anchors, Some(400), 5), 200);
        assert_eq!(borrowed_timestamp(&anchors, Some(400), 15), 200);
    }

    fn room_ids(ids: &[&str]) -> Vec<OwnedRoomId> {
        ids.iter().map(|id| RoomId::parse(id).unwrap()).collect()
    }

    #[test]
    fn test_diff_orders() {
        let old = room_ids(&["!a:x", "!b:x", "!c:x", "!d:x"]);

        // Identity: nothing moves.
        let (kept, removes, inserts) = diff_orders(&old, &old);
        assert_eq!(kept.len(), 4);
        assert!(removes.is_empty());
        assert!(inserts.is_empty());

        // One room moves to the front.
        let new = room_ids(&["!c:x", "!a:x", "!b:x", "!d:x"]);
        let (kept, removes, inserts) = diff_orders(&old, &new);
        assert_eq!(kept.len(), 3);
        assert_eq!(removes, vec![2]);
        assert_eq!(inserts, vec![0]);

        // A removal and an addition.
        let new = room_ids(&["!a:x", "!e:x", "!c:x", "!d:x"]);
        let (kept, removes, inserts) = diff_orders(&old, &new);
        assert_eq!(kept.len(), 3);
        assert_eq!(removes, vec![1]);
        assert_eq!(inserts, vec![1]);

        // Everything changes.
        let new = room_ids(&["!d:x", "!c:x", "!b:x", "!a:x"]);
        let (kept, _removes, _inserts) = diff_orders(&old, &new);
        // Only one room can keep its relative order in a full reversal.
        assert_eq!(kept.len(), 1);
    }

    /// Apply `diffs` to `vector` and check the result equals `expected`.
    fn apply_and_check(
        vector: &mut Vector<RoomListItem>,
        diffs: Vec<VectorDiff<RoomListItem>>,
        expected: &[&RoomId],
    ) {
        for diff in diffs {
            diff.apply(vector);
        }

        let order: Vec<&RoomId> = vector.iter().map(|room| room.room_id()).collect();
        assert_eq!(order, expected);
    }

    #[async_test]
    async fn test_bump_stamp_order_then_timestamps_slot_in() {
        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;
        let [mut room_a, mut room_b, mut room_c, mut room_d] = new_rooms(
            [room_id!("!a:x.y"), room_id!("!b:x.y"), room_id!("!c:x.y"), room_id!("!d:x.y")],
            &client,
            &server,
        )
        .await;

        // No latest events yet: pure bump-stamp order.
        set_bump_stamp(&mut room_a, 10.into()).await;
        set_bump_stamp(&mut room_b, 20.into()).await;
        set_bump_stamp(&mut room_c, 30.into()).await;
        set_bump_stamp(&mut room_d, 40.into()).await;

        let initial: Vector<RoomListItem> =
            [room_a.clone(), room_b.clone(), room_c.clone(), room_d.clone()].into_iter().collect();

        let (sender, receiver) = tokio::sync::mpsc::channel(8);
        let (values, stream) = sorted_by_recency(initial, ReceiverStream::new(receiver));
        pin_mut!(stream);

        // Initial order: by bump stamp, descending.
        let order: Vec<&RoomId> = values.iter().map(|room| room.room_id()).collect();
        assert_eq!(order, [room_d.room_id(), room_c.room_id(), room_b.room_id(), room_a.room_id()]);

        let mut current = values.clone();

        // Latest events are computed for `room_b` (ts 200) and `room_c`
        // (ts 300), and arrive in ONE batch: the reorder must be atomic.
        set_latest_event(&mut room_b, remote(200)).await;
        set_latest_event(&mut room_c, remote(300)).await;

        sender
            .send(vec![
                VectorDiff::Set { index: 1, value: room_b.clone() },
                VectorDiff::Set { index: 2, value: room_c.clone() },
            ])
            .await
            .unwrap();

        let diffs = stream.next().await.unwrap();

        // `room_d` (bump 40, no ts) anchors to `room_c` (bump 30, nearest) and
        // borrows ts 300; its bump 40 > 30 puts it above `room_c`. `room_a`
        // (bump 10, no ts) anchors to `room_b` (bump 20) and sits below it.
        // Expected order: d, c, b, a, i.e. unchanged: zero moves, two Sets.
        assert!(diffs.iter().all(|diff| matches!(diff, VectorDiff::Set { .. })));
        apply_and_check(
            &mut current,
            diffs,
            &[room_d.room_id(), room_c.room_id(), room_b.room_id(), room_a.room_id()],
        );

        // Now `room_a` gets a latest event NEWER than everything (ts 400): it
        // must move to the front in one atomic batch.
        set_latest_event(&mut room_a, remote(400)).await;

        sender.send(vec![VectorDiff::Set { index: 0, value: room_a.clone() }]).await.unwrap();

        let diffs = stream.next().await.unwrap();
        apply_and_check(
            &mut current,
            diffs,
            &[room_a.room_id(), room_d.room_id(), room_c.room_id(), room_b.room_id()],
        );

        assert_pending!(stream);
    }

    /// Reproduces the "poisoned anchor" failure mode seen when dogfooding on a
    /// real account: one room whose latest-event timestamp is fresh but whose
    /// bump stamp is stale (e.g. a just-sent local message before the sync
    /// echo, or any bump/timestamp inconsistency) becomes the nearest anchor
    /// for every timestamp-less room in its bump neighbourhood. They all
    /// borrow its fresh timestamp and pile up at the top of the list with
    /// blank previews, above genuinely recent rooms.
    #[async_test]
    async fn test_inconsistent_anchor_promotes_previewless_rooms() {
        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;
        let [mut poisoned, mut recent, mut old_a, mut old_b] = new_rooms(
            [room_id!("!p:x.y"), room_id!("!n:x.y"), room_id!("!olda:x.y"), room_id!("!oldb:x.y")],
            &client,
            &server,
        )
        .await;

        // A genuinely recent room: high bump stamp, fresh visible message.
        set_bump_stamp(&mut recent, 100.into()).await;
        set_latest_event(&mut recent, remote(500)).await;

        // The poisoned anchor: stale bump stamp but a fresher timestamp.
        set_bump_stamp(&mut poisoned, 5.into()).await;
        set_latest_event(&mut poisoned, remote(1000)).await;

        // Two ancient rooms with no computable latest event (blank preview).
        set_bump_stamp(&mut old_a, 3.into()).await;
        set_bump_stamp(&mut old_b, 4.into()).await;

        let initial: Vector<RoomListItem> =
            [poisoned.clone(), recent.clone(), old_a.clone(), old_b.clone()].into_iter().collect();

        let (values, _stream) = sorted_by_recency(initial, futures_util::stream::pending());

        let order: Vec<&RoomId> = values.iter().map(|room| room.room_id()).collect();

        // What the user should see: the room with the fresh visible message
        // first (or at least the blank ancient rooms nowhere near the top).
        // What the poisoned anchor produces instead is
        // [poisoned, old_b, old_a, recent]: both ancient blank-preview rooms
        // outrank the genuinely recent room.
        assert_eq!(
            order,
            [poisoned.room_id(), recent.room_id(), old_b.room_id(), old_a.room_id()],
            "ancient previewless rooms must not outrank a room with a fresh visible message"
        );
    }

    #[async_test]
    async fn test_value_only_update_does_not_reorder() {
        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;
        let [mut room_a, mut room_b] =
            new_rooms([room_id!("!a:x.y"), room_id!("!b:x.y")], &client, &server).await;

        set_bump_stamp(&mut room_a, 1.into()).await;
        set_bump_stamp(&mut room_b, 2.into()).await;

        let initial: Vector<RoomListItem> = [room_a.clone(), room_b.clone()].into_iter().collect();

        let (sender, receiver) = tokio::sync::mpsc::channel(8);
        let (values, stream) = sorted_by_recency(initial, ReceiverStream::new(receiver));
        pin_mut!(stream);

        let order: Vec<&RoomId> = values.iter().map(|room| room.room_id()).collect();
        assert_eq!(order, [room_b.room_id(), room_a.room_id()]);

        // A value update with unchanged ordering inputs: a single `Set` at the
        // sorted position, no moves.
        sender.send(vec![VectorDiff::Set { index: 0, value: room_a.clone() }]).await.unwrap();

        let diffs = stream.next().await.unwrap();
        assert_eq!(diffs.len(), 1);
        assert!(matches!(&diffs[0], VectorDiff::Set { index: 1, .. }));

        assert_pending!(stream);
    }

    #[async_test]
    async fn test_reset_is_forwarded_as_sorted_reset() {
        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;
        let [mut room_a, mut room_b] =
            new_rooms([room_id!("!a:x.y"), room_id!("!b:x.y")], &client, &server).await;

        set_bump_stamp(&mut room_a, 1.into()).await;
        set_bump_stamp(&mut room_b, 2.into()).await;

        let (sender, receiver) = tokio::sync::mpsc::channel(8);
        let (_values, stream) = sorted_by_recency(Vector::new(), ReceiverStream::new(receiver));
        pin_mut!(stream);

        sender
            .send(vec![VectorDiff::Reset {
                values: [room_a.clone(), room_b.clone()].into_iter().collect(),
            }])
            .await
            .unwrap();

        let diffs = stream.next().await.unwrap();
        assert_eq!(diffs.len(), 1);
        let VectorDiff::Reset { values } = &diffs[0] else {
            panic!("expected a Reset, got {diffs:?}");
        };
        let order: Vec<&RoomId> = values.iter().map(|room| room.room_id()).collect();
        assert_eq!(order, [room_b.room_id(), room_a.room_id()]);

        assert_pending!(stream);
    }
}
