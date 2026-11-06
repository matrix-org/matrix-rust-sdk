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
//! # The order
//!
//! 1. Rooms whose latest event is an unsent local event come first.
//! 2. Then by recency, most recent first:
//!    - a room with a latest event is ordered by its timestamp;
//!    - a room without one is slotted in by its bump stamp: it borrows the
//!      smallest timestamp among the _anchors_ (rooms with both a timestamp and
//!      a bump stamp) whose bump stamp is at or above its own. With no anchors,
//!      this is a pure bump-stamp order.
//! 3. Ties are broken by display name, then by room ID.
//!
//! # Why bump stamps?
//!
//! Two signals describe how recent a room is:
//!
//! - the timestamp of its latest event, which is what the room list shows. It
//!   is computed lazily by the client, so many rooms don't have one yet (e.g.
//!   right after login), or ever (e.g. their newest events aren't displayable);
//! - its bump stamp (`RoomInfo::recency_stamp`, the MSC4186 `bump_stamp`), the
//!   server's recency counter. Nearly every room has one, but it isn't a time,
//!   so it can't be compared with a timestamp.
//!
//! The timestamp wins whenever a room has one. A room without one is placed
//! relative to the anchors, the rooms having both, by comparing bump stamps:
//! it borrows an anchor's timestamp, then sorts by its own bump stamp.
//!
//! ```text
//! room  bump stamp  timestamp     recency (timestamp, bump stamp)
//! D     40          -         ->  (300, 40)  above every anchor: borrows the top one's
//! A     30          300       ->  (300, 30)  anchor
//! P     25          -         ->  (300, 25)  first anchor at or above 25 is A
//! B     20          200       ->  (200, 20)  anchor
//! C     10          -         ->  (200, 10)  first anchor at or above 10 is B
//! ```
//!
//! Sorted by recency, this gives D, A, P, B, C: the server's bump-stamp order,
//! with the timestamps of the anchors respected.
//!
//! # Why the _smallest_ timestamp?
//!
//! Bump stamps and timestamps don't always agree. A just-sent message, before
//! its sync echo, gives its room a fresh timestamp but an old bump stamp:
//!
//! ```text
//! room  bump stamp  timestamp
//! R     100         500        recent room
//! S     5           1000       fresh timestamp, stale bump stamp
//! O     4           -          old room without a preview
//! ```
//!
//! If O borrowed from the nearest anchor, S, it would get 1000 and jump above
//! R. Borrowing the smallest timestamp among all the anchors at or above its
//! bump stamp (S and R) gives it 500 instead, so it stays below R: a room
//! without a preview never outranks a room the server considers more recent.
//!
//! # Updates
//!
//! This is not a [`Sorter`](super::sorters::Sorter): placing a room without a
//! timestamp depends on all the anchors, so it can't be decided by comparing
//! two rooms alone. Instead, [`OrderingState::process`] handles each batch of
//! input diffs as a whole:
//!
//! - a `Reset` or `Clear` re-sorts everything and emits a single `Reset` or
//!   `Clear`;
//! - `Set`s that don't change any sort input (see [`SortKey`]) can't move a
//!   room, and are forwarded at the rooms' sorted positions;
//! - anything else re-sorts the whole list, and emits the minimal set of moves
//!   from the old order to the new one (see [`diff_orders`]), so all the
//!   reorders caused by one batch are applied at once.

use std::{
    cmp::Ordering,
    collections::{HashMap, HashSet},
};

use async_stream::stream;
use eyeball_im::{Vector, VectorDiff};
use futures_util::{Stream, StreamExt as _, pin_mut};
use ruma::{OwnedRoomId, RoomId};

use super::RoomListItem;

/// Sort `initial_rooms` and `input_stream` by recency, see the module
/// documentation.
pub(super) fn sorted_by_recency<S>(
    initial_rooms: Vector<RoomListItem>,
    input_stream: S,
) -> (Vector<RoomListItem>, impl Stream<Item = Vec<VectorDiff<RoomListItem>>>)
where
    S: Stream<Item = Vec<VectorDiff<RoomListItem>>>,
{
    let mut state = OrderingState::new(initial_rooms);
    let sorted_rooms = state.sorted_rooms();

    let stream = stream! {
        pin_mut!(input_stream);

        while let Some(batch) = input_stream.next().await {
            let output = state.process(batch);

            if !output.is_empty() {
                yield output;
            }
        }
    };

    (sorted_rooms, stream)
}

/// The state of the [`sorted_by_recency`] adapter.
struct OrderingState {
    /// The input values, in input order, kept in sync with the input diffs.
    unsorted_rooms: Vector<RoomListItem>,

    /// The current output order.
    sorted_room_ids: Vec<OwnedRoomId>,

    /// The position of every room in `sorted_room_ids`.
    positions: HashMap<OwnedRoomId, usize>,

    /// The last seen sort key of every room, to detect updates that cannot
    /// change the order.
    sort_keys: HashMap<OwnedRoomId, SortKey>,
}

impl OrderingState {
    fn new(initial_rooms: Vector<RoomListItem>) -> Self {
        let mut this = Self {
            unsorted_rooms: initial_rooms,
            sorted_room_ids: Vec::new(),
            positions: HashMap::new(),
            sort_keys: HashMap::new(),
        };
        this.set_order(sort_rooms(&this.unsorted_rooms));

        this
    }

    /// Make `sorted_room_ids` the current order, and refresh `positions` and
    /// `sort_keys` from it.
    fn set_order(&mut self, sorted_room_ids: Vec<OwnedRoomId>) {
        self.positions = sorted_room_ids
            .iter()
            .enumerate()
            .map(|(index, room_id)| (room_id.clone(), index))
            .collect();
        self.sorted_room_ids = sorted_room_ids;
        self.sort_keys = self
            .unsorted_rooms
            .iter()
            .map(|room| (room.room_id().to_owned(), SortKey::of(room)))
            .collect();
    }

    fn rooms_by_id(&self) -> HashMap<&RoomId, &RoomListItem> {
        self.unsorted_rooms.iter().map(|room| (room.room_id(), room)).collect()
    }

    /// The rooms, in sorted order.
    fn sorted_rooms(&self) -> Vector<RoomListItem> {
        let rooms_by_id = self.rooms_by_id();

        self.sorted_room_ids.iter().map(|room_id| rooms_by_id[&**room_id].clone()).collect()
    }

    /// Process one batch of input diffs, and return the batch of output diffs
    /// applying to the sorted order.
    fn process(&mut self, batch: Vec<VectorDiff<RoomListItem>>) -> Vec<VectorDiff<RoomListItem>> {
        // 1. Collect the updated rooms, and classify the diffs.
        let mut updated_rooms: Vec<RoomListItem> = Vec::new();
        let mut rooms_added_or_removed = false;
        let mut is_reset = false;

        for diff in &batch {
            match diff {
                VectorDiff::Set { value, .. } => {
                    updated_rooms.push(value.clone());
                }

                VectorDiff::Insert { value, .. }
                | VectorDiff::PushBack { value }
                | VectorDiff::PushFront { value } => {
                    rooms_added_or_removed = true;
                    updated_rooms.push(value.clone());
                }

                VectorDiff::Append { values } => {
                    rooms_added_or_removed = true;
                    updated_rooms.extend(values.iter().cloned());
                }

                VectorDiff::Remove { .. }
                | VectorDiff::Truncate { .. }
                | VectorDiff::PopFront
                | VectorDiff::PopBack => {
                    rooms_added_or_removed = true;
                }

                VectorDiff::Reset { .. } | VectorDiff::Clear => {
                    is_reset = true;
                }
            }
        }

        // 2. Apply the batch to the unsorted rooms.
        for diff in batch {
            diff.apply(&mut self.unsorted_rooms);
        }

        // 3. A reset: re-sort everything, and emit a single `Reset` or `Clear`.
        if is_reset {
            self.set_order(sort_rooms(&self.unsorted_rooms));

            return if self.unsorted_rooms.is_empty() {
                vec![VectorDiff::Clear]
            } else {
                vec![VectorDiff::Reset { values: self.sorted_rooms() }]
            };
        }

        // 4. Only `Set`s with unchanged sort keys: nothing can move, forward
        //    the `Set`s at the rooms' sorted positions.
        let sort_key_changed = updated_rooms
            .iter()
            .any(|room| self.sort_keys.get(room.room_id()) != Some(&SortKey::of(room)));

        if !rooms_added_or_removed && !sort_key_changed {
            return updated_rooms
                .into_iter()
                .filter_map(|room| {
                    let index = *self.positions.get(room.room_id())?;
                    Some(VectorDiff::Set { index, value: room })
                })
                .collect();
        }

        // 5. Otherwise: re-sort, and emit the moves from the old order to the
        //    new one, followed by the `Set`s of the updated rooms that didn't
        //    move (the moved ones are inserted with their new value already).
        let new_sorted_room_ids = sort_rooms(&self.unsorted_rooms);
        let (kept, removes, inserts) = diff_orders(&self.sorted_room_ids, &new_sorted_room_ids);
        self.set_order(new_sorted_room_ids);

        let rooms_by_id = self.rooms_by_id();
        let mut output = Vec::with_capacity(removes.len() + inserts.len());

        for index in removes {
            output.push(VectorDiff::Remove { index });
        }

        for index in inserts {
            let room_id: &RoomId = &self.sorted_room_ids[index];
            output.push(VectorDiff::Insert { index, value: rooms_by_id[room_id].clone() });
        }

        for room in updated_rooms {
            if kept.contains(room.room_id()) {
                let index = self.positions[room.room_id()];
                output.push(VectorDiff::Set { index, value: room });
            }
        }

        output
    }
}

/// What a room's position depends on, to detect updates that can't reorder.
#[derive(Clone, PartialEq, Eq)]
struct SortKey {
    unsent: bool,
    timestamp: Option<u64>,
    bump_stamp: Option<u64>,
    name: Option<String>,
}

impl SortKey {
    fn of(room: &RoomListItem) -> Self {
        Self {
            unsent: room.cached_latest_event_is_unsent,
            timestamp: room.cached_latest_event_timestamp.map(|ts| ts.get().into()),
            bump_stamp: room.cached_recency_stamp.map(Into::into),
            name: room.cached_display_name.clone(),
        }
    }
}

/// Compute the sorted order of `rooms` as a list of room IDs.
fn sort_rooms(rooms: &Vector<RoomListItem>) -> Vec<OwnedRoomId> {
    let rooms: Vec<&RoomListItem> = rooms.iter().collect();
    let recencies = compute_recencies(&rooms);

    let mut indices: Vec<usize> = (0..rooms.len()).collect();
    indices.sort_unstable_by(|&left, &right| {
        cmp_rooms(rooms[left], recencies[left], rooms[right], recencies[right])
    });

    indices.into_iter().map(|index| rooms[index].room_id().to_owned()).collect()
}

/// A room's recency: its timestamp (possibly borrowed), then its bump stamp.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Recency {
    timestamp: u64,
    bump_stamp: u64,
}

/// Compute the [`Recency`] of every room in `rooms`. Rooms with neither a
/// timestamp nor a bump stamp have none and sort last.
fn compute_recencies(rooms: &[&RoomListItem]) -> Vec<Option<Recency>> {
    // The anchors, sorted by bump stamp.
    let mut anchors: Vec<Anchor> = rooms
        .iter()
        .filter_map(|room| match (room.cached_latest_event_timestamp, room.cached_recency_stamp) {
            (Some(timestamp), Some(bump_stamp)) => {
                Some(Anchor { bump_stamp: bump_stamp.into(), timestamp: timestamp.get().into() })
            }
            _ => None,
        })
        .collect();
    anchors.sort_unstable();

    let top_anchor_timestamp = anchors.last().map(|anchor| anchor.timestamp);

    // Replace each anchor's timestamp with the smallest timestamp among the
    // anchors at or above it (a suffix minimum), so that looking up a single
    // anchor in `borrowed_timestamp` gives the smallest timestamp among all
    // the anchors at or above a bump stamp. See the module documentation for
    // why.
    let mut suffix_min = u64::MAX;
    for anchor in anchors.iter_mut().rev() {
        suffix_min = suffix_min.min(anchor.timestamp);
        anchor.timestamp = suffix_min;
    }

    rooms
        .iter()
        .map(|room| {
            let timestamp = room.cached_latest_event_timestamp.map(|ts| u64::from(ts.get()));
            let bump_stamp = room.cached_recency_stamp.map(u64::from);

            match (timestamp, bump_stamp) {
                (Some(timestamp), Some(bump_stamp)) => Some(Recency { timestamp, bump_stamp }),
                // No bump stamp: sub-order below rooms with the same timestamp.
                (Some(timestamp), None) => Some(Recency { timestamp, bump_stamp: 0 }),
                (None, Some(bump_stamp)) => Some(Recency {
                    timestamp: borrowed_timestamp(&anchors, top_anchor_timestamp, bump_stamp),
                    bump_stamp,
                }),
                (None, None) => None,
            }
        })
        .collect()
}

/// A room with both a timestamp and a bump stamp, from which rooms without a
/// timestamp borrow one.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Anchor {
    bump_stamp: u64,
    timestamp: u64,
}

/// The timestamp borrowed by a room without a timestamp, given its
/// `bump_stamp`.
///
/// - At or below some anchor: the (suffix minimum) timestamp of the first
///   anchor at or above it.
/// - Above every anchor: the top anchor's real timestamp, so the room sorts
///   just above it, as its bump stamp is higher.
/// - No anchors at all: 0, so rooms are ordered by bump stamp only.
fn borrowed_timestamp(
    anchors: &[Anchor],
    top_anchor_timestamp: Option<u64>,
    bump_stamp: u64,
) -> u64 {
    let index = anchors.partition_point(|anchor| anchor.bump_stamp < bump_stamp);

    match anchors.get(index) {
        Some(anchor) => anchor.timestamp,
        None => top_anchor_timestamp.unwrap_or(0),
    }
}

/// Compare two rooms given their [`Recency`].
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

/// The minimal moves from the `old` order to the `new` one.
///
/// The rooms that keep their relative order don't need to move: only the
/// others are removed and inserted again at their new position. The largest
/// such group of rooms is the longest increasing subsequence of their old
/// positions, taken in the new order.
///
/// For example, from `[a, b, c, d]` to `[c, a, b, d]`, `a, b, d` keep their
/// relative order, so the only move is `c`: removed at 2, inserted at 0.
///
/// Returns `(kept, removes, inserts)`: the rooms that don't move, the `old`
/// indices to remove in descending order, and the `new` indices to insert in
/// ascending order. Applying the removes, then the inserts, transforms `old`
/// into `new`.
fn diff_orders(
    old: &[OwnedRoomId],
    new: &[OwnedRoomId],
) -> (HashSet<OwnedRoomId>, Vec<usize>, Vec<usize>) {
    let old_positions: HashMap<&RoomId, usize> =
        old.iter().enumerate().map(|(index, room_id)| (room_id.as_ref(), index)).collect();

    // For each room in the new order, its position in the old order (if any).
    let old_positions_in_new_order: Vec<Option<usize>> =
        new.iter().map(|room_id| old_positions.get(&**room_id).copied()).collect();

    // Patience sorting: `tails[length - 1]` is the `new` index of the smallest
    // tail of an increasing subsequence of that length.
    let mut tails: Vec<usize> = Vec::new();
    let mut parents: Vec<Option<usize>> = vec![None; new.len()];

    for (new_index, maybe_old_position) in old_positions_in_new_order.iter().enumerate() {
        let Some(old_position) = maybe_old_position else { continue };

        let length = tails
            .partition_point(|&tail| old_positions_in_new_order[tail].unwrap() < *old_position);

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

#[cfg(test)]
mod tests {
    use assert_matches::assert_matches;
    use matrix_sdk::{
        latest_events::{LatestEventValue, RemoteLatestEventValue},
        test_utils::mocks::MatrixMockServer,
    };
    use matrix_sdk_base::RoomInfoNotableUpdateReasons;
    use matrix_sdk_test::async_test;
    use ruma::{
        MilliSecondsSinceUnixEpoch, UInt, events::room::message::RoomMessageEventContent, room_id,
        serde::Raw,
    };
    use serde_json::json;
    use stream_assert::assert_pending;
    use tokio_stream::wrappers::ReceiverStream;

    use super::{super::filters::new_rooms, *};

    #[async_test]
    async fn test_bump_stamp_order_then_timestamps_slot_in() {
        let [mut room_a, mut room_b, mut room_c, room_d] = rooms([
            (room_id!("!a:x.y"), 10),
            (room_id!("!b:x.y"), 20),
            (room_id!("!c:x.y"), 30),
            (room_id!("!d:x.y"), 40),
        ])
        .await;
        let [a, b, c, d] = [room_a.room_id(), room_b.room_id(), room_c.room_id(), room_d.room_id()]
            .map(ToOwned::to_owned);

        let (sender, receiver) = tokio::sync::mpsc::channel(8);
        let (mut current, stream) = sorted_by_recency(
            [room_a.clone(), room_b.clone(), room_c.clone(), room_d.clone()].into_iter().collect(),
            ReceiverStream::new(receiver),
        );
        pin_mut!(stream);

        // No latest events yet: pure bump-stamp order.
        apply_and_check(&mut current, vec![], &[&d, &c, &b, &a]);

        // `room_b` (ts 200) and `room_c` (ts 300) get latest events in one
        // batch. `room_d` borrows ts 300 and stays above `room_c`; `room_a`
        // borrows ts 200 and stays below `room_b`: no moves.
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
        assert!(diffs.iter().all(|diff| matches!(diff, VectorDiff::Set { .. })));
        apply_and_check(&mut current, diffs, &[&d, &c, &b, &a]);

        // `room_a` gets the newest latest event: it moves to the front.
        set_latest_event(&mut room_a, remote(400)).await;

        sender.send(vec![VectorDiff::Set { index: 0, value: room_a.clone() }]).await.unwrap();

        apply_and_check(&mut current, stream.next().await.unwrap(), &[&a, &d, &c, &b]);
        assert_pending!(stream);
    }

    /// A room with a fresh timestamp but a stale bump stamp must not drag the
    /// previewless rooms near its bump stamp above a genuinely recent room.
    #[async_test]
    async fn test_inconsistent_anchor_does_not_promote_previewless_rooms() {
        let [mut poisoned, mut recent, old_a, old_b] = rooms([
            (room_id!("!p:x.y"), 5),
            (room_id!("!n:x.y"), 100),
            (room_id!("!olda:x.y"), 3),
            (room_id!("!oldb:x.y"), 4),
        ])
        .await;

        set_latest_event(&mut recent, remote(500)).await;
        set_latest_event(&mut poisoned, remote(1000)).await;

        assert_eq!(
            sorted(&[&poisoned, &recent, &old_a, &old_b]),
            room_ids(&["!p:x.y", "!n:x.y", "!oldb:x.y", "!olda:x.y"]),
        );
    }

    #[async_test]
    async fn test_sort_rules() {
        let [unsent, recent, recent_without_bump, named_b, named_a, unnamed_1, unnamed_2, empty] =
            rooms([
                (room_id!("!unsent:x.y"), 0),
                (room_id!("!recent:x.y"), 0),
                (room_id!("!nobump:x.y"), 0),
                (room_id!("!b:x.y"), 0),
                (room_id!("!a:x.y"), 0),
                (room_id!("!1:x.y"), 0),
                (room_id!("!2:x.y"), 0),
                (room_id!("!empty:x.y"), 0),
            ])
            .await;

        let stamps = |timestamp, bump_stamp| SortFields {
            timestamp: Some(timestamp),
            bump_stamp: Some(bump_stamp),
            ..Default::default()
        };

        // An unsent local latest event comes first, even if older.
        let unsent = with_fields(&unsent, SortFields { unsent: true, ..stamps(1, 1) });
        let recent = with_fields(&recent, stamps(100, 100));
        // Same timestamp but no bump stamp: just below.
        let recent_without_bump = with_fields(
            &recent_without_bump,
            SortFields { timestamp: Some(100), ..Default::default() },
        );
        // Same recency: by name, then by room ID.
        let named_b = with_fields(&named_b, SortFields { name: Some("b"), ..stamps(50, 50) });
        let named_a = with_fields(&named_a, SortFields { name: Some("a"), ..stamps(50, 50) });
        let unnamed_1 = with_fields(&unnamed_1, stamps(10, 10));
        let unnamed_2 = with_fields(&unnamed_2, stamps(10, 10));
        // Neither a timestamp nor a bump stamp: last.
        let empty = with_fields(&empty, SortFields::default());

        assert_eq!(
            sorted(&[
                &empty,
                &unnamed_2,
                &unnamed_1,
                &named_b,
                &named_a,
                &recent_without_bump,
                &recent,
                &unsent,
            ]),
            room_ids(&[
                "!unsent:x.y",
                "!recent:x.y",
                "!nobump:x.y",
                "!a:x.y",
                "!b:x.y",
                "!1:x.y",
                "!2:x.y",
                "!empty:x.y",
            ]),
        );
    }

    #[async_test]
    async fn test_timestamp_less_invite_is_slotted_by_bump_stamp() {
        let [mut old, mut invite, mut new] = rooms([
            (room_id!("!old:x.y"), 10),
            (room_id!("!invite:x.y"), 20),
            (room_id!("!new:x.y"), 30),
        ])
        .await;

        set_latest_event(&mut old, remote(100)).await;
        set_latest_event(&mut new, remote(300)).await;

        // A stripped-state invite has no timestamp: it must sit between the
        // two rooms by its bump stamp.
        set_latest_event(
            &mut invite,
            LatestEventValue::RemoteInvite { event_id: None, timestamp: None, inviter: None },
        )
        .await;

        assert_eq!(
            sorted(&[&old, &invite, &new]),
            room_ids(&["!new:x.y", "!invite:x.y", "!old:x.y"]),
        );
    }

    #[async_test]
    async fn test_set_without_sort_key_change_does_not_reorder() {
        let [room_a, room_b] = rooms([(room_id!("!a:x.y"), 1), (room_id!("!b:x.y"), 2)]).await;

        let (sender, receiver) = tokio::sync::mpsc::channel(8);
        let (_values, stream) = sorted_by_recency(
            [room_a.clone(), room_b.clone()].into_iter().collect(),
            ReceiverStream::new(receiver),
        );
        pin_mut!(stream);

        // Unchanged sort key: a single `Set` at the sorted position.
        sender.send(vec![VectorDiff::Set { index: 0, value: room_a.clone() }]).await.unwrap();

        let diffs = stream.next().await.unwrap();
        assert_eq!(diffs.len(), 1);
        assert!(matches!(&diffs[0], VectorDiff::Set { index: 1, .. }));

        assert_pending!(stream);
    }

    #[async_test]
    async fn test_rooms_added_and_removed() {
        let [room_a, room_b, room_c] =
            rooms([(room_id!("!a:x.y"), 1), (room_id!("!b:x.y"), 2), (room_id!("!c:x.y"), 3)])
                .await;
        let [a, b, c] = [room_a.room_id(), room_b.room_id(), room_c.room_id()];

        let (sender, receiver) = tokio::sync::mpsc::channel(8);
        let (mut current, stream) = sorted_by_recency(
            [room_a.clone(), room_b.clone()].into_iter().collect(),
            ReceiverStream::new(receiver),
        );
        pin_mut!(stream);

        apply_and_check(&mut current, vec![], &[b, a]);

        // `room_c` is pushed at the back of the input, but sorts first.
        sender.send(vec![VectorDiff::PushBack { value: room_c.clone() }]).await.unwrap();
        apply_and_check(&mut current, stream.next().await.unwrap(), &[c, b, a]);

        // `room_b` is removed from the input.
        sender.send(vec![VectorDiff::Remove { index: 1 }]).await.unwrap();
        apply_and_check(&mut current, stream.next().await.unwrap(), &[c, a]);

        assert_pending!(stream);
    }

    #[async_test]
    async fn test_reset_is_forwarded_as_sorted_reset() {
        let [room_a, room_b] = rooms([(room_id!("!a:x.y"), 1), (room_id!("!b:x.y"), 2)]).await;

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

        // A reset to an empty list is forwarded as a `Clear`.
        sender.send(vec![VectorDiff::Reset { values: Vector::new() }]).await.unwrap();
        assert_matches!(stream.next().await.unwrap().as_slice(), [VectorDiff::Clear]);

        assert_pending!(stream);
    }

    #[test]
    fn test_borrowed_timestamp() {
        // No anchors: 0, i.e. pure bump-stamp order.
        assert_eq!(borrowed_timestamp(&[], None, 42), 0);

        // Consistent anchors (timestamps increase with bump stamps), already
        // carrying their suffix minima (which leave them unchanged here).
        let anchors = to_anchors([(10, 100), (20, 200), (40, 400)]);

        // At or below an anchor's bump stamp: that anchor's timestamp.
        assert_eq!(borrowed_timestamp(&anchors, Some(400), 20), 200);
        assert_eq!(borrowed_timestamp(&anchors, Some(400), 12), 200);
        assert_eq!(borrowed_timestamp(&anchors, Some(400), 5), 100);
        assert_eq!(borrowed_timestamp(&anchors, Some(400), 30), 400);
        // Above every anchor: the top anchor's timestamp.
        assert_eq!(borrowed_timestamp(&anchors, Some(400), 100), 400);

        // An inconsistent anchor (fresh timestamp, stale bump stamp at bump
        // 10): the suffix minima flatten it out for everything above it.
        let anchors = to_anchors([(10, 200), (20, 200), (40, 400)]);

        // Rooms at/below the poisoned anchor's bump stamp cannot borrow its
        // fresh timestamp beyond the smallest above them.
        assert_eq!(borrowed_timestamp(&anchors, Some(400), 5), 200);
        assert_eq!(borrowed_timestamp(&anchors, Some(400), 15), 200);
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

    /// The sort inputs of a room, see [`with_fields`].
    #[derive(Clone, Debug, Default)]
    struct SortFields {
        timestamp: Option<u64>,
        bump_stamp: Option<u64>,
        unsent: bool,
        name: Option<&'static str>,
    }

    /// A copy of `room` with the given sort inputs.
    fn with_fields(room: &RoomListItem, fields: SortFields) -> RoomListItem {
        let mut room = room.clone();
        room.cached_latest_event_timestamp =
            fields.timestamp.map(|ts| MilliSecondsSinceUnixEpoch(UInt::new(ts).unwrap()));
        room.cached_recency_stamp = fields.bump_stamp.map(Into::into);
        room.cached_latest_event_is_unsent = fields.unsent;
        room.cached_display_name = fields.name.map(ToOwned::to_owned);

        room
    }

    /// Create rooms with the given bump stamps.
    async fn rooms<const N: usize>(bump_stamps: [(&RoomId, u64); N]) -> [RoomListItem; N] {
        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;
        let mut rooms = new_rooms(bump_stamps.map(|(room_id, _)| room_id), &client, &server).await;

        for (room, (_, bump_stamp)) in rooms.iter_mut().zip(bump_stamps) {
            room.update_room_info(|mut info| {
                info.update_recency_stamp(bump_stamp.into());
                (info, RoomInfoNotableUpdateReasons::RECENCY_STAMP)
            })
            .await;
            room.refresh_cached_data();
        }

        rooms
    }

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

    /// The initial order of `rooms`.
    fn sorted(rooms: &[&RoomListItem]) -> Vec<OwnedRoomId> {
        let initial = rooms.iter().map(|room| (*room).clone()).collect();
        let (values, _) = sorted_by_recency(initial, futures_util::stream::pending());

        values.iter().map(|room| room.room_id().to_owned()).collect()
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

    fn room_ids(ids: &[&str]) -> Vec<OwnedRoomId> {
        ids.iter().map(|id| RoomId::parse(id).unwrap()).collect()
    }

    /// Build anchors from `(bump stamp, timestamp)` pairs.
    fn to_anchors<const N: usize>(pairs: [(u64, u64); N]) -> [Anchor; N] {
        pairs.map(|(bump_stamp, timestamp)| Anchor { bump_stamp, timestamp })
    }

    mod proptests {
        use proptest::{prelude::*, test_runner::TestRunner};

        use super::*;

        /// Whatever the input batches, applying the output batches to the
        /// sorted list must always give the input list fully re-sorted.
        #[async_test]
        async fn test_process_matches_full_sort() {
            let base = rooms([
                (room_id!("!0:x.y"), 0),
                (room_id!("!1:x.y"), 0),
                (room_id!("!2:x.y"), 0),
                (room_id!("!3:x.y"), 0),
                (room_id!("!4:x.y"), 0),
                (room_id!("!5:x.y"), 0),
            ])
            .await;

            let rooms_from = |rooms: &[(usize, SortFields)]| -> Vector<RoomListItem> {
                let mut seen = HashSet::new();

                rooms
                    .iter()
                    .filter(|(room, _)| seen.insert(*room))
                    .map(|(room, fields)| with_fields(&base[*room], fields.clone()))
                    .collect()
            };

            let strategy =
                (arb_rooms(), prop::collection::vec(prop::collection::vec(arb_op(), 0..4), 0..8));

            TestRunner::default()
                .run(&strategy, |(initial, batches)| {
                    let mut input = rooms_from(&initial);
                    let mut state = OrderingState::new(input.clone());
                    let mut output = state.sorted_rooms();

                    for ops in batches {
                        let mut batch = Vec::new();

                        for op in ops {
                            let diff = match op {
                                Op::Set { index, fields } if !input.is_empty() => {
                                    let index = index % input.len();
                                    let room = with_fields(&input[index], fields);

                                    VectorDiff::Set { index, value: room }
                                }
                                Op::Insert { index, room, fields }
                                    if !input
                                        .iter()
                                        .any(|item| item.room_id() == base[room].room_id()) =>
                                {
                                    let index = index % (input.len() + 1);

                                    VectorDiff::Insert {
                                        index,
                                        value: with_fields(&base[room], fields),
                                    }
                                }
                                Op::Remove { index } if !input.is_empty() => {
                                    VectorDiff::Remove { index: index % input.len() }
                                }
                                Op::Reset { rooms } => {
                                    VectorDiff::Reset { values: rooms_from(&rooms) }
                                }
                                Op::Clear => VectorDiff::Clear,
                                _ => continue,
                            };

                            diff.clone().apply(&mut input);
                            batch.push(diff);
                        }

                        for diff in state.process(batch) {
                            diff.apply(&mut output);
                        }

                        let expected = sort_rooms(&input);
                        let actual: Vec<OwnedRoomId> =
                            output.iter().map(|room| room.room_id().to_owned()).collect();
                        prop_assert_eq!(&actual, &expected);

                        // The output values are the latest input values.
                        for room in &output {
                            let input_room =
                                input.iter().find(|item| item.room_id() == room.room_id()).unwrap();
                            prop_assert!(SortKey::of(room) == SortKey::of(input_room));
                        }
                    }

                    Ok(())
                })
                .unwrap();
        }

        proptest! {
            /// Applying the removes then the inserts turns `old` into `new`.
            #[test]
            fn test_diff_orders_transforms_old_into_new(old in arb_order(), new in arb_order()) {
                let (kept, removes, inserts) = diff_orders(&old, &new);

                let mut result = old;

                for index in removes {
                    result.remove(index);
                }

                prop_assert!(result.iter().all(|room_id| kept.contains(room_id)));

                for index in inserts {
                    result.insert(index, new[index].clone());
                }

                prop_assert_eq!(result, new);
            }
        }

        /// An input operation, interpreted against the current input list.
        #[derive(Clone, Debug)]
        enum Op {
            Set { index: usize, fields: SortFields },
            Insert { index: usize, room: usize, fields: SortFields },
            Remove { index: usize },
            Reset { rooms: Vec<(usize, SortFields)> },
            Clear,
        }

        const NUMBER_OF_ROOMS: usize = 6;

        fn arb_fields() -> impl Strategy<Value = SortFields> {
            // Small domains, to get plenty of ties.
            (
                proptest::option::of(0..4u64),
                proptest::option::of(0..4u64),
                any::<bool>(),
                proptest::option::of(prop::sample::select(vec!["a", "b"])),
            )
                .prop_map(|(timestamp, bump_stamp, unsent, name)| SortFields {
                    timestamp,
                    bump_stamp,
                    unsent,
                    name,
                })
        }

        fn arb_rooms() -> impl Strategy<Value = Vec<(usize, SortFields)>> {
            prop::collection::vec((0..NUMBER_OF_ROOMS, arb_fields()), 0..NUMBER_OF_ROOMS)
        }

        fn arb_op() -> impl Strategy<Value = Op> {
            prop_oneof![
                4 => (any::<usize>(), arb_fields()).prop_map(|(index, fields)| Op::Set { index, fields }),
                2 => (any::<usize>(), 0..NUMBER_OF_ROOMS, arb_fields())
                    .prop_map(|(index, room, fields)| Op::Insert { index, room, fields }),
                2 => any::<usize>().prop_map(|index| Op::Remove { index }),
                1 => arb_rooms().prop_map(|rooms| Op::Reset { rooms }),
                1 => Just(Op::Clear),
            ]
        }

        fn arb_order() -> impl Strategy<Value = Vec<OwnedRoomId>> {
            let ids = room_ids(&["!a:x", "!b:x", "!c:x", "!d:x", "!e:x", "!f:x", "!g:x", "!h:x"]);

            prop::sample::subsequence(ids, 0..=8).prop_shuffle()
        }
    }
}
