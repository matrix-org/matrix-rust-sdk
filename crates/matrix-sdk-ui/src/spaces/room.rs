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

use std::cmp::Ordering;

use matrix_sdk::{Room, RoomHero, RoomState};
use ruma::{
    MilliSecondsSinceUnixEpoch, OwnedMxcUri, OwnedRoomAliasId, OwnedRoomId, OwnedServerName,
    OwnedSpaceChildOrder,
    events::{
        room::{guest_access::GuestAccess, history_visibility::HistoryVisibility},
        space::child::HierarchySpaceChildEvent,
    },
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

    /// Sorts space rooms by various criteria as defined in
    /// https://spec.matrix.org/latest/client-server-api/#ordering-of-children-within-a-space
    pub(crate) fn compare_rooms(
        a: &SpaceRoom,
        b: &SpaceRoom,
        a_state: Option<SpaceRoomChildState>,
        b_state: Option<SpaceRoomChildState>,
    ) -> Ordering {
        match (a_state, b_state) {
            (Some(a_state), Some(b_state)) => match (&a_state.order, &b_state.order) {
                (Some(a_order), Some(b_order)) => a_order
                    .cmp(b_order)
                    .then(a_state.origin_server_ts.cmp(&b_state.origin_server_ts))
                    .then(a.room_id.cmp(&b.room_id)),
                (Some(_), None) => Ordering::Less,
                (None, Some(_)) => Ordering::Greater,
                (None, None) => a_state
                    .origin_server_ts
                    .cmp(&b_state.origin_server_ts)
                    .then(a.room_id.to_string().cmp(&b.room_id.to_string())),
            },
            _ => a.room_id.to_string().cmp(&b.room_id.to_string()),
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct SpaceRoomChildState {
    pub(crate) order: Option<OwnedSpaceChildOrder>,
    pub(crate) origin_server_ts: MilliSecondsSinceUnixEpoch,
}

impl From<&HierarchySpaceChildEvent> for SpaceRoomChildState {
    fn from(event: &HierarchySpaceChildEvent) -> Self {
        SpaceRoomChildState {
            order: event.content.order.clone(),
            origin_server_ts: event.origin_server_ts,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cmp::Ordering;

    use matrix_sdk_test::async_test;
    use ruma::{MilliSecondsSinceUnixEpoch, OwnedRoomId, SpaceChildOrder, owned_room_id, uint};

    use crate::spaces::{SpaceRoom, room::SpaceRoomChildState};

    #[async_test]
    async fn test_room_list_sorting() {
        // Rooms without a `m.space.child` state event should be sorted by their
        // `room_id`
        assert_eq!(
            SpaceRoom::compare_rooms(
                &make_space_room(owned_room_id!("!A:a.b")),
                &make_space_room(owned_room_id!("!B:a.b")),
                None,
                None
            ),
            Ordering::Less
        );

        assert_eq!(
            SpaceRoom::compare_rooms(
                &make_space_room(owned_room_id!("!Marțolea:a.b")),
                &make_space_room(owned_room_id!("!Luana:a.b")),
                None,
                None,
            ),
            Ordering::Greater
        );

        // Rooms without an order provided through the `children_state` should be
        // sorted by their `m.space.child` `origin_server_ts`
        assert_eq!(
            SpaceRoom::compare_rooms(
                &make_space_room(owned_room_id!("!Luana:a.b")),
                &make_space_room(owned_room_id!("!Marțolea:a.b")),
                Some(SpaceRoomChildState {
                    origin_server_ts: MilliSecondsSinceUnixEpoch(uint!(1)),
                    order: None
                }),
                Some(SpaceRoomChildState {
                    origin_server_ts: MilliSecondsSinceUnixEpoch(uint!(0)),
                    order: None
                })
            ),
            Ordering::Greater
        );

        // The `m.space.child` `content.order` field should be used if provided
        assert_eq!(
            SpaceRoom::compare_rooms(
                &make_space_room(owned_room_id!("!Joiana:a.b"),),
                &make_space_room(owned_room_id!("!Mioara:a.b"),),
                Some(SpaceRoomChildState {
                    origin_server_ts: MilliSecondsSinceUnixEpoch(uint!(123)),
                    order: Some(SpaceChildOrder::parse("second").unwrap())
                }),
                Some(SpaceRoomChildState {
                    origin_server_ts: MilliSecondsSinceUnixEpoch(uint!(234)),
                    order: Some(SpaceChildOrder::parse("first").unwrap())
                }),
            ),
            Ordering::Greater
        );

        // The timestamp should be used when the `order` is the same
        assert_eq!(
            SpaceRoom::compare_rooms(
                &make_space_room(owned_room_id!("!Joiana:a.b")),
                &make_space_room(owned_room_id!("!Mioara:a.b")),
                Some(SpaceRoomChildState {
                    origin_server_ts: MilliSecondsSinceUnixEpoch(uint!(1)),
                    order: Some(SpaceChildOrder::parse("Same pasture").unwrap())
                }),
                Some(SpaceRoomChildState {
                    origin_server_ts: MilliSecondsSinceUnixEpoch(uint!(0)),
                    order: Some(SpaceChildOrder::parse("Same pasture").unwrap())
                }),
            ),
            Ordering::Greater
        );

        // And the `room_id` should be used when both the `order` and the
        // `timestamp` are equal
        assert_eq!(
            SpaceRoom::compare_rooms(
                &make_space_room(owned_room_id!("!Joiana:a.b")),
                &make_space_room(owned_room_id!("!Mioara:a.b")),
                Some(SpaceRoomChildState {
                    origin_server_ts: MilliSecondsSinceUnixEpoch(uint!(0)),
                    order: Some(SpaceChildOrder::parse("Same pasture").unwrap())
                }),
                Some(SpaceRoomChildState {
                    origin_server_ts: MilliSecondsSinceUnixEpoch(uint!(0)),
                    order: Some(SpaceChildOrder::parse("Same pasture").unwrap())
                }),
            ),
            Ordering::Less
        );

        // When one of the rooms is missing `children_state` data the other one
        // should take precedence
        assert_eq!(
            SpaceRoom::compare_rooms(
                &make_space_room(owned_room_id!("!Viola:a.b")),
                &make_space_room(owned_room_id!("!Sâmbotina:a.b")),
                None,
                Some(SpaceRoomChildState {
                    origin_server_ts: MilliSecondsSinceUnixEpoch(uint!(0)),
                    order: None
                }),
            ),
            Ordering::Greater
        );

        // If the `order` is missing from one of the rooms but `children_state`
        // is present then the other one should come first
        assert_eq!(
            SpaceRoom::compare_rooms(
                &make_space_room(owned_room_id!("!Sâmbotina:a.b")),
                &make_space_room(owned_room_id!("!Dumana:a.b")),
                Some(SpaceRoomChildState {
                    origin_server_ts: MilliSecondsSinceUnixEpoch(uint!(1)),
                    order: None
                }),
                Some(SpaceRoomChildState {
                    origin_server_ts: MilliSecondsSinceUnixEpoch(uint!(1)),
                    order: Some(SpaceChildOrder::parse("Some pasture").unwrap())
                }),
            ),
            Ordering::Greater
        );
    }

    fn make_space_room(room_id: OwnedRoomId) -> SpaceRoom {
        SpaceRoom {
            room_id,
            canonical_alias: None,
            name: Some("New room name".to_owned()),
            display_name: "Empty room".to_owned(),
            topic: None,
            avatar_url: None,
            room_type: None,
            num_joined_members: 0,
            join_rule: None,
            world_readable: None,
            guest_can_join: false,
            is_direct: None,
            children_count: 0,
            state: None,
            heroes: None,
            via: vec![],
        }
    }
}
