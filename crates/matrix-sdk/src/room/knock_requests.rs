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

use crate::{Error, Room, room::RoomMember};

/// A request to join a room with `knock` join rule.
#[derive(Debug, Clone)]
pub struct KnockRequest {
    room: Room,
    /// The event id of the event containing knock membership change.
    pub event_id: OwnedEventId,
    /// The timestamp when this request was created.
    pub timestamp: Option<UInt>,
    /// Some general room member info to display.
    pub member_info: KnockRequestMemberInfo,
    /// Whether it's been marked as 'seen' by the client.
    pub is_seen: bool,
}

impl KnockRequest {
    pub(crate) fn new(
        room: &Room,
        event_id: &EventId,
        timestamp: Option<UInt>,
        member: KnockRequestMemberInfo,
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

    /// The room id for the `Room` from whose access is requested.
    pub fn room_id(&self) -> &RoomId {
        self.room.room_id()
    }

    /// Marks the knock request as 'seen' so the client can ignore it in the
    /// future.
    pub async fn mark_as_seen(&self) -> Result<(), Error> {
        self.room.mark_knock_requests_as_seen(&[self.member_info.user_id.to_owned()]).await?;
        Ok(())
    }

    /// Accepts the knock request by inviting the user to the room.
    pub async fn accept(&self) -> Result<(), Error> {
        self.room.invite_user_by_id(&self.member_info.user_id).await
    }

    /// Declines the knock request by kicking the user from the room, with an
    /// optional reason.
    pub async fn decline(&self, reason: Option<&str>) -> Result<(), Error> {
        self.room.kick_user(&self.member_info.user_id, reason).await
    }

    /// Declines the knock request by banning the user from the room, with an
    /// optional reason.
    pub async fn decline_and_ban(&self, reason: Option<&str>) -> Result<(), Error> {
        self.room.ban_user(&self.member_info.user_id, reason).await
    }
}

/// General room member info to display along with the join request.
#[derive(Debug, Clone)]
pub struct KnockRequestMemberInfo {
    /// The user id for the room member requesting access.
    pub user_id: OwnedUserId,
    /// The optional display name of the room member requesting access.
    pub display_name: Option<String>,
    /// The optional avatar url of the room member requesting access.
    pub avatar_url: Option<OwnedMxcUri>,
    /// An optional reason why the user wants access to the room.
    pub reason: Option<String>,
}

impl KnockRequestMemberInfo {
    pub(crate) fn from_member(member: &RoomMember) -> Self {
        Self {
            user_id: member.user_id().to_owned(),
            display_name: member.display_name().map(ToOwned::to_owned),
            avatar_url: member.avatar_url().map(ToOwned::to_owned),
            reason: member.event().reason().map(ToOwned::to_owned),
        }
    }
}

// The http mocking library is not supported for wasm32
#[cfg(all(test, not(target_family = "wasm")))]
mod tests {
    use matrix_sdk_test::{JoinedRoomBuilder, async_test, event_factory::EventFactory};
    use ruma::{
        EventId, event_id, events::room::member::MembershipState, owned_user_id, room_id, user_id,
    };

    use crate::{
        Room,
        room::knock_requests::{KnockRequest, KnockRequestMemberInfo},
        test_utils::mocks::MatrixMockServer,
    };

    #[async_test]
    async fn test_mark_as_seen() {
        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;
        let room_id = room_id!("!a:b.c");
        let event_id = event_id!("$a:b.c");
        let user_id = user_id!("@alice:b.c");

        let f = EventFactory::new().room(room_id);
        let joined_room_builder = JoinedRoomBuilder::new(room_id).add_state_bulk(vec![
            f.member(user_id).membership(MembershipState::Knock).event_id(event_id).into(),
        ]);
        let room = server.sync_room(&client, joined_room_builder).await;

        let knock_request = make_knock_request(&room, Some(event_id));

        // When we mark the knock request as seen
        knock_request.mark_as_seen().await.expect("Failed to mark as seen");

        // Then we can check it was successfully marked as seen from the room
        let seen_ids =
            room.get_seen_knock_request_ids().await.expect("Failed to get seen join request ids");
        assert_eq!(seen_ids.len(), 1);
        assert_eq!(
            seen_ids.into_iter().next().expect("Couldn't load next item"),
            (event_id.to_owned(), user_id.to_owned())
        );
    }

    #[async_test]
    async fn test_accept() {
        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;
        let room_id = room_id!("!a:b.c");

        let room = server.sync_joined_room(&client, room_id).await;

        let knock_request = make_knock_request(&room, None);

        // The /invite endpoint must be called once
        server.mock_invite_user_by_id().ok().mock_once().mount().await;

        // When we accept the knock request
        knock_request.accept().await.expect("Failed to accept the request");
    }

    #[async_test]
    async fn test_decline() {
        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;
        let room_id = room_id!("!a:b.c");

        let room = server.sync_joined_room(&client, room_id).await;

        let knock_request = make_knock_request(&room, None);

        // The /kick endpoint must be called once
        server.mock_kick_user().ok().mock_once().mount().await;

        // When we decline the knock request
        knock_request.decline(None).await.expect("Failed to decline the request");
    }

    #[async_test]
    async fn test_decline_and_ban() {
        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;
        let room_id = room_id!("!a:b.c");

        let room = server.sync_joined_room(&client, room_id).await;

        let knock_request = make_knock_request(&room, None);

        // The /ban endpoint must be called once
        server.mock_ban_user().ok().mock_once().mount().await;

        // When we decline the knock request and ban the user from the room
        knock_request
            .decline_and_ban(None)
            .await
            .expect("Failed to decline the request and ban the user");
    }

    fn make_knock_request(room: &Room, event_id: Option<&EventId>) -> KnockRequest {
        KnockRequest::new(
            room,
            event_id.unwrap_or(event_id!("$a:b.c")),
            None,
            KnockRequestMemberInfo {
                user_id: owned_user_id!("@alice:b.c"),
                display_name: None,
                avatar_url: None,
                reason: None,
            },
            false,
        )
    }
}
