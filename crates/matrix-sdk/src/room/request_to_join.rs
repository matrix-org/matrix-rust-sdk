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

use js_int::UInt;
use ruma::{EventId, OwnedEventId, OwnedMxcUri, OwnedUserId, RoomId};

use crate::{room::RoomMember, Error, Room};

/// A request to join a room with `knock` join rule.
#[derive(Debug, Clone)]
pub struct JoinRequest {
    room: Room,
    /// The event id of the event containing knock membership change.
    pub event_id: OwnedEventId,
    /// The timestamp when this request was created.
    pub timestamp: Option<UInt>,
    /// Some general room member info to display.
    pub member_info: RequestToJoinMemberInfo,
    /// Whether it's been marked as 'seen' by the client.
    pub is_seen: bool,
}

impl JoinRequest {
    pub(crate) fn new(
        room: &Room,
        event_id: &EventId,
        timestamp: Option<UInt>,
        member: RequestToJoinMemberInfo,
        is_seen: bool,
    ) -> Self {
        Self {
            room: room.clone(),
            event_id: event_id.to_owned(),
            timestamp,
            member_info: member,
            is_seen,
        }
    }

    /// The room id for the `Room` form whose access is requested.
    pub fn room_id(&self) -> &RoomId {
        self.room.room_id()
    }

    /// Marks the request to join as 'seen' so the client can ignore it in the
    /// future.
    pub async fn mark_as_seen(&self) -> Result<(), Error> {
        self.room.mark_join_requests_as_seen(&[self.event_id.to_owned()]).await?;
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

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use matrix_sdk_test::async_test;
    use ruma::{event_id, owned_user_id, room_id, EventId};

    use crate::{
        room::request_to_join::{JoinRequest, RequestToJoinMemberInfo},
        test_utils::mocks::MatrixMockServer,
        Room,
    };

    #[async_test]
    async fn test_mark_as_seen() {
        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;
        let room_id = room_id!("!a:b.c");
        let event_id = event_id!("$a:b.c");

        let room = server.sync_joined_room(&client, room_id).await;

        let join_request = mock_join_request(&room, Some(event_id));

        // When we mark the join request as seen
        join_request.mark_as_seen().await.expect("Failed to mark as seen");

        // Then we can check it was successfully marked as seen from the room
        let seen_ids =
            room.get_seen_join_request_ids().await.expect("Failed to get seen join request ids");
        assert_eq!(seen_ids.len(), 1);
        assert_eq!(seen_ids.into_iter().next().expect("Couldn't load next item"), event_id);
    }

    #[async_test]
    async fn test_accept() {
        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;
        let room_id = room_id!("!a:b.c");

        let room = server.sync_joined_room(&client, room_id).await;

        let join_request = mock_join_request(&room, None);

        // The /invite endpoint must be called once
        server.mock_invite_user_by_id().ok().mock_once().mount().await;

        // When we accept the join request
        join_request.accept().await.expect("Failed to accept the request");
    }

    #[async_test]
    async fn test_decline() {
        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;
        let room_id = room_id!("!a:b.c");

        let room = server.sync_joined_room(&client, room_id).await;

        let join_request = mock_join_request(&room, None);

        // The /kick endpoint must be called once
        server.mock_kick_user().ok().mock_once().mount().await;

        // When we decline the join request
        join_request.decline(None).await.expect("Failed to decline the request");
    }

    #[async_test]
    async fn test_decline_and_ban() {
        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;
        let room_id = room_id!("!a:b.c");

        let room = server.sync_joined_room(&client, room_id).await;

        let join_request = mock_join_request(&room, None);

        // The /ban endpoint must be called once
        server.mock_ban_user().ok().mock_once().mount().await;

        // When we decline the join request and ban the user from the room
        join_request
            .decline_and_ban(None)
            .await
            .expect("Failed to decline the request and ban the user");
    }

    fn mock_join_request(room: &Room, event_id: Option<&EventId>) -> JoinRequest {
        JoinRequest::new(
            room,
            event_id.unwrap_or(event_id!("$a:b.c")),
            None,
            RequestToJoinMemberInfo {
                user_id: owned_user_id!("@alice:b.c"),
                display_name: None,
                avatar_url: None,
                reason: None,
            },
            false,
        )
    }
}
