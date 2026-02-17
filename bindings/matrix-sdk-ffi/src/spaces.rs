// Copyright 2025 The Matrix.org Foundation C.I.C.
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

use std::{fmt::Debug, sync::Arc};

use eyeball_im::VectorDiff;
use futures_util::{pin_mut, StreamExt};
use matrix_sdk_common::{SendOutsideWasm, SyncOutsideWasm};
use matrix_sdk_ui::spaces::{
    leave::{LeaveSpaceHandle as UILeaveSpaceHandle, LeaveSpaceRoom as UILeaveSpaceRoom},
    room_list::SpaceRoomListPaginationState,
    SpaceFilter as UISpaceFilter, SpaceRoom as UISpaceRoom, SpaceRoomList as UISpaceRoomList,
    SpaceService as UISpaceService,
};
use ruma::RoomId;

use crate::{
    client::JoinRule,
    error::ClientError,
    room::{Membership, RoomHero},
    room_preview::RoomType,
    runtime::get_runtime_handle,
    TaskHandle,
};

/// The main entry point into the Spaces facilities.
///
/// The spaces service is responsible for retrieving one's joined rooms,
/// building a graph out of their `m.space.parent` and `m.space.child` state
/// events, and providing access to the top-level spaces and their children.
#[derive(uniffi::Object)]
pub struct SpaceService {
    inner: UISpaceService,
}

impl SpaceService {
    /// Creates a new `SpaceService` instance.
    pub(crate) fn new(inner: UISpaceService) -> Self {
        Self { inner }
    }
}

#[matrix_sdk_ffi_macros::export]
impl SpaceService {
    /// Returns a list of all the top-level joined spaces. It will eagerly
    /// compute the latest version and also notify subscribers if there were
    /// any changes.
    pub async fn top_level_joined_spaces(&self) -> Vec<SpaceRoom> {
        self.inner.top_level_joined_spaces().await.into_iter().map(Into::into).collect()
    }

    /// Subscribes to updates on the joined spaces list. If space rooms are
    /// joined or left, the stream will yield diffs that reflect the changes.
    pub async fn subscribe_to_top_level_joined_spaces(
        &self,
        listener: Box<dyn SpaceServiceJoinedSpacesListener>,
    ) -> Arc<TaskHandle> {
        let (initial_values, mut stream) = self.inner.subscribe_to_top_level_joined_spaces().await;

        listener.on_update(vec![SpaceListUpdate::Reset {
            values: initial_values.into_iter().map(Into::into).collect(),
        }]);

        Arc::new(TaskHandle::new(get_runtime_handle().spawn(async move {
            while let Some(diffs) = stream.next().await {
                listener.on_update(diffs.into_iter().map(Into::into).collect());
            }
        })))
    }

    /// Space filters provide access to a custom subset of the space graph that
    /// can be used in tandem with the [`crate::RoomListService`] to narrow
    /// down the presented rooms.
    ///
    /// They are limited to the first 2 levels of the graph, with the first
    /// level only containing direct descendants while the second holds the rest
    /// of them recursively.
    pub async fn space_filters(&self) -> Vec<SpaceFilter> {
        self.inner.space_filters().await.into_iter().map(|s| s.into()).collect()
    }

    /// Subscribe to changes or updates to the space filters.
    pub async fn subscribe_to_space_filters(
        &self,
        listener: Box<dyn SpaceServiceSpaceFiltersListener>,
    ) -> Arc<TaskHandle> {
        let (initial_values, mut stream) = self.inner.subscribe_to_space_filters().await;

        listener.on_update(vec![SpaceFilterUpdate::Reset {
            values: initial_values.into_iter().map(Into::into).collect(),
        }]);

        Arc::new(TaskHandle::new(get_runtime_handle().spawn(async move {
            while let Some(diffs) = stream.next().await {
                listener.on_update(diffs.into_iter().map(Into::into).collect());
            }
        })))
    }

    /// Returns a flattened list containing all the spaces where the user has
    /// permission to send `m.space.child` state events.
    ///
    /// Note: Unlike [`Self::top_level_joined_spaces()`], this method does not
    /// recompute the space graph, nor does it notify subscribers about changes.
    pub async fn editable_spaces(&self) -> Vec<SpaceRoom> {
        self.inner.editable_spaces().await.into_iter().map(Into::into).collect()
    }

    /// Returns a `SpaceRoomList` for the given space ID.
    pub async fn space_room_list(
        &self,
        space_id: String,
    ) -> Result<Arc<SpaceRoomList>, ClientError> {
        let space_id = RoomId::parse(space_id)?;
        Ok(Arc::new(SpaceRoomList::new(self.inner.space_room_list(space_id).await)))
    }

    /// Returns all known direct-parents of a given space room ID.
    pub async fn joined_parents_of_child(
        &self,
        child_id: String,
    ) -> Result<Vec<SpaceRoom>, ClientError> {
        let child_id = RoomId::parse(child_id)?;

        let parents = self.inner.joined_parents_of_child(&child_id).await;

        Ok(parents.into_iter().map(Into::into).collect())
    }

    /// Returns the corresponding `SpaceRoom` for the given room ID, or `None`
    /// if it isn't known.
    pub async fn get_space_room(&self, room_id: String) -> Result<Option<SpaceRoom>, ClientError> {
        let room_id = RoomId::parse(room_id.as_str())?;
        Ok(self.inner.get_space_room(&room_id).await.map(Into::into))
    }

    pub async fn add_child_to_space(
        &self,
        child_id: String,
        space_id: String,
    ) -> Result<(), ClientError> {
        let space_id = RoomId::parse(space_id)?;
        let child_id = RoomId::parse(child_id)?;

        self.inner.add_child_to_space(child_id, space_id).await.map_err(ClientError::from)
    }

    pub async fn remove_child_from_space(
        &self,
        child_id: String,
        space_id: String,
    ) -> Result<(), ClientError> {
        let space_id = RoomId::parse(space_id)?;
        let child_id = RoomId::parse(child_id)?;

        self.inner.remove_child_from_space(child_id, space_id).await.map_err(ClientError::from)
    }

    /// Start a space leave process returning a [`LeaveSpaceHandle`] from which
    /// rooms can be retrieved in reversed BFS order starting from the requested
    /// `space_id` graph node. If the room is unknown then an error will be
    /// returned.
    ///
    /// Once the rooms to be left are chosen the handle can be used to leave
    /// them.
    pub async fn leave_space(
        &self,
        space_id: String,
    ) -> Result<Arc<LeaveSpaceHandle>, ClientError> {
        let space_id = RoomId::parse(space_id)?;

        let handle = self.inner.leave_space(&space_id).await.map_err(ClientError::from)?;

        Ok(Arc::new(handle.into()))
    }
}

/// The `SpaceRoomList` represents a paginated list of direct rooms
/// that belong to a particular space.
///
/// It can be used to paginate through the list (and have live updates on the
/// pagination state) as well as subscribe to changes as rooms are joined or
/// left.
///
/// The `SpaceRoomList` also automatically subscribes to client room changes
/// and updates the list accordingly as rooms are joined or left.
#[derive(uniffi::Object)]
pub struct SpaceRoomList {
    inner: UISpaceRoomList,
}

impl SpaceRoomList {
    /// Creates a new `SpaceRoomList` for the underlying UI crate room list.
    fn new(inner: UISpaceRoomList) -> Self {
        Self { inner }
    }
}

#[matrix_sdk_ffi_macros::export]
impl SpaceRoomList {
    /// Returns the space of the room list if known.
    pub fn space(&self) -> Option<SpaceRoom> {
        self.inner.space().map(Into::into)
    }

    /// Subscribe to space updates.
    pub fn subscribe_to_space_updates(
        &self,
        listener: Box<dyn SpaceRoomListSpaceListener>,
    ) -> Arc<TaskHandle> {
        let space_updates = self.inner.subscribe_to_space_updates();

        Arc::new(TaskHandle::new(get_runtime_handle().spawn(async move {
            pin_mut!(space_updates);

            while let Some(space) = space_updates.next().await {
                listener.on_update(space.map(Into::into));
            }
        })))
    }

    /// Returns if the room list is currently paginating or not.
    pub fn pagination_state(&self) -> SpaceRoomListPaginationState {
        self.inner.pagination_state()
    }

    /// Subscribe to pagination updates.
    pub fn subscribe_to_pagination_state_updates(
        &self,
        listener: Box<dyn SpaceRoomListPaginationStateListener>,
    ) -> Arc<TaskHandle> {
        let pagination_state = self.inner.subscribe_to_pagination_state_updates();

        Arc::new(TaskHandle::new(get_runtime_handle().spawn(async move {
            pin_mut!(pagination_state);

            while let Some(state) = pagination_state.next().await {
                listener.on_update(state);
            }
        })))
    }

    /// Return the current list of rooms.
    pub fn rooms(&self) -> Vec<SpaceRoom> {
        self.inner.rooms().into_iter().map(Into::into).collect()
    }

    /// Subscribes to room list updates.
    pub fn subscribe_to_room_update(
        &self,
        listener: Box<dyn SpaceRoomListEntriesListener>,
    ) -> Arc<TaskHandle> {
        let (initial_values, mut stream) = self.inner.subscribe_to_room_updates();

        listener.on_update(vec![SpaceListUpdate::Reset {
            values: initial_values.into_iter().map(Into::into).collect(),
        }]);

        Arc::new(TaskHandle::new(get_runtime_handle().spawn(async move {
            while let Some(diffs) = stream.next().await {
                listener.on_update(diffs.into_iter().map(Into::into).collect());
            }
        })))
    }

    /// Ask the list to retrieve the next page if the end hasn't been reached
    /// yet. Otherwise it no-ops.
    pub async fn paginate(&self) -> Result<(), ClientError> {
        self.inner.paginate().await.map_err(ClientError::from)
    }

    /// Clears the room list back to its initial state so that any new changes
    /// to the hierarchy will be included the next time [`Self::paginate`] is
    /// called.
    ///
    /// This is useful when you've added or removed children from the space as
    /// the list is based on a cached state that lives server-side, meaning
    /// the /hierarchy request needs to be restarted from scratch to pick up
    /// the changes.
    pub async fn reset(&self) {
        self.inner.reset().await;
    }
}

#[matrix_sdk_ffi_macros::export(callback_interface)]
pub trait SpaceRoomListSpaceListener: SendOutsideWasm + SyncOutsideWasm + Debug {
    fn on_update(&self, space: Option<SpaceRoom>);
}

#[matrix_sdk_ffi_macros::export(callback_interface)]
pub trait SpaceRoomListPaginationStateListener: SendOutsideWasm + SyncOutsideWasm + Debug {
    fn on_update(&self, pagination_state: SpaceRoomListPaginationState);
}

#[matrix_sdk_ffi_macros::export(callback_interface)]
pub trait SpaceRoomListEntriesListener: SendOutsideWasm + SyncOutsideWasm + Debug {
    fn on_update(&self, rooms: Vec<SpaceListUpdate>);
}

#[matrix_sdk_ffi_macros::export(callback_interface)]
pub trait SpaceServiceJoinedSpacesListener: SendOutsideWasm + SyncOutsideWasm + Debug {
    fn on_update(&self, room_updates: Vec<SpaceListUpdate>);
}

#[matrix_sdk_ffi_macros::export(callback_interface)]
pub trait SpaceServiceSpaceFiltersListener: SendOutsideWasm + SyncOutsideWasm + Debug {
    fn on_update(&self, filter_updates: Vec<SpaceFilterUpdate>);
}

/// Structure representing a room in a space and aggregated information
/// relevant to the UI layer.
#[derive(uniffi::Record)]
pub struct SpaceRoom {
    /// The ID of the room.
    pub room_id: String,
    /// The canonical alias of the room, if any.
    pub canonical_alias: Option<String>,
    /// The room's name from the room state event if received from sync, or one
    /// that's been computed otherwise.
    pub display_name: String,
    /// Room name as defined by the room state event only.
    pub raw_name: Option<String>,
    /// The topic of the room, if any.
    pub topic: Option<String>,
    /// The URL for the room's avatar, if one is set.
    pub avatar_url: Option<String>,
    /// The type of room from `m.room.create`, if any.
    pub room_type: RoomType,
    /// The number of members joined to the room.
    pub num_joined_members: u64,
    /// The join rule of the room.
    pub join_rule: Option<JoinRule>,
    /// Whether the room may be viewed by users without joining.
    pub world_readable: Option<bool>,
    /// Whether guest users may join the room and participate in it.
    pub guest_can_join: bool,

    /// Whether this room is a direct room.
    ///
    /// Only set if the room is known to the client otherwise we
    /// assume DMs shouldn't be exposed publicly in spaces.
    pub is_direct: Option<bool>,
    /// The number of children room this has, if a space.
    pub children_count: u64,
    /// Whether this room is joined, left etc.
    pub state: Option<Membership>,
    /// A list of room members considered to be heroes.
    pub heroes: Option<Vec<RoomHero>>,
    /// The via parameters of the room.
    pub via: Vec<String>,
}

impl From<UISpaceRoom> for SpaceRoom {
    fn from(room: UISpaceRoom) -> Self {
        Self {
            room_id: room.room_id.into(),
            canonical_alias: room.canonical_alias.map(|alias| alias.into()),
            display_name: room.display_name,
            raw_name: room.name,
            topic: room.topic,
            avatar_url: room.avatar_url.map(|url| url.into()),
            room_type: room.room_type.into(),
            num_joined_members: room.num_joined_members,
            join_rule: room.join_rule.map(Into::into),
            world_readable: room.world_readable,
            guest_can_join: room.guest_can_join,
            is_direct: room.is_direct,
            children_count: room.children_count,
            state: room.state.map(Into::into),
            heroes: room.heroes.map(|heroes| heroes.into_iter().map(Into::into).collect()),
            via: room.via.into_iter().map(Into::into).collect(),
        }
    }
}

#[derive(uniffi::Enum)]
pub enum SpaceListUpdate {
    Append { values: Vec<SpaceRoom> },
    Clear,
    PushFront { value: SpaceRoom },
    PushBack { value: SpaceRoom },
    PopFront,
    PopBack,
    Insert { index: u32, value: SpaceRoom },
    Set { index: u32, value: SpaceRoom },
    Remove { index: u32 },
    Truncate { length: u32 },
    Reset { values: Vec<SpaceRoom> },
}

impl From<VectorDiff<UISpaceRoom>> for SpaceListUpdate {
    fn from(diff: VectorDiff<UISpaceRoom>) -> Self {
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

#[derive(uniffi::Enum)]
pub enum SpaceFilterUpdate {
    Append { values: Vec<SpaceFilter> },
    Clear,
    PushFront { value: SpaceFilter },
    PushBack { value: SpaceFilter },
    PopFront,
    PopBack,
    Insert { index: u32, value: SpaceFilter },
    Set { index: u32, value: SpaceFilter },
    Remove { index: u32 },
    Truncate { length: u32 },
    Reset { values: Vec<SpaceFilter> },
}

impl From<VectorDiff<UISpaceFilter>> for SpaceFilterUpdate {
    fn from(diff: VectorDiff<UISpaceFilter>) -> Self {
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

/// The `LeaveSpaceHandle` processes rooms to be left in the order they were
/// provided by the [`SpaceService`] and annotates them with extra data to
/// inform the leave process e.g. if the current user is the last room admin.
///
/// Once the upstream client decides what rooms should actually be left, the
/// handle provides a method to execute that too.
#[derive(uniffi::Object)]
pub struct LeaveSpaceHandle {
    inner: UILeaveSpaceHandle,
}

#[matrix_sdk_ffi_macros::export]
impl LeaveSpaceHandle {
    /// A list of rooms to be left which next to normal [`SpaceRoom`] data also
    /// include leave specific information.
    pub fn rooms(&self) -> Vec<LeaveSpaceRoom> {
        let rooms = self.inner.rooms();
        rooms.iter().map(|room| room.clone().into()).collect()
    }

    /// Bulk leave the given rooms. Stops when encountering an error.
    pub async fn leave(&self, room_ids: Vec<String>) -> Result<(), ClientError> {
        let room_ids = room_ids.iter().map(RoomId::parse).collect::<Result<Vec<_>, _>>()?;

        self.inner
            .leave(|room| room_ids.contains(&room.space_room.room_id))
            .await
            .map_err(ClientError::from)
    }
}

impl From<UILeaveSpaceHandle> for LeaveSpaceHandle {
    fn from(handle: UILeaveSpaceHandle) -> Self {
        LeaveSpaceHandle { inner: handle }
    }
}

/// Space leaving specific room that groups normal [`SpaceRoom`] details with
/// information about the leaving user's role.
#[derive(uniffi::Record)]
pub struct LeaveSpaceRoom {
    /// The underlying [`SpaceRoom`]
    pub space_room: SpaceRoom,
    /// Whether the user is the last owner in the room. This helps clients
    /// better inform the user about the consequences of leaving the room.
    pub is_last_owner: bool,
    /// If the room creators have infinite PL.
    pub are_creators_privileged: bool,
}

impl From<UILeaveSpaceRoom> for LeaveSpaceRoom {
    fn from(room: UILeaveSpaceRoom) -> Self {
        LeaveSpaceRoom {
            space_room: room.space_room.into(),
            is_last_owner: room.is_last_owner,
            are_creators_privileged: room.are_creators_privileged,
        }
    }
}

#[derive(uniffi::Record)]
pub struct SpaceFilter {
    /// The underlying [`SpaceRoom`]
    space_room: SpaceRoom,
    /// The level of the space filter in the tree/hierarchy.
    /// At this point in time the filters are limited to the first 2 levels.
    level: u8,
    /// The room identifiers of the descendants of this space.
    /// For top level spaces (level 0) these will be direct descendants while
    /// for first level spaces they will be all other descendants, recursively.
    descendants: Vec<String>,
}

impl From<UISpaceFilter> for SpaceFilter {
    fn from(filter: UISpaceFilter) -> Self {
        SpaceFilter {
            space_room: filter.space_room.into(),
            level: filter.level,
            descendants: filter.descendants.into_iter().map(|id| id.to_string()).collect(),
        }
    }
}
