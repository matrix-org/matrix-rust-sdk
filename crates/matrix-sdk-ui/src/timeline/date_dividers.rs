// Copyright 2024 The Matrix.org Foundation C.I.C.
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

//! Algorithm to adjust (insert/replace/remove) date dividers after new events
//! have been received from any source.

use std::{fmt::Display, sync::Arc};

use chrono::{Datelike, Local, TimeZone};
use ruma::MilliSecondsSinceUnixEpoch;
use tracing::{Level, error, event_enabled, instrument, trace, warn};

use super::{
    DateDividerMode, TimelineItem, TimelineItemKind, VirtualTimelineItem,
    controller::{ObservableItemsTransaction, TimelineMetadata},
};

#[derive(Debug, PartialEq)]
struct Date {
    year: i32,
    month: u32,
    day: u32,
}

impl Date {
    fn is_same_month_as(&self, date: Date) -> bool {
        self.year == date.year && self.month == date.month
    }
}

/// Converts a timestamp since Unix Epoch to a year, month and day.
fn timestamp_to_date(ts: MilliSecondsSinceUnixEpoch) -> Date {
    let datetime = Local
        .timestamp_millis_opt(ts.0.into())
        // Only returns `None` if date is after Dec 31, 262143 BCE.
        .single()
        // Fallback to the current date to avoid issues with malicious
        // homeservers.
        .unwrap_or_else(Local::now);

    Date { year: datetime.year(), month: datetime.month(), day: datetime.day() }
}

/// Algorithm ensuring that date dividers are adjusted correctly, according to
/// new items that have been inserted.
pub(super) struct DateDividerAdjuster {
    /// The list of recorded operations to apply, after analyzing the latest
    /// items.
    ops: Vec<DateDividerOperation>,

    /// A boolean indicating whether the struct has been used and thus must be
    /// mark unused manually by calling [`Self::run`].
    consumed: bool,

    mode: DateDividerMode,
}

impl Drop for DateDividerAdjuster {
    fn drop(&mut self) {
        // Only run the assert if we're not currently panicking.
        if !std::thread::panicking() && !self.consumed {
            error!("a DateDividerAdjuster has not been consumed with run()");
        }
    }
}

/// A descriptor for a previous item.
struct PrevItemDesc<'a> {
    /// The index of the item in the `self.items` array.
    item_index: usize,

    /// The previous timeline item.
    item: &'a Arc<TimelineItem>,

    // The insert position of the operation in the `ops` array.
    insert_op_at: usize,
}

impl DateDividerAdjuster {
    pub fn new(mode: DateDividerMode) -> Self {
        Self {
            ops: Default::default(),
            // The adjuster starts as consumed, and it will be marked no consumed iff it's used
            // with `mark_used`.
            consumed: true,
            mode,
        }
    }

    /// Marks this [`DateDividerAdjuster`] as used, which means it'll require a
    /// call to [`DateDividerAdjuster::run`] before getting dropped.
    pub fn mark_used(&mut self) {
        // Mark the adjuster as needing to be consumed.
        self.consumed = false;
    }

    /// Ensures that date separators are properly inserted/removed when needs
    /// be.
    #[instrument(skip_all)]
    pub fn run(&mut self, items: &mut ObservableItemsTransaction<'_>, meta: &mut TimelineMetadata) {
        // We're going to record vector operations like inserting, replacing and
        // removing date dividers. Since we may remove or insert new items,
        // recorded offsets will change as we're iterating over the array. The
        // only way this is possible is because we're recording operations
        // happening in non-decreasing order of the indices, i.e. we can't do an
        // operation on index I and then on any index J<I later.
        //
        // Note we can't "just" iterate in reverse order, because we may have a
        // `Remove(i)` followed by a `Replace((i+1) -1)`, which wouldn't do what
        // we want, if running in reverse order.
        //
        // Also note that we can remove a few items at position J, then later decide to
        // replace/remove an item (in `handle_event`) at position I, with I<J. That
        // would break the above invariant (that operations happen in
        // non-decreasing order of the indices), so we must record the insert
        // position for an operation related to the previous item.

        let mut prev_item: Option<PrevItemDesc<'_>> = None;
        let mut latest_event_ts = None;

        for (i, item) in items.iter_remotes_and_locals_regions() {
            match item.kind() {
                TimelineItemKind::Virtual(VirtualTimelineItem::DateDivider(ts)) => {
                    // Record what the last alive item pair is only if we haven't removed the date
                    // divider.
                    if !self.handle_date_divider(i, *ts, prev_item.as_ref().map(|desc| desc.item)) {
                        prev_item = Some(PrevItemDesc {
                            item_index: i,
                            item,
                            insert_op_at: self.ops.len(),
                        });
                    }
                }

                TimelineItemKind::Event(event) => {
                    let ts = event.timestamp();

                    self.handle_event(i, ts, prev_item, latest_event_ts);

                    prev_item =
                        Some(PrevItemDesc { item_index: i, item, insert_op_at: self.ops.len() });
                    latest_event_ts = Some(ts);
                }

                TimelineItemKind::Virtual(VirtualTimelineItem::ReadMarker)
                | TimelineItemKind::Virtual(VirtualTimelineItem::TimelineStart) => {
                    // Nothing to do.
                }
            }
        }

        // Also chase trailing date dividers explicitly, by iterating from the end to
        // the start. Since they wouldn't be the prev_item of anything, we
        // wouldn't analyze them in the previous loop.
        for (i, item) in items.iter_remotes_and_locals_regions().rev() {
            if item.is_date_divider() {
                // The item is a trailing date divider: remove it, if it wasn't already
                // scheduled for deletion.
                if !self
                    .ops
                    .iter()
                    .any(|op| matches!(op, DateDividerOperation::Remove(j) if i == *j))
                {
                    trace!("removing trailing date divider @ {i}");

                    // Find the index at which to insert the removal operation. It must be before
                    // any other operation on a bigger index, to maintain the
                    // non-decreasing invariant.
                    let index =
                        self.ops.iter().position(|op| op.index() > i).unwrap_or(self.ops.len());

                    self.ops.insert(index, DateDividerOperation::Remove(i));
                }
            }

            if item.is_event() {
                // Stop as soon as we run into the first (trailing) event.
                break;
            }
        }

        // Only record the initial state if we've enabled the trace log level, and not
        // otherwise.
        let initial_state = if event_enabled!(Level::TRACE) {
            Some(
                items
                    .iter_remotes_and_locals_regions()
                    .map(|(_i, timeline_item)| timeline_item.clone())
                    .collect(),
            )
        } else {
            None
        };

        self.process_ops(items, meta);

        // Then check invariants.
        if let Some(report) = self.check_invariants(items, initial_state) {
            error!(sentry = true, %report, "day divider invariants violated");
            #[cfg(any(debug_assertions, test))]
            panic!("There was an error checking date separator invariants");
        }

        self.consumed = true;
    }

    /// Decides what to do with a date divider.
    ///
    /// Returns whether it's been removed or not.
    #[inline]
    fn handle_date_divider(
        &mut self,
        i: usize,
        ts: MilliSecondsSinceUnixEpoch,
        prev_item: Option<&Arc<TimelineItem>>,
    ) -> bool {
        let Some(prev_item) = prev_item else {
            // No interesting item prior to the date divider: it must be the first one,
            // nothing to do.
            return false;
        };

        match prev_item.kind() {
            TimelineItemKind::Event(event) => {
                // This date divider is preceded by an event.
                if self.is_same_date_divider_group_as(event.timestamp(), ts) {
                    // The event has the same date as the date divider: remove the current date
                    // divider.
                    trace!("removing date divider following event with same timestamp @ {i}");
                    self.ops.push(DateDividerOperation::Remove(i));
                    return true;
                }
            }

            TimelineItemKind::Virtual(VirtualTimelineItem::DateDivider(_)) => {
                trace!("removing duplicate date divider @ {i}");
                // This date divider is preceded by another one: remove the current one.
                self.ops.push(DateDividerOperation::Remove(i));
                return true;
            }

            TimelineItemKind::Virtual(VirtualTimelineItem::ReadMarker)
            | TimelineItemKind::Virtual(VirtualTimelineItem::TimelineStart) => {
                // Nothing to do.
            }
        }

        false
    }

    #[inline]
    fn handle_event(
        &mut self,
        i: usize,
        ts: MilliSecondsSinceUnixEpoch,
        prev_item_desc: Option<PrevItemDesc<'_>>,
        latest_event_ts: Option<MilliSecondsSinceUnixEpoch>,
    ) {
        let Some(PrevItemDesc { item_index, insert_op_at, item }) = prev_item_desc else {
            // The event was the first item, so there wasn't any date divider before it:
            // insert one.
            trace!("inserting the first date divider @ {}", i);
            self.ops.push(DateDividerOperation::Insert(i, ts));
            return;
        };

        match item.kind() {
            TimelineItemKind::Event(prev_event) => {
                // The event is preceded by another event. If they're not the same date,
                // insert a date divider.
                let prev_ts = prev_event.timestamp();

                if !self.is_same_date_divider_group_as(prev_ts, ts) {
                    trace!(
                        "inserting date divider @ {} between two events with different dates",
                        i
                    );
                    self.ops.push(DateDividerOperation::Insert(i, ts));
                }
            }

            TimelineItemKind::Virtual(VirtualTimelineItem::DateDivider(prev_ts)) => {
                let event_date = timestamp_to_date(ts);

                // The event is preceded by a date divider.
                if timestamp_to_date(*prev_ts) != event_date {
                    // The date divider is wrong. Should we replace it with the correct value, or
                    // remove it entirely?
                    if let Some(last_event_ts) = latest_event_ts
                        && timestamp_to_date(last_event_ts) == event_date
                    {
                        // There's a previous event with the same date: remove the divider.
                        trace!(
                            "removed date divider @ {item_index} between two events \
                                 that have the same date"
                        );
                        self.ops.insert(insert_op_at, DateDividerOperation::Remove(item_index));
                        return;
                    }

                    // There's no previous event or there's one with a different date: replace
                    // the current divider.
                    trace!("replacing date divider @ {item_index} with new timestamp from event");
                    self.ops.insert(insert_op_at, DateDividerOperation::Replace(item_index, ts));
                }
            }

            TimelineItemKind::Virtual(VirtualTimelineItem::ReadMarker)
            | TimelineItemKind::Virtual(VirtualTimelineItem::TimelineStart) => {
                // Nothing to do.
            }
        }
    }

    fn process_ops(&self, items: &mut ObservableItemsTransaction<'_>, meta: &mut TimelineMetadata) {
        // Record the deletion offset.
        let mut offset = 0i64;
        // Remember what the maximum index was, so we can assert that it's
        // non-decreasing.
        let mut max_i = 0;

        for op in &self.ops {
            match *op {
                DateDividerOperation::Insert(i, ts) => {
                    assert!(i >= max_i, "trying to insert at {i} < max_i={max_i}");

                    let at = (i64::try_from(i).unwrap() + offset)
                        .min(i64::try_from(items.len()).unwrap());
                    assert!(at >= 0);
                    let at = at as usize;

                    items.push_date_divider(
                        at,
                        meta.new_timeline_item(VirtualTimelineItem::DateDivider(ts)),
                    );

                    offset += 1;
                    max_i = i;
                }

                DateDividerOperation::Replace(i, ts) => {
                    assert!(i >= max_i, "trying to replace at {i} < max_i={max_i}");

                    let at = i64::try_from(i).unwrap() + offset;
                    assert!(at >= 0);
                    let at = at as usize;

                    let replaced = &items[at];
                    if !replaced.is_date_divider() {
                        error!("we replaced a non date-divider @ {i}: {:?}", replaced.kind());
                    }

                    let unique_id = replaced.unique_id();
                    let item = TimelineItem::new(
                        VirtualTimelineItem::DateDivider(ts),
                        unique_id.to_owned(),
                    );

                    items.replace(at, item);
                    max_i = i;
                }

                DateDividerOperation::Remove(i) => {
                    assert!(i >= max_i, "trying to replace at {i} < max_i={max_i}");

                    let at = i64::try_from(i).unwrap() + offset;
                    assert!(at >= 0);

                    let removed = items.remove(at as usize);
                    if !removed.is_date_divider() {
                        error!("we removed a non date-divider @ {i}: {:?}", removed.kind());
                    }

                    offset -= 1;
                    max_i = i;
                }
            }
        }
    }

    /// Checks the invariants that must hold at any time after inserting date
    /// dividers.
    ///
    /// Returns a report if and only if there was at least one error.
    fn check_invariants<'a, 'o>(
        &mut self,
        items: &'a ObservableItemsTransaction<'o>,
        initial_state: Option<Vec<Arc<TimelineItem>>>,
    ) -> Option<DateDividerInvariantsReport<'a, 'o>> {
        let mut report = DateDividerInvariantsReport {
            initial_state,
            errors: Vec::new(),
            operations: std::mem::take(&mut self.ops),
            final_state: items,
        };

        // Assert invariants.
        // 1. The timeline starts with a date divider, if it's not only virtual items.
        {
            let mut i = items.first_remotes_region_index();
            while let Some(item) = items.get(i) {
                if let Some(virt) = item.as_virtual() {
                    if matches!(virt, VirtualTimelineItem::DateDivider(_)) {
                        // We found a date divider among the first virtual items: stop here.
                        break;
                    }
                } else {
                    // We found an event, but we didn't have a date divider: report an error.
                    report.errors.push(DateDividerInsertError::FirstItemNotDateDivider);
                    break;
                }
                i += 1;
            }
        }

        // 2. There are no two date dividers following each other.
        {
            let mut prev_was_date_divider = false;
            for (i, item) in items.iter_remotes_and_locals_regions() {
                if item.is_date_divider() {
                    if prev_was_date_divider {
                        report.errors.push(DateDividerInsertError::DuplicateDateDivider { at: i });
                    }
                    prev_was_date_divider = true;
                } else {
                    prev_was_date_divider = false;
                }
            }
        };

        // 3. There's no trailing date divider.
        if let Some(last) = items.last()
            && last.is_date_divider()
        {
            report.errors.push(DateDividerInsertError::TrailingDateDivider);
        }

        // 4. Items are properly separated with date dividers.
        {
            let mut prev_event_ts = None;
            let mut prev_date_divider_ts = None;

            for (i, item) in items.iter_remotes_and_locals_regions() {
                if let Some(ev) = item.as_event() {
                    let ts = ev.timestamp();

                    // We have the same date as the previous event we've seen.
                    if let Some(prev_ts) = prev_event_ts
                        && !self.is_same_date_divider_group_as(prev_ts, ts)
                    {
                        report.errors.push(
                            DateDividerInsertError::MissingDateDividerBetweenEvents { at: i },
                        );
                    }

                    // There is a date divider before us, and it's the same date as our timestamp.
                    if let Some(prev_ts) = prev_date_divider_ts {
                        if !self.is_same_date_divider_group_as(prev_ts, ts) {
                            report.errors.push(
                                DateDividerInsertError::InconsistentDateAfterPreviousDateDivider {
                                    at: i,
                                },
                            );
                        }
                    } else {
                        report
                            .errors
                            .push(DateDividerInsertError::MissingDateDividerBeforeEvent { at: i });
                    }

                    prev_event_ts = Some(ts);
                } else if let TimelineItemKind::Virtual(VirtualTimelineItem::DateDivider(ts)) =
                    item.kind()
                {
                    // The previous date divider is for a different date.
                    if let Some(prev_ts) = prev_date_divider_ts
                        && self.is_same_date_divider_group_as(prev_ts, *ts)
                    {
                        report.errors.push(DateDividerInsertError::DuplicateDateDivider { at: i });
                    }

                    prev_event_ts = None;
                    prev_date_divider_ts = Some(*ts);
                }
            }
        }

        // 5. If there was a read marker at the beginning, there should be one at the
        //    end.
        if let Some(state) = &report.initial_state
            && state.iter().any(|item| item.is_read_marker())
            && !report
                .final_state
                .iter_remotes_and_locals_regions()
                .any(|(_i, item)| item.is_read_marker())
        {
            report.errors.push(DateDividerInsertError::ReadMarkerDisappeared);
        }

        if report.errors.is_empty() { None } else { Some(report) }
    }

    /// Returns whether the two dates for the given timestamps are the same or
    /// not.
    fn is_same_date_divider_group_as(
        &self,
        lhs: MilliSecondsSinceUnixEpoch,
        rhs: MilliSecondsSinceUnixEpoch,
    ) -> bool {
        match self.mode {
            DateDividerMode::Daily => timestamp_to_date(lhs) == timestamp_to_date(rhs),
            DateDividerMode::Monthly => {
                timestamp_to_date(lhs).is_same_month_as(timestamp_to_date(rhs))
            }
        }
    }
}

#[derive(Debug)]
enum DateDividerOperation {
    Insert(usize, MilliSecondsSinceUnixEpoch),
    Replace(usize, MilliSecondsSinceUnixEpoch),
    Remove(usize),
}

impl DateDividerOperation {
    fn index(&self) -> usize {
        match self {
            DateDividerOperation::Insert(i, _)
            | DateDividerOperation::Replace(i, _)
            | DateDividerOperation::Remove(i) => *i,
        }
    }
}

/// A report returned by [`DateDividerAdjuster::check_invariants`].
struct DateDividerInvariantsReport<'a, 'o> {
    /// Initial state before inserting the items.
    initial_state: Option<Vec<Arc<TimelineItem>>>,
    /// The operations that have been applied on the list.
    operations: Vec<DateDividerOperation>,
    /// Final state after inserting the date dividers.
    final_state: &'a ObservableItemsTransaction<'o>,
    /// Errors encountered in the algorithm.
    errors: Vec<DateDividerInsertError>,
}

impl Display for DateDividerInvariantsReport<'_, '_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Write all the items of a slice of timeline items.
        fn write_items(
            f: &mut std::fmt::Formatter<'_>,
            items: &[Arc<TimelineItem>],
        ) -> std::fmt::Result {
            for (i, item) in items.iter().enumerate() {
                if let TimelineItemKind::Virtual(VirtualTimelineItem::DateDivider(ts)) = item.kind()
                {
                    writeln!(f, "#{i} --- {}", ts.0)?;
                } else if let Some(event) = item.as_event() {
                    // id: timestamp
                    writeln!(
                        f,
                        "#{i} {}: {}",
                        event
                            .event_id()
                            .map_or_else(|| "(no event id)".to_owned(), |id| id.to_string()),
                        event.timestamp().0
                    )?;
                } else {
                    writeln!(f, "#{i} (other virtual item)")?;
                }
            }

            Ok(())
        }

        if let Some(initial_state) = &self.initial_state {
            writeln!(f, "Initial state:")?;
            write_items(f, initial_state)?;

            writeln!(f, "\nOperations to apply:")?;
            for op in &self.operations {
                match *op {
                    DateDividerOperation::Insert(i, ts) => writeln!(f, "insert @ {i}: {}", ts.0)?,
                    DateDividerOperation::Replace(i, ts) => writeln!(f, "replace @ {i}: {}", ts.0)?,
                    DateDividerOperation::Remove(i) => writeln!(f, "remove @ {i}")?,
                }
            }

            writeln!(f, "\nFinal state:")?;
            write_items(
                f,
                self.final_state
                    .iter_remotes_and_locals_regions()
                    .map(|(_i, item)| item.clone())
                    .collect::<Vec<_>>()
                    .as_slice(),
            )?;

            writeln!(f)?;
        }

        for err in &self.errors {
            writeln!(f, "{err}")?;
        }

        Ok(())
    }
}

#[derive(Debug, thiserror::Error)]
enum DateDividerInsertError {
    /// The first item isn't a date divider.
    #[error("The first item isn't a date divider")]
    FirstItemNotDateDivider,

    /// There are two date dividers for the same date.
    #[error("Duplicate date divider @ {at}.")]
    DuplicateDateDivider { at: usize },

    /// The last item is a date divider.
    #[error("The last item is a date divider.")]
    TrailingDateDivider,

    /// Two events are following each other but they have different dates
    /// without a date divider between them.
    #[error("Missing date divider between events @ {at}")]
    MissingDateDividerBetweenEvents { at: usize },

    /// Some event is missing a date divider before it.
    #[error("Missing date divider before event @ {at}")]
    MissingDateDividerBeforeEvent { at: usize },

    /// An event and the previous date divider aren't focused on the same date.
    #[error("Event @ {at} and the previous date divider aren't targeting the same date")]
    InconsistentDateAfterPreviousDateDivider { at: usize },

    /// The read marker has been removed.
    #[error("The read marker has been removed")]
    ReadMarkerDisappeared,
}

#[cfg(test)]
mod tests {
    use assert_matches2::assert_let;
    use ruma::{
        MilliSecondsSinceUnixEpoch, owned_event_id, owned_user_id,
        room_version_rules::RoomVersionRules, uint,
    };

    use super::{super::controller::ObservableItems, DateDividerAdjuster};
    use crate::timeline::{
        DateDividerMode, EventTimelineItem, MsgLikeContent, TimelineItemContent,
        VirtualTimelineItem,
        controller::TimelineMetadata,
        date_dividers::timestamp_to_date,
        event_item::{EventTimelineItemKind, RemoteEventTimelineItem},
    };

    fn event_with_ts(timestamp: MilliSecondsSinceUnixEpoch) -> EventTimelineItem {
        let event_kind = EventTimelineItemKind::Remote(RemoteEventTimelineItem {
            event_id: owned_event_id!("$1"),
            transaction_id: None,
            read_receipts: Default::default(),
            is_own: false,
            is_highlighted: false,
            encryption_info: None,
            original_json: None,
            latest_edit_json: None,
            origin: crate::timeline::event_item::RemoteEventOrigin::Sync,
        });
        EventTimelineItem::new(
            owned_user_id!("@alice:example.org"),
            crate::timeline::TimelineDetails::Pending,
            None,
            None,
            timestamp,
            TimelineItemContent::MsgLike(MsgLikeContent::redacted()),
            event_kind,
            false,
        )
    }

    fn test_metadata() -> TimelineMetadata {
        TimelineMetadata::new(owned_user_id!("@a:b.c"), RoomVersionRules::V11, None, None, false)
    }

    #[test]
    fn test_no_trailing_date_divider() {
        let mut items = ObservableItems::new();
        let mut txn = items.transaction();

        let mut meta = test_metadata();

        let timestamp = MilliSecondsSinceUnixEpoch(uint!(42));
        let timestamp_next_day =
            MilliSecondsSinceUnixEpoch((42 + 3600 * 24 * 1000).try_into().unwrap());

        txn.push_back(meta.new_timeline_item(event_with_ts(timestamp)), None);
        txn.push_back(
            meta.new_timeline_item(VirtualTimelineItem::DateDivider(timestamp_next_day)),
            None,
        );
        txn.push_back(meta.new_timeline_item(VirtualTimelineItem::ReadMarker), None);

        let mut adjuster = DateDividerAdjuster::new(DateDividerMode::Daily);
        adjuster.run(&mut txn, &mut meta);

        txn.commit();

        let mut iter = items.iter();

        assert_let!(Some(item) = iter.next());
        assert!(item.is_date_divider());

        assert_let!(Some(item) = iter.next());
        assert!(item.is_remote_event());

        assert_let!(Some(item) = iter.next());
        assert!(item.is_read_marker());

        assert!(iter.next().is_none());
    }

    #[test]
    fn test_read_marker_in_between_event_and_date_divider() {
        let mut items = ObservableItems::new();
        let mut txn = items.transaction();

        let mut meta = test_metadata();

        let timestamp = MilliSecondsSinceUnixEpoch(uint!(42));
        let timestamp_next_day =
            MilliSecondsSinceUnixEpoch((42 + 3600 * 24 * 1000).try_into().unwrap());
        assert_ne!(timestamp_to_date(timestamp), timestamp_to_date(timestamp_next_day));

        let event = event_with_ts(timestamp);
        txn.push_back(meta.new_timeline_item(event.clone()), None);
        txn.push_back(
            meta.new_timeline_item(VirtualTimelineItem::DateDivider(timestamp_next_day)),
            None,
        );
        txn.push_back(meta.new_timeline_item(VirtualTimelineItem::ReadMarker), None);
        txn.push_back(meta.new_timeline_item(event), None);

        let mut adjuster = DateDividerAdjuster::new(DateDividerMode::Daily);
        adjuster.run(&mut txn, &mut meta);

        txn.commit();

        let mut iter = items.iter();

        assert!(iter.next().unwrap().is_date_divider());
        assert!(iter.next().unwrap().is_remote_event());
        assert!(iter.next().unwrap().is_read_marker());
        assert!(iter.next().unwrap().is_remote_event());
        assert!(iter.next().is_none());
    }

    #[test]
    fn test_read_marker_in_between_date_dividers() {
        let mut items = ObservableItems::new();
        let mut txn = items.transaction();

        let mut meta = test_metadata();

        let timestamp = MilliSecondsSinceUnixEpoch(uint!(42));
        let timestamp_next_day =
            MilliSecondsSinceUnixEpoch((42 + 3600 * 24 * 1000).try_into().unwrap());
        assert_ne!(timestamp_to_date(timestamp), timestamp_to_date(timestamp_next_day));

        txn.push_back(meta.new_timeline_item(event_with_ts(timestamp)), None);
        txn.push_back(meta.new_timeline_item(VirtualTimelineItem::DateDivider(timestamp)), None);
        txn.push_back(meta.new_timeline_item(VirtualTimelineItem::DateDivider(timestamp)), None);
        txn.push_back(meta.new_timeline_item(VirtualTimelineItem::ReadMarker), None);
        txn.push_back(meta.new_timeline_item(VirtualTimelineItem::DateDivider(timestamp)), None);
        txn.push_back(meta.new_timeline_item(event_with_ts(timestamp_next_day)), None);

        let mut adjuster = DateDividerAdjuster::new(DateDividerMode::Daily);
        adjuster.run(&mut txn, &mut meta);

        txn.commit();

        let mut iter = items.iter();

        assert!(iter.next().unwrap().is_date_divider());
        assert!(iter.next().unwrap().is_remote_event());
        assert!(iter.next().unwrap().is_read_marker());
        assert!(iter.next().unwrap().is_date_divider());
        assert!(iter.next().unwrap().is_remote_event());
        assert!(iter.next().is_none());
    }

    #[test]
    fn test_remove_all_date_dividers() {
        let mut items = ObservableItems::new();
        let mut txn = items.transaction();

        let mut meta = test_metadata();

        let timestamp = MilliSecondsSinceUnixEpoch(uint!(42));
        let timestamp_next_day =
            MilliSecondsSinceUnixEpoch((42 + 3600 * 24 * 1000).try_into().unwrap());
        assert_ne!(timestamp_to_date(timestamp), timestamp_to_date(timestamp_next_day));

        txn.push_back(meta.new_timeline_item(event_with_ts(timestamp_next_day)), None);
        txn.push_back(meta.new_timeline_item(VirtualTimelineItem::DateDivider(timestamp)), None);
        txn.push_back(meta.new_timeline_item(VirtualTimelineItem::DateDivider(timestamp)), None);
        txn.push_back(meta.new_timeline_item(event_with_ts(timestamp_next_day)), None);

        let mut adjuster = DateDividerAdjuster::new(DateDividerMode::Daily);
        adjuster.run(&mut txn, &mut meta);

        txn.commit();

        let mut iter = items.iter();

        assert!(iter.next().unwrap().is_date_divider());
        assert!(iter.next().unwrap().is_remote_event());
        assert!(iter.next().unwrap().is_remote_event());
        assert!(iter.next().is_none());
    }

    #[test]
    fn test_event_read_marker_spurious_date_divider() {
        let mut items = ObservableItems::new();
        let mut txn = items.transaction();

        let mut meta = test_metadata();

        let timestamp = MilliSecondsSinceUnixEpoch(uint!(42));

        txn.push_back(meta.new_timeline_item(event_with_ts(timestamp)), None);
        txn.push_back(meta.new_timeline_item(VirtualTimelineItem::ReadMarker), None);
        txn.push_back(meta.new_timeline_item(VirtualTimelineItem::DateDivider(timestamp)), None);

        let mut adjuster = DateDividerAdjuster::new(DateDividerMode::Daily);
        adjuster.run(&mut txn, &mut meta);

        txn.commit();

        let mut iter = items.iter();

        assert!(iter.next().unwrap().is_date_divider());
        assert!(iter.next().unwrap().is_remote_event());
        assert!(iter.next().unwrap().is_read_marker());
        assert!(iter.next().is_none());
    }

    #[test]
    fn test_multiple_trailing_date_dividers() {
        let mut items = ObservableItems::new();
        let mut txn = items.transaction();

        let mut meta = test_metadata();

        let timestamp = MilliSecondsSinceUnixEpoch(uint!(42));

        txn.push_back(meta.new_timeline_item(VirtualTimelineItem::ReadMarker), None);
        txn.push_back(meta.new_timeline_item(VirtualTimelineItem::DateDivider(timestamp)), None);
        txn.push_back(meta.new_timeline_item(VirtualTimelineItem::DateDivider(timestamp)), None);

        let mut adjuster = DateDividerAdjuster::new(DateDividerMode::Daily);
        adjuster.run(&mut txn, &mut meta);

        txn.commit();

        let mut iter = items.iter();

        assert!(iter.next().unwrap().is_read_marker());
        assert!(iter.next().is_none());
    }

    #[test]
    fn test_start_with_read_marker() {
        let mut items = ObservableItems::new();
        let mut txn = items.transaction();

        let mut meta = test_metadata();
        let timestamp = MilliSecondsSinceUnixEpoch(uint!(42));

        txn.push_back(meta.new_timeline_item(VirtualTimelineItem::ReadMarker), None);
        txn.push_back(meta.new_timeline_item(event_with_ts(timestamp)), None);

        let mut adjuster = DateDividerAdjuster::new(DateDividerMode::Daily);
        adjuster.run(&mut txn, &mut meta);

        txn.commit();

        let mut iter = items.iter();

        assert!(iter.next().unwrap().is_read_marker());
        assert!(iter.next().unwrap().is_date_divider());
        assert!(iter.next().unwrap().is_remote_event());
        assert!(iter.next().is_none());
    }

    #[test]
    fn test_daily_divider_mode() {
        let mut items = ObservableItems::new();
        let mut txn = items.transaction();

        let mut meta = test_metadata();

        txn.push_back(
            meta.new_timeline_item(event_with_ts(MilliSecondsSinceUnixEpoch(uint!(0)))),
            None,
        );
        txn.push_back(
            meta.new_timeline_item(event_with_ts(MilliSecondsSinceUnixEpoch(uint!(86_400_000)))), // One day later
            None,
        );
        txn.push_back(
            meta.new_timeline_item(event_with_ts(MilliSecondsSinceUnixEpoch(uint!(2_678_400_000)))), // One month later
            None,
        );

        let mut adjuster = DateDividerAdjuster::new(DateDividerMode::Daily);
        adjuster.run(&mut txn, &mut meta);

        txn.commit();

        let mut iter = items.iter();

        assert!(iter.next().unwrap().is_date_divider());
        assert!(iter.next().unwrap().is_remote_event());
        assert!(iter.next().unwrap().is_date_divider());
        assert!(iter.next().unwrap().is_remote_event());
        assert!(iter.next().unwrap().is_date_divider());
        assert!(iter.next().unwrap().is_remote_event());
        assert!(iter.next().is_none());
    }

    #[test]
    fn test_monthly_divider_mode() {
        let mut items = ObservableItems::new();
        let mut txn = items.transaction();

        let mut meta = test_metadata();

        txn.push_back(
            // Start one day later than the origin, to make this test pass on all timezones.
            // Let's call this time T.
            meta.new_timeline_item(event_with_ts(MilliSecondsSinceUnixEpoch(uint!(86_400_000)))),
            None,
        );
        txn.push_back(
            // One day later (T+1 day).
            meta.new_timeline_item(event_with_ts(MilliSecondsSinceUnixEpoch(uint!(172_800_000)))),
            None,
        );
        txn.push_back(
            // One month later (T+31 days).
            meta.new_timeline_item(event_with_ts(MilliSecondsSinceUnixEpoch(uint!(2_764_800_000)))),
            None,
        );

        let mut adjuster = DateDividerAdjuster::new(DateDividerMode::Monthly);
        adjuster.run(&mut txn, &mut meta);

        txn.commit();

        let mut iter = items.iter();

        assert!(iter.next().unwrap().is_date_divider());
        assert!(iter.next().unwrap().is_remote_event());
        assert!(iter.next().unwrap().is_remote_event());
        assert!(iter.next().unwrap().is_date_divider());
        assert!(iter.next().unwrap().is_remote_event());
        assert!(iter.next().is_none());
    }
}
