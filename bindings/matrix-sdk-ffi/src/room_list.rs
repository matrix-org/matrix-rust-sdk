use std::{fmt::Debug, sync::Arc};

use eyeball_im::VectorDiff;
use futures_util::{pin_mut, StreamExt};
use ruma::RoomId;

use crate::{
    Client, EventTimelineItem, RoomListEntry, TaskHandle, TimelineDiff, TimelineItem,
    TimelineListener, RUNTIME,
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

            while let Some(_) = sync_stream.next().await {
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

    async fn room(&self, room_id: String) -> Result<Arc<RoomListItem>, RoomListError> {
        let room_id = <&RoomId>::try_from(room_id.as_str()).map_err(RoomListError::from)?;

        Ok(Arc::new(RoomListItem { inner: Arc::new(self.inner.room(room_id).await?) }))
    }
}

#[derive(uniffi::Record)]
pub struct RoomListEntriesResult {
    pub entries: Vec<RoomListEntry>,
    pub entries_stream: Arc<TaskHandle>,
}

// Also declared in the UDL file.
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

// Also declared in the UDL file.
pub trait RoomListStateListener: Send + Sync + Debug {
    fn on_update(&self, state: RoomListState);
}

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

// Also declared in the UDL file.
pub trait RoomListEntriesListener: Send + Sync + Debug {
    fn on_update(&self, room_entries_update: RoomListEntriesUpdate);
}

#[derive(uniffi::Object)]
pub struct RoomListItem {
    inner: Arc<matrix_sdk_ui::room_list::Room>,
}

#[uniffi::export]
impl RoomListItem {
    fn name(&self) -> Option<String> {
        RUNTIME.block_on(async { self.inner.name().await })
    }

    async fn timeline(&self, listener: Box<dyn TimelineListener>) -> RoomListItemTimelineResult {
        let timeline = self.inner.timeline().await;
        let (items, items_stream) = timeline.subscribe().await;

        RoomListItemTimelineResult {
            items: items.into_iter().map(TimelineItem::from_arc).collect(),
            items_stream: Arc::new(TaskHandle::new(RUNTIME.spawn(async move {
                pin_mut!(items_stream);

                while let Some(diff) = items_stream.next().await {
                    listener.on_update(Arc::new(TimelineDiff::new(diff)))
                }
            }))),
        }
    }

    fn latest_event(&self) -> Option<Arc<EventTimelineItem>> {
        RUNTIME.block_on(async {
            self.inner.latest_event().await.map(EventTimelineItem).map(Arc::new)
        })
    }
}

#[derive(uniffi::Record)]
pub struct RoomListItemTimelineResult {
    pub items: Vec<Arc<TimelineItem>>,
    pub items_stream: Arc<TaskHandle>,
}
