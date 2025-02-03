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

use std::{ops::Deref, sync::Arc};

use imbl::Vector;
use ruma::EventId;

#[cfg(doc)]
use super::controller::TimelineMetadata;
use super::{
    event_item::EventTimelineItemKind, item::TimelineUniqueId, EventTimelineItem,
    ReactionsByKeyBySender, TimelineEventItemId, TimelineItem,
};

pub(super) struct EventTimelineItemWithId<'a> {
    pub inner: &'a EventTimelineItem,
    /// Internal identifier generated by [`TimelineMetadata`].
    pub internal_id: &'a TimelineUniqueId,
}

impl EventTimelineItemWithId<'_> {
    /// Create a clone of the underlying [`TimelineItem`] with the given kind.
    pub fn with_inner_kind(&self, kind: impl Into<EventTimelineItemKind>) -> Arc<TimelineItem> {
        TimelineItem::new(self.inner.with_kind(kind), self.internal_id.clone())
    }

    /// Create a clone of the underlying [`TimelineItem`] with the given
    /// reactions.
    pub fn with_reactions(&self, reactions: ReactionsByKeyBySender) -> Arc<TimelineItem> {
        let content = self.inner.content().with_reactions(reactions);

        // Do not use `Self::with_content` which may override the latest_edit_json.
        // TODO: it's likely incorrect?
        let mut event_item = self.inner.clone();
        event_item.content = content;

        TimelineItem::new(event_item, self.internal_id.clone())
    }
}

impl Deref for EventTimelineItemWithId<'_> {
    type Target = EventTimelineItem;

    fn deref(&self) -> &Self::Target {
        self.inner
    }
}

#[inline(always)]
fn rfind_event_item_internal(
    items: &Vector<Arc<TimelineItem>>,
    mut f: impl FnMut(&EventTimelineItemWithId<'_>) -> bool,
) -> Option<(usize, EventTimelineItemWithId<'_>)> {
    items
        .iter()
        .enumerate()
        .filter_map(|(idx, item)| {
            Some((
                idx,
                EventTimelineItemWithId { inner: item.as_event()?, internal_id: &item.internal_id },
            ))
        })
        .rfind(|(_, it)| f(it))
}

/// Finds an item in the vector of `items` given a predicate `f`.
///
/// WARNING/FIXME: this does a linear scan of the items, so this can be slow if
/// there are many items in the timeline.
pub(super) fn rfind_event_item(
    items: &Vector<Arc<TimelineItem>>,
    mut f: impl FnMut(&EventTimelineItem) -> bool,
) -> Option<(usize, EventTimelineItemWithId<'_>)> {
    rfind_event_item_internal(items, |item_with_id| f(item_with_id.inner))
}

/// Find the timeline item that matches the given event id, if any.
///
/// WARNING: Linear scan of the items, see documentation of
/// [`rfind_event_item`].
pub(super) fn rfind_event_by_id<'a>(
    items: &'a Vector<Arc<TimelineItem>>,
    event_id: &EventId,
) -> Option<(usize, EventTimelineItemWithId<'a>)> {
    rfind_event_item(items, |it| it.event_id() == Some(event_id))
}

/// Find the timeline item that matches the given item (event or transaction)
/// id, if any.
///
/// WARNING: Linear scan of the items, see documentation of
/// [`rfind_event_item`].
pub(super) fn rfind_event_by_item_id<'a>(
    items: &'a Vector<Arc<TimelineItem>>,
    item_id: &TimelineEventItemId,
) -> Option<(usize, EventTimelineItemWithId<'a>)> {
    match item_id {
        TimelineEventItemId::TransactionId(txn_id) => {
            rfind_event_item(items, |item| match &item.kind {
                EventTimelineItemKind::Local(local) => local.transaction_id == *txn_id,
                EventTimelineItemKind::Remote(remote) => {
                    remote.transaction_id.as_deref() == Some(txn_id)
                }
            })
        }
        TimelineEventItemId::EventId(event_id) => rfind_event_by_id(items, event_id),
    }
}
