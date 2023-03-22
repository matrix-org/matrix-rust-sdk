mod builder;
mod request_generator;

use std::{
    cmp::min,
    collections::BTreeMap,
    fmt::Debug,
    iter,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, RwLock as StdRwLock,
    },
};

pub use builder::*;
use eyeball::unique::Observable;
use eyeball_im::{ObservableVector, VectorDiff};
use futures_core::Stream;
use imbl::Vector;
pub(super) use request_generator::*;
use ruma::{
    api::client::sync::sync_events::v4, assign, events::StateEventType, OwnedRoomId, RoomId, UInt,
};
use serde::{Deserialize, Serialize};
use tracing::{debug, error, instrument, warn};

use super::{Error, FrozenSlidingSyncRoom, SlidingSyncRoom};
use crate::Result;

/// Holding a specific filtered list within the concept of sliding sync.
/// Main entrypoint to the `SlidingSync`:
///
/// ```no_run
/// # use futures::executor::block_on;
/// # use matrix_sdk::Client;
/// # use url::Url;
/// # block_on(async {
/// # let homeserver = Url::parse("http://example.com")?;
/// let client = Client::new(homeserver).await?;
/// let sliding_sync =
///     client.sliding_sync().await.add_fullsync_list().build().await?;
///
/// # anyhow::Ok(())
/// # });
/// ```
///
/// It is OK to clone this type as much as you need: cloning it is cheap.
#[derive(Clone, Debug)]
pub struct SlidingSyncList {
    inner: Arc<SlidingSyncListInner>,
}

impl SlidingSyncList {
    /// Get the name of the list.
    pub fn name(&self) -> &str {
        self.inner.name.as_str()
    }

    /// Calculate the next request and return it.
    pub(super) fn next_request(&mut self) -> Option<v4::SyncRequestList> {
        self.inner.next_request()
    }

    pub(crate) fn set_from_cold(
        &mut self,
        maximum_number_of_rooms: Option<u32>,
        rooms_list: Vector<RoomListEntry>,
    ) {
        Observable::set(&mut self.inner.state.write().unwrap(), SlidingSyncState::Preloaded);
        self.inner.is_cold.store(true, Ordering::SeqCst);
        Observable::set(
            &mut self.inner.maximum_number_of_rooms.write().unwrap(),
            maximum_number_of_rooms,
        );

        let mut lock = self.inner.rooms_list.write().unwrap();
        lock.clear();
        lock.append(rooms_list);
    }

    /// Create a new [`SlidingSyncListBuilder`].
    pub fn builder() -> SlidingSyncListBuilder {
        SlidingSyncListBuilder::new()
    }

    /// Return a builder with the same settings as before
    pub fn new_builder(&self) -> SlidingSyncListBuilder {
        let mut builder = Self::builder()
            .name(&self.inner.name)
            .sync_mode(self.inner.sync_mode.clone())
            .sort(self.inner.sort.clone())
            .required_state(self.inner.required_state.clone())
            .full_sync_batch_size(self.inner.full_sync_batch_size)
            .full_sync_maximum_number_of_rooms_to_fetch(
                self.inner.full_sync_maximum_number_of_rooms_to_fetch,
            )
            .send_updates_for_items(self.inner.send_updates_for_items)
            .filters(self.inner.filters.clone())
            .ranges(self.inner.ranges.read().unwrap().clone());

        if let Some(timeline_limit) = Observable::get(&self.inner.timeline_limit.read().unwrap()) {
            builder = builder.timeline_limit(*timeline_limit);
        }

        builder
    }

    /// Set the ranges to fetch.
    ///
    /// Remember to cancel the existing stream and fetch a new one as this will
    /// only be applied on the next request.
    pub fn set_ranges<U>(&self, ranges: Vec<(U, U)>) -> &Self
    where
        U: Into<UInt>,
    {
        let ranges = ranges.into_iter().map(|(a, b)| (a.into(), b.into())).collect();
        Observable::set(&mut self.inner.ranges.write().unwrap(), ranges);

        self
    }

    /// Reset the ranges to a particular set
    ///
    /// Remember to cancel the existing stream and fetch a new one as this will
    /// only be applied on the next request.
    pub fn set_range<U>(&self, start: U, end: U) -> &Self
    where
        U: Into<UInt>,
    {
        self.inner.set_range(start, end);

        self
    }

    /// Set the ranges to fetch
    ///
    /// Remember to cancel the existing stream and fetch a new one as this will
    /// only be applied on the next request.
    pub fn add_range<U>(&self, start: U, end: U) -> &Self
    where
        U: Into<UInt>,
    {
        Observable::update(&mut self.inner.ranges.write().unwrap(), |ranges| {
            ranges.push((start.into(), end.into()));
        });

        self
    }

    /// Set the ranges to fetch
    ///
    /// Note: sending an empty list of ranges is, according to the spec, to be
    /// understood that the consumer doesn't care about changes of the room
    /// order but you will only receive updates when for rooms entering or
    /// leaving the set.
    ///
    /// Remember to cancel the existing stream and fetch a new one as this will
    /// only be applied on the next request.
    pub fn reset_ranges(&self) -> &Self {
        Observable::set(&mut self.inner.ranges.write().unwrap(), Vec::new());

        self
    }

    /// Get the current state.
    pub fn state(&self) -> SlidingSyncState {
        self.inner.state.read().unwrap().clone()
    }

    /// Get a stream of state.
    pub fn state_stream(&self) -> impl Stream<Item = SlidingSyncState> {
        Observable::subscribe(&self.inner.state.read().unwrap())
    }

    /// Get the timeline limit.
    pub fn timeline_limit(&self) -> Option<UInt> {
        **self.inner.timeline_limit.read().unwrap()
    }

    /// Set timeline limit.
    pub fn set_timeline_limit<U>(&self, timeline: Option<U>)
    where
        U: Into<UInt>,
    {
        let timeline = timeline.map(Into::into);

        Observable::set(&mut self.inner.timeline_limit.write().unwrap(), timeline);
    }

    /// Get the current rooms list.
    pub fn rooms_list<R>(&self) -> Vec<R>
    where
        R: for<'a> From<&'a RoomListEntry>,
    {
        self.inner.rooms_list.read().unwrap().iter().map(|e| R::from(e)).collect()
    }

    /// Get a stream of rooms list.
    pub fn rooms_list_stream(&self) -> impl Stream<Item = VectorDiff<RoomListEntry>> {
        ObservableVector::subscribe(&self.inner.rooms_list.read().unwrap())
    }

    /// Get the maximum number of rooms. See [`Self::maximum_number_of_rooms`]
    /// to learn more.
    pub fn maximum_number_of_rooms(&self) -> Option<u32> {
        **self.inner.maximum_number_of_rooms.read().unwrap()
    }

    /// Get a stream of rooms count.
    pub fn maximum_number_of_rooms_stream(&self) -> impl Stream<Item = Option<u32>> {
        Observable::subscribe(&self.inner.maximum_number_of_rooms.read().unwrap())
    }

    /// Get a stream of `room_updated_broadcast`.
    pub fn rooms_updated_broadcast_stream(&self) -> impl Stream<Item = ()> {
        Observable::subscribe(&self.inner.rooms_updated_broadcast.read().unwrap())
    }

    /// Find the current valid position of the room in the list `room_list`.
    ///
    /// Only matches against the current ranges and only against filled items.
    /// Invalid items are ignore. Return the total position the item was
    /// found in the room_list, return None otherwise.
    pub fn find_room_in_list(&self, room_id: &RoomId) -> Option<usize> {
        self.inner.find_room_in_list(room_id)
    }

    /// Find the current valid position of the rooms in the lists `room_list`.
    ///
    /// Only matches against the current ranges and only against filled items.
    /// Invalid items are ignore. Return the total position the items that were
    /// found in the `room_list`, will skip any room not found in the
    /// `rooms_list`.
    pub fn find_rooms_in_list(&self, room_ids: &[OwnedRoomId]) -> Vec<(usize, OwnedRoomId)> {
        self.inner.find_rooms_in_list(room_ids)
    }

    /// Return the `room_id` at the given index.
    pub fn get_room_id(&self, index: usize) -> Option<OwnedRoomId> {
        self.inner
            .rooms_list
            .read()
            .unwrap()
            .get(index)
            .and_then(|room_list_entry| room_list_entry.as_room_id().map(ToOwned::to_owned))
    }

    // Handle the response from the server.
    #[instrument(skip(self, ops), fields(name = self.name(), ops_count = ops.len()))]
    pub(super) fn handle_response(
        &mut self,
        maximum_number_of_rooms: u32,
        ops: &Vec<v4::SyncOp>,
        updated_rooms: &Vec<OwnedRoomId>,
    ) -> Result<bool, Error> {
        let response = self.inner.update_state(
            maximum_number_of_rooms,
            ops,
            &self.inner.request_generator.read().unwrap().ranges,
            updated_rooms,
        )?;
        self.inner.update_request_generator_state(maximum_number_of_rooms);

        Ok(response)
    }
}

#[derive(Debug)]
pub(super) struct SlidingSyncListInner {
    /// Which [`SlidingSyncMode`] to start this list under.
    sync_mode: SlidingSyncMode,

    /// Sort the rooms list by this.
    sort: Vec<String>,

    /// Required states to return per room.
    required_state: Vec<(StateEventType, String)>,

    /// When doing a full-sync, the ranges of rooms to load are extended by this
    /// `full_sync_batch_size` size.
    full_sync_batch_size: u32,

    /// When doing a full-sync, it is possible to limit the total number of
    /// rooms to load by using this field.
    full_sync_maximum_number_of_rooms_to_fetch: Option<u32>,

    /// Whether the list should send `UpdatedAt`-Diff signals for rooms
    /// that have changed.
    send_updates_for_items: bool,

    /// Any filters to apply to the query.
    filters: Option<v4::SyncRequestListFilters>,

    /// The maximum number of timeline events to query for.
    timeline_limit: StdRwLock<Observable<Option<UInt>>>,

    /// Name of this list to easily recognize them.
    name: String,

    /// The state this list is in.
    state: StdRwLock<Observable<SlidingSyncState>>,

    /// The total number of rooms that is possible to interact with for the
    /// given list.
    ///
    /// It's not the total rooms that have been fetched. The server tells the
    /// client that it's possible to fetch this amount of rooms maximum.
    /// Since this number can change according to the list filters, it's
    /// observable.
    maximum_number_of_rooms: StdRwLock<Observable<Option<u32>>>,

    /// The rooms in order.
    rooms_list: StdRwLock<ObservableVector<RoomListEntry>>,

    /// The ranges windows of the list.
    #[allow(clippy::type_complexity)] // temporarily
    ranges: StdRwLock<Observable<Vec<(UInt, UInt)>>>,

    /// Get informed if anything in the room changed.
    ///
    /// If you only care to know about changes once all of them have applied
    /// (including the total), subscribe to this observable.
    rooms_updated_broadcast: StdRwLock<Observable<()>>,

    is_cold: AtomicBool,

    /// The request generator, i.e. a type that yields the appropriate list
    /// request. See [`SlidingSyncListRequestGenerator`] to learn more.
    request_generator: StdRwLock<SlidingSyncListRequestGenerator>,
}

impl SlidingSyncListInner {
    fn set_range<U>(&self, start: U, end: U)
    where
        U: Into<UInt>,
    {
        let value = vec![(start.into(), end.into())];
        Observable::set(&mut self.ranges.write().unwrap(), value);
    }

    fn next_request(&self) -> Option<v4::SyncRequestList> {
        {
            // Use a dedicated scope to ensure the lock is released before continuing.
            let mut request_generator = self.request_generator.write().unwrap();

            match request_generator.kind {
                // Cases where all rooms have been fully loaded.
                SlidingSyncListRequestGeneratorKind::PagingFullSync {
                    fully_loaded: true, ..
                }
                | SlidingSyncListRequestGeneratorKind::GrowingFullSync {
                    fully_loaded: true, ..
                }
                | SlidingSyncListRequestGeneratorKind::Selective => {
                    // Let's copy all the ranges from `SlidingSyncList`.
                    request_generator.ranges = self.ranges.read().unwrap().clone();
                }

                SlidingSyncListRequestGeneratorKind::PagingFullSync {
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
                    request_generator.ranges = vec![create_range(
                        range_start,
                        range_desired_size,
                        maximum_number_of_rooms_to_fetch,
                        **self.maximum_number_of_rooms.read().unwrap(),
                    )?];
                }

                SlidingSyncListRequestGeneratorKind::GrowingFullSync {
                    number_of_fetched_rooms,
                    batch_size,
                    maximum_number_of_rooms_to_fetch,
                    ..
                } => {
                    // In growing-mode, range always starts from 0. However, the end is growing by
                    // adding `batch_size` to the previous number of fetched rooms.
                    let range_start = 0;
                    let range_desired_size = number_of_fetched_rooms.saturating_add(batch_size);

                    // Create a new range, and use it as the current set of ranges.
                    request_generator.ranges = vec![create_range(
                        range_start,
                        range_desired_size,
                        maximum_number_of_rooms_to_fetch,
                        **self.maximum_number_of_rooms.read().unwrap(),
                    )?];
                }
            }
        }

        // Here we go.
        Some(self.request())
    }

    /// Build a [`SyncRequestList`][v4::SyncRequestList] based on the current
    /// state of the request generator.
    #[instrument(skip(self), fields(name = self.name, ranges = ?&self.ranges))]
    fn request(&self) -> v4::SyncRequestList {
        let ranges = self.request_generator.read().unwrap().ranges.clone();
        let sort = self.sort.clone();
        let required_state = self.required_state.clone();
        let timeline_limit = **self.timeline_limit.read().unwrap();
        let filters = self.filters.clone();

        assign!(v4::SyncRequestList::default(), {
            ranges,
            room_details: assign!(v4::RoomDetailsConfig::default(), {
                required_state,
                timeline_limit,
            }),
            sort,
            filters,
        })
    }

    // Update the [`SlidingSyncListInner`]'s state.
    fn update_state(
        &self,
        maximum_number_of_rooms: u32,
        ops: &Vec<v4::SyncOp>,
        ranges: &[(UInt, UInt)],
        updated_rooms: &Vec<OwnedRoomId>,
    ) -> Result<bool, Error> {
        let ranges = ranges
            .iter()
            .map(|(start, end)| ((*start).try_into().unwrap(), (*end).try_into().unwrap()))
            .collect::<Vec<(usize, usize)>>();

        let current_maximum_number_of_rooms = **self.maximum_number_of_rooms.read().unwrap();

        if current_maximum_number_of_rooms.is_none()
            || current_maximum_number_of_rooms == Some(0)
            || self.is_cold.load(Ordering::SeqCst)
        {
            debug!("first run, replacing rooms list");

            // first response, we do that slightly differently
            let mut rooms_list = ObservableVector::new();
            rooms_list.append(
                iter::repeat(RoomListEntry::Empty).take(maximum_number_of_rooms as usize).collect(),
            );

            // then we apply it
            room_ops(&mut rooms_list, ops, &ranges)?;

            {
                let mut lock = self.rooms_list.write().unwrap();
                lock.clear();
                lock.append(rooms_list.into_inner());
            }

            Observable::set(
                &mut self.maximum_number_of_rooms.write().unwrap(),
                Some(maximum_number_of_rooms),
            );
            self.is_cold.store(false, Ordering::SeqCst);

            return Ok(true);
        }

        debug!("regular update");

        let mut missing = maximum_number_of_rooms
            .checked_sub(self.rooms_list.read().unwrap().len() as u32)
            .unwrap_or_default();
        let mut changed = false;

        if missing > 0 {
            let mut list = self.rooms_list.write().unwrap();

            while missing > 0 {
                list.push_back(RoomListEntry::Empty);
                missing -= 1;
            }

            changed = true;
        }

        {
            // keep the lock scoped so that the later `find_rooms_in_list` doesn't deadlock
            let mut rooms_list = self.rooms_list.write().unwrap();

            if !ops.is_empty() {
                room_ops(&mut rooms_list, ops, &ranges)?;
                changed = true;
            } else {
                debug!("no rooms operations found");
            }
        }

        {
            let mut lock = self.maximum_number_of_rooms.write().unwrap();

            if **lock != Some(maximum_number_of_rooms) {
                Observable::set(&mut lock, Some(maximum_number_of_rooms));
                changed = true;
            }
        }

        if self.send_updates_for_items && !updated_rooms.is_empty() {
            let found_lists = self.find_rooms_in_list(updated_rooms);

            if !found_lists.is_empty() {
                debug!("room details found");
                let mut rooms_list = self.rooms_list.write().unwrap();

                for (pos, room_id) in found_lists {
                    // trigger an `UpdatedAt` update
                    rooms_list.set(pos, RoomListEntry::Filled(room_id));
                    changed = true;
                }
            }
        }

        if changed {
            Observable::set(&mut self.rooms_updated_broadcast.write().unwrap(), ());
        }

        Ok(changed)
    }

    /// Update the state of the [`SlidingSyncListRequestGenerator`].
    fn update_request_generator_state(&self, maximum_number_of_rooms: u32) {
        let mut request_generator = self.request_generator.write().unwrap();

        let Some(range_end) = request_generator.ranges.first().map(|(_start, end)| u32::try_from(*end).unwrap()) else {
            error!(name = self.name, "The request generator must have a range.");

            return;
        };

        match &mut request_generator.kind {
            SlidingSyncListRequestGeneratorKind::PagingFullSync {
                number_of_fetched_rooms,
                fully_loaded,
                maximum_number_of_rooms_to_fetch,
                ..
            }
            | SlidingSyncListRequestGeneratorKind::GrowingFullSync {
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
                    // Update the number of fetched rooms forward. Do not forget that ranges are
                    // inclusive, so let's add 1.
                    *number_of_fetched_rooms = range_end.saturating_add(1);

                    // The list is still not fully loaded.
                    *fully_loaded = false;

                    // Update the _list range_ to cover from 0 to `range_end`.
                    // The list' range is different from the request generator (this) range.
                    self.set_range(0, range_end);

                    // Finally, let's update the list' state.
                    Observable::update_eq(&mut self.state.write().unwrap(), |state| {
                        *state = SlidingSyncState::PartiallyLoaded;
                    });
                }
                // Otherwise the current range has reached its maximum, we switched to `FullyLoaded`
                // mode.
                else {
                    // The number of fetched rooms is set to the maximum too.
                    *number_of_fetched_rooms = range_maximum;

                    // We update the `fully_loaded` marker.
                    *fully_loaded = true;

                    // The range is covering the entire list, from 0 to its maximum.
                    self.set_range(0, range_maximum);

                    // Finally, let's update the list' state.
                    Observable::update_eq(&mut self.state.write().unwrap(), |state| {
                        *state = SlidingSyncState::FullyLoaded;
                    });
                }
            }

            SlidingSyncListRequestGeneratorKind::Selective => {
                // Selective mode always loads everything.
                Observable::update_eq(&mut self.state.write().unwrap(), |state| {
                    *state = SlidingSyncState::FullyLoaded;
                });
            }
        }
    }

    fn find_room_in_list(&self, room_id: &RoomId) -> Option<usize> {
        let ranges = self.ranges.read().unwrap();
        let listing = self.rooms_list.read().unwrap();

        for (start_uint, end_uint) in ranges.iter() {
            let mut current_position: usize = (*start_uint).try_into().unwrap();
            let end: usize = (*end_uint).try_into().unwrap();
            let room_list_entries = listing.iter().skip(current_position);

            for room_list_entry in room_list_entries {
                if let RoomListEntry::Filled(this_room_id) = room_list_entry {
                    if room_id == this_room_id {
                        return Some(current_position);
                    }
                }

                if current_position == end {
                    break;
                }

                current_position += 1;
            }
        }

        None
    }

    fn find_rooms_in_list(&self, room_ids: &[OwnedRoomId]) -> Vec<(usize, OwnedRoomId)> {
        let ranges = self.ranges.read().unwrap();
        let listing = self.rooms_list.read().unwrap();
        let mut rooms_found = Vec::new();

        for (start_uint, end_uint) in ranges.iter() {
            let mut current_position: usize = (*start_uint).try_into().unwrap();
            let end: usize = (*end_uint).try_into().unwrap();
            let room_list_entries = listing.iter().skip(current_position);

            for room_list_entry in room_list_entries {
                if let RoomListEntry::Filled(room_id) = room_list_entry {
                    if room_ids.contains(room_id) {
                        rooms_found.push((current_position, room_id.clone()));
                    }
                }

                if current_position == end {
                    break;
                }

                current_position += 1;
            }
        }

        rooms_found
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub(super) struct FrozenSlidingSyncList {
    #[serde(default, rename = "rooms_count", skip_serializing_if = "Option::is_none")]
    pub(super) maximum_number_of_rooms: Option<u32>,
    #[serde(default, skip_serializing_if = "Vector::is_empty")]
    pub(super) rooms_list: Vector<RoomListEntry>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub(super) rooms: BTreeMap<OwnedRoomId, FrozenSlidingSyncRoom>,
}

impl FrozenSlidingSyncList {
    pub(super) fn freeze(
        source_list: &SlidingSyncList,
        rooms_map: &BTreeMap<OwnedRoomId, SlidingSyncRoom>,
    ) -> Self {
        let mut rooms = BTreeMap::new();
        let mut rooms_list = Vector::new();

        for room_list_entry in source_list.inner.rooms_list.read().unwrap().iter() {
            match room_list_entry {
                RoomListEntry::Filled(room_id) | RoomListEntry::Invalidated(room_id) => {
                    rooms.insert(
                        room_id.clone(),
                        rooms_map.get(room_id).expect("room doesn't exist").into(),
                    );
                }

                _ => {}
            };

            rooms_list.push_back(room_list_entry.freeze_by_ref());
        }

        FrozenSlidingSyncList {
            maximum_number_of_rooms: source_list.maximum_number_of_rooms(),
            rooms_list,
            rooms,
        }
    }
}

#[instrument(skip(operations))]
fn room_ops(
    rooms_list: &mut ObservableVector<RoomListEntry>,
    operations: &Vec<v4::SyncOp>,
    room_ranges: &Vec<(usize, usize)>,
) -> Result<(), Error> {
    let index_in_range = |idx| room_ranges.iter().any(|(start, end)| idx >= *start && idx <= *end);

    for operation in operations {
        match &operation.op {
            v4::SlidingOp::Sync => {
                let start: u32 = operation
                    .range
                    .ok_or_else(|| {
                        Error::BadResponse(
                            "`range` must be present for Sync and Update operation".to_owned(),
                        )
                    })?
                    .0
                    .try_into()
                    .map_err(|error| {
                        Error::BadResponse(format!("`range` not a valid int: {error}"))
                    })?;
                let room_ids = operation.room_ids.clone();

                room_ids
                    .into_iter()
                    .enumerate()
                    .map(|(i, r)| {
                        let idx = start as usize + i;

                        if idx >= rooms_list.len() {
                            rooms_list.insert(idx, RoomListEntry::Filled(r));
                        } else {
                            rooms_list.set(idx, RoomListEntry::Filled(r));
                        }
                    })
                    .count();
            }

            v4::SlidingOp::Delete => {
                let position: u32 = operation
                    .index
                    .ok_or_else(|| {
                        Error::BadResponse(
                            "`index` must be present for DELETE operation".to_owned(),
                        )
                    })?
                    .try_into()
                    .map_err(|error| {
                        Error::BadResponse(format!("`index` not a valid int for DELETE: {error}"))
                    })?;
                rooms_list.set(position as usize, RoomListEntry::Empty);
            }

            v4::SlidingOp::Insert => {
                let position: usize = operation
                    .index
                    .ok_or_else(|| {
                        Error::BadResponse(
                            "`index` must be present for INSERT operation".to_owned(),
                        )
                    })?
                    .try_into()
                    .map_err(|error| {
                        Error::BadResponse(format!("`index` not a valid int for INSERT: {error}"))
                    })?;

                let room = RoomListEntry::Filled(operation.room_id.clone().ok_or_else(|| {
                    Error::BadResponse("`room_id` must be present for INSERT operation".to_owned())
                })?);
                let mut offset = 0usize;

                loop {
                    // Find the next empty slot and drop it.
                    let (previous_position, overflow) = position.overflowing_sub(offset);
                    let check_previous = !overflow && index_in_range(previous_position);

                    let (next_position, overflow) = position.overflowing_add(offset);
                    let check_next = !overflow
                        && next_position < rooms_list.len()
                        && index_in_range(next_position);

                    if !check_previous && !check_next {
                        return Err(Error::BadResponse(
                            "We were asked to insert but could not find any direction to shift to"
                                .to_owned(),
                        ));
                    }

                    if check_previous && rooms_list[previous_position].is_empty_or_invalidated() {
                        // we only check for previous, if there are items left
                        rooms_list.remove(previous_position);

                        break;
                    } else if check_next && rooms_list[next_position].is_empty_or_invalidated() {
                        rooms_list.remove(next_position);

                        break;
                    } else {
                        // Let's check the next position.
                        offset += 1;
                    }
                }

                rooms_list.insert(position, room);
            }

            v4::SlidingOp::Invalidate => {
                let max_len = rooms_list.len();
                let (mut position, end): (usize, usize) = if let Some(range) = operation.range {
                    (
                        range.0.try_into().map_err(|error| {
                            Error::BadResponse(format!("`range.0` not a valid int: {error}"))
                        })?,
                        range.1.try_into().map_err(|error| {
                            Error::BadResponse(format!("`range.1` not a valid int: {error}"))
                        })?,
                    )
                } else {
                    return Err(Error::BadResponse(
                        "`range` must be given on `Invalidate` operation".to_owned(),
                    ));
                };

                if position > end {
                    return Err(Error::BadResponse(
                        "Invalid invalidation, end smaller than start".to_owned(),
                    ));
                }

                // Ranges are inclusive up to the last index. e.g. `[0, 10]`; `[0, 0]`.
                // ensure we pick them all up.
                while position <= end {
                    if position >= max_len {
                        break; // how does this happen?
                    }

                    let room_id =
                        if let Some(RoomListEntry::Filled(room_id)) = rooms_list.get(position) {
                            Some(room_id.clone())
                        } else {
                            None
                        };

                    if let Some(room_id) = room_id {
                        rooms_list.set(position, RoomListEntry::Invalidated(room_id));
                    } else {
                        rooms_list.set(position, RoomListEntry::Empty);
                    }

                    position += 1;
                }
            }

            unknown_operation => {
                warn!("Unknown operation occurred: {unknown_operation:?}");
            }
        }
    }

    Ok(())
}

/// The state the [`SlidingSyncList`] is in.
///
/// The lifetime of a `SlidingSyncList` usually starts at `NotLoaded` or
/// `Preloaded` (if it is restored from a cache). When loading rooms in a list,
/// depending of the [`SlidingSyncMode`], it moves to `PartiallyLoaded` or
/// `FullyLoaded`. The lifetime of a `SlidingSync` usually starts at a
///
/// If the client has been offline for a while, though, the `SlidingSyncList`
/// might return back to `PartiallyLoaded` at any point.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SlidingSyncState {
    /// Sliding Sync has not started to load anything yet.
    #[default]
    #[serde(rename = "Cold")]
    NotLoaded,
    /// Sliding Sync has been preloaded, i.e. restored from a cache for example.
    #[serde(rename = "Preload")]
    Preloaded,
    /// Updates are received from the loaded rooms, and new rooms are being
    /// fetched in the background.
    #[serde(rename = "CatchingUp")]
    PartiallyLoaded,
    /// Updates are received for all the loaded rooms, and all rooms have been
    /// loaded!
    #[serde(rename = "Live")]
    FullyLoaded,
}

/// How a [`SlidingSyncList`] fetches the data.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SlidingSyncMode {
    /// Fully sync all rooms in the background, page by page of `batch_size`,
    /// like `0..=19`, `20..=39`, 40..=59` etc. assuming the `batch_size` is 20.
    #[serde(alias = "FullSync")]
    PagingFullSync,
    /// Fully sync all rooms in the background, with a growing window of
    /// `batch_size`, like `0..=19`, `0..=39`, `0..=59` etc. assuming the
    /// `batch_size` is 20.
    GrowingFullSync,
    /// Only sync the specific defined windows/ranges.
    #[default]
    Selective,
}

/// The Entry in the Sliding Sync room list per Sliding Sync list.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[cfg_attr(test, derive(PartialEq))]
pub enum RoomListEntry {
    /// This entry isn't known at this point and thus considered `Empty`.
    #[default]
    Empty,
    /// There was `OwnedRoomId` but since the server told us to invalid this
    /// entry. it is considered stale.
    Invalidated(OwnedRoomId),
    /// This entry is followed with `OwnedRoomId`.
    Filled(OwnedRoomId),
}

impl RoomListEntry {
    /// Is this entry empty or invalidated?
    pub fn is_empty_or_invalidated(&self) -> bool {
        matches!(self, Self::Empty | Self::Invalidated(_))
    }

    /// Return the inner `room_id` if the entry' state is not empty.
    pub fn as_room_id(&self) -> Option<&RoomId> {
        match &self {
            Self::Empty => None,
            Self::Invalidated(room_id) | Self::Filled(room_id) => Some(room_id.as_ref()),
        }
    }

    /// Clone this entry, but freeze it, i.e. if the entry is empty, it remains
    /// empty, otherwise it is invalidated.
    fn freeze_by_ref(&self) -> Self {
        match &self {
            Self::Empty => Self::Empty,
            Self::Invalidated(room_id) | Self::Filled(room_id) => {
                Self::Invalidated(room_id.clone())
            }
        }
    }
}

impl<'a> From<&'a RoomListEntry> for RoomListEntry {
    fn from(value: &'a RoomListEntry) -> Self {
        value.clone()
    }
}

#[cfg(test)]
mod tests {
    use std::ops::{Deref, Not};

    use imbl::vector;
    use matrix_sdk_base::deserialized_responses::TimelineEvent;
    use ruma::{assign, events::room::message::RoomMessageEventContent, room_id, serde::Raw, uint};
    use serde_json::json;

    use super::*;

    macro_rules! assert_json_roundtrip {
        (from $type:ty: $rust_value:expr => $json_value:expr) => {
            let json = serde_json::to_value(&$rust_value).unwrap();
            assert_eq!(json, $json_value);

            let rust: $type = serde_json::from_value(json).unwrap();
            assert_eq!(rust, $rust_value);
        };
    }

    macro_rules! assert_json_eq {
        (from $type:ty: $rust_value:expr => $json_value:expr) => {
            let json = serde_json::to_value(&$rust_value).unwrap();
            assert_eq!(json, $json_value);
        };
    }

    macro_rules! assert_fields_eq {
        ($left:ident == $right:ident on fields { $( $field:ident $( with $accessor:expr )? ),+ $(,)* } ) => {
            $(
                let left = {
                    let $field = & $left . $field;

                    $( let $field = $accessor ; )?

                    $field
                };
                let right = {
                    let $field = & $right . $field;

                    $( let $field = $accessor ; )?

                    $field
                };

                assert_eq!(
                    left,
                    right,
                    concat!(
                        "`",
                        stringify!($left),
                        ".",
                        stringify!($field),
                        "` doesn't match with `",
                        stringify!($right),
                        ".",
                        stringify!($field),
                        "`."
                    )
                );
            )*
        }
    }

    macro_rules! ranges {
        ( $( ( $start:literal, $end:literal ) ),* $(,)* ) => {
            vec![$(
                (
                    uint!($start),
                    uint!($end),
                )
            ),+]
        }
    }

    #[test]
    fn test_sliding_sync_list_new_builder() {
        let list = SlidingSyncList {
            inner: Arc::new(SlidingSyncListInner {
                sync_mode: SlidingSyncMode::GrowingFullSync,
                sort: vec!["foo".to_string(), "bar".to_string()],
                required_state: vec![(StateEventType::RoomName, "baz".to_owned())],
                full_sync_batch_size: 42,
                full_sync_maximum_number_of_rooms_to_fetch: Some(153),
                send_updates_for_items: true,
                filters: Some(assign!(v4::SyncRequestListFilters::default(), {
                    is_dm: Some(true),
                })),
                timeline_limit: StdRwLock::new(Observable::new(Some(uint!(7)))),
                name: "qux".to_string(),
                state: StdRwLock::new(Observable::new(SlidingSyncState::FullyLoaded)),
                maximum_number_of_rooms: StdRwLock::new(Observable::new(Some(11))),
                rooms_list: StdRwLock::new(ObservableVector::from(vector![RoomListEntry::Empty])),
                ranges: StdRwLock::new(Observable::new(vec![(uint!(0), uint!(9))])),
                rooms_updated_broadcast: StdRwLock::new(Observable::new(())),
                is_cold: AtomicBool::new(true),
                request_generator: StdRwLock::new(
                    SlidingSyncListRequestGenerator::new_growing_full_sync(42, Some(153)),
                ),
            }),
        };

        let new_list = list.new_builder().build().unwrap();
        let list = list.inner;
        let new_list = new_list.inner;

        assert_fields_eq!(
            list == new_list on fields {
                sync_mode,
                sort,
                required_state,
                full_sync_batch_size,
                full_sync_maximum_number_of_rooms_to_fetch,
                send_updates_for_items,
                filters with filters.as_ref().unwrap().is_dm,
                timeline_limit with **timeline_limit.read().unwrap(),
                name,
                ranges with ranges.read().unwrap().clone(),
            }
        );

        assert_eq!(*Observable::get(&new_list.state.read().unwrap()), SlidingSyncState::NotLoaded);
        assert!(new_list.maximum_number_of_rooms.read().unwrap().deref().is_none());
        assert!(new_list.rooms_list.read().unwrap().deref().is_empty());
        assert!(new_list.is_cold.load(Ordering::SeqCst).not());
    }

    #[test]
    fn test_sliding_sync_list_set_ranges() {
        let list = SlidingSyncList::builder()
            .name("foo")
            .sync_mode(SlidingSyncMode::Selective)
            .ranges(ranges![(0, 1), (2, 3)])
            .build()
            .unwrap();

        {
            let lock = list.inner.ranges.read().unwrap();
            let ranges = Observable::get(&lock);

            assert_eq!(ranges, &ranges![(0, 1), (2, 3)]);
        }

        list.set_ranges(ranges![(4, 5), (6, 7)]);

        {
            let lock = list.inner.ranges.read().unwrap();
            let ranges = Observable::get(&lock);

            assert_eq!(ranges, &ranges![(4, 5), (6, 7)]);
        }
    }

    #[test]
    fn test_sliding_sync_list_set_range() {
        let list = SlidingSyncList::builder()
            .name("foo")
            .sync_mode(SlidingSyncMode::Selective)
            .ranges(ranges![(0, 1), (2, 3)])
            .build()
            .unwrap();

        {
            let lock = list.inner.ranges.read().unwrap();
            let ranges = Observable::get(&lock);

            assert_eq!(ranges, &ranges![(0, 1), (2, 3)]);
        }

        list.set_range(4u32, 5);

        {
            let lock = list.inner.ranges.read().unwrap();
            let ranges = Observable::get(&lock);

            assert_eq!(ranges, &ranges![(4, 5)]);
        }
    }

    #[test]
    fn test_sliding_sync_list_add_range() {
        let list = SlidingSyncList::builder()
            .name("foo")
            .sync_mode(SlidingSyncMode::Selective)
            .ranges(ranges![(0, 1)])
            .build()
            .unwrap();

        {
            let lock = list.inner.ranges.read().unwrap();
            let ranges = Observable::get(&lock);

            assert_eq!(ranges, &ranges![(0, 1)]);
        }

        list.add_range(2u32, 3);

        {
            let lock = list.inner.ranges.read().unwrap();
            let ranges = Observable::get(&lock);

            assert_eq!(ranges, &ranges![(0, 1), (2, 3)]);
        }
    }

    #[test]
    fn test_sliding_sync_list_reset_ranges() {
        let list = SlidingSyncList::builder()
            .name("foo")
            .sync_mode(SlidingSyncMode::Selective)
            .ranges(ranges![(0, 1)])
            .build()
            .unwrap();

        {
            let lock = list.inner.ranges.read().unwrap();
            let ranges = Observable::get(&lock);

            assert_eq!(ranges, &ranges![(0, 1)]);
        }

        list.reset_ranges();

        {
            let lock = list.inner.ranges.read().unwrap();
            let ranges = Observable::get(&lock);

            assert!(ranges.is_empty());
        }
    }

    #[test]
    fn test_sliding_sync_list_timeline_limit() {
        let list = SlidingSyncList::builder()
            .name("foo")
            .sync_mode(SlidingSyncMode::Selective)
            .ranges(ranges![(0, 1)])
            .timeline_limit(7u32)
            .build()
            .unwrap();

        {
            let lock = list.inner.timeline_limit.read().unwrap();
            let timeline_limit = Observable::get(&lock);

            assert_eq!(timeline_limit, &Some(uint!(7)));
        }

        list.set_timeline_limit(Some(42u32));

        {
            let lock = list.inner.timeline_limit.read().unwrap();
            let timeline_limit = Observable::get(&lock);

            assert_eq!(timeline_limit, &Some(uint!(42)));
        }

        list.set_timeline_limit::<UInt>(None);

        {
            let lock = list.inner.timeline_limit.read().unwrap();
            let timeline_limit = Observable::get(&lock);

            assert_eq!(timeline_limit, &None);
        }
    }

    /*
    #[test]
    fn check_find_room_in_list() -> Result<()> {
        let list = SlidingSyncList::builder().name("foo").add_range(0u32, 9u32).build().unwrap();
        let full_window_update: v4::SyncOp = serde_json::from_value(json!({
            "op": "SYNC",
            "range": [0, 9],
            "room_ids": [
                "!A00000:matrix.example",
                "!A00001:matrix.example",
                "!A00002:matrix.example",
                "!A00003:matrix.example",
                "!A00004:matrix.example",
                "!A00005:matrix.example",
                "!A00006:matrix.example",
                "!A00007:matrix.example",
                "!A00008:matrix.example",
                "!A00009:matrix.example"
            ],
        }))
        .unwrap();

        list.handle_response(
            10u32,
            &vec![full_window_update],
            &vec![(uint!(0), uint!(9))],
            &vec![],
        )
        .unwrap();

        let a02 = room_id!("!A00002:matrix.example").to_owned();
        let a05 = room_id!("!A00005:matrix.example").to_owned();
        let a09 = room_id!("!A00009:matrix.example").to_owned();

        assert_eq!(list.find_room_in_list(&a02), Some(2));
        assert_eq!(list.find_room_in_list(&a05), Some(5));
        assert_eq!(list.find_room_in_list(&a09), Some(9));

        assert_eq!(
            list.find_rooms_in_list(&[a02.clone(), a05.clone(), a09.clone()]),
            vec![(2, a02.clone()), (5, a05.clone()), (9, a09.clone())]
        );

        // we invalidate a few in the center
        let update: v4::SyncOp = serde_json::from_value(json!({
            "op": "INVALIDATE",
            "range": [4, 7],
        }))
        .unwrap();

        list.handle_response(
            10u32,
            &vec![update],
            // &vec![(uint!(0), uint!(3)), (uint!(8), uint!(9))],
            &vec![],
        )
        .unwrap();

        assert_eq!(list.find_room_in_list(room_id!("!A00002:matrix.example")), Some(2));
        assert_eq!(list.find_room_in_list(room_id!("!A00005:matrix.example")), None);
        assert_eq!(list.find_room_in_list(room_id!("!A00009:matrix.example")), Some(9));

        assert_eq!(
            list.find_rooms_in_list(&[a02.clone(), a05, a09.clone()]),
            vec![(2, a02), (9, a09)]
        );

        Ok(())
    }
    */

    macro_rules! assert_ranges {
        (
            list = $list:ident,
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
                    let request = $list.next_request().unwrap();

                    assert_eq!(request.ranges, [ $( (uint!( $range_start ), uint!( $range_end )) ),* ]);

                    // Fake a response.
                    let _ = $list.handle_response($maximum_number_of_rooms, &vec![], &vec![]);

                    assert_eq!($list.inner.request_generator.read().unwrap().is_fully_loaded(), $is_fully_loaded);
                    assert_eq!($list.state(), SlidingSyncState::$list_state);
                }
            )*
        };
    }

    #[test]
    fn test_generator_paging_full_sync() {
        let mut list = SlidingSyncList::builder()
            .sync_mode(crate::SlidingSyncMode::PagingFullSync)
            .name("testing")
            .full_sync_batch_size(10)
            .build()
            .unwrap();

        assert_ranges! {
            list = list,
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
        let mut list = SlidingSyncList::builder()
            .sync_mode(crate::SlidingSyncMode::PagingFullSync)
            .name("testing")
            .full_sync_batch_size(10)
            .full_sync_maximum_number_of_rooms_to_fetch(22)
            .build()
            .unwrap();

        assert_ranges! {
            list = list,
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
        let mut list = SlidingSyncList::builder()
            .sync_mode(crate::SlidingSyncMode::GrowingFullSync)
            .name("testing")
            .full_sync_batch_size(10)
            .build()
            .unwrap();

        assert_ranges! {
            list = list,
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
        let mut list = SlidingSyncList::builder()
            .sync_mode(crate::SlidingSyncMode::GrowingFullSync)
            .name("testing")
            .full_sync_batch_size(10)
            .full_sync_maximum_number_of_rooms_to_fetch(22)
            .build()
            .unwrap();

        assert_ranges! {
            list = list,
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
        let mut list = SlidingSyncList::builder()
            .sync_mode(crate::SlidingSyncMode::Selective)
            .name("testing")
            .ranges(vec![(0u32, 10), (42, 153)])
            .build()
            .unwrap();

        assert_ranges! {
            list = list,
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

    #[test]
    fn test_room_list_entry_is_empty_or_invalidated() {
        let room_id = room_id!("!foo:bar.org");

        assert!(RoomListEntry::Empty.is_empty_or_invalidated());
        assert!(RoomListEntry::Invalidated(room_id.to_owned()).is_empty_or_invalidated());
        assert!(RoomListEntry::Filled(room_id.to_owned()).is_empty_or_invalidated().not());
    }

    #[test]
    fn test_room_list_entry_as_room_id() {
        let room_id = room_id!("!foo:bar.org");

        assert_eq!(RoomListEntry::Empty.as_room_id(), None);
        assert_eq!(RoomListEntry::Invalidated(room_id.to_owned()).as_room_id(), Some(room_id));
        assert_eq!(RoomListEntry::Filled(room_id.to_owned()).as_room_id(), Some(room_id));
    }

    #[test]
    fn test_room_list_entry_freeze() {
        let room_id = room_id!("!foo:bar.org");

        assert_eq!(RoomListEntry::Empty.freeze_by_ref(), RoomListEntry::Empty);
        assert_eq!(
            RoomListEntry::Invalidated(room_id.to_owned()).freeze_by_ref(),
            RoomListEntry::Invalidated(room_id.to_owned())
        );
        assert_eq!(
            RoomListEntry::Filled(room_id.to_owned()).freeze_by_ref(),
            RoomListEntry::Invalidated(room_id.to_owned())
        );
    }

    #[test]
    fn test_room_list_entry_serialization() {
        let room_id = room_id!("!foo:bar.org");

        assert_json_roundtrip!(from RoomListEntry: RoomListEntry::Empty => json!("Empty"));
        assert_json_roundtrip!(from RoomListEntry: RoomListEntry::Invalidated(room_id.to_owned()) => json!({"Invalidated": "!foo:bar.org"}));
        assert_json_roundtrip!(from RoomListEntry: RoomListEntry::Filled(room_id.to_owned()) => json!({"Filled": "!foo:bar.org"}));
    }

    #[test]
    fn test_sliding_sync_mode_serialization() {
        assert_json_roundtrip!(from SlidingSyncMode: SlidingSyncMode::PagingFullSync => json!("PagingFullSync"));
        assert_json_roundtrip!(from SlidingSyncMode: SlidingSyncMode::GrowingFullSync => json!("GrowingFullSync"));
        assert_json_roundtrip!(from SlidingSyncMode: SlidingSyncMode::Selective => json!("Selective"));

        // Specificity: `PagingFullSync` has a serde alias.
        let alias: SlidingSyncMode = serde_json::from_value(json!("FullSync")).unwrap();
        assert_eq!(alias, SlidingSyncMode::PagingFullSync);
    }

    #[test]
    fn test_sliding_sync_state_serialization() {
        assert_json_roundtrip!(from SlidingSyncState: SlidingSyncState::NotLoaded => json!("Cold"));
        assert_json_roundtrip!(from SlidingSyncState: SlidingSyncState::Preloaded => json!("Preload"));
        assert_json_roundtrip!(from SlidingSyncState: SlidingSyncState::PartiallyLoaded => json!("CatchingUp"));
        assert_json_roundtrip!(from SlidingSyncState: SlidingSyncState::FullyLoaded => json!("Live"));
    }

    #[test]
    fn test_frozen_sliding_sync_list_serialization() {
        // Doing a roundtrip isn't possible because `v4::SlidingSyncRoom` doesn't
        // implement `PartialEq`.
        assert_json_eq!(
            from FrozenSlidingSyncList:

            FrozenSlidingSyncList {
                maximum_number_of_rooms: Some(42),
                rooms_list: vector![RoomListEntry::Empty],
                rooms: {
                    let mut rooms = BTreeMap::new();
                    rooms.insert(
                        room_id!("!foo:bar.org").to_owned(),
                        FrozenSlidingSyncRoom {
                            room_id: room_id!("!foo:bar.org").to_owned(),
                            inner: v4::SlidingSyncRoom::default(),
                            prev_batch: Some("let it go!".to_owned()),
                            timeline_queue: vector![TimelineEvent::new(
                                Raw::new(&json!({
                                    "content": RoomMessageEventContent::text_plain("let it gooo!"),
                                    "type": "m.room.message",
                                    "event_id": "$xxxxx:example.org",
                                    "room_id": "!someroom:example.com",
                                    "origin_server_ts": 2189,
                                    "sender": "@bob:example.com",
                                }))
                                .unwrap()
                                .cast(),
                            )
                            .into()],
                        },
                    );

                    rooms
                },
            }
            =>
            json!({
                "rooms_count": 42,
                "rooms_list": ["Empty"],
                "rooms": {
                    "!foo:bar.org": {
                        "room_id": "!foo:bar.org",
                        "inner": {},
                        "prev_batch": "let it go!",
                        "timeline": [
                            {
                                "event": {
                                    "content": {
                                        "body": "let it gooo!",
                                        "msgtype": "m.text",
                                    },
                                    "event_id": "$xxxxx:example.org",
                                    "origin_server_ts": 2189,
                                    "room_id": "!someroom:example.com",
                                    "sender": "@bob:example.com",
                                    "type": "m.room.message",
                                },
                                "encryption_info": null,
                            }
                        ],
                    },
                },
            })
        );
    }
}
