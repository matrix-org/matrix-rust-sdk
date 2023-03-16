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

use eyeball::unique::Observable;
use ruma::{api::client::sync::sync_events::v4, assign, OwnedRoomId, UInt};
use tracing::{error, instrument};

use super::{Error, SlidingSyncList, SlidingSyncState};

/// The kind of request generator.
#[derive(Debug)]
enum GeneratorKind {
    // Growing-mode (see [`SlidingSyncMode`]).
    GrowingFullSync {
        // Number of fetched rooms.
        number_of_fetched_rooms: u32,
        // Size of the batch, used to grow the range to fetch more rooms.
        batch_size: u32,
        // Maximum number of rooms to fetch (see
        // [`SlidingSyncList::full_sync_maximum_number_of_rooms_to_fetch`]).
        maximum_number_of_rooms_to_fetch: Option<u32>,
        // Whether all rooms have been loaded.
        fully_loaded: bool,
    },

    // Paging-mode (see [`SlidingSyncMode`]).
    PagingFullSync {
        // Number of fetched rooms.
        number_of_fetched_rooms: u32,
        // Size of the batch, used to grow the range to fetch more rooms.
        batch_size: u32,
        // Maximum number of rooms to fetch (see
        // [`SlidingSyncList::full_sync_maximum_number_of_rooms_to_fetch`]).
        maximum_number_of_rooms_to_fetch: Option<u32>,
        // Whether all romms have been loaded.
        fully_loaded: bool,
    },

    // Selective-mode (see [`SlidingSyncMode`]).
    Selective,
}

/// A request generator for [`SlidingSyncList`].
#[derive(Debug)]
pub(in super::super) struct SlidingSyncListRequestGenerator {
    /// The parent [`SlidingSyncList`] object that has created this request
    /// generator.
    list: SlidingSyncList,
    /// The current range used by this request generator.
    ranges: Vec<(UInt, UInt)>,
    /// The kind of request generator.
    kind: GeneratorKind,
}

impl SlidingSyncListRequestGenerator {
    /// Create a new request generator configured for paging-mode.
    pub(super) fn new_with_paging_full_sync(list: SlidingSyncList) -> Self {
        let batch_size = list.full_sync_batch_size;
        let maximum_number_of_rooms_to_fetch = list.full_sync_maximum_number_of_rooms_to_fetch;
        // If a range exists, let's consider it's been used to load existing room. So
        // let's start from the end of the range. It can be useful when we resume a sync
        // for example. Otherwise let's use the default value.
        let number_of_fetched_rooms = list
            .ranges
            .read()
            .unwrap()
            .first()
            .map(|(_start, end)| u32::try_from(*end).unwrap().saturating_add(1))
            .unwrap_or_default();

        Self {
            list,
            ranges: Vec::new(),
            kind: GeneratorKind::PagingFullSync {
                number_of_fetched_rooms,
                batch_size,
                maximum_number_of_rooms_to_fetch,
                fully_loaded: false,
            },
        }
    }

    /// Create a new request generator configured for growing-mode.
    pub(super) fn new_with_growing_full_sync(list: SlidingSyncList) -> Self {
        let batch_size = list.full_sync_batch_size;
        let maximum_number_of_rooms_to_fetch = list.full_sync_maximum_number_of_rooms_to_fetch;
        // If a range exists, let's consider it's been used to load existing room. So
        // let's start from the end of the range. It can be useful when we resume a sync
        // for example. Otherwise let's use the default value.
        let number_of_fetched_rooms = list
            .ranges
            .read()
            .unwrap()
            .first()
            .map(|(_start, end)| u32::try_from(*end).unwrap().saturating_add(1))
            .unwrap_or_default();

        Self {
            list,
            ranges: Vec::new(),
            kind: GeneratorKind::GrowingFullSync {
                number_of_fetched_rooms,
                batch_size,
                maximum_number_of_rooms_to_fetch,
                fully_loaded: false,
            },
        }
    }

    /// Create a new request generator configured for selective-mode.
    pub(super) fn new_selective(list: SlidingSyncList) -> Self {
        Self { list, ranges: Vec::new(), kind: GeneratorKind::Selective }
    }

    /// Build a [`SyncRequestList`][v4::SyncRequestList].
    #[instrument(skip(self), fields(name = self.list.name, ranges = ?&self.ranges))]
    fn build_request(&self) -> v4::SyncRequestList {
        let sort = self.list.sort.clone();
        let required_state = self.list.required_state.clone();
        let timeline_limit = **self.list.timeline_limit.read().unwrap();
        let filters = self.list.filters.clone();

        assign!(v4::SyncRequestList::default(), {
            ranges: self.ranges.clone(),
            room_details: assign!(v4::RoomDetailsConfig::default(), {
                required_state,
                timeline_limit,
            }),
            sort,
            filters,
        })
    }

    // Handle the response from the server.
    #[instrument(skip_all, fields(name = self.list.name, rooms_count, has_ops = !ops.is_empty()))]
    pub(in super::super) fn handle_response(
        &mut self,
        maximum_number_of_rooms: u32,
        ops: &Vec<v4::SyncOp>,
        updated_rooms: &Vec<OwnedRoomId>,
    ) -> Result<bool, Error> {
        let response =
            self.list.handle_response(maximum_number_of_rooms, ops, &self.ranges, updated_rooms)?;

        self.update_state(maximum_number_of_rooms);

        Ok(response)
    }

    /// Update the state of the generator.
    fn update_state(&mut self, maximum_number_of_rooms: u32) {
        let Some(range_end) = self.ranges.first().map(|(_start, end)| u32::try_from(*end).unwrap()) else {
            error!(name = self.list.name, "The request generator must have a range.");

            return;
        };

        match &mut self.kind {
            GeneratorKind::PagingFullSync {
                number_of_fetched_rooms,
                fully_loaded,
                maximum_number_of_rooms_to_fetch,
                ..
            }
            | GeneratorKind::GrowingFullSync {
                number_of_fetched_rooms,
                fully_loaded,
                maximum_number_of_rooms_to_fetch,
                ..
            } => {
                // Calculate the maximum bound for the range.
                // At this step, the server has given us a maximum number of rooms for this
                // list. That's our `range_maximum`.
                let mut range_maximum = maximum_number_of_rooms;

                // But maybe the user has defined a maximum number of rooms to fetch? In this
                // case, let's take the minimum of the two.
                if let Some(maximum_number_of_rooms_to_fetch) = maximum_number_of_rooms_to_fetch {
                    range_maximum = min(range_maximum, *maximum_number_of_rooms_to_fetch);
                }

                // Finally, ranges are inclusive!
                range_maximum = range_maximum.saturating_sub(1);

                // Now, we know what the maximum bound for the range is.

                // The current range hasn't reached its maximum, let's continue.
                if range_end < range_maximum {
                    // Update the _list range_ to cover from 0 to `range_end`.
                    // The list range is different from the request generator (this) range.
                    self.list.set_range(0, range_end);

                    // Update the number of fetched rooms forward. Do not forget that ranges are
                    // inclusive, so let's add 1.
                    *number_of_fetched_rooms = range_end.saturating_add(1);

                    // The list is still not fully loaded.
                    *fully_loaded = false;

                    // Finally, let's update the list' state.
                    Observable::update_eq(&mut self.list.state.write().unwrap(), |state| {
                        *state = SlidingSyncState::PartiallyLoaded;
                    });
                }
                // Otherwise the current range has reached its maximum, we switched to `FullyLoaded`
                // mode.
                else {
                    // The range is covering the entire list, from 0 to its maximum.
                    self.list.set_range(0, range_maximum);

                    // The number of fetched rooms is set to the maximum too.
                    *number_of_fetched_rooms = range_maximum;

                    // And we update the `fully_loaded` marker.
                    *fully_loaded = true;

                    // Finally, let's update the list' state.
                    Observable::update_eq(&mut self.list.state.write().unwrap(), |state| {
                        *state = SlidingSyncState::FullyLoaded;
                    });
                }
            }

            GeneratorKind::Selective => {
                // Selective mode always loads everything.
                Observable::update_eq(&mut self.list.state.write().unwrap(), |state| {
                    *state = SlidingSyncState::FullyLoaded;
                });
            }
        }
    }

    #[cfg(test)]
    fn is_fully_loaded(&self) -> bool {
        match self.kind {
            GeneratorKind::PagingFullSync { fully_loaded, .. }
            | GeneratorKind::GrowingFullSync { fully_loaded, .. } => fully_loaded,
            GeneratorKind::Selective => true,
        }
    }
}

fn create_range(
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

impl Iterator for SlidingSyncListRequestGenerator {
    type Item = v4::SyncRequestList;

    fn next(&mut self) -> Option<Self::Item> {
        match self.kind {
            // Cases where all rooms have been fully loaded.
            GeneratorKind::PagingFullSync { fully_loaded: true, .. }
            | GeneratorKind::GrowingFullSync { fully_loaded: true, .. }
            | GeneratorKind::Selective => {
                // Let's copy all the ranges from the parent `SlidingSyncList`, and build a
                // request for them.
                self.ranges = self.list.ranges.read().unwrap().clone();

                // Here we go.
                Some(self.build_request())
            }

            GeneratorKind::PagingFullSync {
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
                    self.list.maximum_number_of_rooms(),
                )?];

                // Here we go.
                Some(self.build_request())
            }

            GeneratorKind::GrowingFullSync {
                number_of_fetched_rooms,
                batch_size,
                maximum_number_of_rooms_to_fetch,
                ..
            } => {
                // In growing-mode, range always starts from 0. However, the end is growing by
                // adding `batch_size` to the previous number of fetched rooms.
                let range_start = 0;
                let range_desired_size = number_of_fetched_rooms.saturating_add(batch_size);

                self.ranges = vec![create_range(
                    range_start,
                    range_desired_size,
                    maximum_number_of_rooms_to_fetch,
                    self.list.maximum_number_of_rooms(),
                )?];

                // Here we go.
                Some(self.build_request())
            }
        }
    }
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

    macro_rules! assert_request_and_response {
        (
            list = $list:ident,
            generator = $generator:ident,
            maximum_number_of_rooms = $maximum_number_of_rooms:expr,
            $(
                next => {
                    ranges = $( [ $range_start:literal ; $range_end:literal ] ),+ ,
                    is_fully_loaded = $is_fully_loaded:expr,
                    list_state = $list_state:ident,
                }
            ),*
            $(,)*
        ) => {
            // That's the initial state.
            assert_eq!($list.state(), SlidingSyncState::NotLoaded);

            $(
                {
                    // Generate a new request.
                    let request = $generator.next().unwrap();

                    assert_eq!(request.ranges, [ $( (uint!( $range_start ), uint!( $range_end )) ),* ]);

                    // Fake a response.
                    let _ = $generator.handle_response($maximum_number_of_rooms, &vec![], &vec![]);

                    assert_eq!($generator.is_fully_loaded(), $is_fully_loaded);
                    assert_eq!($list.state(), SlidingSyncState::$list_state);
                }
            )*
        };
    }

    #[test]
    fn test_generator_paging_full_sync() {
        let list = SlidingSyncList::builder()
            .sync_mode(crate::SlidingSyncMode::PagingFullSync)
            .name("testing")
            .full_sync_batch_size(10)
            .build()
            .unwrap();
        let mut generator = list.request_generator();

        assert_request_and_response! {
            list = list,
            generator = generator,
            maximum_number_of_rooms = 25,
            next => {
                ranges = [0; 9],
                is_fully_loaded = false,
                list_state = PartiallyLoaded,
            },
            next => {
                ranges = [10; 19],
                is_fully_loaded = false,
                list_state = PartiallyLoaded,
            },
            // The maximum number of rooms is reached!
            next => {
                ranges = [20; 24],
                is_fully_loaded = true,
                list_state = FullyLoaded,
            },
            // Now it's fully loaded, so the same request must be produced everytime.
            next => {
                ranges = [0; 24], // the range starts at 0 now!
                is_fully_loaded = true,
                list_state = FullyLoaded,
            },
            next => {
                ranges = [0; 24],
                is_fully_loaded = true,
                list_state = FullyLoaded,
            },
        };
    }

    #[test]
    fn test_generator_paging_full_sync_with_a_maximum_number_of_rooms_to_fetch() {
        let list = SlidingSyncList::builder()
            .sync_mode(crate::SlidingSyncMode::PagingFullSync)
            .name("testing")
            .full_sync_batch_size(10)
            .full_sync_maximum_number_of_rooms_to_fetch(22)
            .build()
            .unwrap();
        let mut generator = list.request_generator();

        assert_request_and_response! {
            list = list,
            generator = generator,
            maximum_number_of_rooms = 25,
            next => {
                ranges = [0; 9],
                is_fully_loaded = false,
                list_state = PartiallyLoaded,
            },
            next => {
                ranges = [10; 19],
                is_fully_loaded = false,
                list_state = PartiallyLoaded,
            },
            // The maximum number of rooms to fetch is reached!
            next => {
                ranges = [20; 21],
                is_fully_loaded = true,
                list_state = FullyLoaded,
            },
            // Now it's fully loaded, so the same request must be produced everytime.
            next => {
                ranges = [0; 21], // the range starts at 0 now!
                is_fully_loaded = true,
                list_state = FullyLoaded,
            },
            next => {
                ranges = [0; 21],
                is_fully_loaded = true,
                list_state = FullyLoaded,
            },
        };
    }

    #[test]
    fn test_generator_growing_full_sync() {
        let list = SlidingSyncList::builder()
            .sync_mode(crate::SlidingSyncMode::GrowingFullSync)
            .name("testing")
            .full_sync_batch_size(10)
            .build()
            .unwrap();
        let mut generator = list.request_generator();

        assert_request_and_response! {
            list = list,
            generator = generator,
            maximum_number_of_rooms = 25,
            next => {
                ranges = [0; 9],
                is_fully_loaded = false,
                list_state = PartiallyLoaded,
            },
            next => {
                ranges = [0; 19],
                is_fully_loaded = false,
                list_state = PartiallyLoaded,
            },
            // The maximum number of rooms is reached!
            next => {
                ranges = [0; 24],
                is_fully_loaded = true,
                list_state = FullyLoaded,
            },
            // Now it's fully loaded, so the same request must be produced everytime.
            next => {
                ranges = [0; 24],
                is_fully_loaded = true,
                list_state = FullyLoaded,
            },
            next => {
                ranges = [0; 24],
                is_fully_loaded = true,
                list_state = FullyLoaded,
            },
        };
    }

    #[test]
    fn test_generator_growing_full_sync_with_a_maximum_number_of_rooms_to_fetch() {
        let list = SlidingSyncList::builder()
            .sync_mode(crate::SlidingSyncMode::GrowingFullSync)
            .name("testing")
            .full_sync_batch_size(10)
            .full_sync_maximum_number_of_rooms_to_fetch(22)
            .build()
            .unwrap();
        let mut generator = list.request_generator();

        assert_request_and_response! {
            list = list,
            generator = generator,
            maximum_number_of_rooms = 25,
            next => {
                ranges = [0; 9],
                is_fully_loaded = false,
                list_state = PartiallyLoaded,
            },
            next => {
                ranges = [0; 19],
                is_fully_loaded = false,
                list_state = PartiallyLoaded,
            },
            // The maximum number of rooms is reached!
            next => {
                ranges = [0; 21],
                is_fully_loaded = true,
                list_state = FullyLoaded,
            },
            // Now it's fully loaded, so the same request must be produced everytime.
            next => {
                ranges = [0; 21],
                is_fully_loaded = true,
                list_state = FullyLoaded,
            },
            next => {
                ranges = [0; 21],
                is_fully_loaded = true,
                list_state = FullyLoaded,
            },
        };
    }

    #[test]
    fn test_generator_selective() {
        let list = SlidingSyncList::builder()
            .sync_mode(crate::SlidingSyncMode::Selective)
            .name("testing")
            .ranges(vec![(0u32, 10), (42, 153)])
            .build()
            .unwrap();
        let mut generator = list.request_generator();

        assert_request_and_response! {
            list = list,
            generator = generator,
            maximum_number_of_rooms = 25,
            // The maximum number of rooms is reached directly!
            next => {
                ranges = [0; 10], [42; 153],
                is_fully_loaded = true,
                list_state = FullyLoaded,
            },
            // Now it's fully loaded, so the same request must be produced everytime.
            next => {
                ranges = [0; 10], [42; 153],
                is_fully_loaded = true,
                list_state = FullyLoaded,
            },
            next => {
                ranges = [0; 10], [42; 153],
                is_fully_loaded = true,
                list_state = FullyLoaded,
            }
        };
    }
}
