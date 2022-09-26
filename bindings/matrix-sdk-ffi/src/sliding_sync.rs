use std::sync::{Arc, RwLock};

use futures_signals::{
    signal::SignalExt,
    signal_vec::{SignalVecExt, VecDiff},
};
use futures_util::{pin_mut, StreamExt};
use matrix_sdk::ruma::{
    api::client::sync::sync_events::{
        v4::RoomSubscription as RumaRoomSubscription,
        UnreadNotificationsCount as RumaUnreadNotificationsCount,
    },
    assign, IdParseError, OwnedRoomId,
};
pub use matrix_sdk::{
    Client as MatrixClient, RoomListEntry as MatrixRoomEntry,
    SlidingSyncBuilder as MatrixSlidingSyncBuilder, SlidingSyncMode, SlidingSyncState,
};
use tokio::task::JoinHandle;

use super::{Client, RUNTIME};
use crate::helpers::unwrap_or_clone_arc;

pub struct StoppableSpawn {
    handle: Arc<RwLock<Option<JoinHandle<()>>>>,
}

impl StoppableSpawn {
    fn with_handle(handle: JoinHandle<()>) -> StoppableSpawn {
        StoppableSpawn { handle: Arc::new(RwLock::new(Some(handle))) }
    }

    fn with_handle_ref(handle: Arc<RwLock<Option<JoinHandle<()>>>>) -> StoppableSpawn {
        StoppableSpawn { handle }
    }

    pub fn cancel(&self) {
        if let Some(handle) = self.handle.write().unwrap().take() {
            handle.abort();
        }
    }
    pub fn is_cancelled(&self) -> bool {
        self.handle.read().unwrap().is_none()
    }
}

pub struct UnreadNotificationsCount {
    highlight_count: u32,
    notification_count: u32,
}

impl UnreadNotificationsCount {
    pub fn highlight_count(&self) -> u32 {
        self.highlight_count
    }
    pub fn notification_count(&self) -> u32 {
        self.notification_count
    }
    pub fn has_notifications(&self) -> bool {
        self.notification_count != 0 || self.highlight_count != 0
    }
}

impl From<RumaUnreadNotificationsCount> for UnreadNotificationsCount {
    fn from(inner: RumaUnreadNotificationsCount) -> Self {
        UnreadNotificationsCount {
            highlight_count: inner
                .highlight_count
                .and_then(|x| x.try_into().ok())
                .unwrap_or_default(),
            notification_count: inner
                .notification_count
                .and_then(|x| x.try_into().ok())
                .unwrap_or_default(),
        }
    }
}

pub struct SlidingSyncRoom {
    inner: matrix_sdk::SlidingSyncRoom,
    client: Client,
}

impl SlidingSyncRoom {
    pub fn name(&self) -> Option<String> {
        self.inner.name().map(ToOwned::to_owned)
    }

    pub fn room_id(&self) -> String {
        self.inner.room_id().to_string()
    }

    pub fn is_dm(&self) -> Option<bool> {
        self.inner.is_dm
    }

    pub fn is_initial(&self) -> Option<bool> {
        self.inner.initial
    }
    pub fn is_loading_more(&self) -> bool {
        self.inner.is_loading_more()
    }

    pub fn has_unread_notifications(&self) -> bool {
        !self.inner.unread_notifications.is_empty()
    }

    pub fn unread_notifications(&self) -> Arc<UnreadNotificationsCount> {
        Arc::new(self.inner.unread_notifications.clone().into())
    }

    #[allow(clippy::significant_drop_in_scrutinee)]
    pub fn latest_room_message(&self) -> Option<Arc<crate::messages::AnyMessage>> {
        let messages = self.inner.timeline();
        // room is having the latest events at the end,
        let lock = messages.lock_ref();
        for m in lock.iter().rev() {
            if let Some(e) = crate::messages::sync_event_to_message(m.clone().into()) {
                return Some(e);
            }
        }
        None
    }

    pub fn full_room(&self) -> Option<Arc<super::Room>> {
        self.client.get_room(self.inner.room_id()).map(|room| Arc::new(super::Room::new(room)))
    }
}

pub struct UpdateSummary {
    /// The views (according to their name), which have seen an update
    pub views: Vec<String>,
    pub rooms: Vec<String>,
}

pub struct RequiredState {
    pub key: String,
    pub value: String,
}

pub struct RoomSubscription {
    pub required_state: Option<Vec<RequiredState>>,
    pub timeline_limit: Option<u32>,
}

impl TryInto<RumaRoomSubscription> for RoomSubscription {
    type Error = anyhow::Error;
    fn try_into(self) -> anyhow::Result<RumaRoomSubscription> {
        Ok(assign!(RumaRoomSubscription::default(), {
            required_state: self.required_state.map(|r|
                r.into_iter().map(|s| (s.key.into(), s.value)).collect()
            ).unwrap_or_default(),
            timeline_limit: self.timeline_limit.map(|u| u.into())
        }))
    }
}

impl From<matrix_sdk::UpdateSummary> for UpdateSummary {
    fn from(other: matrix_sdk::UpdateSummary) -> UpdateSummary {
        UpdateSummary {
            views: other.views,
            rooms: other.rooms.into_iter().map(|r| r.as_str().to_owned()).collect(),
        }
    }
}

pub enum SlidingSyncViewRoomsListDiff {
    Replace { values: Vec<RoomListEntry> },
    InsertAt { index: u32, value: RoomListEntry },
    UpdateAt { index: u32, value: RoomListEntry },
    RemoveAt { index: u32 },
    Move { old_index: u32, new_index: u32 },
    Push { value: RoomListEntry },
}

impl From<VecDiff<MatrixRoomEntry>> for SlidingSyncViewRoomsListDiff {
    fn from(other: VecDiff<MatrixRoomEntry>) -> Self {
        match other {
            VecDiff::Replace { values } => SlidingSyncViewRoomsListDiff::Replace {
                values: values.into_iter().map(|e| (&e).into()).collect(),
            },
            VecDiff::InsertAt { index, value } => SlidingSyncViewRoomsListDiff::InsertAt {
                index: index as u32,
                value: (&value).into(),
            },
            VecDiff::UpdateAt { index, value } => SlidingSyncViewRoomsListDiff::UpdateAt {
                index: index as u32,
                value: (&value).into(),
            },
            VecDiff::RemoveAt { index } => {
                SlidingSyncViewRoomsListDiff::RemoveAt { index: index as u32 }
            }
            VecDiff::Move { old_index, new_index } => SlidingSyncViewRoomsListDiff::Move {
                old_index: old_index as u32,
                new_index: new_index as u32,
            },
            VecDiff::Push { value } => {
                SlidingSyncViewRoomsListDiff::Push { value: (&value).into() }
            }
            _ => unimplemented!("Clear and Pop aren't provided within sliding sync"),
        }
    }
}

#[derive(Clone, Debug)]
pub enum RoomListEntry {
    Empty,
    Invalidated { room_id: String },
    Filled { room_id: String },
}

impl From<&MatrixRoomEntry> for RoomListEntry {
    fn from(other: &MatrixRoomEntry) -> Self {
        match other {
            MatrixRoomEntry::Empty => RoomListEntry::Empty,
            MatrixRoomEntry::Filled(b) => RoomListEntry::Filled { room_id: b.to_string() },
            MatrixRoomEntry::Invalidated(b) => {
                RoomListEntry::Invalidated { room_id: b.to_string() }
            }
        }
    }
}
pub trait SlidingSyncViewRoomItemsObserver: Sync + Send {
    fn did_receive_update(&self);
}

pub trait SlidingSyncViewRoomListObserver: Sync + Send {
    fn did_receive_update(&self, diff: SlidingSyncViewRoomsListDiff);
}

pub trait SlidingSyncViewRoomsCountObserver: Sync + Send {
    fn did_receive_update(&self, new_count: u32);
}

pub trait SlidingSyncViewStateObserver: Sync + Send {
    fn did_receive_update(&self, new_state: SlidingSyncState);
}
#[derive(Clone, Default)]
pub struct SlidingSyncViewBuilder {
    inner: matrix_sdk::SlidingSyncViewBuilder,
}

impl SlidingSyncViewBuilder {
    pub fn new() -> Self {
        Default::default()
    }

    pub fn sync_mode(self: Arc<Self>, mode: SlidingSyncMode) -> Arc<Self> {
        let mut builder = unwrap_or_clone_arc(self);
        builder.inner = builder.inner.sync_mode(mode);
        Arc::new(builder)
    }

    pub fn sort(self: Arc<Self>, sort: Vec<String>) -> Arc<Self> {
        let mut builder = unwrap_or_clone_arc(self);
        builder.inner = builder.inner.sort(sort);
        Arc::new(builder)
    }

    pub fn required_state(self: Arc<Self>, required_state: Vec<RequiredState>) -> Arc<Self> {
        let mut builder = unwrap_or_clone_arc(self);
        builder.inner = builder
            .inner
            .required_state(required_state.into_iter().map(|s| (s.key.into(), s.value)).collect());
        Arc::new(builder)
    }

    pub fn batch_size(self: Arc<Self>, batch_size: u32) -> Arc<Self> {
        let mut builder = unwrap_or_clone_arc(self);
        builder.inner = builder.inner.batch_size(batch_size);
        Arc::new(builder)
    }

    pub fn timeline_limit(self: Arc<Self>, limit: u32) -> Arc<Self> {
        let mut builder = unwrap_or_clone_arc(self);
        builder.inner = builder.inner.timeline_limit(limit);
        Arc::new(builder)
    }

    pub fn no_timeline_limit(self: Arc<Self>) -> Arc<Self> {
        let mut builder = unwrap_or_clone_arc(self);
        builder.inner = builder.inner.no_timeline_limit();
        Arc::new(builder)
    }

    pub fn name(self: Arc<Self>, name: String) -> Arc<Self> {
        let mut builder = unwrap_or_clone_arc(self);
        builder.inner = builder.inner.name(name);
        Arc::new(builder)
    }

    pub fn ranges(self: Arc<Self>, ranges: Vec<(u32, u32)>) -> Arc<Self> {
        let mut builder = unwrap_or_clone_arc(self);
        builder.inner = builder.inner.ranges(ranges);
        Arc::new(builder)
    }

    pub fn add_range(self: Arc<Self>, from: u32, to: u32) -> Arc<Self> {
        let mut builder = unwrap_or_clone_arc(self);
        builder.inner = builder.inner.add_range(from, to);
        Arc::new(builder)
    }

    pub fn reset_ranges(self: Arc<Self>) -> Arc<Self> {
        let mut builder = unwrap_or_clone_arc(self);
        builder.inner = builder.inner.reset_ranges();
        Arc::new(builder)
    }

    pub fn build(self: Arc<Self>) -> anyhow::Result<Arc<SlidingSyncView>> {
        let builder = unwrap_or_clone_arc(self);
        Ok(Arc::new(builder.inner.build()?.into()))
    }
}

#[derive(Clone)]
pub struct SlidingSyncView {
    inner: matrix_sdk::SlidingSyncView,
}

impl From<matrix_sdk::SlidingSyncView> for SlidingSyncView {
    fn from(inner: matrix_sdk::SlidingSyncView) -> Self {
        SlidingSyncView { inner }
    }
}

impl SlidingSyncView {
    pub fn observe_state(
        &self,
        observer: Box<dyn SlidingSyncViewStateObserver>,
    ) -> Arc<StoppableSpawn> {
        let mut signal = self.inner.state.signal_cloned().to_stream();
        Arc::new(StoppableSpawn::with_handle(RUNTIME.spawn(async move {
            loop {
                if let Some(new_state) = signal.next().await {
                    observer.did_receive_update(new_state);
                }
            }
        })))
    }

    pub fn observe_room_list(
        &self,
        observer: Box<dyn SlidingSyncViewRoomListObserver>,
    ) -> Arc<StoppableSpawn> {
        let mut room_list = self.inner.rooms_list.signal_vec_cloned().to_stream();
        Arc::new(StoppableSpawn::with_handle(RUNTIME.spawn(async move {
            loop {
                if let Some(diff) = room_list.next().await {
                    observer.did_receive_update(diff.into());
                }
            }
        })))
    }

    pub fn observe_room_items(
        &self,
        observer: Box<dyn SlidingSyncViewRoomItemsObserver>,
    ) -> Arc<StoppableSpawn> {
        let mut rooms_updated = self.inner.rooms_updated_broadcaster.signal_cloned().to_stream();
        Arc::new(StoppableSpawn::with_handle(RUNTIME.spawn(async move {
            loop {
                if rooms_updated.next().await.is_some() {
                    observer.did_receive_update();
                }
            }
        })))
    }

    pub fn observe_rooms_count(
        &self,
        observer: Box<dyn SlidingSyncViewRoomsCountObserver>,
    ) -> Arc<StoppableSpawn> {
        let mut rooms_count = self.inner.rooms_count.signal_cloned().to_stream();
        Arc::new(StoppableSpawn::with_handle(RUNTIME.spawn(async move {
            loop {
                if let Some(Some(new)) = rooms_count.next().await {
                    observer.did_receive_update(new);
                }
            }
        })))
    }

    /// Reset the ranges to a particular set
    ///
    /// Remember to cancel the existing stream and fetch a new one as this will
    /// only be applied on the next request.
    pub fn set_range(&self, start: u32, end: u32) {
        self.inner.set_range(start, end);
    }

    /// Set the ranges to fetch
    ///
    /// Remember to cancel the existing stream and fetch a new one as this will
    /// only be applied on the next request.
    pub fn add_range(&self, start: u32, end: u32) {
        self.inner.add_range(start, end);
    }

    pub fn reset_ranges(&self) {
        self.inner.reset_ranges();
    }

    pub fn current_room_count(&self) -> Option<u32> {
        self.inner.rooms_count.get_cloned()
    }

    pub fn current_rooms_list(&self) -> Vec<RoomListEntry> {
        self.inner.rooms_list.lock_ref().as_slice().iter().map(|e| e.into()).collect()
    }
}

pub trait SlidingSyncObserver: Sync + Send {
    fn did_receive_sync_update(&self, summary: UpdateSummary);
}

pub struct SlidingSync {
    inner: matrix_sdk::SlidingSync,
    client: Client,
    observer: Arc<RwLock<Option<Box<dyn SlidingSyncObserver>>>>,
    sync_handle: Arc<RwLock<Option<JoinHandle<()>>>>,
}

impl SlidingSync {
    fn new(inner: matrix_sdk::SlidingSync, client: Client) -> Self {
        SlidingSync { inner, client, observer: Default::default(), sync_handle: Default::default() }
    }

    pub fn set_observer(&self, observer: Option<Box<dyn SlidingSyncObserver>>) {
        *self.observer.write().unwrap() = observer;
    }

    pub fn subscribe(
        &self,
        room_id: String,
        settings: Option<RoomSubscription>,
    ) -> anyhow::Result<()> {
        let settings =
            if let Some(settings) = settings { Some(settings.try_into()?) } else { None };
        self.inner.subscribe(room_id.try_into()?, settings);
        Ok(())
    }

    pub fn unsubscribe(&self, room_id: String) -> anyhow::Result<()> {
        self.inner.unsubscribe(room_id.try_into()?);
        Ok(())
    }

    #[allow(clippy::significant_drop_in_scrutinee)]
    pub fn get_view(&self, name: String) -> Option<Arc<SlidingSyncView>> {
        let views = self.inner.views.lock_ref();
        for s in views.iter() {
            if s.name == name {
                return Some(Arc::new(SlidingSyncView { inner: s.clone() }));
            }
        }
        None
    }

    pub fn get_room(&self, room_id: String) -> anyhow::Result<Option<Arc<SlidingSyncRoom>>> {
        Ok(self
            .inner
            .get_room(OwnedRoomId::try_from(room_id)?)
            .map(|inner| Arc::new(SlidingSyncRoom { inner, client: self.client.clone() })))
    }

    pub fn get_rooms(
        &self,
        room_ids: Vec<String>,
    ) -> anyhow::Result<Vec<Option<Arc<SlidingSyncRoom>>>> {
        let actual_ids = room_ids
            .into_iter()
            .map(OwnedRoomId::try_from)
            .collect::<Result<Vec<OwnedRoomId>, IdParseError>>()?;
        Ok(self
            .inner
            .get_rooms(actual_ids.into_iter())
            .into_iter()
            .map(|o| {
                o.map(|inner| Arc::new(SlidingSyncRoom { inner, client: self.client.clone() }))
            })
            .collect())
    }

    pub fn sync(&self) -> Arc<StoppableSpawn> {
        let inner = self.inner.clone();
        let observer = self.observer.clone();
        let spawn = Arc::new(StoppableSpawn::with_handle_ref(self.sync_handle.clone()));
        let inner_spawn = spawn.clone();
        {
            let mut sync_handle = self.sync_handle.write().unwrap();

            if let Some(handle) = sync_handle.take() {
                handle.abort();
            }

            *sync_handle = Some(RUNTIME.spawn(async move {
                let stream = inner.stream().await.unwrap();
                pin_mut!(stream);
                loop {
                    let update = match stream.next().await {
                        Some(Ok(u)) => u,
                        Some(Err(e)) => {
                            // FIXME: send this over the FFI
                            tracing::warn!("Sliding Sync failure: {:?}", e);
                            continue;
                        }
                        None => {
                            tracing::debug!("No update from loop, cancelled");
                            break;
                        }
                    };
                    if let Some(ref observer) = *observer.read().unwrap() {
                        observer.did_receive_sync_update(update.into());
                    } else {
                        // when the observer has been removed
                        // we cancel the loop
                        inner_spawn.cancel();
                        break;
                    }
                }
            }));
        }

        spawn
    }
}

#[derive(Clone)]
pub struct SlidingSyncBuilder {
    inner: MatrixSlidingSyncBuilder,
    client: Client,
}

impl SlidingSyncBuilder {
    pub fn homeserver(self: Arc<Self>, url: String) -> anyhow::Result<Arc<Self>> {
        let mut builder = unwrap_or_clone_arc(self);
        builder.inner = builder.inner.homeserver(url.parse()?);
        Ok(Arc::new(builder))
    }

    pub fn add_fullsync_view(self: Arc<Self>) -> Arc<Self> {
        let mut builder = unwrap_or_clone_arc(self);
        builder.inner = builder.inner.add_fullsync_view();
        Arc::new(builder)
    }

    pub fn no_views(self: Arc<Self>) -> Arc<Self> {
        let mut builder = unwrap_or_clone_arc(self);
        builder.inner = builder.inner.no_views();
        Arc::new(builder)
    }

    pub fn add_view(self: Arc<Self>, v: Arc<SlidingSyncView>) -> Arc<Self> {
        let mut builder = unwrap_or_clone_arc(self);
        let view = unwrap_or_clone_arc(v);
        builder.inner = builder.inner.add_view(view.inner);
        Arc::new(builder)
    }

    pub fn build(self: Arc<Self>) -> anyhow::Result<Arc<SlidingSync>> {
        let builder = unwrap_or_clone_arc(self);
        Ok(Arc::new(SlidingSync::new(builder.inner.build()?, builder.client)))
    }
}

impl Client {
    pub fn full_sliding_sync(&self) -> anyhow::Result<Arc<SlidingSync>> {
        RUNTIME.block_on(async move {
            let builder = self.client.sliding_sync().await;
            let inner = builder.add_fullsync_view().build()?;
            Ok(Arc::new(SlidingSync::new(inner, self.clone())))
        })
    }

    pub fn sliding_sync(&self) -> Arc<SlidingSyncBuilder> {
        RUNTIME.block_on(async move {
            let inner = self.client.sliding_sync().await;
            Arc::new(SlidingSyncBuilder { inner, client: self.clone() })
        })
    }
}
