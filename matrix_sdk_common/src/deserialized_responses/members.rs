use serde::{Deserialize, Serialize};
use std::{convert::TryFrom, time::SystemTime};

use crate::{
    events::{
        room::member::MemberEventContent, StateEvent, StrippedStateEvent, SyncStateEvent, Unsigned,
    },
    identifiers::{EventId, UserId},
};

use super::AmbiguityChanges;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(
    try_from = "SyncStateEvent<MemberEventContent>",
    into = "SyncStateEvent<MemberEventContent>"
)]
pub struct MemberEvent {
    pub content: MemberEventContent,
    pub event_id: EventId,
    pub origin_server_ts: SystemTime,
    pub prev_content: Option<MemberEventContent>,
    pub sender: UserId,
    pub state_key: UserId,
    pub unsigned: Unsigned,
}

impl TryFrom<SyncStateEvent<MemberEventContent>> for MemberEvent {
    type Error = crate::identifiers::Error;

    fn try_from(event: SyncStateEvent<MemberEventContent>) -> Result<Self, Self::Error> {
        Ok(MemberEvent {
            content: event.content,
            event_id: event.event_id,
            origin_server_ts: event.origin_server_ts,
            prev_content: event.prev_content,
            sender: event.sender,
            state_key: UserId::try_from(event.state_key)?,
            unsigned: event.unsigned,
        })
    }
}

impl TryFrom<StateEvent<MemberEventContent>> for MemberEvent {
    type Error = crate::identifiers::Error;

    fn try_from(event: StateEvent<MemberEventContent>) -> Result<Self, Self::Error> {
        Ok(MemberEvent {
            content: event.content,
            event_id: event.event_id,
            origin_server_ts: event.origin_server_ts,
            prev_content: event.prev_content,
            sender: event.sender,
            state_key: UserId::try_from(event.state_key)?,
            unsigned: event.unsigned,
        })
    }
}

impl Into<SyncStateEvent<MemberEventContent>> for MemberEvent {
    fn into(self) -> SyncStateEvent<MemberEventContent> {
        SyncStateEvent {
            content: self.content,
            event_id: self.event_id,
            sender: self.sender,
            origin_server_ts: self.origin_server_ts,
            state_key: self.state_key.to_string(),
            prev_content: self.prev_content,
            unsigned: self.unsigned,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(
    try_from = "StrippedStateEvent<MemberEventContent>",
    into = "StrippedStateEvent<MemberEventContent>"
)]
pub struct StrippedMemberEvent {
    pub content: MemberEventContent,
    pub sender: UserId,
    pub state_key: UserId,
}

impl TryFrom<StrippedStateEvent<MemberEventContent>> for StrippedMemberEvent {
    type Error = crate::identifiers::Error;

    fn try_from(event: StrippedStateEvent<MemberEventContent>) -> Result<Self, Self::Error> {
        Ok(StrippedMemberEvent {
            content: event.content,
            sender: event.sender,
            state_key: UserId::try_from(event.state_key)?,
        })
    }
}

impl Into<StrippedStateEvent<MemberEventContent>> for StrippedMemberEvent {
    fn into(self) -> StrippedStateEvent<MemberEventContent> {
        StrippedStateEvent {
            content: self.content,
            sender: self.sender,
            state_key: self.state_key.to_string(),
        }
    }
}

/// A deserialized response for the rooms members API call.
///
/// [GET /_matrix/client/r0/rooms/{roomId}/members](https://matrix.org/docs/spec/client_server/r0.6.0#get-matrix-client-r0-rooms-roomid-members)
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct MembersResponse {
    /// The list of members events.
    pub chunk: Vec<MemberEvent>,
    /// Collection of ambiguioty changes that room member events trigger.
    pub ambiguity_changes: AmbiguityChanges,
}
