// Copyright 2023 The Matrix.org Foundation C.I.C.
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

use std::{num::NonZeroUsize, sync::Arc};

use matrix_sdk::{locks::RwLock, ring_buffer::RingBuffer};
use ruma::{EventId, OwnedEventId, OwnedUserId, RoomVersionId};
use tracing::trace;

use super::{
    super::{
        reactions::Reactions, rfind_event_by_id, TimelineItem, TimelineItemKind, TimelineUniqueId,
    },
    read_receipts::ReadReceipts,
    state::PendingPollEvents,
    AllRemoteEvents, ObservableItemsTransaction, PendingEdit,
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

    /// The hook to call whenever we run into a unable-to-decrypt event.
    ///
    /// This value is constant over the lifetime of the metadata.
    pub unable_to_decrypt_hook: Option<Arc<UtdHookManager>>,

    /// A boolean indicating whether the room the timeline is attached to is
    /// actually encrypted or not.
    pub is_room_encrypted: Arc<RwLock<Option<bool>>>,

    /// Matrix room version of the timeline's room, or a sensible default.
    ///
    /// This value is constant over the lifetime of the metadata.
    pub room_version: RoomVersionId,

    /// The own [`OwnedUserId`] of the client who opened the timeline.
    own_user_id: OwnedUserId,

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

    /// State helping matching reactions to their associated events, and
    /// stashing pending reactions.
    pub reactions: Reactions,

    /// Associated poll events received before their original poll start event.
    pub pending_poll_events: PendingPollEvents,

    /// Edit events received before the related event they're editing.
    pub pending_edits: RingBuffer<PendingEdit>,

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

/// Maximum number of stash pending edits.
/// SAFETY: 32 is not 0.
const MAX_NUM_STASHED_PENDING_EDITS: NonZeroUsize = unsafe { NonZeroUsize::new_unchecked(32) };

impl TimelineMetadata {
    pub(in crate::timeline) fn new(
        own_user_id: OwnedUserId,
        room_version: RoomVersionId,
        internal_id_prefix: Option<String>,
        unable_to_decrypt_hook: Option<Arc<UtdHookManager>>,
        is_room_encrypted: Option<bool>,
    ) -> Self {
        Self {
            own_user_id,
            next_internal_id: Default::default(),
            reactions: Default::default(),
            pending_poll_events: Default::default(),
            pending_edits: RingBuffer::new(MAX_NUM_STASHED_PENDING_EDITS),
            fully_read_event: Default::default(),
            // It doesn't make sense to set this to false until we fill the `fully_read_event`
            // field, otherwise we'll keep on exiting early in `Self::update_read_marker`.
            has_up_to_date_read_marker_item: true,
            read_receipts: Default::default(),
            room_version,
            unable_to_decrypt_hook,
            internal_id_prefix,
            is_room_encrypted: Arc::new(RwLock::new(is_room_encrypted)),
        }
    }

    pub(super) fn clear(&mut self) {
        // Note: we don't clear the next internal id to avoid bad cases of stale unique
        // ids across timeline clears.
        self.reactions.clear();
        self.pending_poll_events.clear();
        self.pending_edits.clear();
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

        let read_marker_idx = items.iter().rposition(|item| item.is_read_marker());

        let mut fully_read_event_idx =
            rfind_event_by_id(items, fully_read_event).map(|(idx, _)| idx);

        if let Some(i) = &mut fully_read_event_idx {
            // The item at position `i` is the first item that's fully read, we're about to
            // insert a read marker just after it.
            //
            // Do another forward pass to skip all the events we've sent too.

            // Find the position of the first element…
            let next = items
                .iter()
                .enumerate()
                // …strictly *after* the fully read event…
                .skip(*i + 1)
                // …that's not virtual and not sent by us…
                .find(|(_, item)| {
                    item.as_event().is_some_and(|event| event.sender() != self.own_user_id)
                })
                .map(|(i, _)| i);

            if let Some(next) = next {
                // `next` point to the first item that's not sent by us, so the *previous* of
                // next is the right place where to insert the fully read marker.
                *i = next.wrapping_sub(1);
            } else {
                // There's no event after the read marker that's not sent by us, i.e. the full
                // timeline has been read: the fully read marker goes to the end.
                *i = items.len().wrapping_sub(1);
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
