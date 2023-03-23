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

use std::cmp::min;

use ruma::UInt;

/// The kind of request generator.
#[derive(Debug)]
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
    pub(super) ranges: Vec<(UInt, UInt)>,
    /// The kind of request generator.
    pub(super) kind: SlidingSyncListRequestGeneratorKind,
}

impl SlidingSyncListRequestGenerator {
    /// Create a new request generator configured for paging-mode.
    pub(super) fn new_paging(
        batch_size: u32,
        maximum_number_of_rooms_to_fetch: Option<u32>,
    ) -> Self {
        Self {
            ranges: Vec::new(),
            kind: SlidingSyncListRequestGeneratorKind::Paging {
                batch_size,
                maximum_number_of_rooms_to_fetch,
                number_of_fetched_rooms: 0,
                fully_loaded: false,
            },
        }
    }

    /// Create a new request generator configured for growing-mode.
    pub(super) fn new_growing(
        batch_size: u32,
        maximum_number_of_rooms_to_fetch: Option<u32>,
    ) -> Self {
        Self {
            ranges: Vec::new(),
            kind: SlidingSyncListRequestGeneratorKind::Growing {
                batch_size,
                maximum_number_of_rooms_to_fetch,
                number_of_fetched_rooms: 0,
                fully_loaded: false,
            },
        }
    }

    /// Create a new request generator configured for selective-mode.
    pub(super) fn new_selective() -> Self {
        Self { ranges: Vec::new(), kind: SlidingSyncListRequestGeneratorKind::Selective }
    }

    pub(super) fn reset(&mut self) {
        self.ranges.clear();

        match &mut self.kind {
            SlidingSyncListRequestGeneratorKind::Paging {
                number_of_fetched_rooms,
                fully_loaded,
                ..
            }
            | SlidingSyncListRequestGeneratorKind::Growing {
                number_of_fetched_rooms,
                fully_loaded,
                ..
            } => {
                *number_of_fetched_rooms = 0;
                *fully_loaded = false;
            }

            SlidingSyncListRequestGeneratorKind::Selective => {}
        }
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

pub(super) fn create_range(
    start: u32,
    desired_size: u32,
    maximum_number_of_rooms_to_fetch: Option<u32>,
    maximum_number_of_rooms: Option<u32>,
) -> Option<(UInt, UInt)> {
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
        return None;
    }

    Some((start.into(), end.into()))
}

#[cfg(test)]
mod tests {
    use ruma::uint;

    use super::*;

    #[test]
    fn test_create_range_from() {
        // From 0, we want 100 items.
        assert_eq!(create_range(0, 100, None, None), Some((uint!(0), uint!(99))));

        // From 100, we want 100 items.
        assert_eq!(create_range(100, 100, None, None), Some((uint!(100), uint!(199))));

        // From 0, we want 100 items, but there is a maximum number of rooms to fetch
        // defined at 50.
        assert_eq!(create_range(0, 100, Some(50), None), Some((uint!(0), uint!(49))));

        // From 49, we want 100 items, but there is a maximum number of rooms to fetch
        // defined at 50. There is 1 item to load.
        assert_eq!(create_range(49, 100, Some(50), None), Some((uint!(49), uint!(49))));

        // From 50, we want 100 items, but there is a maximum number of rooms to fetch
        // defined at 50.
        assert_eq!(create_range(50, 100, Some(50), None), None);

        // From 0, we want 100 items, but there is a maximum number of rooms defined at
        // 50.
        assert_eq!(create_range(0, 100, None, Some(50)), Some((uint!(0), uint!(49))));

        // From 49, we want 100 items, but there is a maximum number of rooms defined at
        // 50. There is 1 item to load.
        assert_eq!(create_range(49, 100, None, Some(50)), Some((uint!(49), uint!(49))));

        // From 50, we want 100 items, but there is a maximum number of rooms defined at
        // 50.
        assert_eq!(create_range(50, 100, None, Some(50)), None);

        // From 0, we want 100 items, but there is a maximum number of rooms to fetch
        // defined at 75, and a maximum number of rooms defined at 50.
        assert_eq!(create_range(0, 100, Some(75), Some(50)), Some((uint!(0), uint!(49))));

        // From 0, we want 100 items, but there is a maximum number of rooms to fetch
        // defined at 50, and a maximum number of rooms defined at 75.
        assert_eq!(create_range(0, 100, Some(50), Some(75)), Some((uint!(0), uint!(49))));
    }
}
