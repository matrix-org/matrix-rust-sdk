// Copyright 2020 Damir Jelić
// Copyright 2020 The Matrix.org Foundation C.I.C.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::{
    collections::BTreeMap,
    fmt::{self, Debug},
    future::Future,
    io::Read,
    pin::Pin,
    sync::Arc,
};

use anyhow::{bail, Context};
use anymap2::any::CloneAnySendSync;
use dashmap::DashMap;
use futures_core::stream::Stream;
use futures_util::{pin_mut, stream::StreamExt};
use matrix_sdk_base::{
    deserialized_responses::SyncResponse,
    media::{MediaEventContent, MediaFormat, MediaRequest, MediaThumbnailSize},
    BaseClient, Session, Store,
};
use matrix_sdk_common::locks::RwLock;
// use core::pin::Pin;
// use futures::{
//     stream::Stream,
//     task::Poll
// };
use ruma::{
    api::client::sync::sliding_sync_events, assign, events::{EventType, AnySyncRoomEvent}, serde::Raw, RoomId, UInt,
};
use tracing::{error, info, instrument, warn};
use url::Url;

use crate::{Client, Result};
/// Define the state the SlidingSync View is in
///
/// The lifetime of a SlidingSync usually starts at a `Preload`, getting a fast
/// response for the first given number of Rooms, then switches into
/// `CatchingUp` during which the view fetches the remaining rooms, usually in
/// order, some times in batches. Once that is ready, it switches into `Live`.
///
/// If the client has been offline for a while, though, the SlidingSync might
/// return back to `CatchingUp` at any point.
#[derive(Debug, Clone, PartialEq)]
pub enum SlidingSyncState {
    /// Hasn't started yet
    Cold,
    /// We are quickly preloading a preview of the most important rooms
    Preload,
    /// We are trying to load all remaining rooms, might be in batches
    CatchingUp,
    /// We are all caught up and now only sync the live responses.
    Live,
}

impl Default for SlidingSyncState {
    fn default() -> Self {
        SlidingSyncState::Cold
    }
}

#[allow(dead_code)]
/// Define the mode by which the the SlidingSyncView is in fetching the data
#[derive(Debug, Clone, PartialEq)]
pub enum SlidingSyncMode {
    /// FullSync all rooms in the background, configured with batch size
    FullSync,
    /// Only sync the specific windows defined
    Selective,
}

impl Default for SlidingSyncMode {
    fn default() -> Self {
        SlidingSyncMode::FullSync
    }
}

#[derive(Clone, Debug)]
pub enum RoomListEntry {
    Empty,
    Invalidated(Box<RoomId>),
    Filled(Box<RoomId>),
}

impl RoomListEntry {
    pub fn none_or_invalid(&self) -> bool{
        matches!{&self, RoomListEntry::Empty | RoomListEntry::Invalidated(_)}
    }
    pub fn room_id(&self) -> Option<Box<RoomId>> {
        match &self  {
            RoomListEntry::Empty => None,
            RoomListEntry::Invalidated(b) | RoomListEntry::Filled(b) => Some(b.clone())
        }
    }
    pub fn as_ref<'a>(&'a self) -> Option<&'a Box<RoomId>> {
        match &self  {
            RoomListEntry::Empty => None,
            RoomListEntry::Invalidated(b) | RoomListEntry::Filled(b) => Some(b)
        }

    }
}

pub type AliveRoomTimeline = Arc<futures_signals::signal_vec::MutableVec<Raw<AnySyncRoomEvent>>>;

impl Default for RoomListEntry {
    fn default() -> Self {
        RoomListEntry::Empty
    }
}


#[derive(Debug, Clone)]
/// Room info as giving by the SlidingSync Feature
pub struct SlidingSyncRoom {
    inner: sliding_sync_events::Room,
    is_loading_more: futures_signals::signal::Mutable<bool>,
    prev_batch: futures_signals::signal::Mutable<Option<String>>,
    timeline: AliveRoomTimeline,
}

impl SlidingSyncRoom {
    pub fn timeline(&self) -> AliveRoomTimeline {
        self.timeline.clone()
    }
    pub fn name(&self) -> &Option<String> {
        &self.inner.name
    }
    pub fn received_newer_timeline(&self, items: &Vec<Raw<AnySyncRoomEvent>>) {
        let mut timeline = self.timeline.lock_mut();
        for e in items {
            timeline.push_cloned(e.clone());
        }
    }
}

impl std::ops::Deref for SlidingSyncRoom {
    type Target = sliding_sync_events::Room;
    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl From<sliding_sync_events::Room> for SlidingSyncRoom {
    fn from(mut inner: sliding_sync_events::Room) -> Self {
        let sliding_sync_events::Room {
            timeline,
            ..
        } =  inner;
        // we overwrite to only keep one copy
        inner.timeline = vec![];
        Self {
            is_loading_more: futures_signals::signal::Mutable::new(false),
            prev_batch: futures_signals::signal::Mutable::new(inner.prev_batch.clone()),
            timeline: Arc::new(futures_signals::signal_vec::MutableVec::new_with_values(timeline)),
            inner,
        }
    }
}


type ViewState = futures_signals::signal::Mutable<SlidingSyncState>;
type SyncMode = futures_signals::signal::Mutable<SlidingSyncMode>;
type PosState = futures_signals::signal::Mutable<Option<String>>;
type RangeState = futures_signals::signal::Mutable<Vec<(UInt, UInt)>>;
type RoomsCount = futures_signals::signal::Mutable<Option<u32>>;
type RoomsList = Arc<futures_signals::signal_vec::MutableVec<RoomListEntry>>;
type RoomsMap = Arc<futures_signals::signal_map::MutableBTreeMap<Box<RoomId>, SlidingSyncRoom>>;
type RoomsSubscriptions = Arc<
    futures_signals::signal_map::MutableBTreeMap<
        Box<RoomId>,
        sliding_sync_events::RoomSubscription,
    >,
>;
type RoomUnsubscribe = Arc<futures_signals::signal_vec::MutableVec<Box<RoomId>>>;
type ViewsList = Arc<futures_signals::signal_vec::MutableVec<SlidingSyncView>>;
pub type Cancel = futures_signals::signal::Mutable<bool>;

use derive_builder::Builder;

#[derive(Debug, Clone)]
pub struct UpdateSummary {
    /// The views (according to their name), which have seen an update
    pub views: Option<Vec<String>>,
    pub rooms: Vec<Box<RoomId>>,
}

#[derive(Clone, Debug, Builder)]
pub struct SlidingSync {
    #[builder(setter(strip_option))]
    homeserver: Option<Url>,

    #[builder(private)]
    client: Client,
    // ------ Inernal state
    #[builder(private, default)]
    pos: PosState,
    #[builder(private, default)]
    pub views: ViewsList,

    #[builder(private, default)]
    subscriptions: RoomsSubscriptions,

    #[builder(private, default)]
    unsubscribe: RoomUnsubscribe,

    /// The rooms details
    #[builder(private, default)]
    rooms: RoomsMap,
}

impl SlidingSyncBuilder {
    /// Convenience function to add a full-sync view to the builder
    pub fn add_fullsync_view(&mut self) -> &mut Self {
        let mut new = self;
        let mut views = new.views.clone().unwrap_or_default();
        views.lock_mut().push_cloned(
            SlidingSyncViewBuilder::default_with_fullsync()
                .build()
                .expect("Building default full sync view doesn't fail"),
        );
        new.views = Some(views);
        new
    }

    /// Reset the views to None
    pub fn no_views(&mut self) -> &mut Self {
        let mut new = self;
        new.views = None;
        new
    }

    /// Add the given view to the list of views
    pub fn add_view(&mut self, mut v: SlidingSyncView) -> &mut Self {
        let mut new = self;

        let rooms = new.rooms.clone().unwrap_or_default();
        new.rooms = Some(rooms.clone());
        v.rooms = rooms;

        let mut views = new.views.clone().unwrap_or_default();
        views.lock_mut().push_cloned(v);
        new.views = Some(views);
        new
    }

}

impl SlidingSync {
    /// Generate a new SlidingSyncBuilder with the same inner settings and views
    /// but without the current state
    pub fn new_builder_copy(&self) -> SlidingSyncBuilder {
        let mut builder = SlidingSyncBuilder::default()
            .client(self.client.clone())
            .views(Arc::new(futures_signals::signal_vec::MutableVec::new_with_values(
                self.views
                    .lock_ref()
                    .to_vec()
                    .iter()
                    .map(|v| {
                        v.new_builder().build().expect("builder worked before, builder works now")
                    })
                    .collect(),
            )))
            .subscriptions(Arc::new(futures_signals::signal_map::MutableBTreeMap::with_values(
                self.subscriptions.lock_ref().to_owned(),
            )))
            .to_owned();

        if let Some(ref h) = self.homeserver {
            builder.homeserver(h.clone());
        }
        builder
    }

    /// Subscribe to a given room.
    ///
    /// Note: this does not cancel any pending request, so make sure to only
    /// poll the stream after you've altered this. If you do that during, it
    /// might take one round trip to take effect.
    pub fn subscribe(
        &self,
        room_id: Box<RoomId>,
        settings: Option<sliding_sync_events::RoomSubscription>,
    ) {
        self.subscriptions.lock_mut().insert_cloned(room_id, settings.unwrap_or_default());
    }

    /// Unsubscribe from a given room.
    ///
    /// Note: this does not cancel any pending request, so make sure to only
    /// poll the stream after you've altered this. If you do that during, it
    /// might take one round trip to take effect.
    pub fn unsubscribe(&self, room_id: Box<RoomId>) {
        if let Some(prev) = self.subscriptions.lock_mut().remove(&room_id) {
            self.unsubscribe.lock_mut().push_cloned(room_id);
        }
    }

    fn handle_response(
        &self,
        resp: sliding_sync_events::Response,
        views: &[SlidingSyncView],
    ) -> anyhow::Result<UpdateSummary> {
        self.client.process_sliding_sync(resp.clone());
        self.pos.replace(Some(resp.pos));
        let mut updated_views = Vec::new();

        if let Some(ops) = resp.ops {
            let mut mapped_ops = Vec::new();
            mapped_ops.resize_with(views.len(), Vec::new);

            for (idx, ops) in ops
                .iter()
                .filter_map(|r| r.deserialize().ok())
                .fold(mapped_ops, |mut mp, op| {
                    let idx: u32 =
                        op.list.try_into().expect("the list index is convertible into u32");
                    mp[idx as usize].push(op);
                    mp
                })
                .iter()
                .enumerate()
            {
                let count: u32 = resp.counts[idx].try_into().context("conversion always works")?;
                views[idx].handle_response(count, ops)?;
                updated_views.push(views[idx].name.clone());
            }
        }

        let views = if updated_views.is_empty() { None } else { Some(updated_views) };

        let rooms  = if let Some(subs) = &resp.room_subscriptions {
            let mut updated = Vec::new();
            let mut rooms = self.rooms.lock_mut();
            for (id, room_data) in subs.iter() {
                if let Some(r) = rooms.get(id) {
                    // FIXME: other updates
                    if !room_data.timeline.is_empty() {
                        r.received_newer_timeline(&room_data.timeline);
                        updated.push(id.clone())
                    }
                }
            }
            updated
        } else {
            Vec::new()
        };


        Ok(UpdateSummary { views, rooms })
    }

    /// Create the inner stream for the view
    pub fn stream<'a>(
        &'a self,
    ) -> anyhow::Result<(Cancel, impl Stream<Item = anyhow::Result<UpdateSummary>> + 'a)> {
        let views = self.views.lock_ref().to_vec();
        let cancel = Cancel::new(false);
        let ret_cancel = cancel.clone();
        let pos = self.pos.clone();

        // FIXME: hack for while the sliding sync server is on a proxy
        let mut inner_client = self.client.inner.http_client.clone();
        let server_versions = self.client.inner.server_versions.clone();
        if let Some(hs) = &self.homeserver {
            inner_client.homeserver = Arc::new(RwLock::new(hs.clone()))
        }

        let final_stream = async_stream::try_stream! {
            let mut remaining_views = views.clone();
            let mut remaining_generators: Vec<SlidingSyncViewRequestGenerator<'_>> = views
                .iter()
                .map(SlidingSyncView::request_generator)
                .collect();
            loop {
                let requests;
                (requests, remaining_generators, remaining_views) = remaining_generators
                    .into_iter()
                    .zip(remaining_views)
                    .fold(
                    (Vec::new(), Vec::new(), Vec::new()), |mut c, (mut g, v)| {
                        if let Some(r) = g.next() {
                            c.0.push(r);
                            c.1.push(g);
                            c.2.push(v);
                        }
                        c
                    });

                if remaining_views.is_empty() {
                    return
                }
                let pos = self.pos.get_cloned();
                let mut req = assign!(sliding_sync_events::Request::new(), {
                    pos: pos.as_deref(),
                });
                req.body.lists = requests;
                req.body.room_subscriptions = {
                    let subs = self.subscriptions.lock_ref().clone();
                    if subs.is_empty() {
                        None
                    } else {
                        Some(subs)
                    }
                };
                req.body.unsubscribe_rooms = {
                    let unsubs = self.unsubscribe.lock_ref().to_vec();
                    if unsubs.is_empty() {
                        None
                    } else {
                        // ToDo: we are clearing the list here pre-maturely
                        // what if the following request fails?
                        self.unsubscribe.lock_mut().clear();
                        Some(unsubs)
                    }
                };
                if cancel.get() {
                    return
                }
                warn!("requesting: {:#?}", req);
                let resp = inner_client.send(req, None, server_versions.clone()).await?;
                if cancel.get() {
                    return
                }

                let updates = self.handle_response(resp, &remaining_views)?;
                yield updates;
            }
        };

        Ok((ret_cancel, final_stream))
    }
}

/// Holding a specific filtered view within the concept of sliding sync
#[derive(Clone, Debug, Builder)]
pub struct SlidingSyncView {
    #[allow(dead_code)]
    #[builder(setter(strip_option), default)]
    sync_mode: SyncMode,

    #[builder(default = "self.default_sort()")]
    sort: Vec<String>,

    #[builder(default = "self.default_required_state()")]
    required_state: Vec<(EventType, String)>,

    #[builder(default = "20")]
    batch_size: u32,

    #[builder(default)]
    filters: Option<Raw<sliding_sync_events::SyncRequestListFilters>>,

    #[builder(setter(name = "timeline_limit_raw"), default)]
    timeline_limit: Option<UInt>,

    // ----- Public state
    /// Name of this view to easily recognise them
    #[builder(setter(into))]
    pub name: String,

    /// The state this view is in
    #[builder(default)]
    pub state: ViewState,
    /// The total known number of rooms,
    #[builder(default)]
    pub rooms_count: RoomsCount,
    /// The rooms in order
    #[builder(default)]
    pub rooms_list: RoomsList,
    /// The rooms details
    #[builder(default)]
    pub rooms: RoomsMap,

    #[builder(setter(name = "ranges_raw"), default)]
    ranges: RangeState,
}

impl SlidingSyncViewBuilder {
    /// Create a Builder set up for full sync
    pub fn default_with_fullsync() -> Self {
        Self::default()
            .name("FullSync")
            .sync_mode(SyncMode::new(SlidingSyncMode::FullSync))
            .to_owned()
    }

    // defaults
    fn default_sort(&self) -> Vec<String> {
        vec!["by_recency".to_string(), "by_name".to_string()]
    }

    fn default_required_state(&self) -> Vec<(EventType, String)> {
        vec![
            (EventType::RoomAvatar, "".to_string()),
            (EventType::RoomMember, "*".to_string()),
            (EventType::RoomEncryption, "".to_string()),
            (EventType::RoomTombstone, "".to_string()),
        ]
    }

    /// Set the ranges to fetch
    pub fn ranges<U: Into<UInt>>(&mut self, range: Vec<(U, U)>) -> &mut Self {
        let mut new = self;
        new.ranges =
            Some(RangeState::new(range.into_iter().map(|(a, b)| (a.into(), b.into())).collect()));
        new
    }

    /// Set the limit of regular events to fetch for the timeline.
    pub fn timeline_limit<U: Into<UInt>>(&mut self, timeline_limit: U) -> &mut Self {
        let mut new = self;
        new.timeline_limit = Some(Some(timeline_limit.into()));
        new
    }

    /// Reset the limit of regular events to fetch for the timeline. It is left
    /// to the server to decide how many to send back
    pub fn no_timeline_limit(&mut self) -> &mut Self {
        let mut new = self;
        new.timeline_limit = None;
        new
    }
}

enum InnerSlidingSyncViewRequestGenerator {
    FullSync(u32, u32), // current position, batch_size
    Live,
}

struct SlidingSyncViewRequestGenerator<'a> {
    view: &'a SlidingSyncView,
    inner: InnerSlidingSyncViewRequestGenerator,
}

impl<'a> SlidingSyncViewRequestGenerator<'a> {
    fn new_with_syncup(view: &'a SlidingSyncView) -> Self {
        let batch_size = view.batch_size.clone();

        SlidingSyncViewRequestGenerator {
            view,
            inner: InnerSlidingSyncViewRequestGenerator::FullSync(0, batch_size),
        }
    }

    fn new_live(view: &'a SlidingSyncView) -> Self {
        SlidingSyncViewRequestGenerator { view, inner: InnerSlidingSyncViewRequestGenerator::Live }
    }

    fn prefetch_request(
        &self,
        start: u32,
        batch_size: u32,
    ) -> (u32, Raw<sliding_sync_events::SyncRequestList>) {
        let end = start + batch_size;
        let ranges = vec![(start.into(), end.into())];
        (end, self.make_request_for_ranges(ranges))
    }

    fn make_request_for_ranges(
        &self,
        ranges: Vec<(UInt, UInt)>,
    ) -> Raw<sliding_sync_events::SyncRequestList> {
        let sort = Some(self.view.sort.clone());
        let required_state = Some(self.view.required_state.clone());
        let timeline_limit = self
            .view
            .timeline_limit
            .clone()
            .map(|v| v.try_into().expect("u32 always fits into UInt"));
        let filters = self.view.filters.clone();

        Raw::new(&assign!(sliding_sync_events::SyncRequestList::default(), {
            ranges,
            required_state,
            sort,
            timeline_limit,
            filters,
        }))
        .expect("Generting request data doesn't fail")
    }

    // generate the next live request
    fn live_request(&self) -> Raw<sliding_sync_events::SyncRequestList> {
        let ranges = self.view.ranges.read_only().get_cloned();
        self.make_request_for_ranges(ranges)
    }
}

impl<'a> core::iter::Iterator for SlidingSyncViewRequestGenerator<'a> {
    type Item = Raw<sliding_sync_events::SyncRequestList>;
    fn next(&mut self) -> Option<Self::Item> {
        if let InnerSlidingSyncViewRequestGenerator::FullSync(cur_pos, _) = self.inner {
            if let Some(count) = self.view.rooms_count.get_cloned() {
                if count <= cur_pos {
                    // we are switching to live mode
                    self.view.state.set_if(SlidingSyncState::Live, |before, _now| {
                        *before == SlidingSyncState::CatchingUp
                    });
                    self.inner = InnerSlidingSyncViewRequestGenerator::Live
                }
            } else {
                // upon first catch up request, we want to switch state
                self.view.state.set_if(SlidingSyncState::Preload, |before, _now| {
                    *before == SlidingSyncState::Cold
                });
            }
        }
        match self.inner {
            InnerSlidingSyncViewRequestGenerator::FullSync(cur_pos, batch_size) => {
                let (end, req) = self.prefetch_request(cur_pos, batch_size);
                self.inner = InnerSlidingSyncViewRequestGenerator::FullSync(end, batch_size);
                self.view.state.set_if(SlidingSyncState::CatchingUp, |before, _now| {
                    *before == SlidingSyncState::Preload
                });
                Some(req)
            }
            InnerSlidingSyncViewRequestGenerator::Live => Some(self.live_request()),
        }
    }
}

impl SlidingSyncView {
    /// Return a builder with the same settings as before
    pub fn new_builder(&self) -> SlidingSyncViewBuilder {
        SlidingSyncViewBuilder::default()
            .name(&self.name)
            .sync_mode(self.sync_mode.clone())
            .sort(self.sort.clone())
            .required_state(self.required_state.clone())
            .batch_size(self.batch_size)
            .ranges(self.ranges.read_only().get_cloned())
            .to_owned()
    }

    /// Set the ranges to fetch
    ///
    /// Remember to cancel the existing stream and fetch a new one as this will
    /// only be applied on the next request.
    pub fn set_ranges(&mut self, range: Vec<(u32, u32)>) -> &mut Self {
        *self.ranges.lock_mut() = range.into_iter().map(|(a, b)| (a.into(), b.into())).collect();
        self
    }

    /// Set the ranges to fetch
    ///
    /// Remember to cancel the existing stream and fetch a new one as this will
    /// only be applied on the next request.
    pub fn add_range(&mut self, start: u32, end: u32) {
        self.ranges.lock_mut().push((start.into(), end.into()));
    }

    /// Return the subset of rooms, starting at offset (default 0) returning
    /// count (or to the end) items
    pub fn get_rooms(
        &self,
        offset: Option<usize>,
        count: Option<usize>,
    ) -> Vec<sliding_sync_events::Room> {
        let start = offset.unwrap_or(0);
        let rooms = self.rooms.lock_ref();
        let listing = self.rooms_list.lock_ref();
        let count = count.unwrap_or_else(|| listing.len() - start);
        listing
            .iter()
            .skip(start)
            .filter_map(|id| id.as_ref())
            .filter_map(|id| rooms.get(id))
            .map(|r| r.inner.clone())
            .take(count)
            .collect()
    }

    fn room_ops(&self, ops: &Vec<sliding_sync_events::SyncOp>) -> anyhow::Result<()> {
        let mut rooms_list = self.rooms_list.lock_mut();
        let mut rooms_map = self.rooms.lock_mut();
        for op in ops {
            let mut room_ids = Vec::new();
            if let Some(rooms) = &op.rooms {
                for room in rooms {
                    let r: Box<RoomId> =
                        room.room_id.clone().context("Sliding Sync without RoomdId")?.parse()?;
                    rooms_map.insert_cloned(r.clone(), room.clone().into());
                    room_ids.push(r);
                }
            } else if let Some(room) = &op.room { // Insert specific 
                let r: Box<RoomId> =
                    room.room_id.clone().context("Sliding Sync without RoomdId")?.parse()?;
                rooms_map.insert_cloned(r.clone(), room.clone().into());
                room_ids.push(r);
            }

            match op.op {
                sliding_sync_events::SlidingOp::Update |
                sliding_sync_events::SlidingOp::Sync => {
                    let start: u32 = op.range.0.try_into()?;
                    room_ids
                        .into_iter()
                        .enumerate()
                        .map(|(i, r)| {
                            let idx = start as usize + i;
                            rooms_list.set_cloned(idx, RoomListEntry::Filled(r));
                        })
                        .count();
                }
                sliding_sync_events::SlidingOp::Delete => {
                    let pos: u32 = op.range.0.try_into()?;
                    rooms_list.set_cloned(pos as usize, RoomListEntry::Empty);
                }
                sliding_sync_events::SlidingOp::Insert => {
                    let pos: u32 = op.range.0.try_into()?;
                    let sliced = rooms_list.as_slice();
                    let room = room_ids.first().cloned().map(|r| RoomListEntry::Filled(r)).unwrap_or_default();
                    let mut dif = 0;
                    loop {
                        let check_prev = dif > pos;
                        let check_after = ((dif + pos) as usize) < sliced.len();
                        if !check_prev && !check_after {
                            bail!("We were asked to insert but could not find any direction to shift to");
                        }

                        if  check_prev && sliced[(pos - dif) as usize].none_or_invalid() {
                            // we only check for previous, if there are items left
                            rooms_list.insert_cloned(pos as usize, room);
                            rooms_list.remove((pos - dif) as usize);
                            break;
                        } else if check_after && sliced[(pos + dif) as usize].none_or_invalid() {
                            rooms_list.remove((pos + dif) as usize);
                            rooms_list.insert_cloned(pos as usize, room);
                            break;
                        } else {
                            // let's check the next position;
                            dif += 1;
                        }
                    }
                }
                sliding_sync_events::SlidingOp::Invalidate => {
                    let max_len = rooms_list.len();
                    let mut pos: u32 = op.range.0.try_into()?;
                    let end: u32 = op.range.1.try_into()?;
                    if pos > end {
                        bail!("Invalid invalidation, end smaller than start");
                    } 

                    while (pos < end) {
                        if (pos as usize >= max_len) {
                            break // how does this happen?
                        }
                        let idx = pos as usize;
                        if let Some(RoomListEntry::Filled(b)) = rooms_list.get(idx) {
                            rooms_list.set_cloned(pos as usize, RoomListEntry::Invalidated(b.clone()));
                        } else {
                            rooms_list.set_cloned(pos as usize, RoomListEntry::Empty);
                        }
                        pos += 1;
                    }
                }
            }
        }

        Ok(())
    }

    fn handle_response(
        &self,
        rooms_count: u32,
        ops: &Vec<sliding_sync_events::SyncOp>,
    ) -> anyhow::Result<()> {
        let mut missing =
            rooms_count.checked_sub(self.rooms_list.lock_ref().len() as u32).unwrap_or_default();
        if missing > 0 {
            let mut list = self.rooms_list.lock_mut();
            list.reserve_exact(missing as usize);
            while missing > 0 {
                list.push_cloned(RoomListEntry::Empty);
                missing -= 1;
            }
            self.rooms_count.replace(Some(rooms_count));
        }

        if !ops.is_empty() {
            self.room_ops(ops)?;
        }

        Ok(())
    }

    fn request_generator<'a>(&'a self) -> SlidingSyncViewRequestGenerator<'a> {
        match self.sync_mode.read_only().get_cloned() {
            SlidingSyncMode::FullSync => SlidingSyncViewRequestGenerator::new_with_syncup(self),
            SlidingSyncMode::Selective => SlidingSyncViewRequestGenerator::new_live(self),
        }
    }
}

impl Client {
    /// Create a SlidingSyncBuilder tied to this client
    pub fn sliding_sync(&self) -> SlidingSyncBuilder {
        SlidingSyncBuilder::default().client(self.clone()).to_owned()
    }
    pub(crate) async fn process_sliding_sync(
        &self,
        response: sliding_sync_events::Response,
    ) -> Result<SyncResponse> {
        let response = self.base_client().process_sliding_sync(response).await?;
        self.handle_sync_response(response).await
    }
}
