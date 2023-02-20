use std::{
    collections::BTreeMap,
    fmt::Debug,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

use derive_builder::Builder;
use futures_signals::{
    signal::Mutable,
    signal_map::MutableBTreeMapLockRef,
    signal_vec::{MutableVec, MutableVecLockMut},
};
use ruma::{
    api::client::sync::sync_events::v4, assign, events::StateEventType, OwnedRoomId, RoomId, UInt,
};
use serde::{Deserialize, Serialize};
use tracing::{debug, error, instrument, trace, warn};

use super::{
    Error, FrozenSlidingSyncRoom, RangeState, RoomListEntry, RoomsCount, RoomsList,
    SlidingSyncMode, SlidingSyncRoom, SlidingSyncState, SyncMode, ViewState,
};
use crate::Result;

/// Holding a specific filtered view within the concept of sliding sync.
/// Main entrypoint to the SlidingSync
///
///
/// ```no_run
/// # use futures::executor::block_on;
/// # use matrix_sdk::Client;
/// # use url::Url;
/// # block_on(async {
/// # let homeserver = Url::parse("http://example.com")?;
/// let client = Client::new(homeserver).await?;
/// let sliding_sync =
///     client.sliding_sync().await.add_fullsync_view().build().await?;
///
/// # anyhow::Ok(())
/// # });
/// ```
#[derive(Clone, Debug, Builder)]
#[builder(build_fn(name = "finish_build"), pattern = "owned", derive(Clone, Debug))]
pub struct SlidingSyncView {
    /// Which SyncMode to start this view under
    #[builder(setter(custom), default)]
    sync_mode: SyncMode,

    /// Sort the rooms list by this
    #[builder(default = "SlidingSyncViewBuilder::default_sort()")]
    sort: Vec<String>,

    /// Required states to return per room
    #[builder(default = "SlidingSyncViewBuilder::default_required_state()")]
    required_state: Vec<(StateEventType, String)>,

    /// How many rooms request at a time when doing a full-sync catch up
    #[builder(default = "20")]
    batch_size: u32,

    /// Whether the view should send `UpdatedAt`-Diff signals for rooms
    /// that have changed
    #[builder(default = "false")]
    send_updates_for_items: bool,

    /// How many rooms request a total hen doing a full-sync catch up
    #[builder(setter(into), default)]
    limit: Option<u32>,

    /// Any filters to apply to the query
    #[builder(default)]
    filters: Option<v4::SyncRequestListFilters>,

    /// The maximum number of timeline events to query for
    #[builder(setter(name = "timeline_limit_raw"), default)]
    pub timeline_limit: Mutable<Option<UInt>>,

    // ----- Public state
    /// Name of this view to easily recognize them
    #[builder(setter(into))]
    pub name: String,

    /// The state this view is in
    #[builder(private, default)]
    pub state: ViewState,

    /// The total known number of rooms,
    #[builder(private, default)]
    pub rooms_count: RoomsCount,

    /// The rooms in order
    #[builder(private, default)]
    pub rooms_list: RoomsList,

    /// The ranges windows of the view
    #[builder(setter(name = "ranges_raw"), default)]
    ranges: RangeState,

    /// Signaling updates on the room list after processing
    #[builder(private)]
    rooms_updated_signal: futures_signals::signal::Sender<()>,

    #[builder(private)]
    is_cold: Arc<AtomicBool>,

    /// Get informed if anything in the room changed
    ///
    /// If you only care to know about changes once all of them have applied
    /// (including the total) listen to a clone of this signal.
    #[builder(private)]
    pub rooms_updated_broadcaster:
        futures_signals::signal::Broadcaster<futures_signals::signal::Receiver<()>>,
}

#[derive(Serialize, Deserialize)]
pub(super) struct FrozenSlidingSyncView {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) rooms_count: Option<u32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(super) rooms_list: Vec<RoomListEntry>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub(super) rooms: BTreeMap<OwnedRoomId, FrozenSlidingSyncRoom>,
}

impl FrozenSlidingSyncView {
    pub(super) fn freeze(
        source_view: &SlidingSyncView,
        rooms_map: &MutableBTreeMapLockRef<'_, OwnedRoomId, SlidingSyncRoom>,
    ) -> Self {
        let mut rooms = BTreeMap::new();
        let mut rooms_list = Vec::new();
        for entry in source_view.rooms_list.lock_ref().iter() {
            match entry {
                RoomListEntry::Filled(o) | RoomListEntry::Invalidated(o) => {
                    rooms.insert(o.clone(), rooms_map.get(o).expect("rooms always exists").into());
                }
                _ => {}
            };

            rooms_list.push(entry.freeze());
        }
        FrozenSlidingSyncView {
            rooms_count: *source_view.rooms_count.lock_ref(),
            rooms_list,
            rooms,
        }
    }
}

impl SlidingSyncView {
    pub(crate) fn set_from_cold(
        &mut self,
        rooms_count: Option<u32>,
        rooms_list: Vec<RoomListEntry>,
    ) {
        self.state.set(SlidingSyncState::Preload);
        self.is_cold.store(true, Ordering::SeqCst);
        self.rooms_count.replace(rooms_count);
        self.rooms_list.lock_mut().replace_cloned(rooms_list);
    }
}

/// the default name for the full sync view
pub const FULL_SYNC_VIEW_NAME: &str = "full-sync";

impl SlidingSyncViewBuilder {
    /// Create a Builder set up for full sync
    pub fn default_with_fullsync() -> Self {
        Self::default().name(FULL_SYNC_VIEW_NAME).sync_mode(SlidingSyncMode::PagingFullSync)
    }

    /// Build the view
    pub fn build(mut self) -> Result<SlidingSyncView, SlidingSyncViewBuilderError> {
        let (sender, receiver) = futures_signals::signal::channel(());
        self.is_cold = Some(Arc::new(AtomicBool::new(false)));
        self.rooms_updated_signal = Some(sender);
        self.rooms_updated_broadcaster = Some(futures_signals::signal::Broadcaster::new(receiver));
        self.finish_build()
    }

    fn default_sort() -> Vec<String> {
        vec!["by_recency".to_owned(), "by_name".to_owned()]
    }

    fn default_required_state() -> Vec<(StateEventType, String)> {
        vec![
            (StateEventType::RoomEncryption, "".to_owned()),
            (StateEventType::RoomTombstone, "".to_owned()),
        ]
    }

    /// Set the Syncing mode
    pub fn sync_mode(mut self, sync_mode: SlidingSyncMode) -> Self {
        self.sync_mode = Some(SyncMode::new(sync_mode));
        self
    }

    /// Set the ranges to fetch
    pub fn ranges<U: Into<UInt>>(mut self, range: Vec<(U, U)>) -> Self {
        self.ranges =
            Some(RangeState::new(range.into_iter().map(|(a, b)| (a.into(), b.into())).collect()));
        self
    }

    /// Set a single range fetch
    pub fn set_range<U: Into<UInt>>(mut self, from: U, to: U) -> Self {
        self.ranges = Some(RangeState::new(vec![(from.into(), to.into())]));
        self
    }

    /// Set the ranges to fetch
    pub fn add_range<U: Into<UInt>>(mut self, from: U, to: U) -> Self {
        let r = self.ranges.get_or_insert_with(|| RangeState::new(Vec::new()));
        r.lock_mut().push((from.into(), to.into()));
        self
    }

    /// Set the ranges to fetch
    pub fn reset_ranges(mut self) -> Self {
        self.ranges = None;
        self
    }

    /// Set the limit of regular events to fetch for the timeline.
    pub fn timeline_limit<U: Into<UInt>>(mut self, timeline_limit: U) -> Self {
        self.timeline_limit = Some(Mutable::new(Some(timeline_limit.into())));
        self
    }

    /// Reset the limit of regular events to fetch for the timeline. It is left
    /// to the server to decide how many to send back
    pub fn no_timeline_limit(mut self) -> Self {
        self.timeline_limit = None;
        self
    }
}

enum InnerSlidingSyncViewRequestGenerator {
    GrowingFullSync { position: u32, batch_size: u32, limit: Option<u32>, live: bool },
    PagingFullSync { position: u32, batch_size: u32, limit: Option<u32>, live: bool },
    Live,
}

pub(super) struct SlidingSyncViewRequestGenerator {
    view: SlidingSyncView,
    ranges: Vec<(usize, usize)>,
    inner: InnerSlidingSyncViewRequestGenerator,
}

impl SlidingSyncViewRequestGenerator {
    fn new_with_paging_syncup(view: SlidingSyncView) -> Self {
        let batch_size = view.batch_size;
        let limit = view.limit;
        let position = view
            .ranges
            .get_cloned()
            .first()
            .map(|(_start, end)| u32::try_from(*end).unwrap())
            .unwrap_or_default();

        SlidingSyncViewRequestGenerator {
            view,
            ranges: Default::default(),
            inner: InnerSlidingSyncViewRequestGenerator::PagingFullSync {
                position,
                batch_size,
                limit,
                live: false,
            },
        }
    }

    fn new_with_growing_syncup(view: SlidingSyncView) -> Self {
        let batch_size = view.batch_size;
        let limit = view.limit;
        let position = view
            .ranges
            .get_cloned()
            .first()
            .map(|(_start, end)| u32::try_from(*end).unwrap())
            .unwrap_or_default();

        SlidingSyncViewRequestGenerator {
            view,
            ranges: Default::default(),
            inner: InnerSlidingSyncViewRequestGenerator::GrowingFullSync {
                position,
                batch_size,
                limit,
                live: false,
            },
        }
    }

    fn new_live(view: SlidingSyncView) -> Self {
        SlidingSyncViewRequestGenerator {
            view,
            ranges: Default::default(),
            inner: InnerSlidingSyncViewRequestGenerator::Live,
        }
    }

    fn prefetch_request(
        &mut self,
        start: u32,
        batch_size: u32,
        limit: Option<u32>,
    ) -> v4::SyncRequestList {
        let calc_end = start + batch_size;
        let end = match limit {
            Some(l) => std::cmp::min(l, calc_end),
            _ => calc_end,
        };
        self.make_request_for_ranges(vec![(start.into(), end.into())])
    }

    #[instrument(skip(self), fields(name = self.view.name))]
    fn make_request_for_ranges(&mut self, ranges: Vec<(UInt, UInt)>) -> v4::SyncRequestList {
        let sort = self.view.sort.clone();
        let required_state = self.view.required_state.clone();
        let timeline_limit = self.view.timeline_limit.get_cloned();
        let filters = self.view.filters.clone();

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
        let ranges = self.view.ranges.read_only().get_cloned();
        self.make_request_for_ranges(ranges)
    }

    #[instrument(skip_all, fields(name = self.view.name, rooms_count, has_ops = !ops.is_empty()))]
    pub(super) fn handle_response(
        &mut self,
        rooms_count: u32,
        ops: &Vec<v4::SyncOp>,
        rooms: &Vec<OwnedRoomId>,
    ) -> Result<bool, Error> {
        let res = self.view.handle_response(rooms_count, ops, &self.ranges, rooms)?;
        self.update_state(rooms_count.saturating_sub(1)); // index is 0 based, count is 1 based
        Ok(res)
    }

    fn update_state(&mut self, max_index: u32) {
        let Some((_start, range_end)) = self.ranges.first() else {
            error!("Why don't we have any ranges?");
            return
        };

        let end = if &(max_index as usize) < range_end { max_index } else { *range_end as u32 };

        trace!(end, max_index, range_end, name = self.view.name, "updating state");

        match &mut self.inner {
            InnerSlidingSyncViewRequestGenerator::PagingFullSync {
                position, live, limit, ..
            }
            | InnerSlidingSyncViewRequestGenerator::GrowingFullSync {
                position, live, limit, ..
            } => {
                let max = limit.map(|limit| std::cmp::min(limit, max_index)).unwrap_or(max_index);
                trace!(end, max, name = self.view.name, "updating state");
                if end >= max {
                    trace!(name = self.view.name, "going live");
                    // we are switching to live mode
                    self.view.set_range(0, max);
                    *position = max;
                    *live = true;

                    self.view.state.set_if(SlidingSyncState::Live, |before, _now| {
                        !matches!(before, SlidingSyncState::Live)
                    });
                } else {
                    *position = end;
                    *live = false;
                    self.view.set_range(0, end);
                    self.view.state.set_if(SlidingSyncState::CatchingUp, |before, _now| {
                        !matches!(before, SlidingSyncState::CatchingUp)
                    });
                }
            }
            InnerSlidingSyncViewRequestGenerator::Live => {
                self.view.state.set_if(SlidingSyncState::Live, |before, _now| {
                    !matches!(before, SlidingSyncState::Live)
                });
            }
        }
    }
}

impl Iterator for SlidingSyncViewRequestGenerator {
    type Item = v4::SyncRequestList;

    fn next(&mut self) -> Option<Self::Item> {
        match self.inner {
            InnerSlidingSyncViewRequestGenerator::PagingFullSync { live, .. }
            | InnerSlidingSyncViewRequestGenerator::GrowingFullSync { live, .. }
                if live =>
            {
                Some(self.live_request())
            }
            InnerSlidingSyncViewRequestGenerator::PagingFullSync {
                position,
                batch_size,
                limit,
                ..
            } => Some(self.prefetch_request(position, batch_size, limit)),
            InnerSlidingSyncViewRequestGenerator::GrowingFullSync {
                position,
                batch_size,
                limit,
                ..
            } => Some(self.prefetch_request(0, position + batch_size, limit)),
            InnerSlidingSyncViewRequestGenerator::Live => Some(self.live_request()),
        }
    }
}

#[instrument(skip(ops))]
fn room_ops(
    rooms_list: &mut MutableVecLockMut<'_, RoomListEntry>,
    ops: &Vec<v4::SyncOp>,
    room_ranges: &Vec<(usize, usize)>,
) -> Result<(), Error> {
    let index_in_range = |idx| room_ranges.iter().any(|(start, end)| idx >= *start && idx <= *end);
    for op in ops {
        match &op.op {
            v4::SlidingOp::Sync => {
                let start: u32 = op
                    .range
                    .ok_or_else(|| {
                        Error::BadResponse(
                            "`range` must be present for Sync and Update operation".to_owned(),
                        )
                    })?
                    .0
                    .try_into()
                    .map_err(|e| Error::BadResponse(format!("`range` not a valid int: {e:}")))?;
                let room_ids = op.room_ids.clone();
                room_ids
                    .into_iter()
                    .enumerate()
                    .map(|(i, r)| {
                        let idx = start as usize + i;
                        if idx >= rooms_list.len() {
                            rooms_list.insert_cloned(idx, RoomListEntry::Filled(r));
                        } else {
                            rooms_list.set_cloned(idx, RoomListEntry::Filled(r));
                        }
                    })
                    .count();
            }
            v4::SlidingOp::Delete => {
                let pos: u32 = op
                    .index
                    .ok_or_else(|| {
                        Error::BadResponse(
                            "`index` must be present for DELETE operation".to_owned(),
                        )
                    })?
                    .try_into()
                    .map_err(|e| {
                        Error::BadResponse(format!("`index` not a valid int for DELETE: {e:}"))
                    })?;
                rooms_list.set_cloned(pos as usize, RoomListEntry::Empty);
            }
            v4::SlidingOp::Insert => {
                let pos: usize = op
                    .index
                    .ok_or_else(|| {
                        Error::BadResponse(
                            "`index` must be present for INSERT operation".to_owned(),
                        )
                    })?
                    .try_into()
                    .map_err(|e| {
                        Error::BadResponse(format!("`index` not a valid int for INSERT: {e:}"))
                    })?;
                let sliced = rooms_list.as_slice();
                let room = RoomListEntry::Filled(op.room_id.clone().ok_or_else(|| {
                    Error::BadResponse("`room_id` must be present for INSERT operation".to_owned())
                })?);
                let mut dif = 0usize;
                loop {
                    // find the next empty slot and drop it
                    let (prev_p, prev_overflow) = pos.overflowing_sub(dif);
                    let check_prev = !prev_overflow && index_in_range(prev_p);
                    let (next_p, overflown) = pos.overflowing_add(dif);
                    let check_after = !overflown && next_p < sliced.len() && index_in_range(next_p);
                    if !check_prev && !check_after {
                        return Err(Error::BadResponse(
                            "We were asked to insert but could not find any direction to shift to"
                                .to_owned(),
                        ));
                    }

                    if check_prev && sliced[prev_p].empty_or_invalidated() {
                        // we only check for previous, if there are items left
                        rooms_list.remove(prev_p);
                        break;
                    } else if check_after && sliced[next_p].empty_or_invalidated() {
                        rooms_list.remove(next_p);
                        break;
                    } else {
                        // let's check the next position;
                        dif += 1;
                    }
                }
                rooms_list.insert_cloned(pos, room);
            }
            v4::SlidingOp::Invalidate => {
                let max_len = rooms_list.len();
                let (mut pos, end): (u32, u32) = if let Some(range) = op.range {
                    (
                        range.0.try_into().map_err(|e| {
                            Error::BadResponse(format!("`range.0` not a valid int: {e:}"))
                        })?,
                        range.1.try_into().map_err(|e| {
                            Error::BadResponse(format!("`range.1` not a valid int: {e:}"))
                        })?,
                    )
                } else {
                    return Err(Error::BadResponse(
                        "`range` must be given on `Invalidate` operation".to_owned(),
                    ));
                };

                if pos > end {
                    return Err(Error::BadResponse(
                        "Invalid invalidation, end smaller than start".to_owned(),
                    ));
                }

                // ranges are inclusive up to the last index. e.g. `[0, 10]`; `[0, 0]`.
                // ensure we pick them all up
                while pos <= end {
                    if pos as usize >= max_len {
                        break; // how does this happen?
                    }
                    let idx = pos as usize;
                    let entry = if let Some(RoomListEntry::Filled(b)) = rooms_list.get(idx) {
                        Some(b.clone())
                    } else {
                        None
                    };

                    if let Some(b) = entry {
                        rooms_list.set_cloned(pos as usize, RoomListEntry::Invalidated(b));
                    } else {
                        rooms_list.set_cloned(pos as usize, RoomListEntry::Empty);
                    }
                    pos += 1;
                }
            }
            s => {
                warn!("Unknown operation occurred: {:?}", s);
            }
        }
    }

    Ok(())
}

impl SlidingSyncView {
    /// Return a builder with the same settings as before
    pub fn new_builder(&self) -> SlidingSyncViewBuilder {
        SlidingSyncViewBuilder::default()
            .name(&self.name)
            .sync_mode(self.sync_mode.lock_ref().clone())
            .sort(self.sort.clone())
            .required_state(self.required_state.clone())
            .batch_size(self.batch_size)
            .ranges(self.ranges.read_only().get_cloned())
    }

    /// Set the ranges to fetch
    ///
    /// Remember to cancel the existing stream and fetch a new one as this will
    /// only be applied on the next request.
    pub fn set_ranges(&self, range: Vec<(u32, u32)>) -> &Self {
        *self.ranges.lock_mut() = range.into_iter().map(|(a, b)| (a.into(), b.into())).collect();
        self
    }

    /// Reset the ranges to a particular set
    ///
    /// Remember to cancel the existing stream and fetch a new one as this will
    /// only be applied on the next request.
    pub fn set_range(&self, start: u32, end: u32) -> &Self {
        *self.ranges.lock_mut() = vec![(start.into(), end.into())];
        self
    }

    /// Set the ranges to fetch
    ///
    /// Remember to cancel the existing stream and fetch a new one as this will
    /// only be applied on the next request.
    pub fn add_range(&self, start: u32, end: u32) -> &Self {
        self.ranges.lock_mut().push((start.into(), end.into()));
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
        self.ranges.lock_mut().clear();
        self
    }

    /// Find the current valid position of the room in the view room_list.
    ///
    /// Only matches against the current ranges and only against filled items.
    /// Invalid items are ignore. Return the total position the item was
    /// found in the room_list, return None otherwise.
    pub fn find_room_in_view(&self, room_id: &RoomId) -> Option<usize> {
        let ranges = self.ranges.lock_ref();
        let listing = self.rooms_list.lock_ref();
        for (start_uint, end_uint) in ranges.iter() {
            let mut cur_pos: usize = (*start_uint).try_into().unwrap();
            let end: usize = (*end_uint).try_into().unwrap();
            let iterator = listing.iter().skip(cur_pos);
            for n in iterator {
                if let RoomListEntry::Filled(r) = n {
                    if room_id == r {
                        return Some(cur_pos);
                    }
                }
                if cur_pos == end {
                    break;
                }
                cur_pos += 1;
            }
        }
        None
    }

    /// Find the current valid position of the rooms in the views room_list.
    ///
    /// Only matches against the current ranges and only against filled items.
    /// Invalid items are ignore. Return the total position the items that were
    /// found in the room_list, will skip any room not found in the rooms_list.
    pub fn find_rooms_in_view(&self, room_ids: &[OwnedRoomId]) -> Vec<(usize, OwnedRoomId)> {
        let ranges = self.ranges.lock_ref();
        let listing = self.rooms_list.lock_ref();
        let mut rooms_found = Vec::new();
        for (start_uint, end_uint) in ranges.iter() {
            let mut cur_pos: usize = (*start_uint).try_into().unwrap();
            let end: usize = (*end_uint).try_into().unwrap();
            let iterator = listing.iter().skip(cur_pos);
            for n in iterator {
                if let RoomListEntry::Filled(r) = n {
                    if room_ids.contains(r) {
                        rooms_found.push((cur_pos, r.clone()));
                    }
                }
                if cur_pos == end {
                    break;
                }
                cur_pos += 1;
            }
        }
        rooms_found
    }

    /// Return the room_id at the given index
    pub fn get_room_id(&self, index: usize) -> Option<OwnedRoomId> {
        self.rooms_list.lock_ref().get(index).and_then(|e| e.as_room_id().map(ToOwned::to_owned))
    }

    #[instrument(skip(self, ops), fields(name = self.name, ops_count = ops.len()))]
    pub(super) fn handle_response(
        &self,
        rooms_count: u32,
        ops: &Vec<v4::SyncOp>,
        ranges: &Vec<(usize, usize)>,
        rooms: &Vec<OwnedRoomId>,
    ) -> Result<bool, Error> {
        let current_rooms_count = self.rooms_count.get();
        if current_rooms_count.is_none()
            || current_rooms_count == Some(0)
            || self.is_cold.load(Ordering::SeqCst)
        {
            debug!("first run, replacing rooms list");
            // first response, we do that slightly differently
            let rooms_list =
                MutableVec::new_with_values(vec![RoomListEntry::Empty; rooms_count as usize]);
            // then we apply it
            let mut locked = rooms_list.lock_mut();
            room_ops(&mut locked, ops, ranges)?;
            self.rooms_list.lock_mut().replace_cloned(locked.as_slice().to_vec());
            self.rooms_count.set(Some(rooms_count));
            self.is_cold.store(false, Ordering::SeqCst);
            return Ok(true);
        }

        debug!("regular update");
        let mut missing =
            rooms_count.checked_sub(self.rooms_list.lock_ref().len() as u32).unwrap_or_default();
        let mut changed = false;
        if missing > 0 {
            let mut list = self.rooms_list.lock_mut();
            list.reserve_exact(missing as usize);
            while missing > 0 {
                list.push_cloned(RoomListEntry::Empty);
                missing -= 1;
            }
            changed = true;
        }

        {
            // keep the lock scoped so that the later find_rooms_in_view doesn't deadlock
            let mut rooms_list = self.rooms_list.lock_mut();

            if !ops.is_empty() {
                room_ops(&mut rooms_list, ops, ranges)?;
                changed = true;
            } else {
                debug!("no rooms operations found");
            }
        }

        if self.rooms_count.get() != Some(rooms_count) {
            self.rooms_count.set(Some(rooms_count));
            changed = true;
        }

        if self.send_updates_for_items && !rooms.is_empty() {
            let found_views = self.find_rooms_in_view(rooms);
            if !found_views.is_empty() {
                debug!("room details found");
                let mut rooms_list = self.rooms_list.lock_mut();
                for (pos, room_id) in found_views {
                    // trigger an `UpdatedAt` update
                    rooms_list.set_cloned(pos, RoomListEntry::Filled(room_id));
                    changed = true;
                }
            }
        }

        if changed {
            if let Err(e) = self.rooms_updated_signal.send(()) {
                warn!("Could not inform about rooms updated: {:?}", e);
            }
        }

        Ok(changed)
    }

    pub(super) fn request_generator(&self) -> SlidingSyncViewRequestGenerator {
        match self.sync_mode.read_only().get_cloned() {
            SlidingSyncMode::PagingFullSync => {
                SlidingSyncViewRequestGenerator::new_with_paging_syncup(self.clone())
            }
            SlidingSyncMode::GrowingFullSync => {
                SlidingSyncViewRequestGenerator::new_with_growing_syncup(self.clone())
            }
            SlidingSyncMode::Selective => SlidingSyncViewRequestGenerator::new_live(self.clone()),
        }
    }
}
