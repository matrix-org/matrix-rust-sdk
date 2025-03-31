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

use as_variant::as_variant;

use super::{EventTimelineItem, VirtualTimelineItem};

/// Opaque unique identifier for a timeline item.
///
/// It is transferred whenever a timeline item is updated. This can be used as a
/// stable identifier for UI purposes, as well as operations on the event
/// represented by the item.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct TimelineUniqueId(pub String);

/// The type of timeline item.
#[derive(Clone, Debug)]
#[allow(clippy::large_enum_variant)]
pub enum TimelineItemKind {
    /// An event or aggregation of multiple events.
    Event(EventTimelineItem),
    /// An item that doesn't correspond to an event, for example the user's
    /// own read marker, or a date divider.
    Virtual(VirtualTimelineItem),
}

/// A single entry in timeline.
#[derive(Clone, Debug)]
pub struct TimelineItem {
    pub(crate) kind: TimelineItemKind,
    pub(crate) internal_id: TimelineUniqueId,
}

impl TimelineItem {
    /// Create a new `TimelineItem` with the given kind and internal id.
    pub(crate) fn new(
        kind: impl Into<TimelineItemKind>,
        internal_id: TimelineUniqueId,
    ) -> Arc<Self> {
        Arc::new(TimelineItem { kind: kind.into(), internal_id })
    }

    /// Create a clone of the current `TimelineItem` with the given kind.
    pub(crate) fn with_kind(&self, kind: impl Into<TimelineItemKind>) -> Arc<Self> {
        Arc::new(Self { kind: kind.into(), internal_id: self.internal_id.clone() })
    }

    /// Get the [`TimelineItemKind`] of this item.
    pub fn kind(&self) -> &TimelineItemKind {
        &self.kind
    }

    /// Get the inner `EventTimelineItem`, if this is a
    /// [`TimelineItemKind::Event`].
    pub fn as_event(&self) -> Option<&EventTimelineItem> {
        as_variant!(&self.kind, TimelineItemKind::Event)
    }

    /// Get the inner `VirtualTimelineItem`, if this is a
    /// [`TimelineItemKind::Virtual`].
    pub fn as_virtual(&self) -> Option<&VirtualTimelineItem> {
        as_variant!(&self.kind, TimelineItemKind::Virtual)
    }

    /// Get a unique ID for this timeline item.
    ///
    /// It identifies the item on a best-effort basis. For instance, edits
    /// to an [`EventTimelineItem`] will not change the ID of the
    /// enclosing `TimelineItem`. For some virtual items like date
    /// dividers, identity isn't easy to define though and you might
    /// see a new ID getting generated for a date divider that you
    /// perceive to be "the same" as a previous one.
    pub fn unique_id(&self) -> &TimelineUniqueId {
        &self.internal_id
    }

    pub(crate) fn read_marker() -> Arc<TimelineItem> {
        Arc::new(Self {
            kind: TimelineItemKind::Virtual(VirtualTimelineItem::ReadMarker),
            internal_id: TimelineUniqueId("__read_marker".to_owned()),
        })
    }

    pub(crate) fn is_local_echo(&self) -> bool {
        matches!(&self.kind, TimelineItemKind::Event(ev) if ev.is_local_echo())
    }

    pub(crate) fn is_remote_event(&self) -> bool {
        matches!(&self.kind, TimelineItemKind::Event(ev) if ev.is_remote_event())
    }

    pub(crate) fn is_event(&self) -> bool {
        matches!(&self.kind, TimelineItemKind::Event(_))
    }

    /// Check whether this item is a (virtual) date divider.
    pub fn is_date_divider(&self) -> bool {
        matches!(self.kind, TimelineItemKind::Virtual(VirtualTimelineItem::DateDivider(_)))
    }

    pub(crate) fn is_read_marker(&self) -> bool {
        matches!(self.kind, TimelineItemKind::Virtual(VirtualTimelineItem::ReadMarker))
    }

    /// Check whether this item is a (virtual) timeline start item.
    pub fn is_timeline_start(&self) -> bool {
        matches!(self.kind, TimelineItemKind::Virtual(VirtualTimelineItem::TimelineStart))
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
