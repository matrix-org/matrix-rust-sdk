use std::{fmt::Debug, sync::Arc, time::Duration};

use eyeball_im::VectorDiff;
use futures_util::{pin_mut, StreamExt, TryFutureExt};
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
use matrix_sdk_ui::{
    room_list_service::{
        filters::{
            new_filter_all, new_filter_any, new_filter_category, new_filter_favourite,
            new_filter_fuzzy_match_room_name, new_filter_invite, new_filter_joined,
            new_filter_non_left, new_filter_none, new_filter_normalized_match_room_name,
            new_filter_unread, RoomCategory,
        },
        BoxedFilterFn,
    },
    timeline::default_event_filter,
    unable_to_decrypt_hook::UtdHookManager,
};
use tokio::sync::RwLock;

use crate::{
    error::ClientError,
    room::Room,
    room_info::RoomInfo,
    timeline::{EventTimelineItem, Timeline},
    timeline_event_filter::TimelineEventTypeFilter,
    TaskHandle, RUNTIME,
};

#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum RoomListError {
    #[error("sliding sync error: {error}")]
    SlidingSync { error: String },
    #[error("unknown list `{list_name}`")]
    UnknownList { list_name: String },
    #[error("input cannot be applied")]
    InputCannotBeApplied,
    #[error("room `{room_name}` not found")]
    RoomNotFound { room_name: String },
    #[error("invalid room ID: {error}")]
    InvalidRoomId { error: String },
    #[error("A timeline instance already exists for room {room_name}")]
    TimelineAlreadyExists { room_name: String },
    #[error("A timeline instance hasn't been initialized for room {room_name}")]
    TimelineNotInitialized { room_name: String },
    #[error("Timeline couldn't be initialized: {error}")]
    InitializingTimeline { error: String },
    #[error("Event cache ran into an error: {error}")]
    EventCache { error: String },
}

impl From<matrix_sdk_ui::room_list_service::Error> for RoomListError {
    fn from(value: matrix_sdk_ui::room_list_service::Error) -> Self {
        use matrix_sdk_ui::room_list_service::Error::*;

        match value {
            SlidingSync(error) => Self::SlidingSync { error: error.to_string() },
            UnknownList(list_name) => Self::UnknownList { list_name },
            InputCannotBeApplied(_) => Self::InputCannotBeApplied,
            RoomNotFound(room_id) => Self::RoomNotFound { room_name: room_id.to_string() },
            TimelineAlreadyExists(room_id) => {
                Self::TimelineAlreadyExists { room_name: room_id.to_string() }
            }
            InitializingTimeline(source) => {
                Self::InitializingTimeline { error: source.to_string() }
            }
            EventCache(error) => Self::EventCache { error: error.to_string() },
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
    pub(crate) utd_hook: Option<Arc<UtdHookManager>>,
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

    async fn room(&self, room_id: String) -> Result<Arc<RoomListItem>, RoomListError> {
        let room_id = <&RoomId>::try_from(room_id.as_str()).map_err(RoomListError::from)?;

        Ok(Arc::new(RoomListItem {
            inner: Arc::new(self.inner.room(room_id).await?),
            utd_hook: self.utd_hook.clone(),
        }))
    }

    async fn all_rooms(self: Arc<Self>) -> Result<Arc<RoomList>, RoomListError> {
        Ok(Arc::new(RoomList {
            room_list_service: self.clone(),
            inner: Arc::new(self.inner.all_rooms().await.map_err(RoomListError::from)?),
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

#[uniffi::export(async_runtime = "tokio")]
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
            self.inner.entries_with_dynamic_adapters(
                page_size.try_into().unwrap(),
                self.room_list_service.inner.client().roominfo_update_receiver(),
            );

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

    async fn room(&self, room_id: String) -> Result<Arc<RoomListItem>, RoomListError> {
        self.room_list_service.room(room_id).await
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
        let FilterWrapper(filter) = FilterWrapper::from(&self.client, kind);
        self.inner.set_filter(filter)
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
    All { filters: Vec<RoomListEntriesDynamicFilterKind> },
    Any { filters: Vec<RoomListEntriesDynamicFilterKind> },
    NonLeft,
    Joined,
    Unread,
    Favourite,
    Invite,
    Category { expect: RoomListFilterCategory },
    None,
    NormalizedMatchRoomName { pattern: String },
    FuzzyMatchRoomName { pattern: String },
}

#[derive(uniffi::Enum)]
pub enum RoomListFilterCategory {
    Group,
    People,
}

impl From<RoomListFilterCategory> for RoomCategory {
    fn from(value: RoomListFilterCategory) -> Self {
        match value {
            RoomListFilterCategory::Group => Self::Group,
            RoomListFilterCategory::People => Self::People,
        }
    }
}

/// Custom internal type to transform a `RoomListEntriesDynamicFilterKind` into
/// a `BoxedFilterFn`.
struct FilterWrapper(BoxedFilterFn);

impl FilterWrapper {
    fn from(client: &matrix_sdk::Client, value: RoomListEntriesDynamicFilterKind) -> Self {
        use RoomListEntriesDynamicFilterKind as Kind;

        match value {
            Kind::All { filters } => Self(Box::new(new_filter_all(
                filters.into_iter().map(|filter| FilterWrapper::from(client, filter).0).collect(),
            ))),
            Kind::Any { filters } => Self(Box::new(new_filter_any(
                filters.into_iter().map(|filter| FilterWrapper::from(client, filter).0).collect(),
            ))),
            Kind::NonLeft => Self(Box::new(new_filter_non_left(client))),
            Kind::Joined => Self(Box::new(new_filter_joined(client))),
            Kind::Unread => Self(Box::new(new_filter_unread(client))),
            Kind::Favourite => Self(Box::new(new_filter_favourite(client))),
            Kind::Invite => Self(Box::new(new_filter_invite(client))),
            Kind::Category { expect } => Self(Box::new(new_filter_category(client, expect.into()))),
            Kind::None => Self(Box::new(new_filter_none())),
            Kind::NormalizedMatchRoomName { pattern } => {
                Self(Box::new(new_filter_normalized_match_room_name(client, &pattern)))
            }
            Kind::FuzzyMatchRoomName { pattern } => {
                Self(Box::new(new_filter_fuzzy_match_room_name(client, &pattern)))
            }
        }
    }
}

#[derive(uniffi::Object)]
pub struct RoomListItem {
    inner: Arc<matrix_sdk_ui::room_list_service::Room>,
    utd_hook: Option<Arc<UtdHookManager>>,
}

#[uniffi::export(async_runtime = "tokio")]
impl RoomListItem {
    fn id(&self) -> String {
        self.inner.id().to_string()
    }

    /// Returns the room's name from the state event if available, otherwise
    /// compute a room name based on the room's nature (DM or not) and number of
    /// members.
    fn display_name(&self) -> Option<String> {
        RUNTIME.block_on(self.inner.computed_display_name())
    }

    fn avatar_url(&self) -> Option<String> {
        self.inner.avatar_url().map(|uri| uri.to_string())
    }

    fn is_direct(&self) -> bool {
        RUNTIME.block_on(self.inner.inner_room().is_direct()).unwrap_or(false)
    }

    fn canonical_alias(&self) -> Option<String> {
        self.inner.inner_room().canonical_alias().map(|alias| alias.to_string())
    }

    pub async fn room_info(&self) -> Result<RoomInfo, ClientError> {
        let latest_event = self.inner.latest_event().await.map(EventTimelineItem).map(Arc::new);

        Ok(RoomInfo::new(self.inner.inner_room(), latest_event).await?)
    }

    /// Build a full `Room` FFI object, filling its associated timeline.
    ///
    /// If its internal timeline hasn't been initialized, it'll fail.
    fn full_room(&self) -> Result<Arc<Room>, RoomListError> {
        if let Some(timeline) = self.inner.timeline() {
            Ok(Arc::new(Room::with_timeline(
                self.inner.inner_room().clone(),
                Arc::new(RwLock::new(Some(Timeline::from_arc(timeline)))),
            )))
        } else {
            Err(RoomListError::TimelineNotInitialized {
                room_name: self.inner.inner_room().room_id().to_string(),
            })
        }
    }

    /// Checks whether the Room's timeline has been initialized before.
    fn is_timeline_initialized(&self) -> bool {
        self.inner.is_timeline_initialized()
    }

    /// Initializes the timeline for this room using the provided parameters.
    ///
    /// * `event_type_filter` - An optional [`TimelineEventTypeFilter`] to be
    ///   used to filter timeline events besides the default timeline filter. If
    ///   `None` is passed, only the default timeline filter will be used.
    /// * `internal_id_prefix` - An optional String that will be prepended to
    ///   all the timeline item's internal IDs, making it possible to
    ///   distinguish different timeline instances from each other.
    async fn init_timeline(
        &self,
        event_type_filter: Option<Arc<TimelineEventTypeFilter>>,
        internal_id_prefix: Option<String>,
    ) -> Result<(), RoomListError> {
        let mut timeline_builder = self
            .inner
            .default_room_timeline_builder()
            .await
            .map_err(|err| RoomListError::InitializingTimeline { error: err.to_string() })?;

        if let Some(event_type_filter) = event_type_filter {
            timeline_builder = timeline_builder.event_filter(move |event, room_version_id| {
                // Always perform the default filter first
                default_event_filter(event, room_version_id) && event_type_filter.filter(event)
            });
        }

        if let Some(internal_id_prefix) = internal_id_prefix {
            timeline_builder = timeline_builder.with_internal_id_prefix(internal_id_prefix);
        }

        if let Some(utd_hook) = self.utd_hook.clone() {
            timeline_builder = timeline_builder.with_unable_to_decrypt_hook(utd_hook);
        }

        self.inner.init_timeline_with_builder(timeline_builder).map_err(RoomListError::from).await
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
    pub include_heroes: Option<bool>,
}

impl From<RoomSubscription> for RumaRoomSubscription {
    fn from(val: RoomSubscription) -> Self {
        assign!(RumaRoomSubscription::default(), {
            required_state: val.required_state.map(|r|
                r.into_iter().map(|s| (s.key.into(), s.value)).collect()
            ).unwrap_or_default(),
            timeline_limit: val.timeline_limit.map(|u| u.into()),
            include_heroes: val.include_heroes,
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
