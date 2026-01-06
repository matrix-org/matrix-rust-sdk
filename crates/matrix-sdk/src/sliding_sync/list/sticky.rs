use ruma::{api::client::sync::sync_events::v5 as http, events::StateEventType};

use crate::sliding_sync::sticky_parameters::StickyData;

/// The set of `SlidingSyncList` request parameters that are *sticky*, as
/// defined by the [Sliding Sync MSC](https://github.com/matrix-org/matrix-spec-proposals/blob/kegan/sync-v3/proposals/3575-sync.md).
#[derive(Debug)]
pub(super) struct SlidingSyncListStickyParameters {
    /// Any filters to apply to the query.
    filters: Option<http::request::ListFilters>,
}

impl SlidingSyncListStickyParameters {
    pub fn new(filters: Option<http::request::ListFilters>) -> Self {
        // Consider that each list will have at least one parameter set, so invalidate
        // it by default.
        Self { filters }
    }
}

impl StickyData for SlidingSyncListStickyParameters {
    type Request = http::request::List;

    fn apply(&self, request: &mut Self::Request) {
        request.filters = self.filters.clone();
    }
}
