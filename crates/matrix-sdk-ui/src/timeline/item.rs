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

use super::{EventTimelineItem, VirtualTimelineItem};

#[derive(Clone, Debug)]
#[allow(clippy::large_enum_variant)]
pub enum TimelineItemKind {
    /// An event or aggregation of multiple events.
    Event(EventTimelineItem),
    /// An item that doesn't correspond to an event, for example the user's
    /// own read marker.
    Virtual(VirtualTimelineItem),
}

/// A single entry in timeline.
#[derive(Clone, Debug)]
pub struct TimelineItem {
    pub(crate) kind: TimelineItemKind,
    pub(crate) internal_id: u64,
}

impl TimelineItem {
    pub(crate) fn with_kind(&self, kind: impl Into<TimelineItemKind>) -> Arc<Self> {
        Arc::new(Self { kind: kind.into(), internal_id: self.internal_id })
    }

    /// Get the [`TimelineItemKind`] of this item.
    pub fn kind(&self) -> &TimelineItemKind {
        &self.kind
    }

    /// Get the inner `EventTimelineItem`, if this is a
    /// [`TimelineItemKind::Event`].
    pub fn as_event(&self) -> Option<&EventTimelineItem> {
        match &self.kind {
            TimelineItemKind::Event(v) => Some(v),
            _ => None,
        }
    }

    /// Get the inner `VirtualTimelineItem`, if this is a
    /// [`TimelineItemKind::Virtual`].
    pub fn as_virtual(&self) -> Option<&VirtualTimelineItem> {
        match &self.kind {
            TimelineItemKind::Virtual(v) => Some(v),
            _ => None,
        }
    }

    /// Get a unique ID for this timeline item.
    ///
    /// It identifies the item on a best-effort basis. For instance, edits
    /// to an [`EventTimelineItem`] will not change the ID of the
    /// enclosing `TimelineItem`. For some virtual items like day
    /// dividers, identity isn't easy to define though and you might
    /// see a new ID getting generated for a day divider that you
    /// perceive to be "the same" as a previous one.
    pub fn unique_id(&self) -> u64 {
        self.internal_id
    }

    pub(crate) fn read_marker() -> Arc<TimelineItem> {
        Arc::new(Self {
            kind: TimelineItemKind::Virtual(VirtualTimelineItem::ReadMarker),
            internal_id: u64::MAX,
        })
    }

    pub(crate) fn is_virtual(&self) -> bool {
        matches!(self.kind, TimelineItemKind::Virtual(_))
    }

    pub(crate) fn is_day_divider(&self) -> bool {
        matches!(self.kind, TimelineItemKind::Virtual(VirtualTimelineItem::DayDivider(_)))
    }

    pub(crate) fn is_read_marker(&self) -> bool {
        matches!(self.kind, TimelineItemKind::Virtual(VirtualTimelineItem::ReadMarker))
    }
}

impl Deref for TimelineItem {
    type Target = TimelineItemKind;

    fn deref(&self) -> &Self::Target {
        &self.kind
    }
}

impl From<EventTimelineItem> for TimelineItemKind {
    fn from(item: EventTimelineItem) -> Self {
        Self::Event(item)
    }
}

impl From<VirtualTimelineItem> for TimelineItemKind {
    fn from(item: VirtualTimelineItem) -> Self {
        Self::Virtual(item)
    }
}

pub(crate) fn timeline_item(
    kind: impl Into<TimelineItemKind>,
    internal_id: u64,
) -> Arc<TimelineItem> {
    Arc::new(TimelineItem { kind: kind.into(), internal_id })
}
