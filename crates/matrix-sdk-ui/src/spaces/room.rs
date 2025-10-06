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
    OwnedMxcUri, OwnedRoomAliasId, OwnedRoomId, OwnedServerName,
    events::room::{guest_access::GuestAccess, history_visibility::HistoryVisibility},
    room::{JoinRuleSummary, RoomSummary, RoomType},
};

/// Structure representing a room in a space and aggregated information
/// relevant to the UI layer.
#[derive(Debug, Clone, PartialEq)]
pub struct SpaceRoom {
    /// The ID of the room.
    pub room_id: OwnedRoomId,
    /// The canonical alias of the room, if any.
    pub canonical_alias: Option<OwnedRoomAliasId>,
    /// The name of the room, if any.
    pub name: Option<String>,
    /// Calculated display name based on the room's name, aliases, and members.
    pub display_name: String,
    /// The topic of the room, if any.
    pub topic: Option<String>,
    /// The URL for the room's avatar, if one is set.
    pub avatar_url: Option<OwnedMxcUri>,
    /// The type of room from `m.room.create`, if any.
    pub room_type: Option<RoomType>,
    /// The number of members joined to the room.
    pub num_joined_members: u64,
    /// The join rule of the room.
    pub join_rule: Option<JoinRuleSummary>,
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
    pub state: Option<RoomState>,
    /// A list of room members considered to be heroes.
    pub heroes: Option<Vec<RoomHero>>,
    /// The via parameters of the room.
    pub via: Vec<OwnedServerName>,
}

impl SpaceRoom {
    /// Build a `SpaceRoom` from a `RoomSummary` received from the /hierarchy
    /// endpoint.
    pub(crate) fn new_from_summary(
        summary: &RoomSummary,
        known_room: Option<Room>,
        children_count: u64,
        via: Vec<OwnedServerName>,
    ) -> Self {
        let display_name = matrix_sdk_base::Room::compute_display_name_with_fields(
            summary.name.clone(),
            summary.canonical_alias.as_deref(),
            known_room.as_ref().map(|r| r.heroes().to_vec()).unwrap_or_default(),
            summary.num_joined_members.into(),
        )
        .to_string();

        Self {
            room_id: summary.room_id.clone(),
            canonical_alias: summary.canonical_alias.clone(),
            name: summary.name.clone(),
            display_name,
            topic: summary.topic.clone(),
            avatar_url: summary.avatar_url.clone(),
            room_type: summary.room_type.clone(),
            num_joined_members: summary.num_joined_members.into(),
            join_rule: Some(summary.join_rule.clone()),
            world_readable: Some(summary.world_readable),
            guest_can_join: summary.guest_can_join,
            is_direct: known_room.as_ref().map(|r| r.direct_targets_length() != 0),
            children_count,
            state: known_room.as_ref().map(|r| r.state()),
            heroes: known_room.map(|r| r.heroes()),
            via,
        }
    }

    /// Build a `SpaceRoom` from a room already known to this client.
    pub(crate) fn new_from_known(known_room: &Room, children_count: u64) -> Self {
        let room_info = known_room.clone_info();

        let name = room_info.name().map(ToOwned::to_owned);
        let display_name = matrix_sdk_base::Room::compute_display_name_with_fields(
            name.clone(),
            room_info.canonical_alias(),
            room_info.heroes().to_vec(),
            known_room.joined_members_count(),
        )
        .to_string();

        Self {
            room_id: room_info.room_id().to_owned(),
            canonical_alias: room_info.canonical_alias().map(ToOwned::to_owned),
            name,
            display_name,
            topic: room_info.topic().map(ToOwned::to_owned),
            avatar_url: room_info.avatar_url().map(ToOwned::to_owned),
            room_type: room_info.room_type().cloned(),
            num_joined_members: known_room.joined_members_count(),
            join_rule: room_info.join_rule().cloned().map(Into::into),
            world_readable: room_info
                .history_visibility()
                .map(|vis| *vis == HistoryVisibility::WorldReadable),
            guest_can_join: known_room.guest_access() == GuestAccess::CanJoin,
            is_direct: Some(known_room.direct_targets_length() != 0),
            children_count,
            state: Some(known_room.state()),
            heroes: Some(room_info.heroes().to_vec()),
            via: vec![],
        }
    }
}
