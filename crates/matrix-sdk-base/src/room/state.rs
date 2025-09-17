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

use bitflags::bitflags;
use ruma::events::room::member::MembershipState;
use serde::{Deserialize, Serialize};

use super::Room;

impl Room {
    /// Get the state of the room.
    pub fn state(&self) -> RoomState {
        self.info.read().room_state
    }
}

/// Enum keeping track in which state the room is, e.g. if our own user is
/// joined, RoomState::Invited, or has left the room.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum RoomState {
    /// The room is in a joined state.
    Joined,
    /// The room is in a left state.
    Left,
    /// The room is in an invited state.
    Invited,
    /// The room is in a knocked state.
    Knocked,
    /// The room is in a banned state.
    Banned,
}

impl From<&MembershipState> for RoomState {
    fn from(membership_state: &MembershipState) -> Self {
        match membership_state {
            MembershipState::Ban => Self::Banned,
            MembershipState::Invite => Self::Invited,
            MembershipState::Join => Self::Joined,
            MembershipState::Knock => Self::Knocked,
            MembershipState::Leave => Self::Left,
            _ => panic!("Unexpected MembershipState: {membership_state}"),
        }
    }
}

bitflags! {
    /// Room state filter as a bitset.
    ///
    /// Note that [`RoomStateFilter::empty()`] doesn't filter the results and
    /// is equivalent to [`RoomStateFilter::all()`].
    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub struct RoomStateFilter: u16 {
        /// The room is in a joined state.
        const JOINED   = 0b00000001;
        /// The room is in an invited state.
        const INVITED  = 0b00000010;
        /// The room is in a left state.
        const LEFT     = 0b00000100;
        /// The room is in a knocked state.
        const KNOCKED  = 0b00001000;
        /// The room is in a banned state.
        const BANNED   = 0b00010000;
    }
}

impl RoomStateFilter {
    /// Whether the given room state matches this `RoomStateFilter`.
    pub fn matches(&self, state: RoomState) -> bool {
        if self.is_empty() {
            return true;
        }

        let bit_state = match state {
            RoomState::Joined => Self::JOINED,
            RoomState::Left => Self::LEFT,
            RoomState::Invited => Self::INVITED,
            RoomState::Knocked => Self::KNOCKED,
            RoomState::Banned => Self::BANNED,
        };

        self.contains(bit_state)
    }

    /// Get this `RoomStateFilter` as a list of matching [`RoomState`]s.
    pub fn as_vec(&self) -> Vec<RoomState> {
        let mut states = Vec::new();

        if self.contains(Self::JOINED) {
            states.push(RoomState::Joined);
        }
        if self.contains(Self::LEFT) {
            states.push(RoomState::Left);
        }
        if self.contains(Self::INVITED) {
            states.push(RoomState::Invited);
        }
        if self.contains(Self::KNOCKED) {
            states.push(RoomState::Knocked);
        }
        if self.contains(Self::BANNED) {
            states.push(RoomState::Banned);
        }

        states
    }
}

#[cfg(test)]
mod tests {
    use matrix_sdk_test::async_test;
    use ruma::owned_room_id;

    use super::{RoomState, RoomStateFilter};
    use crate::test_utils::logged_in_base_client;

    #[async_test]
    async fn test_room_state_filters() {
        let client = logged_in_base_client(None).await;

        let joined_room_id = owned_room_id!("!joined:example.org");
        client.get_or_create_room(&joined_room_id, RoomState::Joined);

        let invited_room_id = owned_room_id!("!invited:example.org");
        client.get_or_create_room(&invited_room_id, RoomState::Invited);

        let left_room_id = owned_room_id!("!left:example.org");
        client.get_or_create_room(&left_room_id, RoomState::Left);

        let knocked_room_id = owned_room_id!("!knocked:example.org");
        client.get_or_create_room(&knocked_room_id, RoomState::Knocked);

        let banned_room_id = owned_room_id!("!banned:example.org");
        client.get_or_create_room(&banned_room_id, RoomState::Banned);

        let joined_rooms = client.rooms_filtered(RoomStateFilter::JOINED);
        assert_eq!(joined_rooms.len(), 1);
        assert_eq!(joined_rooms[0].state(), RoomState::Joined);
        assert_eq!(joined_rooms[0].room_id, joined_room_id);

        let invited_rooms = client.rooms_filtered(RoomStateFilter::INVITED);
        assert_eq!(invited_rooms.len(), 1);
        assert_eq!(invited_rooms[0].state(), RoomState::Invited);
        assert_eq!(invited_rooms[0].room_id, invited_room_id);

        let left_rooms = client.rooms_filtered(RoomStateFilter::LEFT);
        assert_eq!(left_rooms.len(), 1);
        assert_eq!(left_rooms[0].state(), RoomState::Left);
        assert_eq!(left_rooms[0].room_id, left_room_id);

        let knocked_rooms = client.rooms_filtered(RoomStateFilter::KNOCKED);
        assert_eq!(knocked_rooms.len(), 1);
        assert_eq!(knocked_rooms[0].state(), RoomState::Knocked);
        assert_eq!(knocked_rooms[0].room_id, knocked_room_id);

        let banned_rooms = client.rooms_filtered(RoomStateFilter::BANNED);
        assert_eq!(banned_rooms.len(), 1);
        assert_eq!(banned_rooms[0].state(), RoomState::Banned);
        assert_eq!(banned_rooms[0].room_id, banned_room_id);
    }

    #[test]
    fn test_room_state_filters_as_vec() {
        assert_eq!(RoomStateFilter::JOINED.as_vec(), vec![RoomState::Joined]);
        assert_eq!(RoomStateFilter::LEFT.as_vec(), vec![RoomState::Left]);
        assert_eq!(RoomStateFilter::INVITED.as_vec(), vec![RoomState::Invited]);
        assert_eq!(RoomStateFilter::KNOCKED.as_vec(), vec![RoomState::Knocked]);
        assert_eq!(RoomStateFilter::BANNED.as_vec(), vec![RoomState::Banned]);

        // Check all filters are taken into account
        assert_eq!(
            RoomStateFilter::all().as_vec(),
            vec![
                RoomState::Joined,
                RoomState::Left,
                RoomState::Invited,
                RoomState::Knocked,
                RoomState::Banned
            ]
        );
    }
}
