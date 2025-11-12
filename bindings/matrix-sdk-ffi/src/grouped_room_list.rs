use std::{fmt::Debug, sync::Arc};

use eyeball_im::VectorDiff;
use futures_util::StreamExt;
use matrix_sdk_common::{SendOutsideWasm, SyncOutsideWasm};
use matrix_sdk_ui::grouped_room_list::{
    GroupedRoomListItem as UIGroupedRoomListItem,
    GroupedRoomListService as UIGroupedRoomListService,
};

use crate::{
    room::Room, room_list::RoomListEntriesDynamicFilterKind, runtime::get_runtime_handle,
    TaskHandle,
};

#[derive(uniffi::Enum)]
pub enum GroupedRoomListItem {
    Room { room: Arc<Room> },
    Space { space: Arc<Room>, room: Option<Arc<Room>> },
}

impl From<UIGroupedRoomListItem> for GroupedRoomListItem {
    fn from(room: UIGroupedRoomListItem) -> Self {
        match room {
            UIGroupedRoomListItem::Room(room) => {
                GroupedRoomListItem::Room { room: Arc::new(Room::new(room.into_inner(), None)) }
            }
            UIGroupedRoomListItem::Space(space, room) => GroupedRoomListItem::Space {
                space: Arc::new(Room::new(space.into_inner(), None)),
                room: room.map(|room| Arc::new(Room::new(room.into_inner(), None))),
            },
        }
    }
}

#[derive(uniffi::Object)]
pub struct GroupedRoomListService {
    inner: UIGroupedRoomListService,
}

impl GroupedRoomListService {
    pub(crate) fn new(inner: UIGroupedRoomListService) -> Self {
        Self { inner }
    }
}

#[matrix_sdk_ffi_macros::export]
impl GroupedRoomListService {
    pub async fn setup(&self) {
        self.inner.setup().await;
    }

    /// Subscribes to updates on the joined spaces list. If space rooms are
    /// joined or left, the stream will yield diffs that reflect the changes.
    pub async fn subscribe_to_entries(
        &self,
        listener: Box<dyn GroupedRoomListEntriesListener>,
    ) -> Arc<TaskHandle> {
        let (initial_values, mut stream) = self.inner.subscribe_to_room_updates().await;

        listener.on_update(vec![GroupedRoomListUpdate::Reset {
            values: initial_values.into_iter().map(Into::into).collect(),
        }]);

        Arc::new(TaskHandle::new(get_runtime_handle().spawn(async move {
            while let Some(diffs) = stream.next().await {
                listener.on_update(diffs.into_iter().map(Into::into).collect());
            }
        })))
    }

    async fn set_filter(&self, kind: Option<RoomListEntriesDynamicFilterKind>) -> bool {
        self.inner.set_filter(kind.map(|k| k.into())).await
    }
}

#[matrix_sdk_ffi_macros::export(callback_interface)]
pub trait GroupedRoomListEntriesListener: SendOutsideWasm + SyncOutsideWasm + Debug {
    fn on_update(&self, room_updates: Vec<GroupedRoomListUpdate>);
}

#[derive(uniffi::Enum)]
pub enum GroupedRoomListUpdate {
    Append { values: Vec<GroupedRoomListItem> },
    Clear,
    PushFront { value: GroupedRoomListItem },
    PushBack { value: GroupedRoomListItem },
    PopFront,
    PopBack,
    Insert { index: u32, value: GroupedRoomListItem },
    Set { index: u32, value: GroupedRoomListItem },
    Remove { index: u32 },
    Truncate { length: u32 },
    Reset { values: Vec<GroupedRoomListItem> },
}

impl From<VectorDiff<UIGroupedRoomListItem>> for GroupedRoomListUpdate {
    fn from(diff: VectorDiff<UIGroupedRoomListItem>) -> Self {
        match diff {
            VectorDiff::Append { values } => {
                Self::Append { values: values.into_iter().map(|v| v.into()).collect() }
            }
            VectorDiff::Clear => Self::Clear,
            VectorDiff::PushFront { value } => Self::PushFront { value: value.into() },
            VectorDiff::PushBack { value } => Self::PushBack { value: value.into() },
            VectorDiff::PopFront => Self::PopFront,
            VectorDiff::PopBack => Self::PopBack,
            VectorDiff::Insert { index, value } => {
                Self::Insert { index: index as u32, value: value.into() }
            }
            VectorDiff::Set { index, value } => {
                Self::Set { index: index as u32, value: value.into() }
            }
            VectorDiff::Remove { index } => Self::Remove { index: index as u32 },
            VectorDiff::Truncate { length } => Self::Truncate { length: length as u32 },
            VectorDiff::Reset { values } => {
                Self::Reset { values: values.into_iter().map(|v| v.into()).collect() }
            }
        }
    }
}
