//! The logic to generate Sliding Sync list requests.
//!
//! Depending on the [`SlidingSyncMode`], the generated requests aren't the
//! same.
//!
//! In [`SlidingSyncMode::Selective`], it's pretty straightforward:
//!
//! * There is a set of ranges,
//! * Each request asks to load the particular ranges.
//!
//! In [`SlidingSyncMode::PagingFullSync`]:
//!
//! * There is a `batch_size`,
//! * Each request asks to load a new successive range containing exactly
//!   `batch_size` rooms.
//!
//! In [`SlidingSyncMode::GrowingFullSync]:
//!
//! * There is a `batch_size`,
//! * Each request asks to load a new range, always starting from 0, but where
//!   the end is incremented by `batch_size` everytime.
//!
//! The number of rooms to load is capped by the
//! [`SlidingSyncList::maximum_number_of_rooms`], i.e. the real number of
//! rooms it is possible to load. This value comes from the server.
//!
//! The number of rooms to load can _also_ be capped by the
//! [`SlidingSyncList::full_sync_maximum_number_of_rooms_to_fetch`], i.e. a
//! user-specified limit representing the maximum number of rooms the user
//! actually wants to load.

use std::{cmp::min, ops::RangeInclusive};

use super::{Bound, SlidingSyncMode};
use crate::sliding_sync::Error;

/// The kind of request generator.
#[derive(Debug, PartialEq)]
pub(super) enum SlidingSyncListRequestGeneratorKind {
    /// Growing-mode (see [`SlidingSyncMode`]).
    Growing {
        /// Size of the batch, used to grow the range to fetch more rooms.
        batch_size: u32,
        /// Maximum number of rooms to fetch (see
        /// [`SlidingSyncList::full_sync_maximum_number_of_rooms_to_fetch`]).
        maximum_number_of_rooms_to_fetch: Option<u32>,
        /// Number of rooms that have been already fetched.
        number_of_fetched_rooms: u32,
        /// Whether all rooms have been loaded.
        fully_loaded: bool,
    },

    /// Paging-mode (see [`SlidingSyncMode`]).
    Paging {
        /// Size of the batch, used to grow the range to fetch more rooms.
        batch_size: u32,
        /// Maximum number of rooms to fetch (see
        /// [`SlidingSyncList::full_sync_maximum_number_of_rooms_to_fetch`]).
        maximum_number_of_rooms_to_fetch: Option<u32>,
        /// Number of rooms that have been already fetched.
        number_of_fetched_rooms: u32,
        /// Whether all romms have been loaded.
        fully_loaded: bool,
    },

    /// Selective-mode (see [`SlidingSyncMode`]).
    Selective,
}

/// A request generator for [`SlidingSyncList`].
#[derive(Debug)]
pub(in super::super) struct SlidingSyncListRequestGenerator {
    /// The current ranges used by this request generator.
    ranges: Vec<RangeInclusive<u32>>,
    /// The kind of request generator.
    pub(super) kind: SlidingSyncListRequestGeneratorKind,
}

impl SlidingSyncListRequestGenerator {
    pub(super) fn new(sync_mode: SlidingSyncMode) -> Self {
        match sync_mode {
            SlidingSyncMode::Paging { batch_size, maximum_number_of_rooms_to_fetch } => Self {
                ranges: Vec::new(),
                kind: SlidingSyncListRequestGeneratorKind::Paging {
                    batch_size,
                    maximum_number_of_rooms_to_fetch,
                    number_of_fetched_rooms: 0,
                    fully_loaded: false,
                },
            },

            SlidingSyncMode::Growing { batch_size, maximum_number_of_rooms_to_fetch } => Self {
                ranges: Vec::new(),
                kind: SlidingSyncListRequestGeneratorKind::Growing {
                    batch_size,
                    maximum_number_of_rooms_to_fetch,
                    number_of_fetched_rooms: 0,
                    fully_loaded: false,
                },
            },

            SlidingSyncMode::Selective { ranges } => {
                Self { ranges, kind: SlidingSyncListRequestGeneratorKind::Selective }
            }
        }
    }

    pub(super) fn ranges(&self) -> &[RangeInclusive<Bound>] {
        &self.ranges
    }

    pub(super) fn set_ranges(&mut self, ranges: Vec<RangeInclusive<Bound>>) {
        self.ranges = ranges;
    }

    pub(super) fn prepare_for_next_request(
        &mut self,
        maximum_number_of_rooms: Option<u32>,
    ) -> Result<(), Error> {
        match self.kind {
            // Cases where all rooms have been fully loaded.
            SlidingSyncListRequestGeneratorKind::Paging { fully_loaded: true, .. }
            | SlidingSyncListRequestGeneratorKind::Growing { fully_loaded: true, .. }
            | SlidingSyncListRequestGeneratorKind::Selective => {
                // Nothing to do.
            }

            SlidingSyncListRequestGeneratorKind::Paging {
                number_of_fetched_rooms,
                batch_size,
                maximum_number_of_rooms_to_fetch,
                ..
            } => {
                // In paging-mode, range starts at the number of fetched rooms. Since ranges are
                // inclusive, and since the number of fetched rooms starts at 1,
                // not at 0, there is no need to add 1 here.
                let range_start = number_of_fetched_rooms;
                let range_desired_size = batch_size;

                // Create a new range, and use it as the current set of ranges.
                self.ranges = vec![create_range(
                    range_start,
                    range_desired_size,
                    maximum_number_of_rooms_to_fetch,
                    maximum_number_of_rooms,
                )?];
            }

            SlidingSyncListRequestGeneratorKind::Growing {
                number_of_fetched_rooms,
                batch_size,
                maximum_number_of_rooms_to_fetch,
                ..
            } => {
                // In growing-mode, range always starts from 0. However, the end is growing by
                // adding `batch_size` to the previous number of fetched rooms.
                let range_start = 0;
                let range_desired_size = number_of_fetched_rooms.saturating_add(batch_size);

                // Create a new range, and use it as the current set of ranges.
                self.ranges = vec![create_range(
                    range_start,
                    range_desired_size,
                    maximum_number_of_rooms_to_fetch,
                    maximum_number_of_rooms,
                )?];
            }
        }

        Ok(())
    }

    #[cfg(test)]
    pub(super) fn is_fully_loaded(&self) -> bool {
        match self.kind {
            SlidingSyncListRequestGeneratorKind::Paging { fully_loaded, .. }
            | SlidingSyncListRequestGeneratorKind::Growing { fully_loaded, .. } => fully_loaded,
            SlidingSyncListRequestGeneratorKind::Selective => true,
        }
    }
}

fn create_range(
    start: u32,
    desired_size: u32,
    maximum_number_of_rooms_to_fetch: Option<u32>,
    maximum_number_of_rooms: Option<u32>,
) -> Result<RangeInclusive<Bound>, Error> {
    // Calculate the range.
    // The `start` bound is given. Let's calculate the `end` bound.

    // The `end`, by default, is `start` + `desired_size`.
    let mut end = start + desired_size;

    // But maybe the user has defined a maximum number of rooms to fetch? In this
    // case, take the minimum of the two.
    if let Some(maximum_number_of_rooms_to_fetch) = maximum_number_of_rooms_to_fetch {
        end = min(end, maximum_number_of_rooms_to_fetch);
    }

    // But there is more! The server can tell us what is the maximum number of rooms
    // fulfilling a particular list. For example, if the server says there is 42
    // rooms for a particular list, with a `start` of 40 and a `batch_size` of 20,
    // the range must be capped to `[40; 46]`; the range `[40; 60]` would be invalid
    // and could be rejected by the server.
    if let Some(maximum_number_of_rooms) = maximum_number_of_rooms {
        end = min(end, maximum_number_of_rooms);
    }

    // Finally, because the bounds of the range are inclusive, 1 is subtracted.
    end = end.saturating_sub(1);

    // Make sure `start` is smaller than `end`. It can happen if `start` is greater
    // than `maximum_number_of_rooms_to_fetch` or `maximum_number_of_rooms`.
    if start > end {
        return Err(Error::InvalidRange { start, end });
    }

    Ok(RangeInclusive::new(start, end))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sliding_sync::SlidingSyncSelectiveModeBuilder;

    #[test]
    fn test_create_range_from() {
        // From 0, we want 100 items.
        assert_eq!(create_range(0, 100, None, None), Ok(0..=99));

        // From 100, we want 100 items.
        assert_eq!(create_range(100, 100, None, None), Ok(100..=199));

        // From 0, we want 100 items, but there is a maximum number of rooms to fetch
        // defined at 50.
        assert_eq!(create_range(0, 100, Some(50), None), Ok(0..=49));

        // From 49, we want 100 items, but there is a maximum number of rooms to fetch
        // defined at 50. There is 1 item to load.
        assert_eq!(create_range(49, 100, Some(50), None), Ok(49..=49));

        // From 50, we want 100 items, but there is a maximum number of rooms to fetch
        // defined at 50.
        assert_eq!(
            create_range(50, 100, Some(50), None),
            Err(Error::InvalidRange { start: 50, end: 49 })
        );

        // From 0, we want 100 items, but there is a maximum number of rooms defined at
        // 50.
        assert_eq!(create_range(0, 100, None, Some(50)), Ok(0..=49));

        // From 49, we want 100 items, but there is a maximum number of rooms defined at
        // 50. There is 1 item to load.
        assert_eq!(create_range(49, 100, None, Some(50)), Ok(49..=49));

        // From 50, we want 100 items, but there is a maximum number of rooms defined at
        // 50.
        assert_eq!(
            create_range(50, 100, None, Some(50)),
            Err(Error::InvalidRange { start: 50, end: 49 })
        );

        // From 0, we want 100 items, but there is a maximum number of rooms to fetch
        // defined at 75, and a maximum number of rooms defined at 50.
        assert_eq!(create_range(0, 100, Some(75), Some(50)), Ok(0..=49));

        // From 0, we want 100 items, but there is a maximum number of rooms to fetch
        // defined at 50, and a maximum number of rooms defined at 75.
        assert_eq!(create_range(0, 100, Some(50), Some(75)), Ok(0..=49));
    }

    #[test]
    fn test_request_generator_selective_from_sync_mode() {
        let sync_mode = SlidingSyncMode::new_selective(SlidingSyncSelectiveModeBuilder::new());
        let request_generator = SlidingSyncListRequestGenerator::new(sync_mode);

        assert!(request_generator.ranges.is_empty());
        assert_eq!(request_generator.kind, SlidingSyncListRequestGeneratorKind::Selective);
    }

    #[test]
    fn test_request_generator_paging_from_sync_mode() {
        let sync_mode = SlidingSyncMode::new_paging(1, Some(2));
        let request_generator = SlidingSyncListRequestGenerator::new(sync_mode);

        assert!(request_generator.ranges.is_empty());
        assert_eq!(
            request_generator.kind,
            SlidingSyncListRequestGeneratorKind::Paging {
                batch_size: 1,
                maximum_number_of_rooms_to_fetch: Some(2),
                number_of_fetched_rooms: 0,
                fully_loaded: false,
            }
        );
    }

    #[test]
    fn test_request_generator_growing_from_sync_mode() {
        let sync_mode = SlidingSyncMode::new_growing(1, Some(2));
        let request_generator = SlidingSyncListRequestGenerator::new(sync_mode);

        assert!(request_generator.ranges.is_empty());
        assert_eq!(
            request_generator.kind,
            SlidingSyncListRequestGeneratorKind::Growing {
                batch_size: 1,
                maximum_number_of_rooms_to_fetch: Some(2),
                number_of_fetched_rooms: 0,
                fully_loaded: false,
            }
        );
    }
}
