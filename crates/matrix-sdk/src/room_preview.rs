// Copyright 2024 The Matrix.org Foundation C.I.C.
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

//! Preview of a room, whether we've joined it/left it/been invited to it, or
//! not.
//!
//! This offers a few capabilities for previewing the content of the room as
//! well.

use matrix_sdk_base::{RoomInfo, RoomState};
use ruma::{
    api::client::{membership::joined_members, state::get_state_events},
    events::room::{history_visibility::HistoryVisibility, join_rules::JoinRule},
    room::RoomType,
    OwnedMxcUri, OwnedRoomAliasId, RoomId,
};
use tokio::try_join;
use tracing::{instrument, warn};

use crate::{Client, Room};

/// The preview of a room, be it invited/joined/left, or not.
#[derive(Debug)]
pub struct RoomPreview {
    /// The canonical alias for the room.
    pub canonical_alias: Option<OwnedRoomAliasId>,

    /// The room's name, if set.
    pub name: Option<String>,

    /// The room's topic, if set.
    pub topic: Option<String>,

    /// The MXC URI to the room's avatar, if set.
    pub avatar_url: Option<OwnedMxcUri>,

    /// The number of joined members.
    pub num_joined_members: u64,

    /// The room type (space, custom) or nothing, if it's a regular room.
    pub room_type: Option<RoomType>,

    /// What's the join rule for this room?
    pub join_rule: JoinRule,

    /// What's the history visibility for this room?
    pub history_visibility: HistoryVisibility,

    /// Has the current user been invited/joined/left this room?
    ///
    /// Set to `None` if the room is unknown to the user.
    pub state: Option<RoomState>,
}

impl RoomPreview {
    /// Constructs a [`RoomPreview`] from the associated room info.
    ///
    /// Note: not using the room info's state/count of joined members, because
    /// we can do better than that.
    fn from_room_info(
        room_info: RoomInfo,
        num_joined_members: u64,
        state: Option<RoomState>,
    ) -> Self {
        RoomPreview {
            canonical_alias: room_info.canonical_alias().map(ToOwned::to_owned),
            name: room_info.name().map(ToOwned::to_owned),
            topic: room_info.topic().map(ToOwned::to_owned),
            avatar_url: room_info.avatar_url().map(ToOwned::to_owned),
            room_type: room_info.room_type().cloned(),
            join_rule: room_info.join_rule().clone(),
            history_visibility: room_info.history_visibility().clone(),

            num_joined_members,
            state,
        }
    }

    /// Create a room preview from a known room (i.e. one we've been invited to,
    /// we've joined or we've left).
    pub(crate) fn from_known(room: &Room) -> Self {
        Self::from_room_info(room.clone_info(), room.joined_members_count(), Some(room.state()))
    }

    #[instrument(skip(client))]
    pub(crate) async fn from_unknown(client: &Client, room_id: &RoomId) -> crate::Result<Self> {
        // TODO: (optimization) Use the room summary endpoint, if available, as
        // described in https://github.com/deepbluev7/matrix-doc/blob/room-summaries/proposals/3266-room-summary.md

        // TODO: (optimization) Use the room search directory, if available:
        // - if the room directory visibility is public,
        // - then use a public room filter set to this room id

        // Resort to using the room state endpoint, as well as the joined members one.
        let state_request = get_state_events::v3::Request::new(room_id.to_owned());
        let joined_members_request = joined_members::v3::Request::new(room_id.to_owned());

        let (state, joined_members) =
            try_join!(async { client.send(state_request, None).await }, async {
                client.send(joined_members_request, None).await
            })?;

        // Converting from usize to u64 will always work, up to 64-bits devices;
        // otherwise, assume LOTS of members.
        let num_joined_members = joined_members.joined.len().try_into().unwrap_or(u64::MAX);

        let mut room_info = RoomInfo::new(room_id, RoomState::Joined);

        for ev in state.room_state {
            let ev = match ev.deserialize() {
                Ok(ev) => ev,
                Err(err) => {
                    warn!("failed to deserialize state event: {err}");
                    continue;
                }
            };
            room_info.handle_state_event(&ev.into());
        }

        Ok(Self::from_room_info(room_info, num_joined_members, None))
    }
}
