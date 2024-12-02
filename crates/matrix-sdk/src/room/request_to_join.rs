use std::sync::Arc;

use js_int::UInt;
use ruma::{EventId, OwnedEventId, OwnedMxcUri, OwnedUserId, RoomId};

use crate::{room::RoomMember, Error, Room};

/// A request to join a room with `knock` join rule.
#[derive(Debug, Clone)]
pub struct RequestToJoinRoom {
    room: Arc<Room>,
    /// The event id of the event containing knock membership change.
    pub event_id: OwnedEventId,
    /// The timestamp when this request was created.
    pub timestamp: Option<UInt>,
    /// Some general room member info to display.
    pub member_info: RequestToJoinMemberInfo,
    /// Whether it's been marked as 'seen' by the client.
    pub is_seen: bool,
}

impl RequestToJoinRoom {
    pub(crate) fn new(
        room: Arc<Room>,
        event_id: &EventId,
        timestamp: Option<UInt>,
        member: RequestToJoinMemberInfo,
        is_seen: bool,
    ) -> Self {
        Self { room, event_id: event_id.to_owned(), timestamp, member_info: member, is_seen }
    }

    /// The room id for the `Room` form whose access is requested.
    pub fn room_id(&self) -> &RoomId {
        self.room.room_id()
    }

    /// Marks the request to join as 'seen' so the client can ignore it in the
    /// future.
    pub async fn mark_as_seen(&mut self) -> Result<(), Error> {
        self.room.mark_requests_to_join_as_seen(&[self.event_id.to_owned()]).await?;
        Ok(())
    }

    /// Accepts the request to join by inviting the user to the room.
    pub async fn accept(&self) -> Result<(), Error> {
        self.room.invite_user_by_id(&self.member_info.user_id).await
    }

    /// Declines the request to join by kicking the user from the room, with an
    /// optional reason.
    pub async fn decline(&self, reason: Option<&str>) -> Result<(), Error> {
        self.room.kick_user(&self.member_info.user_id, reason).await
    }

    /// Declines the request to join by banning the user from the room, with an
    /// optional reason.
    pub async fn decline_and_ban(&self, reason: Option<&str>) -> Result<(), Error> {
        self.room.ban_user(&self.member_info.user_id, reason).await
    }
}

/// General room member info to display along with the join request.
#[derive(Debug, Clone)]
pub struct RequestToJoinMemberInfo {
    /// The user id for the room member requesting access.
    pub user_id: OwnedUserId,
    /// The optional display name of the room member requesting access.
    pub display_name: Option<String>,
    /// The optional avatar url of the room member requesting access.
    pub avatar_url: Option<OwnedMxcUri>,
    /// An optional reason why the user wants access to the room.
    pub reason: Option<String>,
}

impl From<RoomMember> for RequestToJoinMemberInfo {
    fn from(member: RoomMember) -> Self {
        Self {
            user_id: member.user_id().to_owned(),
            display_name: member.display_name().map(ToOwned::to_owned),
            avatar_url: member.avatar_url().map(ToOwned::to_owned),
            reason: member.event().reason().map(ToOwned::to_owned),
        }
    }
}
