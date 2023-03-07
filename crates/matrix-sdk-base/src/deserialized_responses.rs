// Copyright 2022 The Matrix.org Foundation C.I.C.
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

//! SDK-specific variations of response types from Ruma.

use std::collections::BTreeMap;

pub use matrix_sdk_common::deserialized_responses::*;
use ruma::{
    events::room::member::{
        MembershipState, RoomMemberEvent, RoomMemberEventContent, StrippedRoomMemberEvent,
        SyncRoomMemberEvent,
    },
    serde::Raw,
    EventId, MilliSecondsSinceUnixEpoch, OwnedEventId, OwnedRoomId, OwnedUserId, UserId,
};
use serde::Serialize;

/// A change in ambiguity of room members that an `m.room.member` event
/// triggers.
#[derive(Clone, Debug, Default)]
#[non_exhaustive]
pub struct AmbiguityChange {
    /// Is the member that is contained in the state key of the `m.room.member`
    /// event itself ambiguous because of the event.
    pub member_ambiguous: bool,
    /// Has another user been disambiguated because of this event.
    pub disambiguated_member: Option<OwnedUserId>,
    /// Has another user become ambiguous because of this event.
    pub ambiguated_member: Option<OwnedUserId>,
}

/// Collection of ambiguity changes that room member events trigger.
#[derive(Clone, Debug, Default)]
#[non_exhaustive]
pub struct AmbiguityChanges {
    /// A map from room id to a map of an event id to the `AmbiguityChange` that
    /// the event with the given id caused.
    pub changes: BTreeMap<OwnedRoomId, BTreeMap<OwnedEventId, AmbiguityChange>>,
}

/// A deserialized response for the rooms members API call.
///
/// [`GET /_matrix/client/r0/rooms/{roomId}/members`](https://spec.matrix.org/v1.5/client-server-api/#get_matrixclientv3roomsroomidmembers)
#[derive(Clone, Debug, Default)]
pub struct MembersResponse {
    /// The list of members events.
    pub chunk: Vec<RoomMemberEvent>,
    /// Collection of ambiguity changes that room member events trigger.
    pub ambiguity_changes: AmbiguityChanges,
}

/// Raw version of [`MemberEvent`].
#[derive(Debug, Serialize)]
#[serde(untagged)]
pub enum RawMemberEvent {
    /// A member event from a room in joined or left state.
    Sync(Raw<SyncRoomMemberEvent>),
    /// A member event from a room in invited state.
    Stripped(Raw<StrippedRoomMemberEvent>),
}

impl RawMemberEvent {
    /// Try to deserialize the inner JSON as the expected type.
    pub fn deserialize(&self) -> serde_json::Result<MemberEvent> {
        match self {
            Self::Sync(e) => Ok(MemberEvent::Sync(e.deserialize()?)),
            Self::Stripped(e) => Ok(MemberEvent::Stripped(e.deserialize()?)),
        }
    }
}

/// Wrapper around both MemberEvent-Types
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug)]
pub enum MemberEvent {
    /// A member event from a room in joined or left state.
    Sync(SyncRoomMemberEvent),
    /// A member event from a room in invited state.
    Stripped(StrippedRoomMemberEvent),
}

impl MemberEvent {
    /// The inner Content of the wrapped Event
    pub fn original_content(&self) -> Option<&RoomMemberEventContent> {
        match self {
            MemberEvent::Sync(e) => e.as_original().map(|e| &e.content),
            MemberEvent::Stripped(e) => Some(&e.content),
        }
    }

    /// The sender of this event.
    pub fn sender(&self) -> &UserId {
        match self {
            MemberEvent::Sync(e) => e.sender(),
            MemberEvent::Stripped(e) => &e.sender,
        }
    }

    /// The ID of this event.
    pub fn event_id(&self) -> Option<&EventId> {
        match self {
            MemberEvent::Sync(e) => Some(e.event_id()),
            MemberEvent::Stripped(_) => None,
        }
    }

    /// The Server Timestamp of this event.
    pub fn origin_server_ts(&self) -> Option<MilliSecondsSinceUnixEpoch> {
        match self {
            MemberEvent::Sync(e) => Some(e.origin_server_ts()),
            MemberEvent::Stripped(_) => None,
        }
    }

    /// The membership state of the user
    pub fn membership(&self) -> &MembershipState {
        match self {
            MemberEvent::Sync(e) => e.membership(),
            MemberEvent::Stripped(e) => &e.content.membership,
        }
    }

    /// The user id associated to this member event
    pub fn user_id(&self) -> &UserId {
        match self {
            MemberEvent::Sync(e) => e.state_key(),
            MemberEvent::Stripped(e) => &e.state_key,
        }
    }
}
