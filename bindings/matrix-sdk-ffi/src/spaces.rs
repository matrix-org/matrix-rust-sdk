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

use futures_util::{pin_mut, StreamExt};
use matrix_sdk_common::{SendOutsideWasm, SyncOutsideWasm};
use matrix_sdk_ui::spaces::{
    room_list::SpaceServiceRoomListPaginationState as UISpaceServiceRoomListPaginationState,
    SpaceService as UISpaceService, SpaceServiceRoom as UISpaceServiceRoom,
    SpaceServiceRoomList as UISpaceServiceRoomList,
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

#[derive(uniffi::Object)]
pub struct SpaceService {
    inner: UISpaceService,
}

impl SpaceService {
    pub fn new(inner: UISpaceService) -> Self {
        Self { inner }
    }
}

#[matrix_sdk_ffi_macros::export]
impl SpaceService {
    pub async fn joined_spaces(&self) -> Vec<SpaceServiceRoom> {
        self.inner.joined_spaces().await.into_iter().map(Into::into).collect()
    }

    #[allow(clippy::unused_async)]
    // This method doesn't need to be async but if its not the FFI layer panics
    // with "there is no no reactor running, must be called from the context
    // of a Tokio 1.x runtime" error because the underlying method spawns an
    // async task.
    pub async fn subscribe_to_joined_spaces(
        &self,
        listener: Box<dyn SpaceServiceJoinedSpacesListener>,
    ) -> Arc<TaskHandle> {
        let entries_stream = self.inner.subscribe_to_joined_spaces();

        Arc::new(TaskHandle::new(get_runtime_handle().spawn(async move {
            pin_mut!(entries_stream);

            while let Some(rooms) = entries_stream.next().await {
                listener.on_update(rooms.into_iter().map(Into::into).collect());
            }
        })))
    }

    #[allow(clippy::unused_async)]
    // This method doesn't need to be async but if its not the FFI layer panics
    // with "there is no no reactor running, must be called from the context
    // of a Tokio 1.x runtime" error because the underlying constructor spawns
    // an async task.
    pub async fn space_room_list(
        &self,
        space_id: String,
    ) -> Result<Arc<SpaceServiceRoomList>, ClientError> {
        let space_id = RoomId::parse(space_id)?;
        Ok(Arc::new(SpaceServiceRoomList::new(self.inner.space_room_list(space_id))))
    }
}

#[derive(uniffi::Object)]
pub struct SpaceServiceRoomList {
    inner: UISpaceServiceRoomList,
}

impl SpaceServiceRoomList {
    pub fn new(inner: UISpaceServiceRoomList) -> Self {
        Self { inner }
    }
}

#[matrix_sdk_ffi_macros::export]
impl SpaceServiceRoomList {
    pub fn pagination_state(&self) -> SpaceServiceRoomListPaginationState {
        self.inner.pagination_state().into()
    }

    pub fn subscribe_to_pagination_state_updates(
        &self,
        listener: Box<dyn SpaceServiceRoomListPaginationStateListener>,
    ) -> Arc<TaskHandle> {
        let pagination_state = self.inner.subscribe_to_pagination_state_updates();

        Arc::new(TaskHandle::new(get_runtime_handle().spawn(async move {
            pin_mut!(pagination_state);

            while let Some(state) = pagination_state.next().await {
                listener.on_update(state.into());
            }
        })))
    }

    pub fn rooms(&self) -> Vec<SpaceServiceRoom> {
        self.inner.rooms().into_iter().map(Into::into).collect()
    }

    pub fn subscribe_to_room_update(
        &self,
        listener: Box<dyn SpaceServiceRoomListEntriesListener>,
    ) -> Arc<TaskHandle> {
        let entries_stream = self.inner.subscribe_to_room_updates();

        Arc::new(TaskHandle::new(get_runtime_handle().spawn(async move {
            pin_mut!(entries_stream);

            while let Some(rooms) = entries_stream.next().await {
                listener.on_update(rooms.into_iter().map(Into::into).collect());
            }
        })))
    }

    pub async fn paginate(&self) -> Result<(), ClientError> {
        self.inner.paginate().await.map_err(ClientError::from)
    }
}

#[derive(uniffi::Enum)]
pub enum SpaceServiceRoomListPaginationState {
    Idle { end_reached: bool },
    Loading,
}

impl From<UISpaceServiceRoomListPaginationState> for SpaceServiceRoomListPaginationState {
    fn from(state: UISpaceServiceRoomListPaginationState) -> Self {
        match state {
            UISpaceServiceRoomListPaginationState::Idle { end_reached } => {
                SpaceServiceRoomListPaginationState::Idle { end_reached }
            }
            UISpaceServiceRoomListPaginationState::Loading => {
                SpaceServiceRoomListPaginationState::Loading
            }
        }
    }
}

#[matrix_sdk_ffi_macros::export(callback_interface)]
pub trait SpaceServiceRoomListPaginationStateListener:
    SendOutsideWasm + SyncOutsideWasm + Debug
{
    fn on_update(&self, pagination_state: SpaceServiceRoomListPaginationState);
}

#[matrix_sdk_ffi_macros::export(callback_interface)]
pub trait SpaceServiceRoomListEntriesListener: SendOutsideWasm + SyncOutsideWasm + Debug {
    fn on_update(&self, rooms: Vec<SpaceServiceRoom>);
}

#[matrix_sdk_ffi_macros::export(callback_interface)]
pub trait SpaceServiceJoinedSpacesListener: SendOutsideWasm + SyncOutsideWasm + Debug {
    fn on_update(&self, rooms: Vec<SpaceServiceRoom>);
}

#[derive(uniffi::Record)]
pub struct SpaceServiceRoom {
    pub room_id: String,
    pub canonical_alias: Option<String>,
    pub name: Option<String>,
    pub topic: Option<String>,
    pub avatar_url: Option<String>,
    pub room_type: RoomType,
    pub num_joined_members: u64,
    pub join_rule: Option<JoinRule>,
    pub world_readable: Option<bool>,
    pub guest_can_join: bool,

    pub children_count: u64,
    pub state: Option<Membership>,
    pub heroes: Option<Vec<RoomHero>>,
}

impl From<UISpaceServiceRoom> for SpaceServiceRoom {
    fn from(room: UISpaceServiceRoom) -> Self {
        Self {
            room_id: room.room_id.into(),
            canonical_alias: room.canonical_alias.map(|alias| alias.into()),
            name: room.name,
            topic: room.topic,
            avatar_url: room.avatar_url.map(|url| url.into()),
            room_type: room.room_type.into(),
            num_joined_members: room.num_joined_members,
            join_rule: room.join_rule.map(Into::into),
            world_readable: room.world_readable,
            guest_can_join: room.guest_can_join,
            children_count: room.children_count,
            state: room.state.map(Into::into),
            heroes: room.heroes.map(|heroes| heroes.into_iter().map(Into::into).collect()),
        }
    }
}
