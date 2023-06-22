use std::{
    fmt::Debug,
    sync::{Arc, RwLock},
};

use eyeball_im::VectorDiff;
use futures_util::{pin_mut, StreamExt};
use ruma::RoomId;

use crate::{
    client::Client,
    room::Room,
    sliding_sync::{RoomListEntry, RoomSubscription, UnreadNotificationsCount},
    timeline::EventTimelineItem,
    TaskHandle, RUNTIME,
};
#[uniffi::export]
impl Client {
    /// Get a new `RoomList` instance.
    pub fn room_list(&self) -> Result<Arc<RoomListService>, RoomListError> {
        Ok(Arc::new(RoomListService {
            inner: Arc::new(
                RUNTIME
                    .block_on(async {
                        matrix_sdk_ui::RoomListService::new(self.inner.clone()).await
                    })
                    .map_err(RoomListError::from)?,
            ),
        }))
    }
}

#[derive(uniffi::Error)]
pub enum RoomListError {
    SlidingSync { error: String },
    UnknownList { list_name: String },
    InputHasNotBeenApplied,
    RoomNotFound { room_name: String },
    InvalidRoomId { error: String },
}

impl From<matrix_sdk_ui::room_list::Error> for RoomListError {
    fn from(value: matrix_sdk_ui::room_list::Error) -> Self {
        use matrix_sdk_ui::room_list::Error::*;

        match value {
            SlidingSync(error) => Self::SlidingSync { error: error.to_string() },
            UnknownList(list_name) => Self::UnknownList { list_name },
            InputHasNotBeenApplied(_) => Self::InputHasNotBeenApplied,
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

impl From<RoomListInput> for matrix_sdk_ui::room_list::Input {
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
    inner: Arc<matrix_sdk_ui::RoomListService>,
}

#[uniffi::export]
impl RoomListService {
    fn sync(&self) {
        let this = self.inner.clone();

        RUNTIME.spawn(async move {
            let sync_stream = this.sync();
            pin_mut!(sync_stream);

            while sync_stream.next().await.is_some() {
                // keep going!
            }
        });
    }

    fn stop_sync(&self) -> Result<(), RoomListError> {
        self.inner.stop_sync().map_err(Into::into)
    }

    fn is_syncing(&self) -> bool {
        use matrix_sdk_ui::room_list::State;

        matches!(self.inner.state().get(), State::SettingUp | State::Running)
    }

    fn state(&self, listener: Box<dyn RoomListServiceStateListener>) -> Arc<TaskHandle> {
        let state_stream = self.inner.state();

        Arc::new(TaskHandle::new(RUNTIME.spawn(async move {
            pin_mut!(state_stream);

            while let Some(state) = state_stream.next().await {
                listener.on_update(state.into());
            }
        })))
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
        self.inner.apply_input(input.into()).await.map_err(Into::into)
    }

    fn room(&self, room_id: String) -> Result<Arc<RoomListItem>, RoomListError> {
        let room_id = <&RoomId>::try_from(room_id.as_str()).map_err(RoomListError::from)?;

        Ok(Arc::new(RoomListItem {
            inner: Arc::new(RUNTIME.block_on(async { self.inner.room(room_id).await })?),
        }))
    }
}

#[derive(uniffi::Object)]
pub struct RoomList {
    room_list_service: Arc<RoomListService>,
    inner: Arc<matrix_sdk_ui::room_list::RoomList>,
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
                    listener.on_update(diff.into());
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

impl From<matrix_sdk_ui::room_list::State> for RoomListServiceState {
    fn from(value: matrix_sdk_ui::room_list::State) -> Self {
        use matrix_sdk_ui::room_list::State::*;

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

impl From<matrix_sdk_ui::room_list::RoomListLoadingState> for RoomListLoadingState {
    fn from(value: matrix_sdk_ui::room_list::RoomListLoadingState) -> Self {
        use matrix_sdk_ui::room_list::RoomListLoadingState::*;

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
    fn on_update(&self, room_entries_update: RoomListEntriesUpdate);
}

#[derive(uniffi::Object)]
pub struct RoomListItem {
    inner: Arc<matrix_sdk_ui::room_list::Room>,
}

#[uniffi::export]
impl RoomListItem {
    fn id(&self) -> String {
        self.inner.id().to_string()
    }

    fn name(&self) -> Option<String> {
        RUNTIME.block_on(async { self.inner.name().await })
    }

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

    fn latest_event(&self) -> Option<Arc<EventTimelineItem>> {
        RUNTIME.block_on(async {
            self.inner.latest_event().await.map(EventTimelineItem).map(Arc::new)
        })
    }

    fn has_unread_notifications(&self) -> bool {
        self.inner.has_unread_notifications()
    }

    fn unread_notifications(&self) -> Arc<UnreadNotificationsCount> {
        Arc::new(self.inner.unread_notifications().into())
    }
}
