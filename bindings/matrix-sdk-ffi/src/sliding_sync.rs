use std::sync::{Arc, RwLock};

use anyhow::Context;
use eyeball_im::VectorDiff;
use futures_util::{future::join3, pin_mut, StreamExt};
use matrix_sdk::{
    ruma::{
        api::client::sync::sync_events::{
            v4::{RoomSubscription as RumaRoomSubscription, SyncRequestListFilters},
            UnreadNotificationsCount as RumaUnreadNotificationsCount,
        },
        assign, IdParseError, OwnedRoomId, RoomId,
    },
    sliding_sync::SlidingSyncSelectiveModeBuilder as MatrixSlidingSyncSelectiveModeBuilder,
    LoopCtrl, RoomListEntry as MatrixRoomEntry, SlidingSyncBuilder as MatrixSlidingSyncBuilder,
    SlidingSyncMode, SlidingSyncState,
};
use matrix_sdk_ui::timeline::SlidingSyncRoomExt;
use tracing::{error, warn};

use crate::{
    client::Client,
    error::ClientError,
    helpers::unwrap_or_clone_arc,
    room::{Room, TimelineLock},
    timeline::{EventTimelineItem, TimelineDiff, TimelineItem, TimelineListener},
    TaskHandle, RUNTIME,
};

#[derive(uniffi::Object)]
pub struct UnreadNotificationsCount {
    highlight_count: u32,
    notification_count: u32,
}

#[uniffi::export]
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

#[derive(uniffi::Error)]
pub enum SlidingSyncError {
    /// The response we've received from the server can't be parsed or doesn't
    /// match up with the current expectations on the client side. A
    /// `sync`-restart might be required.
    BadResponse { msg: String },
    /// Called `.build()` on a builder type, but the given required field was
    /// missing.
    BuildMissingField { msg: String },
    /// A `SlidingSyncListRequestGenerator` has been used without having been
    /// initialized. It happens when a response is handled before a request has
    /// been sent. It usually happens when testing.
    RequestGeneratorHasNotBeenInitialized { msg: String },
    /// Ranges have a `start` bound greater than `end`.
    InvalidRange {
        /// Start bound.
        start: u32,
        /// End bound.
        end: u32,
    },
    /// The SlidingSync internal channel is broken.
    InternalChannelIsBroken,
    /// Unknown or other error.
    Unknown { error: String },
}

impl From<matrix_sdk::sliding_sync::Error> for SlidingSyncError {
    fn from(value: matrix_sdk::sliding_sync::Error) -> Self {
        use matrix_sdk::sliding_sync::Error as E;

        match value {
            E::BadResponse(msg) => Self::BadResponse { msg },
            E::RequestGeneratorHasNotBeenInitialized(msg) => {
                Self::RequestGeneratorHasNotBeenInitialized { msg }
            }
            E::InvalidRange { start, end } => Self::InvalidRange { start, end },
            E::InternalChannelIsBroken => Self::InternalChannelIsBroken,
            error => Self::Unknown { error: error.to_string() },
        }
    }
}

#[derive(uniffi::Object)]
pub struct SlidingSyncRoom {
    inner: matrix_sdk::SlidingSyncRoom,
    sliding_sync: matrix_sdk::SlidingSync,
    timeline: TimelineLock,
    client: Client,
}

#[uniffi::export]
impl SlidingSyncRoom {
    pub fn name(&self) -> Option<String> {
        self.inner.name()
    }

    pub fn room_id(&self) -> String {
        self.inner.room_id().to_string()
    }

    pub fn is_dm(&self) -> Option<bool> {
        self.inner.is_dm()
    }

    pub fn is_initial(&self) -> Option<bool> {
        self.inner.is_initial_response()
    }

    pub fn has_unread_notifications(&self) -> bool {
        self.inner.has_unread_notifications()
    }

    pub fn unread_notifications(&self) -> Arc<UnreadNotificationsCount> {
        Arc::new(self.inner.unread_notifications().into())
    }

    pub fn full_room(&self) -> Option<Arc<Room>> {
        self.client
            .inner
            .get_room(self.inner.room_id())
            .map(|room| Arc::new(Room::with_timeline(room, self.timeline.clone())))
    }

    pub fn avatar_url(&self) -> Option<String> {
        Some(self.client.inner.get_room(self.inner.room_id())?.avatar_url()?.into())
    }

    #[allow(clippy::significant_drop_in_scrutinee)]
    pub fn latest_room_message(&self) -> Option<Arc<EventTimelineItem>> {
        let item = RUNTIME.block_on(self.inner.latest_event())?;
        Some(Arc::new(EventTimelineItem(item)))
    }

    pub fn add_timeline_listener(
        &self,
        listener: Box<dyn TimelineListener>,
    ) -> Result<SlidingSyncAddTimelineListenerResult, ClientError> {
        let (items, stoppable_spawn) = self.add_timeline_listener_inner(listener)?;

        Ok(SlidingSyncAddTimelineListenerResult { items, task_handle: Arc::new(stoppable_spawn) })
    }

    pub fn subscribe_to_room(&self, settings: Option<RoomSubscription>) {
        let room_id = self.inner.room_id().to_owned();

        self.sliding_sync.subscribe_to_room(room_id, settings.map(Into::into));
    }

    pub fn unsubscribe_from_room(&self) {
        let room_id = self.inner.room_id().to_owned();

        self.sliding_sync.unsubscribe_from_room(room_id);
    }
}

impl SlidingSyncRoom {
    fn add_timeline_listener_inner(
        &self,
        listener: Box<dyn TimelineListener>,
    ) -> anyhow::Result<(Vec<Arc<TimelineItem>>, TaskHandle)> {
        let mut timeline_lock = self.timeline.write().unwrap();
        let timeline = match &*timeline_lock {
            Some(timeline) => timeline,
            None => {
                let timeline = RUNTIME
                    .block_on(self.inner.timeline())
                    .context("Could not set timeline listener: room not found.")?;
                timeline_lock.insert(Arc::new(timeline))
            }
        };

        #[allow(unknown_lints, clippy::redundant_async_block)] // false positive
        let (timeline_items, timeline_stream) = RUNTIME.block_on(timeline.subscribe());

        let handle_events = async move {
            let listener: Arc<dyn TimelineListener> = listener.into();
            timeline_stream
                .for_each(move |diff| {
                    let listener = listener.clone();
                    let fut = RUNTIME.spawn_blocking(move || {
                        listener.on_update(Arc::new(TimelineDiff::new(diff)))
                    });

                    async move {
                        if let Err(e) = fut.await {
                            error!("Timeline listener error: {e}");
                        }
                    }
                })
                .await;
        };

        let handle_sliding_sync_reset = {
            let reset_broadcast_rx = self.client.sliding_sync_reset_broadcast_tx.subscribe();
            let timeline = timeline.to_owned();
            async move {
                reset_broadcast_rx.for_each(|_| timeline.clear()).await;
            }
        };

        // This in the future could be removed, and the rx handling could be moved
        // inside handle_sliding_sync_reset since we want to reset the sliding
        // sync for ignore user list events
        let handle_ignore_user_list_changes = {
            let ignore_user_list_change_rx =
                self.client.inner.subscribe_to_ignore_user_list_changes();
            let timeline = timeline.to_owned();
            async move {
                ignore_user_list_change_rx.for_each(|_| timeline.clear()).await;
            }
        };

        let items = timeline_items.into_iter().map(TimelineItem::from_arc).collect();
        let task_handle = TaskHandle::new(RUNTIME.spawn(async move {
            join3(handle_events, handle_sliding_sync_reset, handle_ignore_user_list_changes).await;
        }));

        Ok((items, task_handle))
    }
}

#[derive(uniffi::Record)]
pub struct SlidingSyncAddTimelineListenerResult {
    pub items: Vec<Arc<TimelineItem>>,
    pub task_handle: Arc<TaskHandle>,
}

#[derive(uniffi::Record)]
pub struct UpdateSummary {
    /// The lists (according to their name), which have seen an update
    pub lists: Vec<String>,
    pub rooms: Vec<String>,
}

#[derive(uniffi::Record)]
pub struct RequiredState {
    pub key: String,
    pub value: String,
}

#[derive(uniffi::Record)]
pub struct RoomSubscription {
    pub required_state: Option<Vec<RequiredState>>,
    pub timeline_limit: Option<u32>,
}

impl From<RoomSubscription> for RumaRoomSubscription {
    fn from(val: RoomSubscription) -> Self {
        assign!(RumaRoomSubscription::default(), {
            required_state: val.required_state.map(|r|
                r.into_iter().map(|s| (s.key.into(), s.value)).collect()
            ).unwrap_or_default(),
            timeline_limit: val.timeline_limit.map(|u| u.into())
        })
    }
}

impl From<matrix_sdk::UpdateSummary> for UpdateSummary {
    fn from(other: matrix_sdk::UpdateSummary) -> Self {
        Self {
            lists: other.lists,
            rooms: other.rooms.into_iter().map(|r| r.as_str().to_owned()).collect(),
        }
    }
}

#[derive(uniffi::Enum)]
pub enum SlidingSyncListRoomsListDiff {
    Append { values: Vec<RoomListEntry> },
    Insert { index: u32, value: RoomListEntry },
    Set { index: u32, value: RoomListEntry },
    Remove { index: u32 },
    PushBack { value: RoomListEntry },
    PushFront { value: RoomListEntry },
    PopBack,
    PopFront,
    Clear,
    Reset { values: Vec<RoomListEntry> },
}

impl From<VectorDiff<MatrixRoomEntry>> for SlidingSyncListRoomsListDiff {
    fn from(other: VectorDiff<MatrixRoomEntry>) -> Self {
        match other {
            VectorDiff::Append { values } => {
                Self::Append { values: values.into_iter().map(|e| (&e).into()).collect() }
            }
            VectorDiff::Insert { index, value } => {
                Self::Insert { index: index as u32, value: (&value).into() }
            }
            VectorDiff::Set { index, value } => {
                Self::Set { index: index as u32, value: (&value).into() }
            }
            VectorDiff::Remove { index } => Self::Remove { index: index as u32 },
            VectorDiff::PushBack { value } => Self::PushBack { value: (&value).into() },
            VectorDiff::PushFront { value } => Self::PushFront { value: (&value).into() },
            VectorDiff::PopBack => Self::PopBack,
            VectorDiff::PopFront => Self::PopFront,
            VectorDiff::Clear => Self::Clear,
            VectorDiff::Reset { values } => {
                warn!("Room list subscriber lagged behind and was reset");
                Self::Reset { values: values.into_iter().map(|e| (&e).into()).collect() }
            }
        }
    }
}

// Used by `SlidingSync` _and_ `RoomList`. Be careful.
#[derive(Clone, Debug, uniffi::Enum)]
pub enum RoomListEntry {
    Empty,
    Invalidated { room_id: String },
    Filled { room_id: String },
}

impl From<MatrixRoomEntry> for RoomListEntry {
    fn from(value: MatrixRoomEntry) -> Self {
        (&value).into()
    }
}

impl From<&MatrixRoomEntry> for RoomListEntry {
    fn from(value: &MatrixRoomEntry) -> Self {
        match value {
            MatrixRoomEntry::Empty => Self::Empty,
            MatrixRoomEntry::Filled(room_id) => Self::Filled { room_id: room_id.to_string() },
            MatrixRoomEntry::Invalidated(room_id) => {
                Self::Invalidated { room_id: room_id.to_string() }
            }
        }
    }
}

#[uniffi::export(callback_interface)]
pub trait SlidingSyncListRoomItemsObserver: Sync + Send {
    fn did_receive_update(&self);
}

#[uniffi::export(callback_interface)]
pub trait SlidingSyncListRoomListObserver: Sync + Send {
    fn did_receive_update(&self, diff: SlidingSyncListRoomsListDiff);
}

#[uniffi::export(callback_interface)]
pub trait SlidingSyncListRoomsCountObserver: Sync + Send {
    fn did_receive_update(&self, new_count: u32);
}

#[uniffi::export(callback_interface)]
pub trait SlidingSyncListStateObserver: Sync + Send {
    fn did_receive_update(&self, new_state: SlidingSyncState);
}

#[derive(Clone, uniffi::Object)]
pub struct SlidingSyncSelectiveModeBuilder {
    inner: MatrixSlidingSyncSelectiveModeBuilder,
}

#[uniffi::export]
impl SlidingSyncSelectiveModeBuilder {
    #[uniffi::constructor]
    pub fn new() -> Arc<Self> {
        Arc::new(Self { inner: SlidingSyncMode::new_selective() })
    }

    pub fn add_range(self: Arc<Self>, start: u32, end_inclusive: u32) -> Arc<Self> {
        let mut builder = unwrap_or_clone_arc(self);
        builder.inner = builder.inner.add_range(start..=end_inclusive);
        Arc::new(builder)
    }
}

#[derive(Clone, uniffi::Object)]
pub struct SlidingSyncListBuilder {
    inner: matrix_sdk::SlidingSyncListBuilder,
}

#[derive(uniffi::Record)]
pub struct SlidingSyncRequestListFilters {
    pub is_dm: Option<bool>,
    pub spaces: Vec<String>,
    pub is_encrypted: Option<bool>,
    pub is_invite: Option<bool>,
    pub is_tombstoned: Option<bool>,
    pub room_types: Vec<String>,
    pub not_room_types: Vec<String>,
    pub room_name_like: Option<String>,
    pub tags: Vec<String>,
    pub not_tags: Vec<String>,
    // pub extensions: BTreeMap<String, Value>,
}

impl From<SlidingSyncRequestListFilters> for SyncRequestListFilters {
    fn from(value: SlidingSyncRequestListFilters) -> Self {
        let SlidingSyncRequestListFilters {
            is_dm,
            spaces,
            is_encrypted,
            is_invite,
            is_tombstoned,
            room_types,
            not_room_types,
            room_name_like,
            tags,
            not_tags,
        } = value;

        assign!(SyncRequestListFilters::default(), {
            is_dm, spaces, is_encrypted, is_invite, is_tombstoned, room_types, not_room_types, room_name_like, tags, not_tags,
        })
    }
}

#[uniffi::export]
impl SlidingSyncListBuilder {
    #[uniffi::constructor]
    pub fn new(name: String) -> Arc<Self> {
        Arc::new(Self { inner: matrix_sdk::SlidingSyncList::builder(name) })
    }

    pub fn sync_mode_selective(
        self: Arc<Self>,
        selective_mode_builder: Arc<SlidingSyncSelectiveModeBuilder>,
    ) -> Arc<Self> {
        let mut builder = unwrap_or_clone_arc(self);
        let selective_mode_builder = unwrap_or_clone_arc(selective_mode_builder);
        builder.inner = builder.inner.sync_mode(selective_mode_builder.inner);
        Arc::new(builder)
    }

    pub fn sync_mode_paging(
        self: Arc<Self>,
        batch_size: u32,
        maximum_number_of_rooms_to_fetch: Option<u32>,
    ) -> Arc<Self> {
        let mut builder = unwrap_or_clone_arc(self);
        let mut mode_builder = SlidingSyncMode::new_paging(batch_size);
        if let Some(num) = maximum_number_of_rooms_to_fetch {
            mode_builder = mode_builder.maximum_number_of_rooms_to_fetch(num);
        }
        builder.inner = builder.inner.sync_mode(mode_builder);
        Arc::new(builder)
    }

    pub fn sync_mode_growing(
        self: Arc<Self>,
        batch_size: u32,
        maximum_number_of_rooms_to_fetch: Option<u32>,
    ) -> Arc<Self> {
        let mut builder = unwrap_or_clone_arc(self);
        let mut mode_builder = SlidingSyncMode::new_growing(batch_size);
        if let Some(num) = maximum_number_of_rooms_to_fetch {
            mode_builder = mode_builder.maximum_number_of_rooms_to_fetch(num);
        }
        builder.inner = builder.inner.sync_mode(mode_builder);
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

    pub fn filters(self: Arc<Self>, filters: SlidingSyncRequestListFilters) -> Arc<Self> {
        let mut builder = unwrap_or_clone_arc(self);
        builder.inner = builder.inner.filters(Some(filters.into()));
        Arc::new(builder)
    }

    pub fn no_filters(self: Arc<Self>) -> Arc<Self> {
        let mut builder = unwrap_or_clone_arc(self);
        builder.inner = builder.inner.filters(None);
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

    pub fn once_built(self: Arc<Self>, callback: Box<dyn SlidingSyncListOnceBuilt>) -> Arc<Self> {
        let mut builder = unwrap_or_clone_arc(self);

        builder.inner = builder.inner.once_built(
            move |list: matrix_sdk::SlidingSyncList| -> matrix_sdk::SlidingSyncList {
                let list = callback.update_list(Arc::new(list.into()));

                unwrap_or_clone_arc(list).inner
            },
        );
        Arc::new(builder)
    }

    pub fn bump_event_types(self: Arc<Self>, bump_event_types: Vec<String>) -> Arc<Self> {
        let mut builder = unwrap_or_clone_arc(self);
        builder.inner = builder.inner.bump_event_types(
            bump_event_types.into_iter().map(Into::into).collect::<Vec<_>>().as_slice(),
        );
        Arc::new(builder)
    }
}

#[uniffi::export(callback_interface)]
pub trait SlidingSyncListOnceBuilt: Sync + Send {
    fn update_list(&self, list: Arc<SlidingSyncList>) -> Arc<SlidingSyncList>;
}

#[derive(Clone, uniffi::Object)]
pub struct SlidingSyncList {
    inner: matrix_sdk::SlidingSyncList,
}

impl From<matrix_sdk::SlidingSyncList> for SlidingSyncList {
    fn from(inner: matrix_sdk::SlidingSyncList) -> Self {
        SlidingSyncList { inner }
    }
}

#[uniffi::export]
impl SlidingSyncList {
    pub fn observe_state(
        &self,
        observer: Box<dyn SlidingSyncListStateObserver>,
    ) -> Arc<TaskHandle> {
        let mut state_stream = self.inner.state_stream();

        Arc::new(TaskHandle::new(RUNTIME.spawn(async move {
            loop {
                if let Some(new_state) = state_stream.next().await {
                    observer.did_receive_update(new_state);
                }
            }
        })))
    }

    pub fn observe_room_list(
        &self,
        observer: Box<dyn SlidingSyncListRoomListObserver>,
    ) -> Arc<TaskHandle> {
        let (_, mut room_list_stream) = self.inner.room_list_stream();

        Arc::new(TaskHandle::new(RUNTIME.spawn(async move {
            loop {
                if let Some(diff) = room_list_stream.next().await {
                    observer.did_receive_update(diff.into());
                }
            }
        })))
    }

    pub fn observe_rooms_count(
        &self,
        observer: Box<dyn SlidingSyncListRoomsCountObserver>,
    ) -> Arc<TaskHandle> {
        let mut rooms_count_stream = self.inner.maximum_number_of_rooms_stream();

        Arc::new(TaskHandle::new(RUNTIME.spawn(async move {
            loop {
                if let Some(Some(new)) = rooms_count_stream.next().await {
                    observer.did_receive_update(new);
                }
            }
        })))
    }

    /// Get the current list of rooms
    pub fn current_room_list(&self) -> Vec<RoomListEntry> {
        self.inner.room_list()
    }

    /// Total of rooms matching the filter
    pub fn current_room_count(&self) -> Option<u32> {
        self.inner.maximum_number_of_rooms()
    }

    /// The current timeline limit
    pub fn get_timeline_limit(&self) -> Option<u32> {
        self.inner.timeline_limit()
    }

    /// The current timeline limit
    pub fn set_timeline_limit(&self, value: u32) {
        self.inner.set_timeline_limit(Some(value))
    }

    /// Unset the current timeline limit
    pub fn unset_timeline_limit(&self) {
        self.inner.set_timeline_limit(None)
    }

    /// Changes the sync mode, and automatically restarts the sliding sync
    /// internally.
    pub fn set_sync_mode(&self, builder: Arc<SlidingSyncSelectiveModeBuilder>) {
        let builder = unwrap_or_clone_arc(builder);
        self.inner.set_sync_mode(builder.inner);
    }
}

#[uniffi::export(callback_interface)]
pub trait SlidingSyncObserver: Sync + Send {
    fn did_receive_sync_update(&self, summary: UpdateSummary);
}

#[derive(uniffi::Object)]
pub struct SlidingSync {
    inner: matrix_sdk::SlidingSync,
    client: Client,
    observer: Arc<RwLock<Option<Box<dyn SlidingSyncObserver>>>>,
}

impl SlidingSync {
    fn new(inner: matrix_sdk::SlidingSync, client: Client) -> Self {
        Self { inner, client, observer: Default::default() }
    }
}

#[uniffi::export]
impl SlidingSync {
    pub fn set_observer(&self, observer: Option<Box<dyn SlidingSyncObserver>>) {
        *self.observer.write().unwrap() = observer;
    }

    pub fn subscribe_to_room(
        &self,
        room_id: String,
        settings: Option<RoomSubscription>,
    ) -> Result<(), ClientError> {
        let room_id = room_id.try_into()?;

        self.inner.subscribe_to_room(room_id, settings.map(Into::into));

        Ok(())
    }

    pub fn unsubscribe_from_room(&self, room_id: String) -> Result<(), ClientError> {
        let room_id = room_id.try_into()?;

        self.inner.unsubscribe_from_room(room_id);

        Ok(())
    }

    pub fn get_room(&self, room_id: String) -> Result<Option<Arc<SlidingSyncRoom>>, ClientError> {
        let room_id = <&RoomId>::try_from(room_id.as_str())?;

        Ok(RUNTIME.block_on(async move {
            self.inner.get_room(room_id).await.map(|inner| {
                Arc::new(SlidingSyncRoom {
                    inner,
                    sliding_sync: self.inner.clone(),
                    client: self.client.clone(),
                    timeline: Default::default(),
                })
            })
        }))
    }

    pub fn get_rooms(
        &self,
        room_ids: Vec<String>,
    ) -> Result<Vec<Option<Arc<SlidingSyncRoom>>>, ClientError> {
        let actual_ids = room_ids
            .into_iter()
            .map(OwnedRoomId::try_from)
            .collect::<Result<Vec<OwnedRoomId>, IdParseError>>()?;

        Ok(RUNTIME.block_on(async move {
            self.inner
                .get_rooms(actual_ids.into_iter())
                .await
                .into_iter()
                .map(|o| {
                    o.map(|inner| {
                        Arc::new(SlidingSyncRoom {
                            inner,
                            sliding_sync: self.inner.clone(),
                            client: self.client.clone(),
                            timeline: Default::default(),
                        })
                    })
                })
                .collect()
        }))
    }

    pub fn add_list(&self, list_builder: Arc<SlidingSyncListBuilder>) -> Arc<TaskHandle> {
        let this = self.inner.clone();

        Arc::new(TaskHandle::new(RUNTIME.spawn(async move {
            this.add_list(unwrap_or_clone_arc(list_builder).inner).await.unwrap();
        })))
    }

    pub fn add_cached_list(
        &self,
        list_builder: Arc<SlidingSyncListBuilder>,
    ) -> Result<Option<Arc<SlidingSyncList>>, ClientError> {
        RUNTIME.block_on(async move {
            Ok(self
                .inner
                .add_cached_list(list_builder.inner.clone())
                .await?
                .map(|inner| Arc::new(SlidingSyncList { inner })))
        })
    }

    pub fn sync(&self) -> Arc<TaskHandle> {
        let inner = self.inner.clone();
        let client = self.client.clone();
        let observer = self.observer.clone();

        Arc::new(TaskHandle::new(RUNTIME.spawn(async move {
            let stream = inner.sync();
            pin_mut!(stream);

            loop {
                let update_summary = match stream.next().await {
                    Some(Ok(update_summary)) => update_summary,

                    Some(Err(error)) => {
                        if client.process_sync_error(error) == LoopCtrl::Break {
                            warn!("loop was stopped by client error processing");
                            break;
                        } else {
                            continue;
                        }
                    }

                    None => {
                        warn!("SlidingSync sync-loop ended");
                        break;
                    }
                };

                if let Some(ref observer) = *observer.read().unwrap() {
                    observer.did_receive_sync_update(update_summary.into());
                }
            }
        })))
    }

    pub fn stop_sync(&self) -> Result<(), ClientError> {
        self.inner.stop_sync().map_err(Into::into)
    }
}

#[derive(Clone, uniffi::Object)]
pub struct SlidingSyncBuilder {
    inner: MatrixSlidingSyncBuilder,
    client: Client,
}

#[uniffi::export]
impl SlidingSyncBuilder {
    pub fn sliding_sync_proxy(self: Arc<Self>, url: String) -> Result<Arc<Self>, ClientError> {
        let mut builder = unwrap_or_clone_arc(self);
        builder.inner = builder.inner.sliding_sync_proxy(url.parse()?);
        Ok(Arc::new(builder))
    }

    pub fn enable_caching(self: Arc<Self>) -> Result<Arc<Self>, ClientError> {
        let mut builder = unwrap_or_clone_arc(self);
        builder.inner = builder.inner.enable_caching()?;
        Ok(Arc::new(builder))
    }

    pub fn add_list(self: Arc<Self>, list_builder: Arc<SlidingSyncListBuilder>) -> Arc<Self> {
        let mut builder = unwrap_or_clone_arc(self);
        builder.inner = builder.inner.add_list(unwrap_or_clone_arc(list_builder).inner);
        Arc::new(builder)
    }

    pub fn add_cached_list(
        self: Arc<Self>,
        list_builder: Arc<SlidingSyncListBuilder>,
    ) -> Result<Arc<Self>, ClientError> {
        let mut builder = unwrap_or_clone_arc(self);
        let list_builder = unwrap_or_clone_arc(list_builder);
        builder.inner = RUNTIME
            .block_on(async move { builder.inner.add_cached_list(list_builder.inner).await })?;
        Ok(Arc::new(builder))
    }

    pub fn with_common_extensions(self: Arc<Self>) -> Arc<Self> {
        let mut builder = unwrap_or_clone_arc(self);
        builder.inner = builder.inner.with_common_extensions();
        Arc::new(builder)
    }

    pub fn without_e2ee_extension(self: Arc<Self>) -> Arc<Self> {
        let mut builder = unwrap_or_clone_arc(self);
        builder.inner = builder.inner.without_e2ee_extension();
        Arc::new(builder)
    }

    pub fn without_to_device_extension(self: Arc<Self>) -> Arc<Self> {
        let mut builder = unwrap_or_clone_arc(self);
        builder.inner = builder.inner.without_to_device_extension();
        Arc::new(builder)
    }

    pub fn without_account_data_extension(self: Arc<Self>) -> Arc<Self> {
        let mut builder = unwrap_or_clone_arc(self);
        builder.inner = builder.inner.without_account_data_extension();
        Arc::new(builder)
    }

    pub fn without_receipt_extension(self: Arc<Self>) -> Arc<Self> {
        let mut builder = unwrap_or_clone_arc(self);
        builder.inner = builder.inner.without_receipt_extension();
        Arc::new(builder)
    }

    pub fn without_typing_extension(self: Arc<Self>) -> Arc<Self> {
        let mut builder = unwrap_or_clone_arc(self);
        builder.inner = builder.inner.without_typing_extension();
        Arc::new(builder)
    }

    pub fn with_all_extensions(self: Arc<Self>) -> Arc<Self> {
        let mut builder = unwrap_or_clone_arc(self);
        builder.inner = builder.inner.with_all_extensions();
        Arc::new(builder)
    }

    pub fn build(self: Arc<Self>) -> Result<Arc<SlidingSync>, ClientError> {
        let builder = unwrap_or_clone_arc(self);
        RUNTIME.block_on(async move {
            Ok(Arc::new(SlidingSync::new(builder.inner.build().await?, builder.client)))
        })
    }
}

#[uniffi::export]
impl Client {
    /// Creates a new Sliding Sync instance with the given identifier.
    ///
    /// Note: the identifier must be less than 16 chars long.
    pub fn sliding_sync(&self, id: String) -> Result<Arc<SlidingSyncBuilder>, ClientError> {
        let inner = self.inner.sliding_sync(id)?;
        Ok(Arc::new(SlidingSyncBuilder { inner, client: self.clone() }))
    }
}
