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
use tracing::{event_enabled, instrument, trace, warn, Level};

use super::{
    inner::TimelineInnerMetadata, util::timestamp_to_date, TimelineItem, TimelineItemKind,
    VirtualTimelineItem,
};

/// Algorithm ensuring that day dividers are adjusted correctly, according to
/// new items that have been inserted.
#[derive(Default)]
pub(super) struct DayDividerAdjuster {
    ops: Vec<DayDividerOperation>,
}

impl DayDividerAdjuster {
    /// Ensures that date separators are properly inserted/removed when needs
    /// be.
    #[instrument(skip(self))]
    pub fn maybe_adjust_day_dividers(
        mut self,
        items: &mut ObservableVectorTransaction<'_, Arc<TimelineItem>>,
        meta: &mut TimelineInnerMetadata,
    ) {
        // We're going to record vector operations like inserting, replacing and
        // removing day dividers. Since we may remove or insert new items,
        // recorded offsets will change as we're iterating over the array. The
        // only way this is possible is because we're recording operations
        // happening in non-decreasing order of the indices, i.e. we can't do an
        // operation onindex I and then on any index J<I later.
        //
        // Note we can't just iterate in reverse order, because we may have a
        // `Remove(i)` followed by a `Replace((i+1) -1)`, which wouldn't do what
        // we want, if running in reverse order.

        let mut prev_item: Option<&Arc<TimelineItem>> = None;
        let mut latest_event_ts = None;

        for (i, item) in items.iter().enumerate() {
            match item.kind() {
                TimelineItemKind::Virtual(VirtualTimelineItem::DayDivider(ts)) => {
                    self.handle_day_divider(i, *ts, prev_item);

                    prev_item = Some(item);
                }

                TimelineItemKind::Event(event) => {
                    let ts = event.timestamp();

                    self.handle_event(i, ts, prev_item, latest_event_ts);

                    prev_item = Some(item);
                    latest_event_ts = Some(ts);
                }

                TimelineItemKind::Virtual(VirtualTimelineItem::ReadMarker) => {
                    // Nothing to do.
                }
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
            #[cfg(debug)]
            panic!("{report}");
            #[cfg(not(debug))]
            warn!("{report}");
        }
    }

    #[inline]
    fn handle_day_divider(
        &mut self,
        i: usize,
        ts: MilliSecondsSinceUnixEpoch,
        prev_item: Option<&Arc<TimelineItem>>,
    ) {
        let Some(prev_item) = prev_item else {
            // No interesting item prior to the day divider: it must be the first one,
            // nothing to do.
            return;
        };

        match prev_item.kind() {
            TimelineItemKind::Event(event) => {
                // This day divider is preceded by an event.
                if is_same_date_as(event.timestamp(), ts) {
                    // The event has the same date as the day divider: remove the day
                    // divider.
                    trace!("removing day divider following event with same timestamp @ {i}");
                    self.ops.push(DayDividerOperation::Remove(i));
                }
            }

            TimelineItemKind::Virtual(VirtualTimelineItem::DayDivider(_)) => {
                trace!("removing duplicate day divider @ {i}");
                // This day divider is preceded by another one: remove the current one.
                self.ops.push(DayDividerOperation::Remove(i));
            }

            TimelineItemKind::Virtual(VirtualTimelineItem::ReadMarker) => {
                // Nothing to do for read markers.
            }
        }
    }

    #[inline]
    fn handle_event(
        &mut self,
        i: usize,
        ts: MilliSecondsSinceUnixEpoch,
        prev_item: Option<&Arc<TimelineItem>>,
        latest_event_ts: Option<MilliSecondsSinceUnixEpoch>,
    ) {
        let Some(prev_item) = prev_item else {
            // The event was the first item, so there wasn't any day divider before it:
            // insert one.
            trace!("inserting the first day divider @ {}", i);
            self.ops.push(DayDividerOperation::Insert(i, ts));
            return;
        };

        match prev_item.kind() {
            TimelineItemKind::Event(prev_event) => {
                // The event is preceded by another event. If they're not the same date,
                // insert a date divider.
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
                    let mut removed = false;

                    if let Some(last_event_ts) = latest_event_ts {
                        if timestamp_to_date(last_event_ts) == event_date {
                            // There's a previous event with the same date: remove the divider.
                            trace!("removed day divider @ {i} between two events that have the same date");
                            self.ops.push(DayDividerOperation::Remove(i - 1));
                            removed = true;
                        }
                    }

                    if !removed {
                        // There's no previous event or there's one with a different
                        // date: replace the current
                        // divider.
                        trace!("replacing day divider @ {i} with new timestamp from event");
                        self.ops.push(DayDividerOperation::Replace(i - 1, ts));
                    }
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
        meta: &mut TimelineInnerMetadata,
    ) {
        // Record the deletion offset.
        let mut offset = 0i64;
        // Remember what the maximum index was, so we can assert that it's
        // non-decreasing.
        let mut max_i = 0;

        for op in &self.ops {
            match *op {
                DayDividerOperation::Insert(i, ts) => {
                    assert!(i >= max_i);

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
                    assert!(i >= max_i);

                    let at = i64::try_from(i).unwrap() + offset;
                    assert!(at >= 0);
                    let at = at as usize;

                    let replaced = &items[at];
                    assert!(replaced.is_day_divider(), "we replaced a non day-divider");

                    let unique_id = replaced.unique_id();
                    let item = TimelineItem::new(VirtualTimelineItem::DayDivider(ts), unique_id);

                    items.set(at, item);
                    max_i = i;
                }

                DayDividerOperation::Remove(i) => {
                    assert!(i >= max_i);

                    let at = i64::try_from(i).unwrap() + offset;
                    assert!(at >= 0);

                    let removed = items.remove(at as usize);
                    assert!(removed.is_day_divider(), "we removed a non day-divider");

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
        self,
        items: &'a ObservableVectorTransaction<'o, Arc<TimelineItem>>,
        initial_state: Option<Vec<Arc<TimelineItem>>>,
    ) -> Option<DayDividerInvariantsReport<'a, 'o>> {
        let mut report = DayDividerInvariantsReport {
            initial_state,
            errors: Vec::new(),
            operations: self.ops,
            final_state: items,
        };

        // Assert invariants.
        // 1. The timeline starts with a date separator.
        if let Some(item) = items.get(0) {
            if !item.is_day_divider() {
                report.errors.push(DayDividerInsertError::FirstItemNotDayDivider)
            }
        }

        // 2. There are no two date dividers following each other.
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
        if let Some(initial_state) = &self.initial_state {
            writeln!(f, "Initial state:")?;

            for (i, item) in initial_state.iter().enumerate() {
                if let TimelineItemKind::Virtual(VirtualTimelineItem::DayDivider(ts)) = item.kind()
                {
                    writeln!(f, "#{i} --- {}", ts.0)?;
                } else if let Some(event) = item.as_event() {
                    // id: timestamp
                    writeln!(f, "#{i} {}: {}", event.event_id().unwrap(), event.timestamp().0)?;
                } else {
                    writeln!(f, "#{i} (other virtual item)")?;
                }
            }

            writeln!(f, "\nOperations to apply:")?;
            for op in &self.operations {
                match *op {
                    DayDividerOperation::Insert(i, ts) => writeln!(f, "insert @ {i}: {}", ts.0)?,
                    DayDividerOperation::Replace(i, ts) => writeln!(f, "replace @ {i}: {}", ts.0)?,
                    DayDividerOperation::Remove(i) => writeln!(f, "remove @ {i}")?,
                }
            }

            writeln!(f, "\nFinal state:")?;
            for (i, item) in self.final_state.iter().enumerate() {
                if let TimelineItemKind::Virtual(VirtualTimelineItem::DayDivider(ts)) = item.kind()
                {
                    writeln!(f, "#{i} --- {}", ts.0)?;
                } else if let Some(event) = item.as_event() {
                    // id: timestamp
                    writeln!(f, "#{i} {}: {}", event.event_id().unwrap(), event.timestamp().0)?;
                } else {
                    writeln!(f, "#{i} (other virtual item)")?;
                }
            }

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
}
