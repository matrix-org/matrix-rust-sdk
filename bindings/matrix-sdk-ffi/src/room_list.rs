use std::{
    fmt::Debug,
    sync::{Arc, RwLock},
};

use eyeball_im::VectorDiff;
use futures_util::{pin_mut, StreamExt};
use matrix_sdk::sliding_sync::SlidingSyncListLoadingState;
use ruma::RoomId;

use crate::{
    client::Client,
    room::Room,
    sliding_sync::{
        RoomListEntry, RoomSubscription, SlidingSyncListStateObserver, UnreadNotificationsCount,
    },
    timeline::EventTimelineItem,
    TaskHandle, RUNTIME,
};
#[uniffi::export]
impl Client {
    /// Get a new `RoomList` instance.
    pub fn room_list(&self) -> Result<Arc<RoomList>, RoomListError> {
        Ok(Arc::new(RoomList {
            inner: Arc::new(
                RUNTIME
                    .block_on(async { matrix_sdk_ui::RoomList::new(self.inner.clone()).await })
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
pub struct RoomList {
    inner: Arc<matrix_sdk_ui::RoomList>,
}

#[uniffi::export]
impl RoomList {
    fn sync(&self) -> Arc<TaskHandle> {
        let this = self.inner.clone();

        Arc::new(TaskHandle::new(RUNTIME.spawn(async move {
            let sync_stream = this.sync();
            pin_mut!(sync_stream);

            while sync_stream.next().await.is_some() {
                // keep going!
            }
        })))
    }

    fn state(&self, listener: Box<dyn RoomListStateListener>) -> Arc<TaskHandle> {
        let state_stream = self.inner.state();

        Arc::new(TaskHandle::new(RUNTIME.spawn(async move {
            pin_mut!(state_stream);

            while let Some(state) = state_stream.next().await {
                listener.on_update(state.into());
            }
        })))
    }

    async fn entries(
        &self,
        listener: Box<dyn RoomListEntriesListener>,
    ) -> Result<RoomListEntriesResult, RoomListError> {
        let (entries, entries_stream) = self.inner.entries().await.map_err(RoomListError::from)?;

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

    async fn entries_loading_state(
        &self,
        listener: Box<dyn SlidingSyncListStateObserver>,
    ) -> Result<RoomListEntriesLoadingStateResult, RoomListError> {
        let (entries_loading_state, entries_loading_state_stream) =
            self.inner.entries_loading_state().await.map_err(RoomListError::from)?;

        Ok(RoomListEntriesLoadingStateResult {
            entries_loading_state,
            entries_loading_state_stream: Arc::new(TaskHandle::new(RUNTIME.spawn(async move {
                pin_mut!(entries_loading_state_stream);

                while let Some(loading_state) = entries_loading_state_stream.next().await {
                    listener.did_receive_update(loading_state.into());
                }
            }))),
        })
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

#[derive(uniffi::Record)]
pub struct RoomListEntriesResult {
    pub entries: Vec<RoomListEntry>,
    pub entries_stream: Arc<TaskHandle>,
}

#[derive(uniffi::Record)]
pub struct RoomListEntriesLoadingStateResult {
    pub entries_loading_state: SlidingSyncListLoadingState,
    pub entries_loading_state_stream: Arc<TaskHandle>,
}

#[derive(uniffi::Enum)]
pub enum RoomListState {
    Init,
    FirstRooms,
    AllRooms,
    CarryOn,
    Terminated,
}

impl From<matrix_sdk_ui::room_list::State> for RoomListState {
    fn from(value: matrix_sdk_ui::room_list::State) -> Self {
        use matrix_sdk_ui::room_list::State::*;

        match value {
            Init => Self::Init,
            FirstRooms => Self::FirstRooms,
            AllRooms => Self::AllRooms,
            CarryOn => Self::CarryOn,
            Terminated { .. } => Self::Terminated,
        }
    }
}

#[uniffi::export(callback_interface)]
pub trait RoomListStateListener: Send + Sync + Debug {
    fn on_update(&self, state: RoomListState);
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
