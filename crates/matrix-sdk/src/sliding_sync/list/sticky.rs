use matrix_sdk_base::sliding_sync::http;
use ruma::events::StateEventType;

use super::Bound;
use crate::sliding_sync::sticky_parameters::StickyData;

/// The set of `SlidingSyncList` request parameters that are *sticky*, as
/// defined by the [Sliding Sync MSC](https://github.com/matrix-org/matrix-spec-proposals/blob/kegan/sync-v3/proposals/3575-sync.md).
#[derive(Debug)]
pub(super) struct SlidingSyncListStickyParameters {
    /// Required states to return per room.
    required_state: Vec<(StateEventType, String)>,

    /// Return a stripped variant of membership events for the users used to
    /// calculate the room name.
    include_heroes: Option<bool>,

    /// Any filters to apply to the query.
    filters: Option<http::request::ListFilters>,

    /// The maximum number of timeline events to query for.
    timeline_limit: Option<Bound>,
}

impl SlidingSyncListStickyParameters {
    pub fn new(
        required_state: Vec<(StateEventType, String)>,
        include_heroes: Option<bool>,
        filters: Option<http::request::ListFilters>,
        timeline_limit: Option<Bound>,
    ) -> Self {
        // Consider that each list will have at least one parameter set, so invalidate
        // it by default.
        Self { required_state, include_heroes, filters, timeline_limit }
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
    type Request = http::request::List;

    fn apply(&self, request: &mut Self::Request) {
        request.room_details.required_state = self.required_state.to_vec();
        request.room_details.timeline_limit = self.timeline_limit.map(Into::into);
        request.include_heroes = self.include_heroes;
        request.filters = self.filters.clone();
    }
}
