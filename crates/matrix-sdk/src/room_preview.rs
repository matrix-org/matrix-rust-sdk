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

use futures_util::future::join_all;
use matrix_sdk_base::{RoomHero, RoomInfo, RoomState};
use ruma::{
    api::client::{membership::joined_members, state::get_state_events},
    directory::PublicRoomJoinRule,
    events::room::{history_visibility::HistoryVisibility, join_rules::JoinRule},
    room::RoomType,
    space::SpaceRoomJoinRule,
    OwnedMxcUri, OwnedRoomAliasId, OwnedRoomId, OwnedServerName, RoomId, RoomOrAliasId,
};
use tokio::try_join;
use tracing::{instrument, warn};

use crate::{room_directory_search::RoomDirectorySearch, Client, Room};

/// The preview of a room, be it invited/joined/left, or not.
#[derive(Debug, Clone)]
pub struct RoomPreview {
    /// The actual room id for this room.
    ///
    /// Remember the room preview can be fetched from a room alias id, so we
    /// might not know ahead of time what the room id is.
    pub room_id: OwnedRoomId,

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

    /// The number of active members, if known (joined + invited).
    pub num_active_members: Option<u64>,

    /// The room type (space, custom) or nothing, if it's a regular room.
    pub room_type: Option<RoomType>,

    /// What's the join rule for this room?
    pub join_rule: SpaceRoomJoinRule,

    /// Is the room world-readable (i.e. is its history_visibility set to
    /// world_readable)?
    pub is_world_readable: bool,

    /// Has the current user been invited/joined/left this room?
    ///
    /// Set to `None` if the room is unknown to the user.
    pub state: Option<RoomState>,

    /// The `m.room.direct` state of the room, if known.
    pub is_direct: Option<bool>,

    /// Room heroes.
    pub heroes: Option<Vec<RoomHero>>,
}

impl RoomPreview {
    /// Constructs a [`RoomPreview`] from the associated room info.
    ///
    /// Note: not using the room info's state/count of joined members, because
    /// we can do better than that.
    fn from_room_info(
        room_info: RoomInfo,
        is_direct: Option<bool>,
        num_joined_members: u64,
        num_active_members: Option<u64>,
        state: Option<RoomState>,
        computed_display_name: Option<String>,
    ) -> Self {
        RoomPreview {
            room_id: room_info.room_id().to_owned(),
            canonical_alias: room_info.canonical_alias().map(ToOwned::to_owned),
            name: computed_display_name.or_else(|| room_info.name().map(ToOwned::to_owned)),
            topic: room_info.topic().map(ToOwned::to_owned),
            avatar_url: room_info.avatar_url().map(ToOwned::to_owned),
            room_type: room_info.room_type().cloned(),
            join_rule: match room_info.join_rule() {
                JoinRule::Invite => SpaceRoomJoinRule::Invite,
                JoinRule::Knock => SpaceRoomJoinRule::Knock,
                JoinRule::Private => SpaceRoomJoinRule::Private,
                JoinRule::Restricted(_) => SpaceRoomJoinRule::Restricted,
                JoinRule::KnockRestricted(_) => SpaceRoomJoinRule::KnockRestricted,
                JoinRule::Public => SpaceRoomJoinRule::Public,
                _ => {
                    // The JoinRule enum is non-exhaustive. Let's do a white lie and pretend it's
                    // private (a cautious choice).
                    SpaceRoomJoinRule::Private
                }
            },
            is_world_readable: *room_info.history_visibility() == HistoryVisibility::WorldReadable,
            num_joined_members,
            num_active_members,
            state,
            is_direct,
            heroes: Some(room_info.heroes().to_vec()),
        }
    }

    /// Create a room preview from a known room we've joined.
    pub(crate) async fn from_joined(room: &Room) -> Self {
        let is_direct = room.is_direct().await.ok();

        let display_name = room.compute_display_name().await.ok().map(|name| name.to_string());

        Self::from_room_info(
            room.clone_info(),
            is_direct,
            room.joined_members_count(),
            Some(room.active_members_count()),
            Some(room.state()),
            display_name,
        )
    }

    #[instrument(skip(client))]
    pub(crate) async fn from_not_joined(
        client: &Client,
        room_id: OwnedRoomId,
        room_or_alias_id: &RoomOrAliasId,
        via: Vec<OwnedServerName>,
    ) -> crate::Result<Self> {
        // Use the room summary endpoint, if available, as described in
        // https://github.com/deepbluev7/matrix-doc/blob/room-summaries/proposals/3266-room-summary.md
        match Self::from_room_summary(client, room_id.clone(), room_or_alias_id, via.clone()).await
        {
            Ok(res) => return Ok(res),
            Err(err) => {
                warn!("error when previewing room from the room summary endpoint: {err}");
            }
        }

        // Try room directory search next.
        match Self::from_room_directory_search(client, &room_id, room_or_alias_id, via).await {
            Ok(Some(res)) => return Ok(res),
            Ok(None) => warn!("Room '{room_or_alias_id}' not found in room directory search."),
            Err(err) => {
                warn!("Searching for '{room_or_alias_id}' in room directory search failed: {err}");
            }
        }

        // Resort to using the room state endpoint, as well as the joined members one.
        Self::from_state_events(client, &room_id).await
    }

    /// Get a [`RoomPreview`] by searching in the room directory for the
    /// provided room alias or room id and transforming the [`RoomDescription`]
    /// into a preview.
    pub(crate) async fn from_room_directory_search(
        client: &Client,
        room_id: &RoomId,
        room_or_alias_id: &RoomOrAliasId,
        via: Vec<OwnedServerName>,
    ) -> crate::Result<Option<Self>> {
        // Get either the room alias or the room id without the leading identifier char
        let search_term = if room_or_alias_id.is_room_alias_id() {
            Some(room_or_alias_id.as_str()[1..].to_owned())
        } else {
            None
        };

        // If we have no alias, filtering using a room id is impossible, so just take
        // the first 100 results and try to find the current room #YOLO
        let batch_size = if search_term.is_some() { 20 } else { 100 };

        if via.is_empty() {
            // Just search in the current homeserver
            search_for_room_preview_in_room_directory(
                client.clone(),
                search_term,
                batch_size,
                None,
                room_id,
            )
            .await
        } else {
            let mut futures = Vec::new();
            // Search for all servers and retrieve the results
            for server in via {
                futures.push(search_for_room_preview_in_room_directory(
                    client.clone(),
                    search_term.clone(),
                    batch_size,
                    Some(server),
                    room_id,
                ));
            }

            let joined_results = join_all(futures).await;

            Ok(joined_results.into_iter().flatten().next().flatten())
        }
    }

    /// Get a [`RoomPreview`] using MSC3266, if available on the remote server.
    ///
    /// Will fail with a 404 if the API is not available.
    ///
    /// This method is exposed for testing purposes; clients should prefer
    /// `Client::get_room_preview` in general over this.
    pub async fn from_room_summary(
        client: &Client,
        room_id: OwnedRoomId,
        room_or_alias_id: &RoomOrAliasId,
        via: Vec<OwnedServerName>,
    ) -> crate::Result<Self> {
        let request = ruma::api::client::room::get_summary::msc3266::Request::new(
            room_or_alias_id.to_owned(),
            via,
        );

        let response = client.send(request, None).await?;

        // The server returns a `Left` room state for rooms the user has not joined. Be
        // more precise than that, and set it to `None` if we haven't joined
        // that room.
        let cached_room = client.get_room(&room_id);
        let state = if cached_room.is_none() {
            None
        } else {
            response.membership.map(|membership| RoomState::from(&membership))
        };

        let num_active_members = cached_room.as_ref().map(|r| r.active_members_count());

        let is_direct = if let Some(cached_room) = &cached_room {
            cached_room.is_direct().await.ok()
        } else {
            None
        };

        Ok(RoomPreview {
            room_id,
            canonical_alias: response.canonical_alias,
            name: response.name,
            topic: response.topic,
            avatar_url: response.avatar_url,
            num_joined_members: response.num_joined_members.into(),
            num_active_members,
            room_type: response.room_type,
            join_rule: response.join_rule,
            is_world_readable: response.world_readable,
            state,
            is_direct,
            heroes: cached_room.map(|r| r.heroes()),
        })
    }

    /// Get a [`RoomPreview`] using the room state endpoint.
    ///
    /// This is always available on a remote server, but will only work if one
    /// of these two conditions is true:
    ///
    /// - the user has joined the room at some point (i.e. they're still joined
    ///   or they've joined it and left it later).
    /// - the room has an history visibility set to world-readable.
    ///
    /// This method is exposed for testing purposes; clients should prefer
    /// `Client::get_room_preview` in general over this.
    pub async fn from_state_events(client: &Client, room_id: &RoomId) -> crate::Result<Self> {
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

        let room = client.get_room(room_id);
        let state = room.as_ref().map(|room| room.state());
        let num_active_members = room.as_ref().map(|r| r.active_members_count());
        let is_direct = if let Some(room) = room { room.is_direct().await.ok() } else { None };

        Ok(Self::from_room_info(
            room_info,
            is_direct,
            num_joined_members,
            num_active_members,
            state,
            None,
        ))
    }
}

async fn search_for_room_preview_in_room_directory(
    client: Client,
    filter: Option<String>,
    batch_size: u32,
    server: Option<OwnedServerName>,
    expected_room_id: &RoomId,
) -> crate::Result<Option<RoomPreview>> {
    let mut directory_search = RoomDirectorySearch::new(client);
    directory_search.search(filter, batch_size, server).await?;

    let (results, _) = directory_search.results();

    for room_description in results {
        // Iterate until we find a room description with a matching room id
        if room_description.room_id != expected_room_id {
            continue;
        }
        return Ok(Some(RoomPreview {
            room_id: room_description.room_id,
            canonical_alias: room_description.alias,
            name: room_description.name,
            topic: room_description.topic,
            avatar_url: room_description.avatar_url,
            num_joined_members: room_description.joined_members,
            num_active_members: None,
            // Assume it's a room
            room_type: None,
            join_rule: match room_description.join_rule {
                PublicRoomJoinRule::Public => SpaceRoomJoinRule::Public,
                PublicRoomJoinRule::Knock => SpaceRoomJoinRule::Knock,
                PublicRoomJoinRule::_Custom(rule) => SpaceRoomJoinRule::_Custom(rule),
                _ => {
                    panic!("Unexpected PublicRoomJoinRule {:?}", room_description.join_rule)
                }
            },
            is_world_readable: room_description.is_world_readable,
            state: None,
            is_direct: None,
            heroes: None,
        }));
    }

    Ok(None)
}
