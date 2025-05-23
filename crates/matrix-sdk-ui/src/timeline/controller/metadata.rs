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

use std::{
    collections::{BTreeSet, HashMap},
    sync::Arc,
};

use ruma::{EventId, OwnedEventId, OwnedUserId, RoomVersionId};
use tracing::trace;

use super::{
    super::{subscriber::skip::SkipCount, TimelineItem, TimelineItemKind, TimelineUniqueId},
    read_receipts::ReadReceipts,
    Aggregations, AllRemoteEvents, ObservableItemsTransaction,
};
use crate::unable_to_decrypt_hook::UtdHookManager;

#[derive(Clone, Debug)]
pub(in crate::timeline) struct TimelineMetadata {
    // **** CONSTANT FIELDS ****
    /// An optional prefix for internal IDs, defined during construction of the
    /// timeline.
    ///
    /// This value is constant over the lifetime of the metadata.
    internal_id_prefix: Option<String>,

    /// The `count` value for the `Skip` higher-order stream used by the
    /// `TimelineSubscriber`. See its documentation to learn more.
    pub(super) subscriber_skip_count: SkipCount,

    /// The hook to call whenever we run into a unable-to-decrypt event.
    ///
    /// This value is constant over the lifetime of the metadata.
    pub unable_to_decrypt_hook: Option<Arc<UtdHookManager>>,

    /// A boolean indicating whether the room the timeline is attached to is
    /// actually encrypted or not.
    ///
    /// May be false until we fetch the actual room encryption state.
    pub is_room_encrypted: bool,

    /// Matrix room version of the timeline's room, or a sensible default.
    ///
    /// This value is constant over the lifetime of the metadata.
    pub room_version: RoomVersionId,

    /// The own [`OwnedUserId`] of the client who opened the timeline.
    pub(crate) own_user_id: OwnedUserId,

    // **** DYNAMIC FIELDS ****
    /// The next internal identifier for timeline items, used for both local and
    /// remote echoes.
    ///
    /// This is never cleared, but always incremented, to avoid issues with
    /// reusing a stale internal id across timeline clears. We don't expect
    /// we can hit `u64::max_value()` realistically, but if this would
    /// happen, we do a wrapping addition when incrementing this
    /// id; the previous 0 value would have disappeared a long time ago, unless
    /// the device has terabytes of RAM.
    next_internal_id: u64,

    /// Aggregation metadata and pending aggregations.
    pub aggregations: Aggregations,

    /// Given an event, what are all the events that are replies to it?
    ///
    /// Only works for remote events *and* replies which are remote-echoed.
    pub replies: HashMap<OwnedEventId, BTreeSet<OwnedEventId>>,

    /// Identifier of the fully-read event, helping knowing where to introduce
    /// the read marker.
    pub fully_read_event: Option<OwnedEventId>,

    /// Whether we have a fully read-marker item in the timeline, that's up to
    /// date with the room's read marker.
    ///
    /// This is false when:
    /// - The fully-read marker points to an event that is not in the timeline,
    /// - The fully-read marker item would be the last item in the timeline.
    pub has_up_to_date_read_marker_item: bool,

    /// Read receipts related state.
    ///
    /// TODO: move this over to the event cache (see also #3058).
    pub(super) read_receipts: ReadReceipts,
}

impl TimelineMetadata {
    pub(in crate::timeline) fn new(
        own_user_id: OwnedUserId,
        room_version: RoomVersionId,
        internal_id_prefix: Option<String>,
        unable_to_decrypt_hook: Option<Arc<UtdHookManager>>,
        is_room_encrypted: bool,
    ) -> Self {
        Self {
            subscriber_skip_count: SkipCount::new(),
            own_user_id,
            next_internal_id: Default::default(),
            aggregations: Default::default(),
            replies: Default::default(),
            fully_read_event: Default::default(),
            // It doesn't make sense to set this to false until we fill the `fully_read_event`
            // field, otherwise we'll keep on exiting early in `Self::update_read_marker`.
            has_up_to_date_read_marker_item: true,
            read_receipts: Default::default(),
            room_version,
            unable_to_decrypt_hook,
            internal_id_prefix,
            is_room_encrypted,
        }
    }

    pub(super) fn clear(&mut self) {
        // Note: we don't clear the next internal id to avoid bad cases of stale unique
        // ids across timeline clears.
        self.aggregations.clear();
        self.replies.clear();
        self.fully_read_event = None;
        // We forgot about the fully read marker right above, so wait for a new one
        // before attempting to update it for each new timeline item.
        self.has_up_to_date_read_marker_item = true;
        self.read_receipts.clear();
    }

    /// Get the relative positions of two events in the timeline.
    ///
    /// This method assumes that all events since the end of the timeline are
    /// known.
    ///
    /// Returns `None` if none of the two events could be found in the timeline.
    pub(in crate::timeline) fn compare_events_positions(
        &self,
        event_a: &EventId,
        event_b: &EventId,
        all_remote_events: &AllRemoteEvents,
    ) -> Option<RelativePosition> {
        if event_a == event_b {
            return Some(RelativePosition::Same);
        }

        // We can make early returns here because we know all events since the end of
        // the timeline, so the first event encountered is the oldest one.
        for event_meta in all_remote_events.iter().rev() {
            if event_meta.event_id == event_a {
                return Some(RelativePosition::Before);
            }
            if event_meta.event_id == event_b {
                return Some(RelativePosition::After);
            }
        }

        None
    }

    /// Returns the next internal id for a timeline item (and increment our
    /// internal counter).
    fn next_internal_id(&mut self) -> TimelineUniqueId {
        let val = self.next_internal_id;
        self.next_internal_id = self.next_internal_id.wrapping_add(1);
        let prefix = self.internal_id_prefix.as_deref().unwrap_or("");
        TimelineUniqueId(format!("{prefix}{val}"))
    }

    /// Returns a new timeline item with a fresh internal id.
    pub fn new_timeline_item(&mut self, kind: impl Into<TimelineItemKind>) -> Arc<TimelineItem> {
        TimelineItem::new(kind, self.next_internal_id())
    }

    /// Try to update the read marker item in the timeline.
    pub(crate) fn update_read_marker(&mut self, items: &mut ObservableItemsTransaction<'_>) {
        let Some(fully_read_event) = &self.fully_read_event else { return };
        trace!(?fully_read_event, "Updating read marker");

        let read_marker_idx = items
            .iter_remotes_region()
            .rev()
            .find_map(|(idx, item)| item.is_read_marker().then_some(idx));

        let mut fully_read_event_idx = items.iter_remotes_region().rev().find_map(|(idx, item)| {
            (item.as_event()?.event_id() == Some(fully_read_event)).then_some(idx)
        });

        if let Some(fully_read_event_idx) = &mut fully_read_event_idx {
            // The item at position `i` is the first item that's fully read, we're about to
            // insert a read marker just after it.
            //
            // Do another forward pass to skip all the events we've sent too.

            // Find the position of the first element…
            let next = items
                .iter_remotes_region()
                // …strictly *after* the fully read event…
                .skip_while(|(idx, _)| idx <= fully_read_event_idx)
                // …that's not virtual and not sent by us…
                .find_map(|(idx, item)| {
                    (item.as_event()?.sender() != self.own_user_id).then_some(idx)
                });

            if let Some(next) = next {
                // `next` point to the first item that's not sent by us, so the *previous* of
                // next is the right place where to insert the fully read marker.
                *fully_read_event_idx = next.wrapping_sub(1);
            } else {
                // There's no event after the read marker that's not sent by us, i.e. the full
                // timeline has been read: the fully read marker goes to the end, even after the
                // local timeline items.
                //
                // TODO (@hywan): Should we introduce a `items.position_of_last_remote()` to
                // insert before the local timeline items?
                *fully_read_event_idx = items.len().wrapping_sub(1);
            }
        }

        match (read_marker_idx, fully_read_event_idx) {
            (None, None) => {
                // We didn't have a previous read marker, and we didn't find the fully-read
                // event in the timeline items. Don't do anything, and retry on
                // the next event we add.
                self.has_up_to_date_read_marker_item = false;
            }

            (None, Some(idx)) => {
                // Only insert the read marker if it is not at the end of the timeline.
                if idx + 1 < items.len() {
                    let idx = idx + 1;
                    items.insert(idx, TimelineItem::read_marker(), None);
                    self.has_up_to_date_read_marker_item = true;
                } else {
                    // The next event might require a read marker to be inserted at the current
                    // end.
                    self.has_up_to_date_read_marker_item = false;
                }
            }

            (Some(_), None) => {
                // We didn't find the timeline item containing the event referred to by the read
                // marker. Retry next time we get a new event.
                self.has_up_to_date_read_marker_item = false;
            }

            (Some(from), Some(to)) => {
                if from >= to {
                    // The read marker can't move backwards.
                    if from + 1 == items.len() {
                        // The read marker has nothing after it. An item disappeared; remove it.
                        items.remove(from);
                    }
                    self.has_up_to_date_read_marker_item = true;
                    return;
                }

                let prev_len = items.len();
                let read_marker = items.remove(from);

                // Only insert the read marker if it is not at the end of the timeline.
                if to + 1 < prev_len {
                    // Since the fully-read event's index was shifted to the left
                    // by one position by the remove call above, insert the fully-
                    // read marker at its previous position, rather than that + 1
                    items.insert(to, read_marker, None);
                    self.has_up_to_date_read_marker_item = true;
                } else {
                    self.has_up_to_date_read_marker_item = false;
                }
            }
        }
    }
}

/// Result of comparing events position in the timeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::timeline) enum RelativePosition {
    /// Event B is after (more recent than) event A.
    After,
    /// They are the same event.
    Same,
    /// Event B is before (older than) event A.
    Before,
}

/// Metadata about an event that needs to be kept in memory.
#[derive(Debug, Clone)]
pub(in crate::timeline) struct EventMeta {
    /// The ID of the event.
    pub event_id: OwnedEventId,

    /// Whether the event is among the timeline items.
    pub visible: bool,

    /// Foundation for the mapping between remote events to timeline items.
    ///
    /// Let's explain it. The events represent the first set and are stored in
    /// [`ObservableItems::all_remote_events`], and the timeline
    /// items represent the second set and are stored in
    /// [`ObservableItems::items`].
    ///
    /// Each event is mapped to at most one timeline item:
    ///
    /// - `None` if the event isn't rendered in the timeline (e.g. some state
    ///   events, or malformed events) or is rendered as a timeline item that
    ///   attaches to or groups with another item, like reactions,
    /// - `Some(_)` if the event is rendered in the timeline.
    ///
    /// This is neither a surjection nor an injection. Every timeline item may
    /// not be attached to an event, for example with a virtual timeline item.
    /// We can formulate other rules:
    ///
    /// - a timeline item that doesn't _move_ and that is represented by an
    ///   event has a mapping to an event,
    /// - a virtual timeline item has no mapping to an event.
    ///
    /// Imagine the following remote events:
    ///
    /// | index | remote events |
    /// +-------+---------------+
    /// | 0     | `$ev0`        |
    /// | 1     | `$ev1`        |
    /// | 2     | `$ev2`        |
    /// | 3     | `$ev3`        |
    /// | 4     | `$ev4`        |
    /// | 5     | `$ev5`        |
    ///
    /// Once rendered in a timeline, it for example produces:
    ///
    /// | index | item              | related items        |
    /// +-------+-------------------+----------------------+
    /// | 0     | content of `$ev0` |                      |
    /// | 1     | content of `$ev2` | reaction with `$ev4` |
    /// | 2     | date divider      |                      |
    /// | 3     | content of `$ev3` |                      |
    /// | 4     | content of `$ev5` |                      |
    ///
    /// Note the date divider that is a virtual item. Also note `$ev4` which is
    /// a reaction to `$ev2`. Finally note that `$ev1` is not rendered in
    /// the timeline.
    ///
    /// The mapping between remote event index to timeline item index will look
    /// like this:
    ///
    /// | remote event index | timeline item index | comment                                    |
    /// +--------------------+---------------------+--------------------------------------------+
    /// | 0                  | `Some(0)`           | `$ev0` is rendered as the #0 timeline item |
    /// | 1                  | `None`              | `$ev1` isn't rendered in the timeline      |
    /// | 2                  | `Some(1)`           | `$ev2` is rendered as the #1 timeline item |
    /// | 3                  | `Some(3)`           | `$ev3` is rendered as the #3 timeline item |
    /// | 4                  | `None`              | `$ev4` is a reaction to item #1            |
    /// | 5                  | `Some(4)`           | `$ev5` is rendered as the #4 timeline item |
    ///
    /// Note that the #2 timeline item (the day divider) doesn't map to any
    /// remote event, but if it moves, it has an impact on this mapping.
    pub timeline_item_index: Option<usize>,
}

impl EventMeta {
    pub fn new(event_id: OwnedEventId, visible: bool) -> Self {
        Self { event_id, visible, timeline_item_index: None }
    }
}
