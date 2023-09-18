use std::{fmt::Debug, sync::Arc, time::Duration};

use eyeball_im::VectorDiff;
use futures_util::{pin_mut, StreamExt};
use matrix_sdk::{
    ruma::{
        api::client::sync::sync_events::{
            v4::RoomSubscription as RumaRoomSubscription,
            UnreadNotificationsCount as RumaUnreadNotificationsCount,
        },
        assign, RoomId,
    },
    RoomListEntry as MatrixRoomListEntry,
};
use matrix_sdk_ui::room_list_service::filters::{
    new_filter_all, new_filter_fuzzy_match_room_name, new_filter_normalized_match_room_name,
};
use tokio::sync::RwLock;

use crate::{
    error::ClientError, room::Room, room_info::RoomInfo, timeline::EventTimelineItem, TaskHandle,
    RUNTIME,
};

#[derive(uniffi::Error)]
pub enum RoomListError {
    SlidingSync { error: String },
    UnknownList { list_name: String },
    InputCannotBeApplied,
    RoomNotFound { room_name: String },
    InvalidRoomId { error: String },
}

impl From<matrix_sdk_ui::room_list_service::Error> for RoomListError {
    fn from(value: matrix_sdk_ui::room_list_service::Error) -> Self {
        use matrix_sdk_ui::room_list_service::Error::*;

        match value {
            SlidingSync(error) => Self::SlidingSync { error: error.to_string() },
            UnknownList(list_name) => Self::UnknownList { list_name },
            InputCannotBeApplied(_) => Self::InputCannotBeApplied,
            RoomNotFound(room_id) => Self::RoomNotFound { room_name: room_id.to_string() },
        }
    }
}

impl From<ruma::IdParseError> for RoomListError {
    fn from(value: ruma::IdParseError) -> Self {
        Self::InvalidRoomId { error: value.to_string() }
    }
}

#[derive(uniffi::Record)]
pub struct RoomListRange {
    pub start: u32,
    pub end_inclusive: u32,
}

#[derive(uniffi::Enum)]
pub enum RoomListInput {
    Viewport { ranges: Vec<RoomListRange> },
}

impl From<RoomListInput> for matrix_sdk_ui::room_list_service::Input {
    fn from(value: RoomListInput) -> Self {
        match value {
            RoomListInput::Viewport { ranges } => Self::Viewport(
                ranges.iter().map(|range| range.start..=range.end_inclusive).collect(),
            ),
        }
    }
}

#[derive(uniffi::Object)]
pub struct RoomListService {
    pub(crate) inner: Arc<matrix_sdk_ui::RoomListService>,
}

#[uniffi::export(async_runtime = "tokio")]
impl RoomListService {
    fn state(&self, listener: Box<dyn RoomListServiceStateListener>) -> Arc<TaskHandle> {
        let state_stream = self.inner.state();

        Arc::new(TaskHandle::new(RUNTIME.spawn(async move {
            pin_mut!(state_stream);

            while let Some(state) = state_stream.next().await {
                listener.on_update(state.into());
            }
        })))
    }

    fn room(&self, room_id: String) -> Result<Arc<RoomListItem>, RoomListError> {
        let room_id = <&RoomId>::try_from(room_id.as_str()).map_err(RoomListError::from)?;

        Ok(Arc::new(RoomListItem {
            inner: Arc::new(RUNTIME.block_on(async { self.inner.room(room_id).await })?),
        }))
    }

    async fn all_rooms(self: Arc<Self>) -> Result<Arc<RoomList>, RoomListError> {
        Ok(Arc::new(RoomList {
            room_list_service: self.clone(),
            inner: Arc::new(self.inner.all_rooms().await.map_err(RoomListError::from)?),
        }))
    }

    async fn invites(self: Arc<Self>) -> Result<Arc<RoomList>, RoomListError> {
        Ok(Arc::new(RoomList {
            room_list_service: self.clone(),
            inner: Arc::new(self.inner.invites().await.map_err(RoomListError::from)?),
        }))
    }

    async fn apply_input(&self, input: RoomListInput) -> Result<(), RoomListError> {
        self.inner.apply_input(input.into()).await.map(|_| ()).map_err(Into::into)
    }

    fn sync_indicator(
        &self,
        delay_before_showing_in_ms: u32,
        delay_before_hiding_in_ms: u32,
        listener: Box<dyn RoomListServiceSyncIndicatorListener>,
    ) -> Arc<TaskHandle> {
        let sync_indicator_stream = self.inner.sync_indicator(
            Duration::from_millis(delay_before_showing_in_ms.into()),
            Duration::from_millis(delay_before_hiding_in_ms.into()),
        );

        Arc::new(TaskHandle::new(RUNTIME.spawn(async move {
            pin_mut!(sync_indicator_stream);

            while let Some(sync_indicator) = sync_indicator_stream.next().await {
                listener.on_update(sync_indicator.into());
            }
        })))
    }
}

#[derive(uniffi::Object)]
pub struct RoomList {
    room_list_service: Arc<RoomListService>,
    inner: Arc<matrix_sdk_ui::room_list_service::RoomList>,
}

#[uniffi::export]
impl RoomList {
    fn loading_state(
        &self,
        listener: Box<dyn RoomListLoadingStateListener>,
    ) -> Result<RoomListLoadingStateResult, RoomListError> {
        let loading_state = self.inner.loading_state();

        Ok(RoomListLoadingStateResult {
            state: loading_state.get().into(),
            state_stream: Arc::new(TaskHandle::new(RUNTIME.spawn(async move {
                pin_mut!(loading_state);

                while let Some(loading_state) = loading_state.next().await {
                    listener.on_update(loading_state.into());
                }
            }))),
        })
    }

    fn entries(&self, listener: Box<dyn RoomListEntriesListener>) -> RoomListEntriesResult {
        let (entries, entries_stream) = self.inner.entries();

        RoomListEntriesResult {
            entries: entries.into_iter().map(Into::into).collect(),
            entries_stream: Arc::new(TaskHandle::new(RUNTIME.spawn(async move {
                pin_mut!(entries_stream);

                while let Some(diff) = entries_stream.next().await {
                    listener.on_update(diff.into_iter().map(Into::into).collect());
                }
            }))),
        }
    }

    fn entries_with_dynamic_adapters(
        &self,
        page_size: u32,
        listener: Box<dyn RoomListEntriesListener>,
    ) -> RoomListEntriesWithDynamicAdaptersResult {
        let (entries_stream, dynamic_entries_controller) =
            self.inner.entries_with_dynamic_adapters(page_size.try_into().unwrap());

        RoomListEntriesWithDynamicAdaptersResult {
            controller: Arc::new(RoomListDynamicEntriesController::new(
                dynamic_entries_controller,
                self.room_list_service.inner.client(),
            )),
            entries_stream: Arc::new(TaskHandle::new(RUNTIME.spawn(async move {
                pin_mut!(entries_stream);

                while let Some(diff) = entries_stream.next().await {
                    listener.on_update(diff.into_iter().map(Into::into).collect());
                }
            }))),
        }
    }

    fn room(&self, room_id: String) -> Result<Arc<RoomListItem>, RoomListError> {
        self.room_list_service.room(room_id)
    }
}

#[derive(uniffi::Record)]
pub struct RoomListEntriesResult {
    pub entries: Vec<RoomListEntry>,
    pub entries_stream: Arc<TaskHandle>,
}

#[derive(uniffi::Record)]
pub struct RoomListEntriesWithDynamicAdaptersResult {
    pub controller: Arc<RoomListDynamicEntriesController>,
    pub entries_stream: Arc<TaskHandle>,
}

#[derive(uniffi::Record)]
pub struct RoomListLoadingStateResult {
    pub state: RoomListLoadingState,
    pub state_stream: Arc<TaskHandle>,
}

#[derive(uniffi::Enum)]
pub enum RoomListServiceState {
    // Name it `Initial` instead of `Init`, otherwise it creates a keyword conflict in Swift
    // as of 2023-08-21.
    Initial,
    SettingUp,
    Recovering,
    Running,
    Error,
    Terminated,
}

impl From<matrix_sdk_ui::room_list_service::State> for RoomListServiceState {
    fn from(value: matrix_sdk_ui::room_list_service::State) -> Self {
        use matrix_sdk_ui::room_list_service::State as S;

        match value {
            S::Init => Self::Initial,
            S::SettingUp => Self::SettingUp,
            S::Recovering => Self::Recovering,
            S::Running => Self::Running,
            S::Error { .. } => Self::Error,
            S::Terminated { .. } => Self::Terminated,
        }
    }
}

#[derive(uniffi::Enum)]
pub enum RoomListServiceSyncIndicator {
    Show,
    Hide,
}

impl From<matrix_sdk_ui::room_list_service::SyncIndicator> for RoomListServiceSyncIndicator {
    fn from(value: matrix_sdk_ui::room_list_service::SyncIndicator) -> Self {
        use matrix_sdk_ui::room_list_service::SyncIndicator as SI;

        match value {
            SI::Show => Self::Show,
            SI::Hide => Self::Hide,
        }
    }
}

#[derive(uniffi::Enum)]
pub enum RoomListLoadingState {
    NotLoaded,
    Loaded { maximum_number_of_rooms: Option<u32> },
}

impl From<matrix_sdk_ui::room_list_service::RoomListLoadingState> for RoomListLoadingState {
    fn from(value: matrix_sdk_ui::room_list_service::RoomListLoadingState) -> Self {
        use matrix_sdk_ui::room_list_service::RoomListLoadingState as LS;

        match value {
            LS::NotLoaded => Self::NotLoaded,
            LS::Loaded { maximum_number_of_rooms } => Self::Loaded { maximum_number_of_rooms },
        }
    }
}

#[uniffi::export(callback_interface)]
pub trait RoomListServiceStateListener: Send + Sync + Debug {
    fn on_update(&self, state: RoomListServiceState);
}

#[uniffi::export(callback_interface)]
pub trait RoomListLoadingStateListener: Send + Sync + Debug {
    fn on_update(&self, state: RoomListLoadingState);
}

#[uniffi::export(callback_interface)]
pub trait RoomListServiceSyncIndicatorListener: Send + Sync + Debug {
    fn on_update(&self, sync_indicator: RoomListServiceSyncIndicator);
}

#[derive(uniffi::Enum)]
pub enum RoomListEntriesUpdate {
    Append { values: Vec<RoomListEntry> },
    Clear,
    PushFront { value: RoomListEntry },
    PushBack { value: RoomListEntry },
    PopFront,
    PopBack,
    Insert { index: u32, value: RoomListEntry },
    Set { index: u32, value: RoomListEntry },
    Remove { index: u32 },
    Truncate { length: u32 },
    Reset { values: Vec<RoomListEntry> },
}

impl From<VectorDiff<matrix_sdk::RoomListEntry>> for RoomListEntriesUpdate {
    fn from(other: VectorDiff<matrix_sdk::RoomListEntry>) -> Self {
        match other {
            VectorDiff::Append { values } => {
                Self::Append { values: values.into_iter().map(Into::into).collect() }
            }
            VectorDiff::Clear => Self::Clear,
            VectorDiff::PushFront { value } => Self::PushFront { value: value.into() },
            VectorDiff::PushBack { value } => Self::PushBack { value: value.into() },
            VectorDiff::PopFront => Self::PopFront,
            VectorDiff::PopBack => Self::PopBack,
            VectorDiff::Insert { index, value } => {
                Self::Insert { index: u32::try_from(index).unwrap(), value: value.into() }
            }
            VectorDiff::Set { index, value } => {
                Self::Set { index: u32::try_from(index).unwrap(), value: value.into() }
            }
            VectorDiff::Remove { index } => Self::Remove { index: u32::try_from(index).unwrap() },
            VectorDiff::Truncate { length } => {
                Self::Truncate { length: u32::try_from(length).unwrap() }
            }
            VectorDiff::Reset { values } => {
                Self::Reset { values: values.into_iter().map(Into::into).collect() }
            }
        }
    }
}

#[uniffi::export(callback_interface)]
pub trait RoomListEntriesListener: Send + Sync + Debug {
    fn on_update(&self, room_entries_update: Vec<RoomListEntriesUpdate>);
}

#[derive(uniffi::Object)]
pub struct RoomListDynamicEntriesController {
    inner: matrix_sdk_ui::room_list_service::RoomListDynamicEntriesController,
    client: matrix_sdk::Client,
}

impl RoomListDynamicEntriesController {
    fn new(
        dynamic_entries_controller: matrix_sdk_ui::room_list_service::RoomListDynamicEntriesController,
        client: &matrix_sdk::Client,
    ) -> Self {
        Self { inner: dynamic_entries_controller, client: client.clone() }
    }
}

#[uniffi::export]
impl RoomListDynamicEntriesController {
    fn set_filter(&self, kind: RoomListEntriesDynamicFilterKind) -> bool {
        use RoomListEntriesDynamicFilterKind as Kind;

        match kind {
            Kind::All => self.inner.set_filter(new_filter_all()),
            Kind::NormalizedMatchRoomName { pattern } => {
                self.inner.set_filter(new_filter_normalized_match_room_name(&self.client, &pattern))
            }
            Kind::FuzzyMatchRoomName { pattern } => {
                self.inner.set_filter(new_filter_fuzzy_match_room_name(&self.client, &pattern))
            }
        }
    }

    fn add_one_page(&self) {
        self.inner.add_one_page();
    }

    fn reset_to_one_page(&self) {
        self.inner.reset_to_one_page();
    }
}

#[derive(uniffi::Enum)]
pub enum RoomListEntriesDynamicFilterKind {
    All,
    NormalizedMatchRoomName { pattern: String },
    FuzzyMatchRoomName { pattern: String },
}

#[derive(uniffi::Object)]
pub struct RoomListItem {
    inner: Arc<matrix_sdk_ui::room_list_service::Room>,
}

#[uniffi::export(async_runtime = "tokio")]
impl RoomListItem {
    fn id(&self) -> String {
        self.inner.id().to_string()
    }

    fn name(&self) -> Option<String> {
        RUNTIME.block_on(async { self.inner.name().await })
    }

    fn avatar_url(&self) -> Option<String> {
        self.inner.avatar_url().map(|uri| uri.to_string())
    }

    fn is_direct(&self) -> bool {
        RUNTIME.block_on(async { self.inner.inner_room().is_direct().await.unwrap_or(false) })
    }

    fn canonical_alias(&self) -> Option<String> {
        self.inner.inner_room().canonical_alias().map(|alias| alias.to_string())
    }

    pub async fn room_info(&self) -> Result<RoomInfo, ClientError> {
        let avatar_url = self.inner.avatar_url();
        let latest_event = self.inner.latest_event().await.map(EventTimelineItem).map(Arc::new);
        Ok(RoomInfo::new(self.inner.inner_room(), avatar_url, latest_event).await?)
    }

    // Temporary workaround for coroutine leaks on Kotlin
    pub fn room_info_blocking(self: Arc<Self>) -> Result<RoomInfo, ClientError> {
        RUNTIME.block_on(async move { self.room_info().await })
    }

    /// Building a `Room`.
    ///
    /// Be careful that building a `Room` builds its entire `Timeline` at the
    /// same time.
    fn full_room(&self) -> Arc<Room> {
        Arc::new(Room::with_timeline(
            self.inner.inner_room().clone(),
            Arc::new(RwLock::new(Some(RUNTIME.block_on(async { self.inner.timeline().await })))),
        ))
    }

    fn subscribe(&self, settings: Option<RoomSubscription>) {
        self.inner.subscribe(settings.map(Into::into));
    }

    fn unsubscribe(&self) {
        self.inner.unsubscribe();
    }

    async fn latest_event(&self) -> Option<Arc<EventTimelineItem>> {
        self.inner.latest_event().await.map(EventTimelineItem).map(Arc::new)
    }

    fn has_unread_notifications(&self) -> bool {
        self.inner.has_unread_notifications()
    }

    fn unread_notifications(&self) -> Arc<UnreadNotificationsCount> {
        Arc::new(self.inner.unread_notifications().into())
    }
}

#[derive(Clone, Debug, uniffi::Enum)]
pub enum RoomListEntry {
    Empty,
    Invalidated { room_id: String },
    Filled { room_id: String },
}

impl From<MatrixRoomListEntry> for RoomListEntry {
    fn from(value: MatrixRoomListEntry) -> Self {
        (&value).into()
    }
}

impl From<&MatrixRoomListEntry> for RoomListEntry {
    fn from(value: &MatrixRoomListEntry) -> Self {
        match value {
            MatrixRoomListEntry::Empty => Self::Empty,
            MatrixRoomListEntry::Filled(room_id) => Self::Filled { room_id: room_id.to_string() },
            MatrixRoomListEntry::Invalidated(room_id) => {
                Self::Invalidated { room_id: room_id.to_string() }
            }
        }
    }
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

#[derive(uniffi::Object)]
pub struct UnreadNotificationsCount {
    highlight_count: u32,
    notification_count: u32,
}

#[uniffi::export]
impl UnreadNotificationsCount {
    fn highlight_count(&self) -> u32 {
        self.highlight_count
    }

    fn notification_count(&self) -> u32 {
        self.notification_count
    }

    fn has_notifications(&self) -> bool {
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
