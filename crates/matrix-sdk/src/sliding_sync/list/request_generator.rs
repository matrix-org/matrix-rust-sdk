use std::cmp::min;

use eyeball::Observable;
use ruma::{api::client::sync::sync_events::v4, assign, OwnedRoomId, UInt};
use tracing::{error, instrument, trace};

use super::{Error, SlidingSyncList, SlidingSyncState};

enum GeneratorKind {
    GrowingFullSync { position: u32, batch_size: u32, limit: Option<u32>, live: bool },
    PagingFullSync { position: u32, batch_size: u32, limit: Option<u32>, live: bool },
    Live,
}

pub(in super::super) struct SlidingSyncListRequestGenerator {
    list: SlidingSyncList,
    ranges: Vec<(usize, usize)>,
    kind: GeneratorKind,
}

impl SlidingSyncListRequestGenerator {
    pub(super) fn new_with_paging_syncup(list: SlidingSyncList) -> Self {
        let batch_size = list.batch_size;
        let limit = list.limit;
        let position = list
            .ranges
            .read()
            .unwrap()
            .first()
            .map(|(_start, end)| u32::try_from(*end).unwrap())
            .unwrap_or_default();

        Self {
            list,
            ranges: Default::default(),
            kind: GeneratorKind::PagingFullSync { position, batch_size, limit, live: false },
        }
    }

    pub(super) fn new_with_growing_syncup(list: SlidingSyncList) -> Self {
        let batch_size = list.batch_size;
        let limit = list.limit;
        let position = list
            .ranges
            .read()
            .unwrap()
            .first()
            .map(|(_start, end)| u32::try_from(*end).unwrap())
            .unwrap_or_default();

        Self {
            list,
            ranges: Default::default(),
            kind: GeneratorKind::GrowingFullSync { position, batch_size, limit, live: false },
        }
    }

    pub(super) fn new_live(list: SlidingSyncList) -> Self {
        Self { list, ranges: Default::default(), kind: GeneratorKind::Live }
    }

    fn prefetch_request(
        &mut self,
        start: u32,
        batch_size: u32,
        limit: Option<u32>,
    ) -> v4::SyncRequestList {
        let calculated_end = start + batch_size;

        let mut end = match limit {
            Some(limit) => min(limit, calculated_end),
            _ => calculated_end,
        };

        end = match self.list.rooms_count() {
            Some(total_room_count) => min(end, total_room_count - 1),
            _ => end,
        };

        self.make_request_for_ranges(vec![(start.into(), end.into())])
    }

    #[instrument(skip(self), fields(name = self.list.name))]
    fn make_request_for_ranges(&mut self, ranges: Vec<(UInt, UInt)>) -> v4::SyncRequestList {
        let sort = self.list.sort.clone();
        let required_state = self.list.required_state.clone();
        let timeline_limit = **self.list.timeline_limit.read().unwrap();
        let filters = self.list.filters.clone();

        self.ranges = ranges
            .iter()
            .map(|(a, b)| {
                (
                    usize::try_from(*a).expect("range is a valid u32"),
                    usize::try_from(*b).expect("range is a valid u32"),
                )
            })
            .collect();

        assign!(v4::SyncRequestList::default(), {
            ranges: ranges,
            room_details: assign!(v4::RoomDetailsConfig::default(), {
                required_state,
                timeline_limit,
            }),
            sort,
            filters,
        })
    }

    // generate the next live request
    fn live_request(&mut self) -> v4::SyncRequestList {
        let ranges = self.list.ranges.read().unwrap().clone();
        self.make_request_for_ranges(ranges)
    }

    #[instrument(skip_all, fields(name = self.list.name, rooms_count, has_ops = !ops.is_empty()))]
    pub(in super::super) fn handle_response(
        &mut self,
        rooms_count: u32,
        ops: &Vec<v4::SyncOp>,
        rooms: &Vec<OwnedRoomId>,
    ) -> Result<bool, Error> {
        let response = self.list.handle_response(rooms_count, ops, &self.ranges, rooms)?;
        self.update_state(rooms_count.saturating_sub(1)); // index is 0 based, count is 1 based

        Ok(response)
    }

    fn update_state(&mut self, max_index: u32) {
        let Some((_start, range_end)) = self.ranges.first() else {
            error!("Why don't we have any ranges?");

            return;
        };

        let end = if &(max_index as usize) < range_end { max_index } else { *range_end as u32 };

        trace!(end, max_index, range_end, name = self.list.name, "updating state");

        match &mut self.kind {
            GeneratorKind::PagingFullSync { position, live, limit, .. }
            | GeneratorKind::GrowingFullSync { position, live, limit, .. } => {
                let max = limit.map(|limit| min(limit, max_index)).unwrap_or(max_index);

                trace!(end, max, name = self.list.name, "updating state");

                if end >= max {
                    // Switching to live mode.

                    trace!(name = self.list.name, "going live");

                    self.list.set_range(0, max);
                    *position = max;
                    *live = true;

                    Observable::update_eq(&mut self.list.state.write().unwrap(), |state| {
                        *state = SlidingSyncState::Live;
                    });
                } else {
                    *position = end;
                    *live = false;
                    self.list.set_range(0, end);

                    Observable::update_eq(&mut self.list.state.write().unwrap(), |state| {
                        *state = SlidingSyncState::CatchingUp;
                    });
                }
            }

            GeneratorKind::Live => {
                Observable::update_eq(&mut self.list.state.write().unwrap(), |state| {
                    *state = SlidingSyncState::Live;
                });
            }
        }
    }
}

impl Iterator for SlidingSyncListRequestGenerator {
    type Item = v4::SyncRequestList;

    fn next(&mut self) -> Option<Self::Item> {
        match self.kind {
            GeneratorKind::PagingFullSync { live, .. }
            | GeneratorKind::GrowingFullSync { live, .. }
                if live =>
            {
                Some(self.live_request())
            }
            GeneratorKind::PagingFullSync { position, batch_size, limit, .. } => {
                Some(self.prefetch_request(position, batch_size, limit))
            }
            GeneratorKind::GrowingFullSync { position, batch_size, limit, .. } => {
                Some(self.prefetch_request(0, position + batch_size, limit))
            }
            GeneratorKind::Live => Some(self.live_request()),
        }
    }
}
