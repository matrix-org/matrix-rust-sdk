//! Builder for [`SlidingSyncList`].

use std::{
    fmt::Debug,
    sync::{atomic::AtomicBool, Arc, RwLock as StdRwLock},
};

use eyeball::Observable;
use eyeball_im::ObservableVector;
use im::Vector;
use ruma::{api::client::sync::sync_events::v4, events::StateEventType, UInt};

use super::{Error, RoomListEntry, SlidingSyncList, SlidingSyncMode, SlidingSyncState};
use crate::Result;

/// The default name for the full sync list.
pub const FULL_SYNC_LIST_NAME: &str = "full-sync";

/// Builder for [`SlidingSyncList`].
#[derive(Clone, Debug)]
pub struct SlidingSyncListBuilder {
    sync_mode: SlidingSyncMode,
    sort: Vec<String>,
    required_state: Vec<(StateEventType, String)>,
    batch_size: u32,
    send_updates_for_items: bool,
    limit: Option<u32>,
    filters: Option<v4::SyncRequestListFilters>,
    timeline_limit: Option<UInt>,
    name: Option<String>,
    state: SlidingSyncState,
    rooms_count: Option<u32>,
    rooms_list: Vector<RoomListEntry>,
    ranges: Vec<(UInt, UInt)>,
}

impl SlidingSyncListBuilder {
    pub(super) fn new() -> Self {
        Self {
            sync_mode: SlidingSyncMode::default(),
            sort: vec!["by_recency".to_owned(), "by_name".to_owned()],
            required_state: vec![
                (StateEventType::RoomEncryption, "".to_owned()),
                (StateEventType::RoomTombstone, "".to_owned()),
            ],
            batch_size: 20,
            send_updates_for_items: false,
            limit: None,
            filters: None,
            timeline_limit: None,
            name: None,
            state: SlidingSyncState::default(),
            rooms_count: None,
            rooms_list: Vector::new(),
            ranges: Vec::new(),
        }
    }

    /// Create a Builder set up for full sync.
    pub fn default_with_fullsync() -> Self {
        Self::new().name(FULL_SYNC_LIST_NAME).sync_mode(SlidingSyncMode::PagingFullSync)
    }

    /// Which SlidingSyncMode to start this list under.
    pub fn sync_mode(mut self, value: SlidingSyncMode) -> Self {
        self.sync_mode = value;
        self
    }

    /// Sort the rooms list by this.
    pub fn sort(mut self, value: Vec<String>) -> Self {
        self.sort = value;
        self
    }

    /// Required states to return per room.
    pub fn required_state(mut self, value: Vec<(StateEventType, String)>) -> Self {
        self.required_state = value;
        self
    }

    /// How many rooms request at a time when doing a full-sync catch up.
    pub fn batch_size(mut self, value: u32) -> Self {
        self.batch_size = value;
        self
    }

    /// Whether the list should send `UpdatedAt`-Diff signals for rooms that
    /// have changed.
    pub fn send_updates_for_items(mut self, value: bool) -> Self {
        self.send_updates_for_items = value;
        self
    }

    /// How many rooms request a total hen doing a full-sync catch up.
    pub fn limit(mut self, value: impl Into<Option<u32>>) -> Self {
        self.limit = value.into();
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

    /// Set the name of this list, to easily recognize it.
    pub fn name(mut self, value: impl Into<String>) -> Self {
        self.name = Some(value.into());
        self
    }

    /// Set the ranges to fetch
    pub fn ranges<U: Into<UInt>>(mut self, range: Vec<(U, U)>) -> Self {
        self.ranges = range.into_iter().map(|(a, b)| (a.into(), b.into())).collect();
        self
    }

    /// Set a single range fetch
    pub fn set_range<U: Into<UInt>>(mut self, from: U, to: U) -> Self {
        self.ranges = vec![(from.into(), to.into())];
        self
    }

    /// Set the ranges to fetch
    pub fn add_range<U: Into<UInt>>(mut self, from: U, to: U) -> Self {
        self.ranges.push((from.into(), to.into()));
        self
    }

    /// Set the ranges to fetch
    pub fn reset_ranges(mut self) -> Self {
        self.ranges = Default::default();
        self
    }

    /// Build the list
    pub fn build(self) -> Result<SlidingSyncList> {
        let mut rooms_list = ObservableVector::new();
        rooms_list.append(self.rooms_list);

        Ok(SlidingSyncList {
            sync_mode: self.sync_mode,
            sort: self.sort,
            required_state: self.required_state,
            batch_size: self.batch_size,
            send_updates_for_items: self.send_updates_for_items,
            limit: self.limit,
            filters: self.filters,
            timeline_limit: Arc::new(StdRwLock::new(Observable::new(self.timeline_limit))),
            name: self.name.ok_or(Error::BuildMissingField("name"))?,
            state: Arc::new(StdRwLock::new(Observable::new(self.state))),
            rooms_count: Arc::new(StdRwLock::new(Observable::new(self.rooms_count))),
            rooms_list: Arc::new(StdRwLock::new(rooms_list)),
            ranges: Arc::new(StdRwLock::new(Observable::new(self.ranges))),
            is_cold: Arc::new(AtomicBool::new(false)),
            rooms_updated_broadcast: Arc::new(StdRwLock::new(Observable::new(()))),
        })
    }
}
