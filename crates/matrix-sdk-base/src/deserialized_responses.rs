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

use std::{collections::BTreeMap, fmt};

pub use matrix_sdk_common::deserialized_responses::*;
use ruma::{
    events::{
        room::{
            member::{MembershipState, RoomMemberEvent, RoomMemberEventContent},
            power_levels::{RoomPowerLevels, RoomPowerLevelsEventContent},
        },
        AnyStrippedStateEvent, AnySyncStateEvent, EventContentFromType,
        PossiblyRedactedStateEventContent, RedactContent, RedactedStateEventContent,
        StateEventContent, StaticStateEventContent, StrippedStateEvent, SyncStateEvent,
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

/// Wrapper around both versions of any raw state event.
#[derive(Clone, Debug, Serialize)]
#[serde(untagged)]
pub enum RawAnySyncOrStrippedState {
    /// An event from a room in joined or left state.
    Sync(Raw<AnySyncStateEvent>),
    /// An event from a room in invited state.
    Stripped(Raw<AnyStrippedStateEvent>),
}

impl RawAnySyncOrStrippedState {
    /// Try to deserialize the inner JSON as the expected type.
    pub fn deserialize(&self) -> serde_json::Result<AnySyncOrStrippedState> {
        match self {
            Self::Sync(raw) => Ok(AnySyncOrStrippedState::Sync(raw.deserialize()?)),
            Self::Stripped(raw) => Ok(AnySyncOrStrippedState::Stripped(raw.deserialize()?)),
        }
    }

    /// Turns this `RawAnySyncOrStrippedState` into `RawSyncOrStrippedState<C>`
    /// without changing the underlying JSON.
    pub fn cast<C>(self) -> RawSyncOrStrippedState<C>
    where
        C: StaticStateEventContent + RedactContent,
        C::Redacted: RedactedStateEventContent,
    {
        match self {
            Self::Sync(raw) => RawSyncOrStrippedState::Sync(raw.cast()),
            Self::Stripped(raw) => RawSyncOrStrippedState::Stripped(raw.cast()),
        }
    }
}

/// Wrapper around both versions of any state event.
#[derive(Clone, Debug)]
pub enum AnySyncOrStrippedState {
    /// An event from a room in joined or left state.
    Sync(AnySyncStateEvent),
    /// An event from a room in invited state.
    Stripped(AnyStrippedStateEvent),
}

impl AnySyncOrStrippedState {
    /// If this is an `AnySyncStateEvent`, return a reference to the inner
    /// event.
    pub fn as_sync(&self) -> Option<&AnySyncStateEvent> {
        match self {
            Self::Sync(ev) => Some(ev),
            Self::Stripped(_) => None,
        }
    }

    /// If this is an `AnyStrippedStateEvent`, return a reference to the inner
    /// event.
    pub fn as_stripped(&self) -> Option<&AnyStrippedStateEvent> {
        match self {
            Self::Sync(_) => None,
            Self::Stripped(ev) => Some(ev),
        }
    }
}

/// Wrapper around both versions of a raw state event.
#[derive(Clone, Debug, Serialize)]
#[serde(untagged)]
pub enum RawSyncOrStrippedState<C>
where
    C: StaticStateEventContent + RedactContent,
    C::Redacted: RedactedStateEventContent,
{
    /// An event from a room in joined or left state.
    Sync(Raw<SyncStateEvent<C>>),
    /// An event from a room in invited state.
    Stripped(Raw<StrippedStateEvent<C::PossiblyRedacted>>),
}

impl<C> RawSyncOrStrippedState<C>
where
    C: StaticStateEventContent + RedactContent,
    C::Redacted: RedactedStateEventContent + fmt::Debug + Clone,
{
    /// Try to deserialize the inner JSON as the expected type.
    pub fn deserialize(&self) -> serde_json::Result<SyncOrStrippedState<C>>
    where
        C: StaticStateEventContent + EventContentFromType + RedactContent,
        C::Redacted: RedactedStateEventContent<StateKey = C::StateKey> + EventContentFromType,
        C::PossiblyRedacted: PossiblyRedactedStateEventContent + EventContentFromType,
    {
        match self {
            Self::Sync(ev) => Ok(SyncOrStrippedState::Sync(ev.deserialize()?)),
            Self::Stripped(ev) => Ok(SyncOrStrippedState::Stripped(ev.deserialize()?)),
        }
    }
}

/// Raw version of [`MemberEvent`].
pub type RawMemberEvent = RawSyncOrStrippedState<RoomMemberEventContent>;

/// Wrapper around both versions of a state event.
#[derive(Clone, Debug)]
pub enum SyncOrStrippedState<C>
where
    C: StaticStateEventContent + RedactContent,
    C::Redacted: RedactedStateEventContent + fmt::Debug + Clone,
{
    /// An event from a room in joined or left state.
    Sync(SyncStateEvent<C>),
    /// An event from a room in invited state.
    Stripped(StrippedStateEvent<C::PossiblyRedacted>),
}

impl<C> SyncOrStrippedState<C>
where
    C: StaticStateEventContent + RedactContent,
    C::Redacted: RedactedStateEventContent<StateKey = C::StateKey> + fmt::Debug + Clone,
    C::PossiblyRedacted: PossiblyRedactedStateEventContent<StateKey = C::StateKey>,
{
    /// If this is a `SyncStateEvent`, return a reference to the inner event.
    pub fn as_sync(&self) -> Option<&SyncStateEvent<C>> {
        match self {
            Self::Sync(ev) => Some(ev),
            Self::Stripped(_) => None,
        }
    }

    /// If this is a `StrippedStateEvent`, return a reference to the inner
    /// event.
    pub fn as_stripped(&self) -> Option<&StrippedStateEvent<C::PossiblyRedacted>> {
        match self {
            Self::Sync(_) => None,
            Self::Stripped(ev) => Some(ev),
        }
    }

    /// The sender of this event.
    pub fn sender(&self) -> &UserId {
        match self {
            Self::Sync(e) => e.sender(),
            Self::Stripped(e) => &e.sender,
        }
    }

    /// The ID of this event.
    pub fn event_id(&self) -> Option<&EventId> {
        match self {
            Self::Sync(e) => Some(e.event_id()),
            Self::Stripped(_) => None,
        }
    }

    /// The server timestamp of this event.
    pub fn origin_server_ts(&self) -> Option<MilliSecondsSinceUnixEpoch> {
        match self {
            Self::Sync(e) => Some(e.origin_server_ts()),
            Self::Stripped(_) => None,
        }
    }

    /// The state key associated to this state event.
    pub fn state_key(&self) -> &C::StateKey {
        match self {
            Self::Sync(e) => e.state_key(),
            Self::Stripped(e) => &e.state_key,
        }
    }
}

impl<C> SyncOrStrippedState<C>
where
    C: StaticStateEventContent<PossiblyRedacted = C>
        + RedactContent
        + PossiblyRedactedStateEventContent,
    C::Redacted: RedactedStateEventContent<StateKey = <C as StateEventContent>::StateKey>
        + fmt::Debug
        + Clone,
{
    /// The inner content of the wrapped event.
    pub fn original_content(&self) -> Option<&C> {
        match self {
            Self::Sync(e) => e.as_original().map(|e| &e.content),
            Self::Stripped(e) => Some(&e.content),
        }
    }
}

/// Wrapper around both MemberEvent-Types
pub type MemberEvent = SyncOrStrippedState<RoomMemberEventContent>;

impl MemberEvent {
    /// The membership state of the user.
    pub fn membership(&self) -> &MembershipState {
        match self {
            MemberEvent::Sync(e) => e.membership(),
            MemberEvent::Stripped(e) => &e.content.membership,
        }
    }

    /// The user id associated to this member event.
    pub fn user_id(&self) -> &UserId {
        self.state_key()
    }

    /// The name that should be displayed for this member event.
    ///
    /// It there is no `displayname` in the event's content, the localpart or
    /// the user ID is returned.
    pub fn display_name(&self) -> &str {
        self.original_content()
            .and_then(|c| c.displayname.as_deref())
            .unwrap_or_else(|| self.user_id().localpart())
    }
}

impl SyncOrStrippedState<RoomPowerLevelsEventContent> {
    /// The power levels of the event.
    pub fn power_levels(&self) -> RoomPowerLevels {
        match self {
            Self::Sync(e) => e.power_levels(),
            Self::Stripped(e) => e.power_levels(),
        }
    }
}
