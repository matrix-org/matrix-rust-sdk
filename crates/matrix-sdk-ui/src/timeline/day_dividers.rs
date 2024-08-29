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

//! Algorithm to adjust (insert/replace/remove) day dividers after new events
//! have been received from any source.

use std::{fmt::Display, sync::Arc};

use eyeball_im::ObservableVectorTransaction;
use ruma::MilliSecondsSinceUnixEpoch;
use tracing::{error, event_enabled, instrument, trace, warn, Level};

use super::{
    controller::TimelineMetadata, util::timestamp_to_date, TimelineItem, TimelineItemKind,
    VirtualTimelineItem,
};

/// Algorithm ensuring that day dividers are adjusted correctly, according to
/// new items that have been inserted.
pub(super) struct DayDividerAdjuster {
    /// The list of recorded operations to apply, after analyzing the latest
    /// items.
    ops: Vec<DayDividerOperation>,

    /// A boolean indicating whether the struct has been used and thus must be
    /// mark unused manually by calling [`Self::run`].
    consumed: bool,
}

impl Drop for DayDividerAdjuster {
    fn drop(&mut self) {
        // Only run the assert if we're not currently panicking.
        if !std::thread::panicking() && !self.consumed {
            error!("a DayDividerAdjuster has not been consumed with run()");
        }
    }
}

impl Default for DayDividerAdjuster {
    fn default() -> Self {
        Self {
            ops: Default::default(),
            // The adjuster starts as consumed, and it will be marked no consumed iff it's used
            // with `mark_used`.
            consumed: true,
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

impl DayDividerAdjuster {
    /// Marks this [`DayDividerAdjuster`] as used, which means it'll require a
    /// call to [`DayDividerAdjuster::run`] before getting dropped.
    pub fn mark_used(&mut self) {
        // Mark the adjuster as needing to be consumed.
        self.consumed = false;
    }

    /// Ensures that date separators are properly inserted/removed when needs
    /// be.
    #[instrument(skip_all)]
    pub fn run(
        &mut self,
        items: &mut ObservableVectorTransaction<'_, Arc<TimelineItem>>,
        meta: &mut TimelineMetadata,
    ) {
        // We're going to record vector operations like inserting, replacing and
        // removing day dividers. Since we may remove or insert new items,
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

        for (i, item) in items.iter().enumerate() {
            match item.kind() {
                TimelineItemKind::Virtual(VirtualTimelineItem::DayDivider(ts)) => {
                    // Record what the last alive item pair is only if we haven't removed the day
                    // divider.
                    if !self.handle_day_divider(i, *ts, prev_item.as_ref().map(|desc| desc.item)) {
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

                TimelineItemKind::Virtual(VirtualTimelineItem::ReadMarker) => {
                    // Nothing to do.
                }
            }
        }

        // Also chase trailing day dividers explicitly, by iterating from the end to the
        // start. Since they wouldn't be the prev_item of anything, we wouldn't
        // analyze them in the previous loop.
        for (i, item) in items.iter().enumerate().rev() {
            if item.is_day_divider() {
                // The item is a trailing day divider: remove it, if it wasn't already scheduled
                // for deletion.
                if !self
                    .ops
                    .iter()
                    .any(|op| matches!(op, DayDividerOperation::Remove(j) if i == *j))
                {
                    trace!("removing trailing day divider @ {i}");

                    // Find the index at which to insert the removal operation. It must be before
                    // any other operation on a bigger index, to maintain the
                    // non-decreasing invariant.
                    let index =
                        self.ops.iter().position(|op| op.index() > i).unwrap_or(self.ops.len());

                    self.ops.insert(index, DayDividerOperation::Remove(i));
                }
            }

            if item.is_event() {
                // Stop as soon as we run into the first (trailing) event.
                break;
            }
        }

        // Only record the initial state if we've enabled the trace log level, and not
        // otherwise.
        let initial_state =
            if event_enabled!(Level::TRACE) { Some(items.iter().cloned().collect()) } else { None };

        self.process_ops(items, meta);

        // Then check invariants.
        if let Some(report) = self.check_invariants(items, initial_state) {
            warn!("Errors encountered when checking invariants.");
            warn!("{report}");
            #[cfg(any(debug_assertions, test))]
            panic!("There was an error checking date separator invariants");
        }

        self.consumed = true;
    }

    /// Decides what to do with a day divider.
    ///
    /// Returns whether it's been removed or not.
    #[inline]
    fn handle_day_divider(
        &mut self,
        i: usize,
        ts: MilliSecondsSinceUnixEpoch,
        prev_item: Option<&Arc<TimelineItem>>,
    ) -> bool {
        let Some(prev_item) = prev_item else {
            // No interesting item prior to the day divider: it must be the first one,
            // nothing to do.
            return false;
        };

        match prev_item.kind() {
            TimelineItemKind::Event(event) => {
                // This day divider is preceded by an event.
                if is_same_date_as(event.timestamp(), ts) {
                    // The event has the same date as the day divider: remove the current day
                    // divider.
                    trace!("removing day divider following event with same timestamp @ {i}");
                    self.ops.push(DayDividerOperation::Remove(i));
                    return true;
                }
            }

            TimelineItemKind::Virtual(VirtualTimelineItem::DayDivider(_)) => {
                trace!("removing duplicate day divider @ {i}");
                // This day divider is preceded by another one: remove the current one.
                self.ops.push(DayDividerOperation::Remove(i));
                return true;
            }

            TimelineItemKind::Virtual(VirtualTimelineItem::ReadMarker) => {
                // Nothing to do for read markers.
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
            // The event was the first item, so there wasn't any day divider before it:
            // insert one.
            trace!("inserting the first day divider @ {}", i);
            self.ops.push(DayDividerOperation::Insert(i, ts));
            return;
        };

        match item.kind() {
            TimelineItemKind::Event(prev_event) => {
                // The event is preceded by another event. If they're not the same date,
                // insert a day divider.
                let prev_ts = prev_event.timestamp();

                if !is_same_date_as(prev_ts, ts) {
                    trace!("inserting day divider @ {} between two events with different dates", i);
                    self.ops.push(DayDividerOperation::Insert(i, ts));
                }
            }

            TimelineItemKind::Virtual(VirtualTimelineItem::DayDivider(prev_ts)) => {
                let event_date = timestamp_to_date(ts);

                // The event is preceded by a day divider.
                if timestamp_to_date(*prev_ts) != event_date {
                    // The day divider is wrong. Should we replace it with the correct value, or
                    // remove it entirely?
                    if let Some(last_event_ts) = latest_event_ts {
                        if timestamp_to_date(last_event_ts) == event_date {
                            // There's a previous event with the same date: remove the divider.
                            trace!("removed day divider @ {item_index} between two events that have the same date");
                            self.ops.insert(insert_op_at, DayDividerOperation::Remove(item_index));
                            return;
                        }
                    }

                    // There's no previous event or there's one with a different date: replace
                    // the current divider.
                    trace!("replacing day divider @ {item_index} with new timestamp from event");
                    self.ops.insert(insert_op_at, DayDividerOperation::Replace(item_index, ts));
                }
            }

            TimelineItemKind::Virtual(VirtualTimelineItem::ReadMarker) => {
                // Nothing to do.
            }
        }
    }

    fn process_ops(
        &self,
        items: &mut ObservableVectorTransaction<'_, Arc<TimelineItem>>,
        meta: &mut TimelineMetadata,
    ) {
        // Record the deletion offset.
        let mut offset = 0i64;
        // Remember what the maximum index was, so we can assert that it's
        // non-decreasing.
        let mut max_i = 0;

        for op in &self.ops {
            match *op {
                DayDividerOperation::Insert(i, ts) => {
                    assert!(i >= max_i, "trying to insert at {i} < max_i={max_i}");

                    let at = (i64::try_from(i).unwrap() + offset)
                        .min(i64::try_from(items.len()).unwrap());
                    assert!(at >= 0);
                    let at = at as usize;

                    let item = meta.new_timeline_item(VirtualTimelineItem::DayDivider(ts));

                    // Keep push semantics, if we're inserting at the front or the back.
                    if at == items.len() {
                        items.push_back(item);
                    } else if at == 0 {
                        items.push_front(item);
                    } else {
                        items.insert(at, item);
                    }

                    offset += 1;
                    max_i = i;
                }

                DayDividerOperation::Replace(i, ts) => {
                    assert!(i >= max_i, "trying to replace at {i} < max_i={max_i}");

                    let at = i64::try_from(i).unwrap() + offset;
                    assert!(at >= 0);
                    let at = at as usize;

                    let replaced = &items[at];
                    if !replaced.is_day_divider() {
                        error!("we replaced a non day-divider @ {i}: {:?}", replaced.kind());
                    }

                    let unique_id = replaced.unique_id();
                    let item = TimelineItem::new(
                        VirtualTimelineItem::DayDivider(ts),
                        unique_id.to_owned(),
                    );

                    items.set(at, item);
                    max_i = i;
                }

                DayDividerOperation::Remove(i) => {
                    assert!(i >= max_i, "trying to replace at {i} < max_i={max_i}");

                    let at = i64::try_from(i).unwrap() + offset;
                    assert!(at >= 0);

                    let removed = items.remove(at as usize);
                    if !removed.is_day_divider() {
                        error!("we removed a non day-divider @ {i}: {:?}", removed.kind());
                    }

                    offset -= 1;
                    max_i = i;
                }
            }
        }
    }

    /// Checks the invariants that must hold at any time after inserting day
    /// dividers.
    ///
    /// Returns a report if and only if there was at least one error.
    fn check_invariants<'a, 'o>(
        &mut self,
        items: &'a ObservableVectorTransaction<'o, Arc<TimelineItem>>,
        initial_state: Option<Vec<Arc<TimelineItem>>>,
    ) -> Option<DayDividerInvariantsReport<'a, 'o>> {
        let mut report = DayDividerInvariantsReport {
            initial_state,
            errors: Vec::new(),
            operations: std::mem::take(&mut self.ops),
            final_state: items,
        };

        // Assert invariants.
        // 1. The timeline starts with a day divider.
        if let Some(item) = items.get(0) {
            if item.is_read_marker() {
                if let Some(next_item) = items.get(1) {
                    if !next_item.is_day_divider() {
                        report.errors.push(DayDividerInsertError::FirstItemNotDayDivider);
                    }
                }
            } else if !item.is_day_divider() {
                report.errors.push(DayDividerInsertError::FirstItemNotDayDivider);
            }
        }

        // 2. There are no two day dividers following each other.
        {
            let mut prev_was_day_divider = false;
            for (i, item) in items.iter().enumerate() {
                if item.is_day_divider() {
                    if prev_was_day_divider {
                        report.errors.push(DayDividerInsertError::DuplicateDayDivider { at: i });
                    }
                    prev_was_day_divider = true;
                } else {
                    prev_was_day_divider = false;
                }
            }
        };

        // 3. There's no trailing day divider.
        if let Some(last) = items.last() {
            if last.is_day_divider() {
                report.errors.push(DayDividerInsertError::TrailingDayDivider);
            }
        }

        // 4. Items are properly separated with day dividers.
        {
            let mut prev_event_ts = None;
            let mut prev_day_divider_ts = None;

            for (i, item) in items.iter().enumerate() {
                if let Some(ev) = item.as_event() {
                    let ts = ev.timestamp();

                    // We have the same date as the previous event we've seen.
                    if let Some(prev_ts) = prev_event_ts {
                        if !is_same_date_as(prev_ts, ts) {
                            report.errors.push(
                                DayDividerInsertError::MissingDayDividerBetweenEvents { at: i },
                            );
                        }
                    }

                    // There is a day divider before us, and it's the same date as our timestamp.
                    if let Some(prev_ts) = prev_day_divider_ts {
                        if !is_same_date_as(prev_ts, ts) {
                            report.errors.push(
                                DayDividerInsertError::InconsistentDateAfterPreviousDayDivider {
                                    at: i,
                                },
                            );
                        }
                    } else {
                        report
                            .errors
                            .push(DayDividerInsertError::MissingDayDividerBeforeEvent { at: i });
                    }

                    prev_event_ts = Some(ts);
                } else if let TimelineItemKind::Virtual(VirtualTimelineItem::DayDivider(ts)) =
                    item.kind()
                {
                    // The previous day divider is for a different date.
                    if let Some(prev_ts) = prev_day_divider_ts {
                        if is_same_date_as(prev_ts, *ts) {
                            report
                                .errors
                                .push(DayDividerInsertError::DuplicateDayDivider { at: i });
                        }
                    }

                    prev_event_ts = None;
                    prev_day_divider_ts = Some(*ts);
                }
            }
        }

        // 5. If there was a read marker at the beginning, there should be one at the
        //    end.
        if let Some(state) = &report.initial_state {
            if state.iter().any(|item| item.is_read_marker())
                && !report.final_state.iter().any(|item| item.is_read_marker())
            {
                report.errors.push(DayDividerInsertError::ReadMarkerDisappeared);
            }
        }

        if report.errors.is_empty() {
            None
        } else {
            Some(report)
        }
    }
}

#[derive(Debug)]
enum DayDividerOperation {
    Insert(usize, MilliSecondsSinceUnixEpoch),
    Replace(usize, MilliSecondsSinceUnixEpoch),
    Remove(usize),
}

impl DayDividerOperation {
    fn index(&self) -> usize {
        match self {
            DayDividerOperation::Insert(i, _)
            | DayDividerOperation::Replace(i, _)
            | DayDividerOperation::Remove(i) => *i,
        }
    }
}

/// Returns whether the two dates for the given timestamps are the same or not.
#[inline]
fn is_same_date_as(lhs: MilliSecondsSinceUnixEpoch, rhs: MilliSecondsSinceUnixEpoch) -> bool {
    timestamp_to_date(lhs) == timestamp_to_date(rhs)
}

/// A report returned by [`DayDividerAdjuster::check_invariants`].
struct DayDividerInvariantsReport<'a, 'o> {
    /// Initial state before inserting the items.
    initial_state: Option<Vec<Arc<TimelineItem>>>,
    /// The operations that have been applied on the list.
    operations: Vec<DayDividerOperation>,
    /// Final state after inserting the day dividers.
    final_state: &'a ObservableVectorTransaction<'o, Arc<TimelineItem>>,
    /// Errors encountered in the algorithm.
    errors: Vec<DayDividerInsertError>,
}

impl<'a, 'o> Display for DayDividerInvariantsReport<'a, 'o> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Write all the items of a slice of timeline items.
        fn write_items(
            f: &mut std::fmt::Formatter<'_>,
            items: &[Arc<TimelineItem>],
        ) -> std::fmt::Result {
            for (i, item) in items.iter().enumerate() {
                if let TimelineItemKind::Virtual(VirtualTimelineItem::DayDivider(ts)) = item.kind()
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
                    DayDividerOperation::Insert(i, ts) => writeln!(f, "insert @ {i}: {}", ts.0)?,
                    DayDividerOperation::Replace(i, ts) => writeln!(f, "replace @ {i}: {}", ts.0)?,
                    DayDividerOperation::Remove(i) => writeln!(f, "remove @ {i}")?,
                }
            }

            writeln!(f, "\nFinal state:")?;
            write_items(f, self.final_state.iter().cloned().collect::<Vec<_>>().as_slice())?;

            writeln!(f)?;
        }

        for err in &self.errors {
            writeln!(f, "{err}")?;
        }

        Ok(())
    }
}

#[derive(Debug, thiserror::Error)]
enum DayDividerInsertError {
    /// The first item isn't a day divider.
    #[error("The first item isn't a day divider")]
    FirstItemNotDayDivider,

    /// There are two day dividers for the same date.
    #[error("Duplicate day divider @ {at}.")]
    DuplicateDayDivider { at: usize },

    /// The last item is a day divider.
    #[error("The last item is a day divider.")]
    TrailingDayDivider,

    /// Two events are following each other but they have different dates
    /// without a day divider between them.
    #[error("Missing day divider between events @ {at}")]
    MissingDayDividerBetweenEvents { at: usize },

    /// Some event is missing a day divider before it.
    #[error("Missing day divider before event @ {at}")]
    MissingDayDividerBeforeEvent { at: usize },

    /// An event and the previous day divider aren't focused on the same date.
    #[error("Event @ {at} and the previous day divider aren't targeting the same date")]
    InconsistentDateAfterPreviousDayDivider { at: usize },

    /// The read marker has been removed.
    #[error("The read marker has been removed")]
    ReadMarkerDisappeared,
}

#[cfg(test)]
mod tests {
    use assert_matches2::assert_let;
    use eyeball_im::ObservableVector;
    use ruma::{owned_event_id, owned_user_id, uint, MilliSecondsSinceUnixEpoch};

    use super::DayDividerAdjuster;
    use crate::timeline::{
        controller::TimelineMetadata,
        event_item::{EventTimelineItemKind, RemoteEventTimelineItem},
        util::timestamp_to_date,
        EventTimelineItem, TimelineItemContent, VirtualTimelineItem,
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
            timestamp,
            TimelineItemContent::RedactedMessage,
            event_kind,
            Default::default(),
            false,
        )
    }

    #[test]
    fn test_no_trailing_day_divider() {
        let mut items = ObservableVector::new();
        let mut txn = items.transaction();

        let mut meta = TimelineMetadata::new(ruma::RoomVersionId::V11, None, None, false);

        let timestamp = MilliSecondsSinceUnixEpoch(uint!(42));
        let timestamp_next_day =
            MilliSecondsSinceUnixEpoch((42 + 3600 * 24 * 1000).try_into().unwrap());

        txn.push_back(meta.new_timeline_item(event_with_ts(timestamp)));
        txn.push_back(meta.new_timeline_item(VirtualTimelineItem::DayDivider(timestamp_next_day)));
        txn.push_back(meta.new_timeline_item(VirtualTimelineItem::ReadMarker));

        let mut adjuster = DayDividerAdjuster::default();
        adjuster.run(&mut txn, &mut meta);

        txn.commit();

        let mut iter = items.iter();

        assert_let!(Some(item) = iter.next());
        assert!(item.is_day_divider());

        assert_let!(Some(item) = iter.next());
        assert!(item.is_remote_event());

        assert_let!(Some(item) = iter.next());
        assert!(item.is_read_marker());

        assert!(iter.next().is_none());
    }

    #[test]
    fn test_read_marker_in_between_event_and_day_divider() {
        let mut items = ObservableVector::new();
        let mut txn = items.transaction();

        let mut meta = TimelineMetadata::new(ruma::RoomVersionId::V11, None, None, false);

        let timestamp = MilliSecondsSinceUnixEpoch(uint!(42));
        let timestamp_next_day =
            MilliSecondsSinceUnixEpoch((42 + 3600 * 24 * 1000).try_into().unwrap());
        assert_ne!(timestamp_to_date(timestamp), timestamp_to_date(timestamp_next_day));

        let event = event_with_ts(timestamp);
        txn.push_back(meta.new_timeline_item(event.clone()));
        txn.push_back(meta.new_timeline_item(VirtualTimelineItem::DayDivider(timestamp_next_day)));
        txn.push_back(meta.new_timeline_item(VirtualTimelineItem::ReadMarker));
        txn.push_back(meta.new_timeline_item(event));

        let mut adjuster = DayDividerAdjuster::default();
        adjuster.run(&mut txn, &mut meta);

        txn.commit();

        let mut iter = items.iter();

        assert!(iter.next().unwrap().is_day_divider());
        assert!(iter.next().unwrap().is_remote_event());
        assert!(iter.next().unwrap().is_read_marker());
        assert!(iter.next().unwrap().is_remote_event());
        assert!(iter.next().is_none());
    }

    #[test]
    fn test_read_marker_in_between_day_dividers() {
        let mut items = ObservableVector::new();
        let mut txn = items.transaction();

        let mut meta = TimelineMetadata::new(ruma::RoomVersionId::V11, None, None, false);

        let timestamp = MilliSecondsSinceUnixEpoch(uint!(42));
        let timestamp_next_day =
            MilliSecondsSinceUnixEpoch((42 + 3600 * 24 * 1000).try_into().unwrap());
        assert_ne!(timestamp_to_date(timestamp), timestamp_to_date(timestamp_next_day));

        txn.push_back(meta.new_timeline_item(event_with_ts(timestamp)));
        txn.push_back(meta.new_timeline_item(VirtualTimelineItem::DayDivider(timestamp)));
        txn.push_back(meta.new_timeline_item(VirtualTimelineItem::DayDivider(timestamp)));
        txn.push_back(meta.new_timeline_item(VirtualTimelineItem::ReadMarker));
        txn.push_back(meta.new_timeline_item(VirtualTimelineItem::DayDivider(timestamp)));
        txn.push_back(meta.new_timeline_item(event_with_ts(timestamp_next_day)));

        let mut adjuster = DayDividerAdjuster::default();
        adjuster.run(&mut txn, &mut meta);

        txn.commit();

        let mut iter = items.iter();

        assert!(iter.next().unwrap().is_day_divider());
        assert!(iter.next().unwrap().is_remote_event());
        assert!(iter.next().unwrap().is_read_marker());
        assert!(iter.next().unwrap().is_day_divider());
        assert!(iter.next().unwrap().is_remote_event());
        assert!(iter.next().is_none());
    }

    #[test]
    fn test_remove_all_day_dividers() {
        let mut items = ObservableVector::new();
        let mut txn = items.transaction();

        let mut meta = TimelineMetadata::new(ruma::RoomVersionId::V11, None, None, false);

        let timestamp = MilliSecondsSinceUnixEpoch(uint!(42));
        let timestamp_next_day =
            MilliSecondsSinceUnixEpoch((42 + 3600 * 24 * 1000).try_into().unwrap());
        assert_ne!(timestamp_to_date(timestamp), timestamp_to_date(timestamp_next_day));

        txn.push_back(meta.new_timeline_item(event_with_ts(timestamp_next_day)));
        txn.push_back(meta.new_timeline_item(VirtualTimelineItem::DayDivider(timestamp)));
        txn.push_back(meta.new_timeline_item(VirtualTimelineItem::DayDivider(timestamp)));
        txn.push_back(meta.new_timeline_item(event_with_ts(timestamp_next_day)));

        let mut adjuster = DayDividerAdjuster::default();
        adjuster.run(&mut txn, &mut meta);

        txn.commit();

        let mut iter = items.iter();

        assert!(iter.next().unwrap().is_day_divider());
        assert!(iter.next().unwrap().is_remote_event());
        assert!(iter.next().unwrap().is_remote_event());
        assert!(iter.next().is_none());
    }

    #[test]
    fn test_event_read_marker_spurious_day_divider() {
        let mut items = ObservableVector::new();
        let mut txn = items.transaction();

        let mut meta = TimelineMetadata::new(ruma::RoomVersionId::V11, None, None, false);

        let timestamp = MilliSecondsSinceUnixEpoch(uint!(42));

        txn.push_back(meta.new_timeline_item(event_with_ts(timestamp)));
        txn.push_back(meta.new_timeline_item(VirtualTimelineItem::ReadMarker));
        txn.push_back(meta.new_timeline_item(VirtualTimelineItem::DayDivider(timestamp)));

        let mut adjuster = DayDividerAdjuster::default();
        adjuster.run(&mut txn, &mut meta);

        txn.commit();

        let mut iter = items.iter();

        assert!(iter.next().unwrap().is_day_divider());
        assert!(iter.next().unwrap().is_remote_event());
        assert!(iter.next().unwrap().is_read_marker());
        assert!(iter.next().is_none());
    }

    #[test]
    fn test_multiple_trailing_day_dividers() {
        let mut items = ObservableVector::new();
        let mut txn = items.transaction();

        let mut meta = TimelineMetadata::new(ruma::RoomVersionId::V11, None, None, false);

        let timestamp = MilliSecondsSinceUnixEpoch(uint!(42));

        txn.push_back(meta.new_timeline_item(VirtualTimelineItem::ReadMarker));
        txn.push_back(meta.new_timeline_item(VirtualTimelineItem::DayDivider(timestamp)));
        txn.push_back(meta.new_timeline_item(VirtualTimelineItem::DayDivider(timestamp)));

        let mut adjuster = DayDividerAdjuster::default();
        adjuster.run(&mut txn, &mut meta);

        txn.commit();

        let mut iter = items.iter();

        assert!(iter.next().unwrap().is_read_marker());
        assert!(iter.next().is_none());
    }

    #[test]
    fn test_start_with_read_marker() {
        let mut items = ObservableVector::new();
        let mut txn = items.transaction();

        let mut meta = TimelineMetadata::new(ruma::RoomVersionId::V11, None, None, false);

        let timestamp = MilliSecondsSinceUnixEpoch(uint!(42));

        txn.push_back(meta.new_timeline_item(VirtualTimelineItem::ReadMarker));
        txn.push_back(meta.new_timeline_item(event_with_ts(timestamp)));

        let mut adjuster = DayDividerAdjuster::default();
        adjuster.run(&mut txn, &mut meta);

        txn.commit();

        let mut iter = items.iter();

        assert!(iter.next().unwrap().is_read_marker());
        assert!(iter.next().unwrap().is_day_divider());
        assert!(iter.next().unwrap().is_remote_event());
        assert!(iter.next().is_none());
    }
}
