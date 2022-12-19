// Copyright 2022 Benjamin Kampmann
// Copyright 2022 The Matrix.org Foundation C.I.C.
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
    fmt::Debug,
    sync::{
        atomic::{AtomicBool, AtomicU8, Ordering},
        Arc,
    },
};

use futures_core::stream::Stream;
use futures_signals::{signal::Mutable, signal_map::MutableBTreeMap, signal_vec::MutableVec};
use matrix_sdk_base::{deserialized_responses::SyncTimelineEvent, sync::SyncResponse};
use ruma::{
    api::{
        client::{
            error::{ErrorBody, ErrorKind},
            sync::sync_events::v4::{
                self, AccountDataConfig, E2EEConfig, ExtensionsConfig, ToDeviceConfig,
            },
        },
        error::FromHttpResponseError,
    },
    assign,
    events::RoomEventType,
    OwnedRoomId, RoomId, UInt,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use url::Url;

#[cfg(feature = "experimental-timeline")]
use crate::room::timeline::{EventTimelineItem, Timeline};
use crate::{Client, HttpError, Result, RumaApiError};

/// Internal representation of errors in Sliding Sync
#[derive(Error, Debug)]
#[non_exhaustive]
pub enum Error {
    #[error("Received response for {found} lists, yet we have {expected}.")]
    BadViewsCount { found: usize, expected: usize },
    #[error("The sliding sync response could not be handled: {0}")]
    BadResponse(String),
    #[error("Builder went wrong: {0}")]
    SlidingSyncBuilder(#[from] SlidingSyncBuilderError),
}

/// The state the [`SlidingSyncView`] is in.
///
/// The lifetime of a SlidingSync usually starts at a `Preload`, getting a fast
/// response for the first given number of Rooms, then switches into
/// `CatchingUp` during which the view fetches the remaining rooms, usually in
/// order, some times in batches. Once that is ready, it switches into `Live`.
///
/// If the client has been offline for a while, though, the SlidingSync might
/// return back to `CatchingUp` at any point.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SlidingSyncState {
    /// Hasn't started yet
    #[default]
    Cold,
    /// We are quickly preloading a preview of the most important rooms
    Preload,
    /// We are trying to load all remaining rooms, might be in batches
    CatchingUp,
    /// We are all caught up and now only sync the live responses.
    Live,
}

/// The mode by which the the [`SlidingSyncView`] is in fetching the data.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SlidingSyncMode {
    /// fully sync all rooms in the background, page by page of `batch_size`
    #[default]
    #[serde(alias = "FullSync")]
    PagingFullSync,
    /// fully sync all rooms in the background, with a growing window of
    /// `batch_size`,
    GrowingFullSync,
    /// Only sync the specific windows defined
    Selective,
}

impl SlidingSyncMode {
    fn is_full_sync(&self) -> bool {
        matches!(self, SlidingSyncMode::PagingFullSync | SlidingSyncMode::GrowingFullSync)
    }
}

/// The Entry in the sliding sync room list per sliding sync view
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub enum RoomListEntry {
    /// This entry isn't known at this point and thus considered `Empty`
    #[default]
    Empty,
    /// There was `OwnedRoomId` but since the server told us to invalid this
    /// entry. it is considered stale
    Invalidated(OwnedRoomId),
    /// This Entry is followed with `OwnedRoomId`
    Filled(OwnedRoomId),
}

impl RoomListEntry {
    /// Is this entry empty or invalidated?
    pub fn empty_or_invalidated(&self) -> bool {
        matches!(self, RoomListEntry::Empty | RoomListEntry::Invalidated(_))
    }

    /// The inner room_id if given
    pub fn as_room_id(&self) -> Option<&RoomId> {
        match &self {
            RoomListEntry::Empty => None,
            RoomListEntry::Invalidated(b) | RoomListEntry::Filled(b) => Some(b.as_ref()),
        }
    }

    fn freeze(&self) -> RoomListEntry {
        match &self {
            RoomListEntry::Empty => RoomListEntry::Empty,
            RoomListEntry::Invalidated(b) | RoomListEntry::Filled(b) => {
                RoomListEntry::Invalidated(b.clone())
            }
        }
    }
}

pub type AliveRoomTimeline = Arc<MutableVec<SyncTimelineEvent>>;

/// Room info as giving by the SlidingSync Feature.
#[derive(Debug, Clone)]
pub struct SlidingSyncRoom {
    client: Client,
    room_id: OwnedRoomId,
    inner: v4::SlidingSyncRoom,
    is_loading_more: Mutable<bool>,
    is_cold: Arc<AtomicBool>,
    prev_batch: Mutable<Option<String>>,
    timeline: AliveRoomTimeline,
}

#[derive(Serialize, Deserialize)]
struct FrozenSlidingSyncRoom {
    room_id: OwnedRoomId,
    inner: v4::SlidingSyncRoom,
    prev_batch: Option<String>,
    timeline: Vec<SyncTimelineEvent>,
}

impl From<&SlidingSyncRoom> for FrozenSlidingSyncRoom {
    fn from(value: &SlidingSyncRoom) -> Self {
        let locked_tl = value.timeline.lock_ref();
        let tl_len = locked_tl.len();
        let (prev_batch, timeline) = if tl_len > 10 {
            let pos = tl_len - 10;
            (None, locked_tl.iter().skip(pos).cloned().collect())
        } else {
            (value.prev_batch.lock_ref().clone(), locked_tl.to_vec())
        };
        FrozenSlidingSyncRoom {
            prev_batch,
            timeline,
            room_id: value.room_id.clone(),
            inner: value.inner.clone(),
        }
    }
}

impl SlidingSyncRoom {
    fn from_frozen(val: FrozenSlidingSyncRoom, client: Client) -> Self {
        let FrozenSlidingSyncRoom { room_id, inner, prev_batch, timeline } = val;
        SlidingSyncRoom {
            client,
            room_id,
            inner,
            is_loading_more: Mutable::new(false),
            is_cold: Arc::new(AtomicBool::new(true)),
            prev_batch: Mutable::new(prev_batch),
            timeline: Arc::new(MutableVec::new_with_values(timeline)),
        }
    }
}

impl SlidingSyncRoom {
    fn from(
        client: Client,
        room_id: OwnedRoomId,
        mut inner: v4::SlidingSyncRoom,
        timeline: Vec<SyncTimelineEvent>,
    ) -> Self {
        // we overwrite to only keep one copy
        inner.timeline = vec![];
        Self {
            client,
            room_id,
            is_loading_more: Mutable::new(false),
            is_cold: Arc::new(AtomicBool::new(false)),
            prev_batch: Mutable::new(inner.prev_batch.clone()),
            timeline: Arc::new(MutableVec::new_with_values(timeline)),
            inner,
        }
    }

    /// RoomId of this SlidingSyncRoom
    pub fn room_id(&self) -> &OwnedRoomId {
        &self.room_id
    }

    /// Are we currently fetching more timeline events in this room?
    pub fn is_loading_more(&self) -> bool {
        *self.is_loading_more.lock_ref()
    }

    /// the `prev_batch` key to fetch more timeline events for this room
    pub fn prev_batch(&self) -> Option<String> {
        self.prev_batch.lock_ref().clone()
    }

    /// `AliveTimeline` of this room
    #[cfg(not(feature = "experimental-timeline"))]
    pub fn timeline(&self) -> AliveRoomTimeline {
        self.timeline.clone()
    }

    /// `Timeline` of this room
    #[cfg(feature = "experimental-timeline")]
    pub async fn timeline(&self) -> Option<Timeline> {
        Some(self.timeline_no_fully_read_tracking()?.with_fully_read_tracking().await)
    }

    fn timeline_no_fully_read_tracking(&self) -> Option<Timeline> {
        if let Some(room) = self.client.get_room(&self.room_id) {
            let current_timeline = self.timeline.lock_ref().to_vec();
            let prev_batch = self.prev_batch.lock_ref().clone();
            Some(Timeline::with_events(&room, prev_batch, current_timeline))
        } else if let Some(invited_room) = self.client.get_invited_room(&self.room_id) {
            Some(Timeline::with_events(&invited_room, None, vec![]))
        } else {
            tracing::error!(
                room_id = ?self.room_id,
                "Room not found in client. Can't provide a timeline for it"
            );
            None
        }
    }

    /// The latest timeline item of this room.
    ///
    /// Use `Timeline::latest_event` instead if you already have a timeline for
    /// this `SlidingSyncRoom`.
    #[cfg(feature = "experimental-timeline")]
    pub fn latest_event(&self) -> Option<EventTimelineItem> {
        self.timeline_no_fully_read_tracking()?.latest_event()
    }

    /// This rooms name as calculated by the server, if any
    pub fn name(&self) -> Option<&str> {
        self.inner.name.as_deref()
    }

    fn update(&mut self, room_data: &v4::SlidingSyncRoom, timeline: Vec<SyncTimelineEvent>) {
        let v4::SlidingSyncRoom {
            name,
            initial,
            is_dm,
            invite_state,
            unread_notifications,
            required_state,
            prev_batch,
            ..
        } = room_data;

        self.inner.unread_notifications = unread_notifications.clone();

        if name.is_some() {
            self.inner.name = name.clone();
        }
        if initial.is_some() {
            self.inner.initial = *initial;
        }
        if is_dm.is_some() {
            self.inner.is_dm = *is_dm;
        }
        if !invite_state.is_empty() {
            self.inner.invite_state = invite_state.clone();
        }
        if !required_state.is_empty() {
            self.inner.required_state = required_state.clone();
        }

        if let Some(batch) = prev_batch {
            self.prev_batch.lock_mut().replace(batch.clone());
        }

        if !timeline.is_empty() {
            if self.is_cold.load(Ordering::Relaxed) {
                // if we come from cold storage, we hard overwrite
                self.timeline.lock_mut().replace_cloned(timeline);
                self.is_cold.store(false, Ordering::Relaxed);
            } else {
                let mut ref_timeline = self.timeline.lock_mut();
                for e in timeline {
                    ref_timeline.push_cloned(e);
                }
            }
        }
    }
}

impl std::ops::Deref for SlidingSyncRoom {
    type Target = v4::SlidingSyncRoom;
    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

type ViewState = Mutable<SlidingSyncState>;
type SyncMode = Mutable<SlidingSyncMode>;
type PosState = Mutable<Option<String>>;
type RangeState = Mutable<Vec<(UInt, UInt)>>;
type RoomsCount = Mutable<Option<u32>>;
type RoomsList = Arc<MutableVec<RoomListEntry>>;
type RoomsMap = Arc<MutableBTreeMap<OwnedRoomId, SlidingSyncRoom>>;
type RoomsSubscriptions = Arc<MutableBTreeMap<OwnedRoomId, v4::RoomSubscription>>;
type RoomUnsubscribe = Arc<MutableVec<OwnedRoomId>>;
type ViewsList = Arc<MutableVec<SlidingSyncView>>;

use derive_builder::Builder;

/// The Summary of a new SlidingSync Update received
#[derive(Debug, Clone)]
pub struct UpdateSummary {
    /// The views (according to their name), which have seen an update
    pub views: Vec<String>,
    /// The Rooms that have seen updates
    pub rooms: Vec<OwnedRoomId>,
}

/// Configuration for a Sliding Sync Instance
#[derive(Clone, Debug, Builder)]
#[builder(
    name = "SlidingSyncBuilder",
    pattern = "owned",
    build_fn(name = "build_no_cache"),
    derive(Clone, Debug)
)]
pub struct SlidingSyncConfig {
    /// The storage key to keep this cache at and load it from
    #[builder(setter(strip_option), default)]
    storage_key: Option<String>,
    /// Customize the homeserver for sliding sync only
    #[builder(setter(strip_option), default)]
    homeserver: Option<Url>,

    /// The client this sliding sync will be using
    client: Client,
    #[builder(private, default)]
    views: Vec<SlidingSyncView>,
    #[builder(private, default)]
    extensions: Option<ExtensionsConfig>,
    #[builder(private, default)]
    subscriptions: BTreeMap<OwnedRoomId, v4::RoomSubscription>,
}

impl SlidingSyncConfig {
    pub async fn build(self) -> Result<SlidingSync> {
        let SlidingSyncConfig {
            homeserver,
            storage_key,
            client,
            mut views,
            extensions,
            subscriptions,
        } = self;
        let rooms = if let Some(storage_key) = storage_key.as_ref() {
            if let Some(mut f) = client
                .store()
                .get_custom_value(storage_key.as_bytes())
                .await?
                .map(|v| serde_json::from_slice::<FrozenSlidingSync>(&v))
                .transpose()?
            {
                for view in views.iter_mut() {
                    if let Some(frozen_view) = f.views.remove(&view.name) {
                        view.set_from_cold(frozen_view);
                    }
                }

                f.rooms
                    .into_iter()
                    .map(|(k, v)| (k, SlidingSyncRoom::from_frozen(v, client.clone())))
                    .collect()
            } else {
                Default::default()
            }
        } else {
            Default::default()
        };

        let rooms = Arc::new(MutableBTreeMap::with_values(rooms));
        // map the roomsmap into the views:
        for v in &mut views {
            v.rooms = rooms.clone();
        }

        let views = Arc::new(MutableVec::new_with_values(views));

        Ok(SlidingSync {
            homeserver,
            client,
            storage_key,

            views,
            rooms,
            extensions: Mutable::new(extensions),
            failure_count: Default::default(),

            pos: Mutable::new(None),
            subscriptions: Arc::new(MutableBTreeMap::with_values(subscriptions)),
            unsubscribe: Default::default(),
        })
    }
}

impl SlidingSyncBuilder {
    /// Convenience function to add a full-sync view to the builder
    pub fn add_fullsync_view(mut self) -> Self {
        let views = self.views.get_or_insert_with(Default::default);
        views.push(
            SlidingSyncViewBuilder::default_with_fullsync()
                .build()
                .expect("Building default full sync view doesn't fail"),
        );
        self
    }

    /// The cold cache key to read from and store the frozen state at
    pub fn cold_cache<T: ToString>(mut self, name: T) -> Self {
        self.storage_key = Some(Some(name.to_string()));
        self
    }

    /// Do not use the cold cache
    pub fn no_cold_cache(mut self) -> Self {
        self.storage_key = None;
        self
    }

    /// Reset the views to `None`
    pub fn no_views(mut self) -> Self {
        self.views = None;
        self
    }

    /// Add the given view to the list of views
    pub fn add_view(mut self, v: SlidingSyncView) -> Self {
        let views = self.views.get_or_insert_with(Default::default);
        views.push(v);
        self
    }

    /// Activate e2ee, to-device-message and account data extensions if not yet
    /// configured.
    ///
    /// Will leave any extension configuration found untouched, so the order
    /// does not matter.
    pub fn with_common_extensions(mut self) -> Self {
        {
            let mut cfg = self
                .extensions
                .get_or_insert_with(Default::default)
                .get_or_insert_with(Default::default);
            if cfg.to_device.is_none() {
                cfg.to_device = Some(assign!(ToDeviceConfig::default(), {enabled : Some(true)}));
            }

            if cfg.e2ee.is_none() {
                cfg.e2ee = Some(assign!(E2EEConfig::default(), {enabled : Some(true)}));
            }

            if cfg.account_data.is_none() {
                cfg.account_data =
                    Some(assign!(AccountDataConfig::default(), {enabled : Some(true)}));
            }
        }
        self
    }

    /// Set the E2EE extension configuration.
    pub fn with_e2ee_extension(mut self, e2ee: E2EEConfig) -> Self {
        self.extensions
            .get_or_insert_with(Default::default)
            .get_or_insert_with(Default::default)
            .e2ee = Some(e2ee);
        self
    }

    /// Unset the E2EE extension configuration.
    pub fn without_e2ee_extension(mut self) -> Self {
        self.extensions
            .get_or_insert_with(Default::default)
            .get_or_insert_with(Default::default)
            .e2ee = None;
        self
    }

    /// Set the ToDevice extension configuration.
    pub fn with_to_device_extension(mut self, to_device: ToDeviceConfig) -> Self {
        self.extensions
            .get_or_insert_with(Default::default)
            .get_or_insert_with(Default::default)
            .to_device = Some(to_device);
        self
    }

    /// Unset the ToDevice extension configuration.
    pub fn without_to_device_extension(mut self) -> Self {
        self.extensions
            .get_or_insert_with(Default::default)
            .get_or_insert_with(Default::default)
            .to_device = None;
        self
    }

    /// Set the account data extension configuration.
    pub fn with_account_data_extension(mut self, account_data: AccountDataConfig) -> Self {
        self.extensions
            .get_or_insert_with(Default::default)
            .get_or_insert_with(Default::default)
            .account_data = Some(account_data);
        self
    }

    /// Unset the account data extension configuration.
    pub fn without_account_data_extension(mut self) -> Self {
        self.extensions
            .get_or_insert_with(Default::default)
            .get_or_insert_with(Default::default)
            .account_data = None;
        self
    }

    /// Build the Sliding Sync
    ///
    /// if configured, load the cached data from cold storage
    pub async fn build(self) -> Result<SlidingSync> {
        self.build_no_cache().map_err(Error::SlidingSyncBuilder)?.build().await
    }
}

/// The sliding sync instance
#[derive(Clone, Debug)]
pub struct SlidingSync {
    /// Customize the homeserver for sliding sync only
    homeserver: Option<Url>,

    client: Client,

    /// The storage key to keep this cache at and load it from
    storage_key: Option<String>,

    // ------ Internal state
    pos: PosState,

    /// The views of this sliding sync instance
    pub views: ViewsList,

    subscriptions: RoomsSubscriptions,
    unsubscribe: RoomUnsubscribe,

    /// The rooms details
    rooms: RoomsMap,

    /// keeping track of retries and failure cunts
    failure_count: Arc<AtomicU8>,

    extensions: Mutable<Option<ExtensionsConfig>>,
}

#[derive(Serialize, Deserialize)]
struct FrozenSlidingSync {
    views: BTreeMap<String, FrozenSlidingSyncView>,
    rooms: BTreeMap<OwnedRoomId, FrozenSlidingSyncRoom>,
}

impl From<&SlidingSync> for FrozenSlidingSync {
    fn from(v: &SlidingSync) -> Self {
        FrozenSlidingSync {
            views: v.views.lock_ref().iter().map(|v| (v.name.clone(), v.into())).collect(),
            rooms: v.rooms.lock_ref().iter().map(|(k, v)| (k.clone(), v.into())).collect(),
        }
    }
}

impl SlidingSync {
    async fn cache_to_storage(&self) -> Result<()> {
        let Some(storage_key) = self.storage_key.as_ref() else { return Ok(()) };
        let v = serde_json::to_vec(&FrozenSlidingSync::from(self))?;
        self.client.store().set_custom_value(storage_key.as_bytes(), v).await?;
        Ok(())
    }
}

impl SlidingSync {
    /// Generate a new SlidingSyncBuilder with the same inner settings and views
    /// but without the current state
    pub fn new_builder_copy(&self) -> SlidingSyncBuilder {
        let builder = SlidingSyncBuilder::default()
            .client(self.client.clone())
            .views(
                self.views
                    .lock_ref()
                    .to_vec()
                    .iter()
                    .map(|v| {
                        v.new_builder().build().expect("builder worked before, builder works now")
                    })
                    .collect(),
            )
            .subscriptions(self.subscriptions.lock_ref().to_owned());

        if let Some(h) = &self.homeserver {
            builder.homeserver(h.clone())
        } else {
            builder
        }
    }

    /// Subscribe to a given room.
    ///
    /// Note: this does not cancel any pending request, so make sure to only
    /// poll the stream after you've altered this. If you do that during, it
    /// might take one round trip to take effect.
    pub fn subscribe(&self, room_id: OwnedRoomId, settings: Option<v4::RoomSubscription>) {
        self.subscriptions.lock_mut().insert_cloned(room_id, settings.unwrap_or_default());
    }

    /// Unsubscribe from a given room.
    ///
    /// Note: this does not cancel any pending request, so make sure to only
    /// poll the stream after you've altered this. If you do that during, it
    /// might take one round trip to take effect.
    pub fn unsubscribe(&self, room_id: OwnedRoomId) {
        if self.subscriptions.lock_mut().remove(&room_id).is_some() {
            self.unsubscribe.lock_mut().push_cloned(room_id);
        }
    }

    /// Lookup a specific room
    pub fn get_room(&self, room_id: OwnedRoomId) -> Option<SlidingSyncRoom> {
        self.rooms.lock_ref().get(&room_id).cloned()
    }

    fn update_to_device_since(&self, since: String) {
        self.extensions
            .lock_mut()
            .get_or_insert_with(Default::default)
            .to_device
            .get_or_insert_with(Default::default)
            .since = Some(since);
    }

    /// Lookup a set of rooms
    pub fn get_rooms<I: Iterator<Item = OwnedRoomId>>(
        &self,
        room_ids: I,
    ) -> Vec<Option<SlidingSyncRoom>> {
        let rooms = self.rooms.lock_ref();
        room_ids.map(|room_id| rooms.get(&room_id).cloned()).collect()
    }

    async fn handle_response(
        &self,
        resp: v4::Response,
        views: &[SlidingSyncView],
    ) -> Result<UpdateSummary, crate::Error> {
        let mut processed = self.client.process_sliding_sync(resp.clone()).await?;
        tracing::debug!("main client processed.");
        self.pos.replace(Some(resp.pos));
        let update = {
            let mut updated_views = Vec::new();
            if resp.lists.len() != views.len() {
                return Err(Error::BadViewsCount {
                    found: resp.lists.len(),
                    expected: views.len(),
                }
                .into());
            }

            for (view, updates) in std::iter::zip(views, &resp.lists) {
                let count: u32 =
                    updates.count.try_into().expect("the list total count convertible into u32");
                tracing::trace!("view {:?}  update: {:?}", view.name, !updates.ops.is_empty());
                if !updates.ops.is_empty() && view.handle_response(count, &updates.ops)? {
                    updated_views.push(view.name.clone());
                }
            }

            let mut rooms = Vec::new();
            let mut rooms_map = self.rooms.lock_mut();
            for (id, mut room_data) in resp.rooms.into_iter() {
                let timeline = if let Some(joined_room) = processed.rooms.join.remove(&id) {
                    joined_room.timeline.events
                } else {
                    let events = room_data.timeline.into_iter().map(Into::into).collect();
                    room_data.timeline = vec![];
                    events
                };

                if let Some(mut r) = rooms_map.remove(&id) {
                    r.update(&room_data, timeline);
                    rooms_map.insert_cloned(id.clone(), r);
                    rooms.push(id.clone());
                } else {
                    rooms_map.insert_cloned(
                        id.clone(),
                        SlidingSyncRoom::from(self.client.clone(), id.clone(), room_data, timeline),
                    );
                    rooms.push(id);
                }
            }

            // Update the `to-device` next-batch if found.
            if let Some(to_device_since) = resp.extensions.to_device.map(|t| t.next_batch) {
                self.update_to_device_since(to_device_since)
            }
            UpdateSummary { views: updated_views, rooms }
        };

        self.cache_to_storage().await?;

        Ok(update)
    }

    /// Create the inner stream for the view.
    ///
    /// Run this stream to receive new updates from the server.
    pub async fn stream<'a>(
        &self,
    ) -> Result<impl Stream<Item = Result<UpdateSummary, crate::Error>> + '_> {
        let views = self.views.lock_ref().to_vec();
        let extensions = self.extensions.clone();
        let client = self.client.clone();

        Ok(async_stream::stream! {
            let initial_extensions = extensions.lock_ref().clone();
            let mut remaining_views = views.clone();
            let mut remaining_generators: Vec<SlidingSyncViewRequestGenerator<'_>> = views
                .iter()
                .map(SlidingSyncView::request_generator)
                .collect();
            loop {
                let mut requests = Vec::new();
                let mut new_remaining_generators = Vec::new();
                let mut new_remaining_views = Vec::new();

                for (mut generator, view) in std::iter::zip(remaining_generators, remaining_views) {
                    if let Some(request) = generator.next() {
                        requests.push(request);
                        new_remaining_generators.push(generator);
                        new_remaining_views.push(view);
                    }
                }

                if new_remaining_views.is_empty() {
                    return
                }

                remaining_views = new_remaining_views;
                remaining_generators = new_remaining_generators;

                let pos = self.pos.get_cloned();
                let room_subscriptions = self.subscriptions.lock_ref().clone();
                let unsubscribe_rooms = {
                    let unsubs = self.unsubscribe.lock_ref().to_vec();
                    if !unsubs.is_empty() {
                        self.unsubscribe.lock_mut().clear();
                    }
                    unsubs
                };
                let req = assign!(v4::Request::new(), {
                    lists: requests,
                    pos,
                    room_subscriptions,
                    unsubscribe_rooms,
                    extensions: extensions.lock_mut().take().unwrap_or_default(), // extensions are sticky, we pop them here once
                });
                tracing::debug!("requesting");

                let req = client.send_with_homeserver(req, None, self.homeserver.as_ref().map(ToString::to_string));

                #[cfg(feature = "e2e-encryption")]
                let resp_res = {
                    let (e2ee_uploads, resp) = futures_util::join!(client.send_outgoing_requests(), req);
                    if let Err(e) = e2ee_uploads {
                        tracing::error!(error = ?e, "Error while sending outgoing E2EE requests");
                    }
                    resp
                };
                #[cfg(not(feature = "e2e-encryption"))]
                let resp_res = req.await;

                let resp = match resp_res {
                    Ok(r) => {
                        self.failure_count.store(0, Ordering::Relaxed);
                        r
                    },
                    Err(e) => {
                        if matches!(e, HttpError::Api(FromHttpResponseError::Server(RumaApiError::ClientApi(ruma::api::client::Error { body: ErrorBody::Standard { kind: ErrorKind::UnknownPos, .. }, .. })))) {
                            // session expired, let's reset
                            if self.failure_count.fetch_add(1, Ordering::Relaxed) >= 3 {
                                tracing::error!("session expired three times in a row");
                                yield Err(e.into());
                                break
                            }
                            tracing::warn!("Session expired. Restarting sliding sync.");
                            remaining_generators = views
                                .iter()
                                .map(SlidingSyncView::request_generator)
                                .collect();
                            *self.pos.lock_mut() = None;
                            *self.extensions.lock_mut() = initial_extensions.clone();
                            continue
                        }
                        yield Err(e.into());
                        continue
                    }
                };

                tracing::debug!("received");

                let updates =  match self.handle_response(resp, &remaining_views).await {
                    Ok(r) => r,
                    Err(e) => {
                        yield Err(e.into());
                        continue
                    }
                };
                tracing::debug!("handled");
                yield Ok(updates);
            }
        })
    }
}

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
/// let sliding_sync = client.sliding_sync().default_with_fullsync().build()?;
///
/// # })
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
    required_state: Vec<(RoomEventType, String)>,

    /// How many rooms request at a time when doing a full-sync catch up
    #[builder(default = "20")]
    batch_size: u32,

    /// How many rooms request at a time when doing a full-sync catch up
    #[builder(setter(into), default)]
    limit: Option<u32>,

    /// Any filters to apply to the query
    #[builder(default)]
    filters: Option<v4::SyncRequestListFilters>,

    /// The maximum number of timeline events to query for
    #[builder(setter(name = "timeline_limit_raw"), default)]
    timeline_limit: Option<UInt>,

    // ----- Public state
    /// Name of this view to easily recognise them
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
    /// The rooms details
    #[builder(private, default)]
    pub rooms: RoomsMap,

    /// The ranges windows of the view
    #[builder(setter(name = "ranges_raw"), default)]
    ranges: RangeState,

    /// Signaling updates on the roomlist after processing
    #[builder(private)]
    rooms_updated_signal: futures_signals::signal::Sender<()>,

    /// Get informed if anything in the room changed
    ///
    /// If you only care to know about changes once all of them have applied
    /// (including the total) listen to a clone of this signal.
    #[builder(private)]
    pub rooms_updated_broadcaster:
        futures_signals::signal::Broadcaster<futures_signals::signal::Receiver<()>>,
}

#[derive(Serialize, Deserialize)]
struct FrozenSlidingSyncView {
    rooms_count: Option<u32>,
    rooms_list: Vec<RoomListEntry>,
}

impl From<&SlidingSyncView> for FrozenSlidingSyncView {
    fn from(v: &SlidingSyncView) -> Self {
        FrozenSlidingSyncView {
            rooms_count: *v.rooms_count.lock_ref(),
            rooms_list: v.rooms_list.lock_ref().iter().map(|e| e.freeze()).collect(),
        }
    }
}

impl SlidingSyncView {
    fn set_from_cold(&mut self, v: FrozenSlidingSyncView) {
        let FrozenSlidingSyncView { rooms_count, rooms_list } = v;
        if self.sync_mode.lock_ref().is_full_sync() {
            self.state.set(SlidingSyncState::Preload);
        }
        self.rooms_count.replace(rooms_count);
        self.rooms_list.lock_mut().replace_cloned(rooms_list);
    }
}

// /// the default name for the full sync view
pub const FULL_SYNC_VIEW_NAME: &str = "full-sync";

impl SlidingSyncViewBuilder {
    /// Create a Builder set up for full sync
    pub fn default_with_fullsync() -> Self {
        Self::default().name(FULL_SYNC_VIEW_NAME).sync_mode(SlidingSyncMode::PagingFullSync)
    }

    /// Build the view
    pub fn build(mut self) -> Result<SlidingSyncView, SlidingSyncViewBuilderError> {
        let (sender, receiver) = futures_signals::signal::channel(());
        self.rooms_updated_signal = Some(sender);
        self.rooms_updated_broadcaster = Some(futures_signals::signal::Broadcaster::new(receiver));
        self.finish_build()
    }

    // defaults
    fn default_sort() -> Vec<String> {
        vec!["by_recency".to_owned(), "by_name".to_owned()]
    }

    fn default_required_state() -> Vec<(RoomEventType, String)> {
        vec![
            (RoomEventType::RoomEncryption, "".to_owned()),
            (RoomEventType::RoomTombstone, "".to_owned()),
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
        self.timeline_limit = Some(Some(timeline_limit.into()));
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
    GrowingFullSync { position: u32, batch_size: u32, limit: Option<u32> },
    PagingFullSync { position: u32, batch_size: u32, limit: Option<u32> },
    Live,
}

struct SlidingSyncViewRequestGenerator<'a> {
    view: &'a SlidingSyncView,
    inner: InnerSlidingSyncViewRequestGenerator,
}

impl<'a> SlidingSyncViewRequestGenerator<'a> {
    fn new_with_paging_syncup(view: &'a SlidingSyncView) -> Self {
        let batch_size = view.batch_size;
        let limit = view.limit;

        SlidingSyncViewRequestGenerator {
            view,
            inner: InnerSlidingSyncViewRequestGenerator::PagingFullSync {
                position: 0,
                batch_size,
                limit,
            },
        }
    }

    fn new_with_growing_syncup(view: &'a SlidingSyncView) -> Self {
        let batch_size = view.batch_size;
        let limit = view.limit;

        SlidingSyncViewRequestGenerator {
            view,
            inner: InnerSlidingSyncViewRequestGenerator::GrowingFullSync {
                position: 0,
                batch_size,
                limit,
            },
        }
    }

    fn new_live(view: &'a SlidingSyncView) -> Self {
        SlidingSyncViewRequestGenerator { view, inner: InnerSlidingSyncViewRequestGenerator::Live }
    }

    fn prefetch_request(
        &self,
        start: u32,
        batch_size: u32,
        limit: Option<u32>,
    ) -> (u32, v4::SyncRequestList) {
        let calc_end = start + batch_size;
        let end = match limit {
            Some(l) => std::cmp::min(l, calc_end),
            _ => calc_end,
        };
        let ranges = vec![(start.into(), end.into())];
        (end, self.make_request_for_ranges(ranges))
    }

    fn make_request_for_ranges(&self, ranges: Vec<(UInt, UInt)>) -> v4::SyncRequestList {
        let sort = self.view.sort.clone();
        let required_state = self.view.required_state.clone();
        let timeline_limit = self.view.timeline_limit;
        let filters = self.view.filters.clone();

        assign!(v4::SyncRequestList::default(), {
            ranges,
            required_state,
            sort,
            timeline_limit,
            filters,
        })
    }

    // generate the next live request
    fn live_request(&self) -> v4::SyncRequestList {
        let ranges = self.view.ranges.read_only().get_cloned();
        self.make_request_for_ranges(ranges)
    }
}

impl<'a> Iterator for SlidingSyncViewRequestGenerator<'a> {
    type Item = v4::SyncRequestList;

    fn next(&mut self) -> Option<Self::Item> {
        if let InnerSlidingSyncViewRequestGenerator::PagingFullSync { position, limit, .. }
        | InnerSlidingSyncViewRequestGenerator::GrowingFullSync { position, limit, .. } =
            self.inner
        {
            if let Some(count) = self.view.rooms_count.get_cloned() {
                let end = limit.unwrap_or(count);
                if end <= position {
                    // we are switching to live mode
                    self.view.state.set_if(SlidingSyncState::Live, |before, _now| {
                        matches!(before, SlidingSyncState::CatchingUp)
                    });
                    // keep listening to the entire list to learn about position updates
                    self.view.set_range(0, end);
                    self.inner = InnerSlidingSyncViewRequestGenerator::Live
                }
            } else {
                // upon first catch up request, we want to switch state
                self.view.state.set_if(SlidingSyncState::Preload, |before, _now| {
                    matches!(before, SlidingSyncState::Cold)
                });
            }
        }
        match self.inner {
            InnerSlidingSyncViewRequestGenerator::PagingFullSync {
                position,
                batch_size,
                limit,
            } => {
                let (end, req) = self.prefetch_request(position, batch_size, limit);
                self.inner = InnerSlidingSyncViewRequestGenerator::PagingFullSync {
                    position: end,
                    batch_size,
                    limit,
                };
                self.view.state.set_if(SlidingSyncState::CatchingUp, |before, _now| {
                    matches!(before, SlidingSyncState::Preload | SlidingSyncState::Cold)
                });
                Some(req)
            }
            InnerSlidingSyncViewRequestGenerator::GrowingFullSync {
                position,
                batch_size,
                limit,
            } => {
                let (end, req) = self.prefetch_request(0, position + batch_size, limit);
                self.inner = InnerSlidingSyncViewRequestGenerator::GrowingFullSync {
                    position: end,
                    batch_size,
                    limit,
                };
                self.view.state.set_if(SlidingSyncState::CatchingUp, |before, _now| {
                    matches!(before, SlidingSyncState::Preload | SlidingSyncState::Cold)
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

    /// Return the subset of rooms, starting at offset (default 0) returning
    /// count (or to the end) items
    pub fn get_rooms(
        &self,
        offset: Option<usize>,
        count: Option<usize>,
    ) -> Vec<v4::SlidingSyncRoom> {
        let start = offset.unwrap_or(0);
        let rooms = self.rooms.lock_ref();
        let listing = self.rooms_list.lock_ref();
        let count = count.unwrap_or(listing.len() - start);
        listing
            .iter()
            .skip(start)
            .filter_map(|id| id.as_room_id())
            .filter_map(|id| rooms.get(id))
            .map(|r| r.inner.clone())
            .take(count)
            .collect()
    }

    /// Return the room_id at the given index
    pub fn get_room_id(&self, index: usize) -> Option<OwnedRoomId> {
        self.rooms_list.lock_ref().get(index).and_then(|e| e.as_room_id().map(ToOwned::to_owned))
    }

    #[tracing::instrument(skip(self, ops))]
    fn room_ops(&self, ops: &Vec<v4::SyncOp>) -> Result<(), Error> {
        let mut rooms_list = self.rooms_list.lock_mut();
        let _rooms_map = self.rooms.lock_mut();
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
                        .map_err(|e| {
                            Error::BadResponse(format!("`range` not a valid int: {e:}"))
                        })?;
                    let room_ids = op.room_ids.clone();
                    room_ids
                        .into_iter()
                        .enumerate()
                        .map(|(i, r)| {
                            let idx = start as usize + i;
                            rooms_list.set_cloned(idx, RoomListEntry::Filled(r));
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
                        Error::BadResponse(
                            "`room_id` must be present for INSERT operation".to_owned(),
                        )
                    })?);
                    let mut dif = 0usize;
                    loop {
                        // find the next empty slot and drop it
                        let (prev_p, prev_overflow) = pos.overflowing_sub(dif);
                        let check_prev = !prev_overflow;
                        let (next_p, overflown) = pos.overflowing_add(dif);
                        let check_after = !overflown && next_p < sliced.len();
                        if !check_prev && !check_after {
                            return Err(Error::BadResponse("We were asked to insert but could not find any direction to shift to".to_owned()));
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

                    while pos < end {
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
                    tracing::warn!("Unknown operation occurred: {:?}", s);
                }
            }
        }

        Ok(())
    }

    #[tracing::instrument(skip(self, ops))]
    fn handle_response(&self, rooms_count: u32, ops: &Vec<v4::SyncOp>) -> Result<bool, Error> {
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
            self.rooms_count.replace(Some(rooms_count));
            changed = true;
        }

        if !ops.is_empty() {
            self.room_ops(ops)?;
            changed = true;
        }

        if changed {
            if let Err(e) = self.rooms_updated_signal.send(()) {
                tracing::warn!("Could not inform about rooms updated: {:?}", e);
            }
        }

        Ok(changed)
    }

    fn request_generator(&self) -> SlidingSyncViewRequestGenerator<'_> {
        match self.sync_mode.read_only().get_cloned() {
            SlidingSyncMode::PagingFullSync => {
                SlidingSyncViewRequestGenerator::new_with_paging_syncup(self)
            }
            SlidingSyncMode::GrowingFullSync => {
                SlidingSyncViewRequestGenerator::new_with_growing_syncup(self)
            }
            SlidingSyncMode::Selective => SlidingSyncViewRequestGenerator::new_live(self),
        }
    }
}

impl Client {
    /// Create a SlidingSyncBuilder tied to this client
    pub async fn sliding_sync(&self) -> SlidingSyncBuilder {
        SlidingSyncBuilder::default().client(self.clone())
    }

    #[tracing::instrument(skip(self, response))]
    pub(crate) async fn process_sliding_sync(
        &self,
        response: v4::Response,
    ) -> Result<SyncResponse> {
        let response = self.base_client().process_sliding_sync(response).await?;
        tracing::debug!("done processing on base_client");
        self.handle_sync_response(&response).await?;
        Ok(response)
    }
}
