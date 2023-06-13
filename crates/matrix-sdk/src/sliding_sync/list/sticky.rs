use ruma::{
    api::client::sync::sync_events::v4,
    events::{StateEventType, TimelineEventType},
};

use super::Bound;
use crate::sliding_sync::sticky_parameters::StickyData;

/// The set of `SlidingSyncList` request parameters that are *sticky*, as
/// defined by the [Sliding Sync MSC](https://github.com/matrix-org/matrix-spec-proposals/blob/kegan/sync-v3/proposals/3575-sync.md).
#[derive(Debug)]
pub(super) struct SlidingSyncListStickyParameters {
    /// Sort the room list by this.
    sort: Vec<String>,

    /// Required states to return per room.
    required_state: Vec<(StateEventType, String)>,

    /// Any filters to apply to the query.
    filters: Option<v4::SyncRequestListFilters>,

    /// The maximum number of timeline events to query for.
    timeline_limit: Option<Bound>,

    /// The `bump_event_types` field. See
    /// [`SlidingSyncListBuilder::bump_event_types`] to learn more.
    bump_event_types: Vec<TimelineEventType>,
}

impl SlidingSyncListStickyParameters {
    pub fn new(
        sort: Vec<String>,
        required_state: Vec<(StateEventType, String)>,
        filters: Option<v4::SyncRequestListFilters>,
        timeline_limit: Option<Bound>,
        bump_event_types: Vec<TimelineEventType>,
    ) -> Self {
        // Consider that each list will have at least one parameter set, so invalidate
        // it by default.
        Self { sort, required_state, filters, timeline_limit, bump_event_types }
    }
}

impl SlidingSyncListStickyParameters {
    pub(super) fn timeline_limit(&self) -> Option<Bound> {
        self.timeline_limit
    }

    pub(super) fn set_timeline_limit(&mut self, timeline: Option<Bound>) {
        self.timeline_limit = timeline;
    }
}

impl StickyData for SlidingSyncListStickyParameters {
    type Request = v4::SyncRequestList;

    fn apply(&self, request: &mut v4::SyncRequestList) {
        request.sort = self.sort.to_vec();
        request.room_details.required_state = self.required_state.to_vec();
        request.room_details.timeline_limit = self.timeline_limit.map(Into::into);
        request.filters = self.filters.clone();
        request.bump_event_types = self.bump_event_types.clone();
    }
}
