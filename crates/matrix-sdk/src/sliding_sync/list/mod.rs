mod builder;
mod request_generator;

use std::{
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
use ruma::{api::client::sync::sync_events::v4, events::StateEventType, OwnedRoomId, RoomId, UInt};
use serde::{Deserialize, Serialize};
use tracing::{debug, instrument, warn};

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
#[derive(Clone, Debug)]
pub struct SlidingSyncList {
    /// Which SlidingSyncMode to start this list under
    sync_mode: SlidingSyncMode,

    /// Sort the rooms list by this
    sort: Vec<String>,

    /// Required states to return per room
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

    /// The maximum number of timeline events to query for
    pub timeline_limit: Arc<StdRwLock<Observable<Option<UInt>>>>,

    /// Name of this list to easily recognize them
    pub name: String,

    /// The state this list is in.
    state: Arc<StdRwLock<Observable<SlidingSyncState>>>,

    /// The total number of rooms that is possible to interact with for the
    /// given list.
    ///
    /// It's not the total rooms that have been fetched. The server tells the
    /// client that it's possible to fetch this amount of rooms maximum.
    /// Since this number can change according to the list filters, it's
    /// observable.
    maximum_number_of_rooms: Arc<StdRwLock<Observable<Option<u32>>>>,

    /// The rooms in order.
    rooms_list: Arc<StdRwLock<ObservableVector<RoomListEntry>>>,

    /// The ranges windows of the list.
    #[allow(clippy::type_complexity)] // temporarily
    ranges: Arc<StdRwLock<Observable<Vec<(UInt, UInt)>>>>,

    /// Get informed if anything in the room changed.
    ///
    /// If you only care to know about changes once all of them have applied
    /// (including the total), subscribe to this observable.
    pub rooms_updated_broadcast: Arc<StdRwLock<Observable<()>>>,

    is_cold: Arc<AtomicBool>,
}

impl SlidingSyncList {
    pub(crate) fn set_from_cold(
        &mut self,
        maximum_number_of_rooms: Option<u32>,
        rooms_list: Vector<RoomListEntry>,
    ) {
        Observable::set(&mut self.state.write().unwrap(), SlidingSyncState::Preloaded);
        self.is_cold.store(true, Ordering::SeqCst);
        Observable::set(
            &mut self.maximum_number_of_rooms.write().unwrap(),
            maximum_number_of_rooms,
        );

        let mut lock = self.rooms_list.write().unwrap();
        lock.clear();
        lock.append(rooms_list);
    }

    /// Create a new [`SlidingSyncListBuilder`].
    pub fn builder() -> SlidingSyncListBuilder {
        SlidingSyncListBuilder::new()
    }

    /// Return a builder with the same settings as before
    pub fn new_builder(&self) -> SlidingSyncListBuilder {
        Self::builder()
            .name(&self.name)
            .sync_mode(self.sync_mode.clone())
            .sort(self.sort.clone())
            .required_state(self.required_state.clone())
            .full_sync_batch_size(self.full_sync_batch_size)
            .ranges(self.ranges.read().unwrap().clone())
    }

    /// Set the ranges to fetch.
    ///
    /// Remember to cancel the existing stream and fetch a new one as this will
    /// only be applied on the next request.
    pub fn set_ranges(&self, range: Vec<(u32, u32)>) -> &Self {
        let value = range.into_iter().map(|(a, b)| (a.into(), b.into())).collect();
        Observable::set(&mut self.ranges.write().unwrap(), value);

        self
    }

    /// Reset the ranges to a particular set
    ///
    /// Remember to cancel the existing stream and fetch a new one as this will
    /// only be applied on the next request.
    pub fn set_range(&self, start: u32, end: u32) -> &Self {
        let value = vec![(start.into(), end.into())];
        Observable::set(&mut self.ranges.write().unwrap(), value);

        self
    }

    /// Set the ranges to fetch
    ///
    /// Remember to cancel the existing stream and fetch a new one as this will
    /// only be applied on the next request.
    pub fn add_range(&self, start: u32, end: u32) -> &Self {
        Observable::update(&mut self.ranges.write().unwrap(), |ranges| {
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
        Observable::set(&mut self.ranges.write().unwrap(), Vec::new());

        self
    }

    /// Get the current state.
    pub fn state(&self) -> SlidingSyncState {
        self.state.read().unwrap().clone()
    }

    /// Get a stream of state.
    pub fn state_stream(&self) -> impl Stream<Item = SlidingSyncState> {
        Observable::subscribe(&self.state.read().unwrap())
    }

    /// Get the current rooms list.
    pub fn rooms_list<R>(&self) -> Vec<R>
    where
        R: for<'a> From<&'a RoomListEntry>,
    {
        self.rooms_list.read().unwrap().iter().map(|e| R::from(e)).collect()
    }

    /// Get a stream of rooms list.
    pub fn rooms_list_stream(&self) -> impl Stream<Item = VectorDiff<RoomListEntry>> {
        ObservableVector::subscribe(&self.rooms_list.read().unwrap())
    }

    /// Get the maximum number of rooms. See [`Self::maximum_number_of_rooms`]
    /// to learn more.
    pub fn maximum_number_of_rooms(&self) -> Option<u32> {
        **self.maximum_number_of_rooms.read().unwrap()
    }

    /// Get a stream of rooms count.
    pub fn maximum_number_of_rooms_stream(&self) -> impl Stream<Item = Option<u32>> {
        Observable::subscribe(&self.maximum_number_of_rooms.read().unwrap())
    }

    /// Find the current valid position of the room in the list `room_list`.
    ///
    /// Only matches against the current ranges and only against filled items.
    /// Invalid items are ignore. Return the total position the item was
    /// found in the room_list, return None otherwise.
    pub fn find_room_in_list(&self, room_id: &RoomId) -> Option<usize> {
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

    /// Find the current valid position of the rooms in the lists `room_list`.
    ///
    /// Only matches against the current ranges and only against filled items.
    /// Invalid items are ignore. Return the total position the items that were
    /// found in the `room_list`, will skip any room not found in the
    /// `rooms_list`.
    pub fn find_rooms_in_list(&self, room_ids: &[OwnedRoomId]) -> Vec<(usize, OwnedRoomId)> {
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

    /// Return the `room_id` at the given index.
    pub fn get_room_id(&self, index: usize) -> Option<OwnedRoomId> {
        self.rooms_list
            .read()
            .unwrap()
            .get(index)
            .and_then(|room_list_entry| room_list_entry.as_room_id().map(ToOwned::to_owned))
    }

    #[instrument(skip(self, ops), fields(name = self.name, ops_count = ops.len()))]
    pub(super) fn handle_response(
        &self,
        maximum_number_of_rooms: u32,
        ops: &Vec<v4::SyncOp>,
        ranges: &Vec<(UInt, UInt)>,
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

    pub(super) fn request_generator(&self) -> SlidingSyncListRequestGenerator {
        match &self.sync_mode {
            SlidingSyncMode::PagingFullSync => {
                SlidingSyncListRequestGenerator::new_with_paging_full_sync(self.clone())
            }

            SlidingSyncMode::GrowingFullSync => {
                SlidingSyncListRequestGenerator::new_with_growing_full_sync(self.clone())
            }

            SlidingSyncMode::Selective => {
                SlidingSyncListRequestGenerator::new_selective(self.clone())
            }
        }
    }
}

#[derive(Serialize, Deserialize)]
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

        for room_list_entry in source_list.rooms_list.read().unwrap().iter() {
            match room_list_entry {
                RoomListEntry::Filled(room_id) | RoomListEntry::Invalidated(room_id) => {
                    rooms.insert(
                        room_id.clone(),
                        rooms_map.get(room_id).expect("room doesn't exist").into(),
                    );
                }

                _ => {}
            };

            rooms_list.push_back(room_list_entry.freeze());
        }

        FrozenSlidingSyncList {
            maximum_number_of_rooms: **source_list.maximum_number_of_rooms.read().unwrap(),
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
    fn freeze(&self) -> Self {
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
