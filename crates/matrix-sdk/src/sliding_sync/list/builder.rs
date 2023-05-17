//! Builder for [`SlidingSyncList`].

use std::{
    collections::BTreeMap,
    convert::identity,
    fmt,
    ops::RangeInclusive,
    sync::{Arc, RwLock as StdRwLock},
};

use eyeball::unique::Observable;
use eyeball_im::ObservableVector;
use imbl::Vector;
use ruma::{api::client::sync::sync_events::v4, events::StateEventType, OwnedRoomId};
use tokio::sync::mpsc::Sender;

use super::{
    super::SlidingSyncInternalMessage, Bound, SlidingSyncList, SlidingSyncListCachePolicy,
    SlidingSyncListInner, SlidingSyncListRequestGenerator, SlidingSyncMode, SlidingSyncState,
};
use crate::{
    sliding_sync::{cache::restore_sliding_sync_list, FrozenSlidingSyncRoom},
    Client, RoomListEntry,
};

/// Data that might have been read from the cache.
#[derive(Clone)]
struct SlidingSyncListCachedData {
    /// Total number of rooms that is possible to interact with the given list.
    /// See also comment of [`SlidingSyncList::maximum_number_of_rooms`].
    /// May be reloaded from the cache.
    maximum_number_of_rooms: Option<u32>,

    /// List of room entries.
    /// May be reloaded from the cache.
    room_list: Vector<RoomListEntry>,
}

/// Builder for [`SlidingSyncList`].
#[derive(Clone)]
pub struct SlidingSyncListBuilder {
    sync_mode: SlidingSyncMode,
    sort: Vec<String>,
    required_state: Vec<(StateEventType, String)>,
    filters: Option<v4::SyncRequestListFilters>,
    timeline_limit: Option<Bound>,
    name: String,
    ranges: Vec<RangeInclusive<Bound>>,

    /// Should this list be cached and reloaded from the cache?
    cache_policy: SlidingSyncListCachePolicy,

    /// If set, temporary data that's been read from the cache, reloaded from a
    /// `FrozenSlidingSyncList`.
    reloaded_cached_data: Option<SlidingSyncListCachedData>,

    once_built: Arc<Box<dyn Fn(SlidingSyncList) -> SlidingSyncList + Send + Sync>>,
}

// Print debug values for the builder, except `once_built` which is ignored.
impl fmt::Debug for SlidingSyncListBuilder {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SlidingSyncListBuilder")
            .field("sync_mode", &self.sync_mode)
            .field("sort", &self.sort)
            .field("required_state", &self.required_state)
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
            filters: None,
            timeline_limit: None,
            name: name.into(),
            ranges: Vec::new(),
            reloaded_cached_data: None,
            cache_policy: SlidingSyncListCachePolicy::Disabled,
            once_built: Arc::new(Box::new(identity)),
        }
    }

    /// Runs a callback once the list has been built.
    ///
    /// If the list was cached, then the cached fields won't be available in
    /// this callback. Use the streams to get published versions of the
    /// cached fields, once they've been set.
    pub fn once_built<C>(mut self, callback: C) -> Self
    where
        C: Fn(SlidingSyncList) -> SlidingSyncList + Send + Sync + 'static,
    {
        self.once_built = Arc::new(Box::new(callback));
        self
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

    /// Any filters to apply to the query.
    pub fn filters(mut self, value: Option<v4::SyncRequestListFilters>) -> Self {
        self.filters = value;
        self
    }

    /// Set the limit of regular events to fetch for the timeline.
    pub fn timeline_limit(mut self, timeline_limit: Bound) -> Self {
        self.timeline_limit = Some(timeline_limit);
        self
    }

    /// Reset the limit of regular events to fetch for the timeline. It is left
    /// to the server to decide how many to send back
    pub fn no_timeline_limit(mut self) -> Self {
        self.timeline_limit = Default::default();
        self
    }

    /// Set the ranges to fetch.
    pub fn ranges(mut self, ranges: Vec<RangeInclusive<Bound>>) -> Self {
        self.ranges = ranges;
        self
    }

    /// Set a single range to fetch.
    pub fn set_range(mut self, range: RangeInclusive<Bound>) -> Self {
        self.ranges = vec![range];
        self
    }

    /// Set the ranges to fetch.
    pub fn add_range(mut self, range: RangeInclusive<Bound>) -> Self {
        self.ranges.push(range);
        self
    }

    /// Reset the ranges to fetch.
    pub fn reset_ranges(mut self) -> Self {
        self.ranges.clear();
        self
    }

    /// Marks this list as sync'd from the cache, and attempts to reload it from
    /// storage.
    ///
    /// Returns a mapping of the room's data read from the cache, to be
    /// incorporated into the `SlidingSync` bookkeepping.
    pub(in super::super) async fn set_cached_and_reload(
        &mut self,
        client: &Client,
        storage_key: &str,
    ) -> crate::Result<BTreeMap<OwnedRoomId, FrozenSlidingSyncRoom>> {
        self.cache_policy = SlidingSyncListCachePolicy::Enabled;
        if let Some(frozen_list) =
            restore_sliding_sync_list(client.store(), storage_key, &self.name).await?
        {
            assert!(
                self.reloaded_cached_data.is_none(),
                "can't call `set_cached_and_reload` twice"
            );
            self.reloaded_cached_data = Some(SlidingSyncListCachedData {
                maximum_number_of_rooms: frozen_list.maximum_number_of_rooms,
                room_list: frozen_list.room_list,
            });
            Ok(frozen_list.rooms)
        } else {
            Ok(Default::default())
        }
    }

    /// Build the list.
    pub(in super::super) fn build(
        self,
        sliding_sync_internal_channel_sender: Sender<SlidingSyncInternalMessage>,
    ) -> SlidingSyncList {
        let list = SlidingSyncList {
            inner: Arc::new(SlidingSyncListInner {
                // From the builder
                sync_mode: self.sync_mode.clone(),
                sort: self.sort,
                required_state: self.required_state,
                filters: self.filters,
                timeline_limit: StdRwLock::new(Observable::new(self.timeline_limit)),
                name: self.name,
                ranges: StdRwLock::new(Observable::new(self.ranges)),
                cache_policy: self.cache_policy,

                // Computed from the builder.
                request_generator: StdRwLock::new(SlidingSyncListRequestGenerator::new(
                    self.sync_mode,
                )),

                // Values read from deserialization, or that are still equal to the default values
                // otherwise.
                state: StdRwLock::new(Observable::new(Default::default())),
                maximum_number_of_rooms: StdRwLock::new(Observable::new(None)),
                room_list: StdRwLock::new(ObservableVector::from(Vector::new())),

                // Internal data.
                sliding_sync_internal_channel_sender,
            }),
        };

        let once_built = self.once_built;

        let list = once_built(list);

        // If we reloaded from the cache, update values in the list here.
        //
        // Note about ordering: because of the contract with the observables, the
        // initial values, if filled, have to be observable in the `once_built`
        // callback. That's why we're doing this here *after* constructing the
        // list, and not a few lines above.

        if let Some(SlidingSyncListCachedData { maximum_number_of_rooms, room_list }) =
            self.reloaded_cached_data
        {
            // Mark state as preloaded.
            Observable::set(&mut list.inner.state.write().unwrap(), SlidingSyncState::Preloaded);

            // Reload values.
            Observable::set(
                &mut list.inner.maximum_number_of_rooms.write().unwrap(),
                maximum_number_of_rooms,
            );

            let mut prev_room_list = list.inner.room_list.write().unwrap();
            assert!(prev_room_list.is_empty(), "room list was empty on creation above!");
            prev_room_list.append(room_list);
        }

        list
    }
}
