use matrix_sdk_base::sliding_sync::http;
use ruma::events::StateEventType;

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
}

impl SlidingSyncListStickyParameters {
    pub fn new(
        required_state: Vec<(StateEventType, String)>,
        include_heroes: Option<bool>,
        filters: Option<http::request::ListFilters>,
    ) -> Self {
        // Consider that each list will have at least one parameter set, so invalidate
        // it by default.
        Self { required_state, include_heroes, filters }
    }
}

impl StickyData for SlidingSyncListStickyParameters {
    type Request = http::request::List;

    fn apply(&self, request: &mut Self::Request) {
        request.room_details.required_state = self.required_state.to_vec();
        request.include_heroes = self.include_heroes;
        request.filters = self.filters.clone();
    }
}
