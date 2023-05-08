//! Builder for [`SlidingSyncList`].

use std::{
    convert::identity,
    fmt,
    sync::{Arc, RwLock as StdRwLock},
};

use eyeball::unique::Observable;
use eyeball_im::ObservableVector;
use ruma::{api::client::sync::sync_events::v4, events::StateEventType, UInt};
use tokio::sync::mpsc::Sender;

use super::{
    super::SlidingSyncInternalMessage, SlidingSyncList, SlidingSyncListInner,
    SlidingSyncListRequestGenerator, SlidingSyncMode, SlidingSyncState,
};

/// The default name for the full sync list.
pub const FULL_SYNC_LIST_NAME: &str = "full-sync";

/// Builder for [`SlidingSyncList`].
pub struct SlidingSyncListBuilder {
    sync_mode: SlidingSyncMode,
    sort: Vec<String>,
    required_state: Vec<(StateEventType, String)>,
    full_sync_batch_size: u32,
    full_sync_maximum_number_of_rooms_to_fetch: Option<u32>,
    filters: Option<v4::SyncRequestListFilters>,
    timeline_limit: Option<UInt>,
    name: String,
    ranges: Vec<(UInt, UInt)>,
    once_built: Box<dyn FnOnce(SlidingSyncList) -> SlidingSyncList + Send + Sync>,
}

// Clone the builder, except `once_built` which is reset to its default value.
impl Clone for SlidingSyncListBuilder {
    fn clone(&self) -> Self {
        Self {
            sync_mode: self.sync_mode.clone(),
            sort: self.sort.clone(),
            required_state: self.required_state.clone(),
            full_sync_batch_size: self.full_sync_batch_size,
            full_sync_maximum_number_of_rooms_to_fetch: self
                .full_sync_maximum_number_of_rooms_to_fetch,
            filters: self.filters.clone(),
            timeline_limit: self.timeline_limit,
            name: self.name.clone(),
            ranges: self.ranges.clone(),
            once_built: Box::new(identity),
        }
    }
}

// Print debug values for the builder, except `once_built` which is ignored.
impl fmt::Debug for SlidingSyncListBuilder {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SlidingSyncListBuilder")
            .field("sync_mode", &self.sync_mode)
            .field("sort", &self.sort)
            .field("required_state", &self.required_state)
            .field("full_sync_batch_size", &self.full_sync_batch_size)
            .field(
                "full_sync_maximum_number_of_rooms_to_fetch",
                &self.full_sync_maximum_number_of_rooms_to_fetch,
            )
            .field("filters", &self.filters)
            .field("timeline_limit", &self.timeline_limit)
            .field("name", &self.name)
            .field("ranges", &self.ranges)
            .finish_non_exhaustive()
    }
}

impl SlidingSyncListBuilder {
    pub(super) fn new(name: impl Into<String>) -> Self {
        Self {
            sync_mode: SlidingSyncMode::default(),
            sort: vec!["by_recency".to_owned(), "by_name".to_owned()],
            required_state: vec![
                (StateEventType::RoomEncryption, "".to_owned()),
                (StateEventType::RoomTombstone, "".to_owned()),
            ],
            full_sync_batch_size: 20,
            full_sync_maximum_number_of_rooms_to_fetch: None,
            filters: None,
            timeline_limit: None,
            name: name.into(),
            ranges: Vec::new(),
            once_built: Box::new(identity),
        }
    }

    /// foo
    pub fn once_built<C>(mut self, callback: C) -> Self
    where
        C: FnOnce(SlidingSyncList) -> SlidingSyncList + Send + Sync + 'static,
    {
        self.once_built = Box::new(callback);
        self
    }

    /// Create a Builder set up for full sync.
    pub fn default_with_fullsync() -> Self {
        Self::new(FULL_SYNC_LIST_NAME).sync_mode(SlidingSyncMode::Paging)
    }

    /// Which SlidingSyncMode to start this list under.
    pub fn sync_mode(mut self, value: SlidingSyncMode) -> Self {
        self.sync_mode = value;
        self
    }

    /// Sort the room list by this.
    pub fn sort(mut self, value: Vec<String>) -> Self {
        self.sort = value;
        self
    }

    /// Required states to return per room.
    pub fn required_state(mut self, value: Vec<(StateEventType, String)>) -> Self {
        self.required_state = value;
        self
    }

    /// When doing a full-sync, this method defines the value by which ranges of
    /// rooms will be extended.
    pub fn full_sync_batch_size(mut self, value: u32) -> Self {
        self.full_sync_batch_size = value;
        self
    }

    /// When doing a full-sync, this method defines the total limit of rooms to
    /// load (it can be useful for gigantic accounts).
    pub fn full_sync_maximum_number_of_rooms_to_fetch(
        mut self,
        value: impl Into<Option<u32>>,
    ) -> Self {
        self.full_sync_maximum_number_of_rooms_to_fetch = value.into();
        self
    }

    /// Any filters to apply to the query.
    pub fn filters(mut self, value: Option<v4::SyncRequestListFilters>) -> Self {
        self.filters = value;
        self
    }

    /// Set the limit of regular events to fetch for the timeline.
    pub fn timeline_limit<U: Into<UInt>>(mut self, timeline_limit: U) -> Self {
        self.timeline_limit = Some(timeline_limit.into());
        self
    }

    /// Reset the limit of regular events to fetch for the timeline. It is left
    /// to the server to decide how many to send back
    pub fn no_timeline_limit(mut self) -> Self {
        self.timeline_limit = Default::default();
        self
    }

    /// Set the ranges to fetch.
    pub fn ranges<U: Into<UInt>>(mut self, range: Vec<(U, U)>) -> Self {
        self.ranges = range.into_iter().map(|(a, b)| (a.into(), b.into())).collect();
        self
    }

    /// Set a single range fetch.
    pub fn set_range<U: Into<UInt>>(mut self, from: U, to: U) -> Self {
        self.ranges = vec![(from.into(), to.into())];
        self
    }

    /// Set the ranges to fetch.
    pub fn add_range<U: Into<UInt>>(mut self, from: U, to: U) -> Self {
        self.ranges.push((from.into(), to.into()));
        self
    }

    /// Set the ranges to fetch.
    pub fn reset_ranges(mut self) -> Self {
        self.ranges.clear();
        self
    }

    /// Build the list.
    pub(in super::super) fn build(
        self,
        sliding_sync_internal_channel_sender: Sender<SlidingSyncInternalMessage>,
    ) -> SlidingSyncList {
        let request_generator = match &self.sync_mode {
            SlidingSyncMode::Paging => SlidingSyncListRequestGenerator::new_paging(
                self.full_sync_batch_size,
                self.full_sync_maximum_number_of_rooms_to_fetch,
            ),

            SlidingSyncMode::Growing => SlidingSyncListRequestGenerator::new_growing(
                self.full_sync_batch_size,
                self.full_sync_maximum_number_of_rooms_to_fetch,
            ),

            SlidingSyncMode::Selective => SlidingSyncListRequestGenerator::new_selective(),
        };

        let list = SlidingSyncList {
            inner: Arc::new(SlidingSyncListInner {
                // From the builder
                sync_mode: self.sync_mode,
                sort: self.sort,
                required_state: self.required_state,
                filters: self.filters,
                timeline_limit: StdRwLock::new(Observable::new(self.timeline_limit)),
                name: self.name,
                ranges: StdRwLock::new(Observable::new(self.ranges)),

                // Computed from the builder.
                request_generator: StdRwLock::new(request_generator),

                // Default values for the type we are building.
                state: StdRwLock::new(Observable::new(SlidingSyncState::default())),
                maximum_number_of_rooms: StdRwLock::new(Observable::new(None)),
                room_list: StdRwLock::new(ObservableVector::new()),

                sliding_sync_internal_channel_sender,
            }),
        };

        let once_built = self.once_built;

        once_built(list)
    }
}
