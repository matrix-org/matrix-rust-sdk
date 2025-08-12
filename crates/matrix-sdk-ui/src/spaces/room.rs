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
// See the License for that specific language governing permissions and
// limitations under the License.

use matrix_sdk::{Room, RoomHero, RoomState};
use ruma::{
    OwnedMxcUri, OwnedRoomAliasId, OwnedRoomId,
    events::room::{guest_access::GuestAccess, history_visibility::HistoryVisibility},
    room::{JoinRuleSummary, RoomSummary, RoomType},
};

#[derive(Debug, Clone, PartialEq)]
pub struct SpaceRoom {
    pub room_id: OwnedRoomId,
    pub canonical_alias: Option<OwnedRoomAliasId>,
    pub name: Option<String>,
    pub topic: Option<String>,
    pub avatar_url: Option<OwnedMxcUri>,
    pub room_type: Option<RoomType>,
    pub num_joined_members: u64,
    pub join_rule: Option<JoinRuleSummary>,
    pub world_readable: Option<bool>,
    pub guest_can_join: bool,

    pub children_count: u64,
    pub state: Option<RoomState>,
    pub heroes: Option<Vec<RoomHero>>,
}

impl SpaceRoom {
    pub fn new_from_summary(
        summary: &RoomSummary,
        known_room: Option<Room>,
        children_count: u64,
    ) -> Self {
        Self {
            room_id: summary.room_id.clone(),
            canonical_alias: summary.canonical_alias.clone(),
            name: summary.name.clone(),
            topic: summary.topic.clone(),
            avatar_url: summary.avatar_url.clone(),
            room_type: summary.room_type.clone(),
            num_joined_members: summary.num_joined_members.into(),
            join_rule: Some(summary.join_rule.clone()),
            world_readable: Some(summary.world_readable),
            guest_can_join: summary.guest_can_join,
            children_count,
            state: known_room.as_ref().map(|r| r.state()),
            heroes: known_room.map(|r| r.heroes()),
        }
    }

    pub fn new_from_known(known_room: Room, children_count: u64) -> Self {
        let room_info = known_room.clone_info();

        Self {
            room_id: room_info.room_id().to_owned(),
            canonical_alias: room_info.canonical_alias().map(ToOwned::to_owned),
            name: room_info.name().map(ToOwned::to_owned),
            topic: room_info.topic().map(ToOwned::to_owned),
            avatar_url: room_info.avatar_url().map(ToOwned::to_owned),
            room_type: room_info.room_type().cloned(),
            num_joined_members: known_room.joined_members_count(),
            join_rule: room_info.join_rule().cloned().map(Into::into),
            world_readable: room_info
                .history_visibility()
                .map(|vis| *vis == HistoryVisibility::WorldReadable),
            guest_can_join: known_room.guest_access() == GuestAccess::CanJoin,
            children_count,
            state: Some(known_room.state()),
            heroes: Some(room_info.heroes().to_vec()),
        }
    }
}
