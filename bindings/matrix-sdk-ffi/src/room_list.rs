use std::{fmt::Debug, sync::Arc};

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
use tokio::sync::RwLock;

use crate::{room::Room, timeline::EventTimelineItem, TaskHandle, RUNTIME};

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
        RUNTIME
            .spawn(async move {
                Ok(Arc::new(RoomList {
                    room_list_service: self.clone(),
                    inner: Arc::new(self.inner.all_rooms().await.map_err(RoomListError::from)?),
                }))
            })
            .await
            .unwrap()
    }

    async fn invites(self: Arc<Self>) -> Result<Arc<RoomList>, RoomListError> {
        RUNTIME
            .spawn(async move {
                Ok(Arc::new(RoomList {
                    room_list_service: self.clone(),
                    inner: Arc::new(self.inner.invites().await.map_err(RoomListError::from)?),
                }))
            })
            .await
            .unwrap()
    }

    async fn apply_input(self: Arc<Self>, input: RoomListInput) -> Result<(), RoomListError> {
        RUNTIME
            .spawn(async move {
                self.inner.apply_input(input.into()).await.map(|_| ()).map_err(Into::into)
            })
            .await
            .unwrap()
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

    fn entries(
        &self,
        listener: Box<dyn RoomListEntriesListener>,
    ) -> Result<RoomListEntriesResult, RoomListError> {
        let (entries, entries_stream) = self.inner.entries();

        Ok(RoomListEntriesResult {
            entries: entries.into_iter().map(Into::into).collect(),
            entries_stream: Arc::new(TaskHandle::new(RUNTIME.spawn(async move {
                pin_mut!(entries_stream);

                while let Some(diff) = entries_stream.next().await {
                    listener.on_update(diff.into_iter().map(Into::into).collect());
                }
            }))),
        })
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
pub struct RoomListLoadingStateResult {
    pub state: RoomListLoadingState,
    pub state_stream: Arc<TaskHandle>,
}

#[derive(uniffi::Enum)]
pub enum RoomListServiceState {
    Init,
    SettingUp,
    Running,
    Error,
    Terminated,
}

impl From<matrix_sdk_ui::room_list_service::State> for RoomListServiceState {
    fn from(value: matrix_sdk_ui::room_list_service::State) -> Self {
        use matrix_sdk_ui::room_list_service::State::*;

        match value {
            Init => Self::Init,
            SettingUp => Self::SettingUp,
            Running => Self::Running,
            Error { .. } => Self::Error,
            Terminated { .. } => Self::Terminated,
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
        use matrix_sdk_ui::room_list_service::RoomListLoadingState::*;

        match value {
            NotLoaded => Self::NotLoaded,
            Loaded { maximum_number_of_rooms } => Self::Loaded { maximum_number_of_rooms },
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

    pub fn is_direct(&self) -> bool {
        RUNTIME.block_on(async { self.inner.inner_room().is_direct().await.unwrap_or(false) })
    }

    pub fn avatar_url(&self) -> Option<String> {
        self.inner.inner_room().avatar_url().map(|uri| uri.to_string())
    }

    pub fn canonical_alias(&self) -> Option<String> {
        self.inner.inner_room().canonical_alias().map(|alias| alias.to_string())
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
}

#[uniffi::export]
impl RoomListItem {
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
