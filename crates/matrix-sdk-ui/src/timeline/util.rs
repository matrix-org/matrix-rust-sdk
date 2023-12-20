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

use chrono::{Datelike, Local, TimeZone};
use imbl::Vector;
use ruma::{EventId, MilliSecondsSinceUnixEpoch};

use super::{event_item::EventTimelineItemKind, EventTimelineItem, TimelineItem};

pub(super) struct EventTimelineItemWithId<'a> {
    pub inner: &'a EventTimelineItem,
    pub internal_id: u64,
}

impl<'a> EventTimelineItemWithId<'a> {
    pub fn with_inner_kind(&self, kind: impl Into<EventTimelineItemKind>) -> Arc<TimelineItem> {
        Arc::new(TimelineItem {
            kind: self.inner.with_kind(kind).into(),
            internal_id: self.internal_id,
        })
    }
}

impl Deref for EventTimelineItemWithId<'_> {
    type Target = EventTimelineItem;

    fn deref(&self) -> &Self::Target {
        self.inner
    }
}

// FIXME: Put an upper bound on timeline size or add a separate map to look up
// the index of a timeline item by its key, to avoid large linear scans.
pub(super) fn rfind_event_item(
    items: &Vector<Arc<TimelineItem>>,
    mut f: impl FnMut(&EventTimelineItem) -> bool,
) -> Option<(usize, EventTimelineItemWithId<'_>)> {
    items
        .iter()
        .enumerate()
        .filter_map(|(idx, item)| {
            Some((
                idx,
                EventTimelineItemWithId { inner: item.as_event()?, internal_id: item.internal_id },
            ))
        })
        .rfind(|(_, it)| f(it.inner))
}

pub(super) fn rfind_event_by_id<'a>(
    items: &'a Vector<Arc<TimelineItem>>,
    event_id: &EventId,
) -> Option<(usize, EventTimelineItemWithId<'a>)> {
    rfind_event_item(items, |it| it.event_id() == Some(event_id))
}

pub(super) fn find_read_marker(items: &Vector<Arc<TimelineItem>>) -> Option<usize> {
    items.iter().rposition(|item| item.is_read_marker())
}

/// Result of comparing events position in the timeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RelativePosition {
    /// Event B is after (more recent than) event A.
    After,
    /// They are the same event.
    Same,
    /// Event B is before (older than) event A.
    Before,
}

#[derive(PartialEq)]
pub(super) struct Date {
    year: i32,
    month: u32,
    day: u32,
}

/// Converts a timestamp since Unix Epoch to a year, month and day.
pub(super) fn timestamp_to_date(ts: MilliSecondsSinceUnixEpoch) -> Date {
    let datetime = Local
        .timestamp_millis_opt(ts.0.into())
        // Only returns `None` if date is after Dec 31, 262143 BCE.
        .single()
        // Fallback to the current date to avoid issues with malicious
        // homeservers.
        .unwrap_or_else(Local::now);

    Date { year: datetime.year(), month: datetime.month(), day: datetime.day() }
}
