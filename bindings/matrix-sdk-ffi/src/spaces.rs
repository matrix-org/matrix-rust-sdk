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

use matrix_sdk_ui::spaces::{
    SpaceService as UISpaceService, SpaceServiceRoom as UISpaceServiceRoom,
};
use ruma::RoomId;

use crate::{
    client::JoinRule,
    error::ClientError,
    room::{Membership, RoomHero},
    room_preview::RoomType,
};

#[derive(uniffi::Object)]
pub struct SpaceService {
    inner: UISpaceService,
}

impl SpaceService {
    /// Create a new instance of `SpaceService`.
    pub fn new(inner: UISpaceService) -> Self {
        Self { inner }
    }
}

#[matrix_sdk_ffi_macros::export]
impl SpaceService {
    /// Get the list of joined spaces.
    pub fn joined_spaces(&self) -> Vec<SpaceServiceRoom> {
        self.inner.joined_spaces().into_iter().map(Into::into).collect()
    }

    /// Get the top-level children for a given space.
    pub async fn top_level_children_for(
        &self,
        space_id: String,
    ) -> Result<Vec<SpaceServiceRoom>, ClientError> {
        let space_id = RoomId::parse(space_id)?;
        let children = self.inner.top_level_children_for(space_id).await?;
        Ok(children.into_iter().map(Into::into).collect())
    }
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
            state: room.state.map(Into::into),
            heroes: room.heroes.map(|heroes| heroes.into_iter().map(Into::into).collect()),
        }
    }
}
