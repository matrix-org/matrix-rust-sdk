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
    api::client::{
        membership::{joined_members, leave_room},
        state::get_state_events,
    },
    events::room::{history_visibility::HistoryVisibility, join_rules::JoinRule},
    room::RoomType,
    space::SpaceRoomJoinRule,
    OwnedMxcUri, OwnedRoomAliasId, OwnedRoomId, OwnedRoomOrAliasId, OwnedServerName, RoomId,
    RoomOrAliasId,
};
use tokio::try_join;
use tracing::{instrument, warn};

use crate::{Client, Room};

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
}

impl RoomPreview {
    /// Constructs a [`RoomPreview`] from the associated room info.
    ///
    /// Note: not using the room info's state/count of joined members, because
    /// we can do better than that.
    async fn from_room_info(
        client: &Client,
        room_info: RoomInfo,
        num_joined_members: u64,
        state: Option<RoomState>,
    ) -> Self {
        let is_direct = if let Some(room) = client.get_room(room_info.room_id()) {
            room.is_direct().await.ok()
        } else {
            None
        };
        RoomPreview {
            room_id: room_info.room_id().to_owned(),
            canonical_alias: room_info.canonical_alias().map(ToOwned::to_owned),
            name: room_info.name().map(ToOwned::to_owned),
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
            state,
            is_direct,
        }
    }

    /// Create a room preview from a known room (i.e. one we've been invited to,
    /// we've joined or we've left).
    pub(crate) async fn from_known(room: &Room) -> Self {
        Self::from_room_info(
            &room.client,
            room.clone_info(),
            room.joined_members_count(),
            Some(room.state()),
        )
        .await
    }

    #[instrument(skip(client))]
    pub(crate) async fn from_unknown(
        client: &Client,
        room_id: OwnedRoomId,
        room_or_alias_id: &RoomOrAliasId,
        via: Vec<OwnedServerName>,
    ) -> crate::Result<Self> {
        // Use the room summary endpoint, if available, as described in
        // https://github.com/deepbluev7/matrix-doc/blob/room-summaries/proposals/3266-room-summary.md
        match Self::from_room_summary(client, room_id.clone(), room_or_alias_id, via).await {
            Ok(res) => return Ok(res),
            Err(err) => {
                warn!("error when previewing room from the room summary endpoint: {err}");
            }
        }

        // TODO: (optimization) Use the room search directory, if available:
        // - if the room directory visibility is public,
        // - then use a public room filter set to this room id

        // Resort to using the room state endpoint, as well as the joined members one.
        Self::from_state_events(client, &room_id).await
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

        let is_direct = if let Some(cached_room) = cached_room {
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
            room_type: response.room_type,
            join_rule: response.join_rule,
            is_world_readable: response.world_readable,
            state,
            is_direct,
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

        let state = client.get_room(room_id).map(|room| room.state());

        Ok(Self::from_room_info(client, room_info, num_joined_members, state).await)
    }
}

/// A set of actions that can be performed in a [`RoomPreview`].
#[derive(Debug)]
pub struct RoomPreviewActions {
    /// Action used to join a room.
    pub join: JoinRoomAction,
    /// Action used to leave a room.
    pub leave: LeaveRoomAction,
    /// Action used to knock on a room.
    pub knock: KnockRoomAction,
}

/// A helper to join a room and perform any needed post-processing if it
/// succeeds.
#[derive(Debug)]
pub struct JoinRoomAction {
    client: Client,
    room_id: OwnedRoomId,
    room_state: Option<RoomState>,
    is_direct: bool,
}

impl RoomPreviewActions {
    /// Create a new instance given a [`Client`] to perform the actions and a
    /// [`RoomPreview`] to get the needed data.
    pub fn new(client: Client, room_preview: RoomPreview) -> Self {
        let room_id = room_preview.room_id;
        let room_state = room_preview.state;
        let is_direct = room_preview.is_direct.unwrap_or_default();
        let join = JoinRoomAction {
            client: client.clone(),
            room_id: room_id.clone(),
            room_state,
            is_direct,
        };
        let room_or_alias_id: OwnedRoomOrAliasId =
            if let Some(alias) = &room_preview.canonical_alias {
                alias.to_owned().into()
            } else {
                room_id.clone().into()
            };
        let leave = LeaveRoomAction {
            client: client.clone(),
            room_id,
            room_state,
        };
        let knock = KnockRoomAction { client, room_or_alias_id, room_state };
        Self { join, leave, knock }
    }
}

impl JoinRoomAction {
    /// Join a room.
    pub async fn run(&self) -> Result<(), RoomStateActionError> {
        if matches!(self.room_state, Some(RoomState::Invited) | None) {
            match self.client.join_room_by_id(&self.room_id).await {
                Ok(room) => {
                    if self.is_direct {
                        if let Some(e) = room.set_is_direct(true).await.err() {
                            warn!("error when setting room as direct: {e}");
                        }
                    }
                    Ok(())
                }
                Err(e) => Err(RoomStateActionError::from_generic(e)),
            }
        } else {
            Err(RoomStateActionError::WrongRoomState(WrongRoomState::new(
                "None or Invited",
                self.room_state,
            )))
        }
    }
}

/// A helper to leave a room and perform any post-processing needed.
#[derive(Debug)]
pub struct LeaveRoomAction {
    client: Client,
    room_id: OwnedRoomId,
    room_state: Option<RoomState>,
}

impl LeaveRoomAction {
    /// Leave a room.
    pub async fn run(&self) -> Result<(), RoomStateActionError> {
        let Some(state) = self.room_state else {
            return Err(RoomStateActionError::WrongRoomState(WrongRoomState::new(
                "Known room state (not None).",
                None,
            )));
        };

        match state {
            RoomState::Left => Err(RoomStateActionError::WrongRoomState(WrongRoomState::new(
                "Joined, Invited or Knocked",
                Some(state),
            ))),
            _ => {
                let request = leave_room::v3::Request::new(self.room_id.to_owned());
                self.client
                    .send(request, None)
                    .await
                    .map_err(RoomStateActionError::from_generic)?;
                self.client
                    .base_client()
                    .room_left(&self.room_id)
                    .await
                    .map_err(RoomStateActionError::from_generic)
            }
        }
    }
}

/// A helper to leave a room and perform any post-processing needed.
#[derive(Debug)]
pub struct KnockRoomAction {
    client: Client,
    room_or_alias_id: OwnedRoomOrAliasId,
    room_state: Option<RoomState>,
}

impl KnockRoomAction {
    /// Knock on a room.
    pub async fn run(
        &self,
        reason: Option<String>,
        via: Vec<OwnedServerName>,
    ) -> Result<(), RoomStateActionError> {
        let id = self.room_or_alias_id.clone();
        match self.room_state {
            Some(RoomState::Left) | None => {
                self.client
                    .knock(id, reason, via)
                    .await
                    .map_err(RoomStateActionError::from_generic)?;
                Ok(())
            }
            _ => Err(RoomStateActionError::WrongRoomState(WrongRoomState::new(
                "Left or None",
                self.room_state,
            ))),
        }
    }
}

/// Errors to be returned when join/leave room actions fail.
#[derive(thiserror::Error, Debug)]
pub enum RoomStateActionError {
    /// Attempted to call a method on a room that requires the user to
    /// have a specific membership state, but the membership state is
    /// different.
    #[error("wrong room state: {0}")]
    WrongRoomState(WrongRoomState),

    /// An error type happened in an underlying layer, we wrap it.
    #[error(transparent)]
    Generic(#[from] Box<dyn std::error::Error + Send + Sync>),
}

impl RoomStateActionError {
    fn from_generic<E: std::error::Error + Send + Sync + 'static>(err: E) -> Self {
        Self::Generic(Box::new(err))
    }
}

/// An unexpected room preview state was found.
#[derive(Debug, thiserror::Error)]
#[error("expected: {expected}, got: {got:?}")]
pub struct WrongRoomState {
    expected: &'static str,
    got: Option<RoomState>,
}

impl WrongRoomState {
    pub(crate) fn new(expected: &'static str, got: Option<RoomState>) -> Self {
        Self { expected, got }
    }
}
