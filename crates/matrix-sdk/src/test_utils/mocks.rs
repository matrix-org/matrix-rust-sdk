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

//! Helpers to mock a server and have a client automatically connected to that
//! server, for the purpose of integration tests.

#![allow(missing_debug_implementations)]

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

use matrix_sdk_base::deserialized_responses::TimelineEvent;
use matrix_sdk_test::{
    test_json, InvitedRoomBuilder, JoinedRoomBuilder, KnockedRoomBuilder, LeftRoomBuilder,
    SyncResponseBuilder,
};
use percent_encoding::{AsciiSet, CONTROLS};
use ruma::{
    api::client::room::Visibility,
    directory::PublicRoomsChunk,
    events::{
        room::member::RoomMemberEvent, AnyStateEvent, AnyTimelineEvent, MessageLikeEventType,
        StateEventType,
    },
    serde::Raw,
    time::Duration,
    MxcUri, OwnedEventId, OwnedRoomId, RoomId, ServerName,
};
use serde::Deserialize;
use serde_json::{json, Value};
use wiremock::{
    matchers::{body_partial_json, header, method, path, path_regex, query_param},
    Mock, MockBuilder, MockGuard, MockServer, Request, Respond, ResponseTemplate, Times,
};

use super::client::MockClientBuilder;
use crate::{Client, OwnedServerName, Room};

/// A [`wiremock`] [`MockServer`] along with useful methods to help mocking
/// Matrix client-server API endpoints easily.
///
/// It implements mock endpoints, limiting the shared code as much as possible,
/// so the mocks are still flexible to use as scoped/unscoped mounts, named, and
/// so on.
///
/// It works like this:
///
/// * start by saying which endpoint you'd like to mock, e.g.
///   [`Self::mock_room_send()`]. This returns a specialized [`MockEndpoint`]
///   data structure, with its own impl. For this example, it's
///   `MockEndpoint<RoomSendEndpoint>`.
/// * configure the response on the endpoint-specific mock data structure. For
///   instance, if you want the sending to result in a transient failure, call
///   [`MockEndpoint::error500`]; if you want it to succeed and return the event
///   `$42`, call [`MockEndpoint::ok()`]. It's still possible to call
///   [`MockEndpoint::respond_with()`], as we do with wiremock MockBuilder, for
///   maximum flexibility when the helpers aren't sufficient.
/// * once the endpoint's response is configured, for any mock builder, you get
///   a [`MatrixMock`]; this is a plain [`wiremock::Mock`] with the server
///   curried, so one doesn't have to pass it around when calling
///   [`MatrixMock::mount()`] or [`MatrixMock::mount_as_scoped()`]. As such, it
///   mostly defers its implementations to [`wiremock::Mock`] under the hood.
///
/// # Examples
///
/// ```
/// # tokio_test::block_on(async {
/// use matrix_sdk::{ruma::{room_id, event_id}, test_utils::mocks::MatrixMockServer};
/// use serde_json::json;
///
/// // First create the mock server and client pair.
/// let mock_server = MatrixMockServer::new().await;
/// let client = mock_server.client_builder().build().await;
///
/// // Let's say that our rooms are not encrypted.
/// mock_server.mock_room_state_encryption().plain().mount().await;
///
/// // Let us get a room where we will send an event.
/// let room = mock_server
///     .sync_joined_room(&client, room_id!("!room_id:localhost"))
///     .await;
///
/// // Now we mock the endpoint so we can actually send the event.
/// let event_id = event_id!("$some_id");
/// let send_guard = mock_server
///     .mock_room_send()
///     .ok(event_id)
///     .expect(1)
///     .mount_as_scoped()
///     .await;
///
/// // And we send it out.
/// let response = room.send_raw("m.room.message", json!({ "body": "Hello world" })).await?;
///
/// assert_eq!(
///     event_id,
///     response.event_id,
///     "The event ID we mocked should match the one we received when we sent the event"
/// );
/// # anyhow::Ok(()) });
/// ```
pub struct MatrixMockServer {
    server: MockServer,

    /// Make the sync response builder stateful, to keep in memory the batch
    /// token and avoid the client ignoring subsequent responses after the first
    /// one.
    sync_response_builder: Arc<Mutex<SyncResponseBuilder>>,
}

impl MatrixMockServer {
    /// Create a new [`wiremock`] server specialized for Matrix usage.
    pub async fn new() -> Self {
        let server = MockServer::start().await;
        Self { server, sync_response_builder: Default::default() }
    }

    /// Creates a new [`MatrixMockServer`] from a [`wiremock`] server.
    pub fn from_server(server: MockServer) -> Self {
        Self { server, sync_response_builder: Default::default() }
    }

    /// Creates a new [`MockClientBuilder`] configured to use this server,
    /// preconfigured with a session expected by the server endpoints.
    pub fn client_builder(&self) -> MockClientBuilder {
        MockClientBuilder::new(self.server.uri())
    }

    /// Return the underlying [`wiremock`] server.
    pub fn server(&self) -> &MockServer {
        &self.server
    }

    /// Overrides the sync/ endpoint with knowledge that the given
    /// invited/joined/knocked/left room exists, runs a sync and returns the
    /// given room.
    ///
    /// # Examples
    ///
    /// ```
    /// # tokio_test::block_on(async {
    /// use matrix_sdk::{ruma::{room_id, event_id}, test_utils::mocks::MatrixMockServer};
    /// use matrix_sdk_test::LeftRoomBuilder;
    ///
    /// let mock_server = MatrixMockServer::new().await;
    /// let client = mock_server.client_builder().build().await;
    ///
    /// let left_room = mock_server
    ///     .sync_room(&client, LeftRoomBuilder::new(room_id!("!room_id:localhost")))
    ///     .await;
    /// # anyhow::Ok(()) });
    pub async fn sync_room(&self, client: &Client, room_data: impl Into<AnyRoomBuilder>) -> Room {
        let any_room = room_data.into();
        let room_id = any_room.room_id().to_owned();

        self.mock_sync()
            .ok_and_run(client, move |builder| match any_room {
                AnyRoomBuilder::Invited(invited) => {
                    builder.add_invited_room(invited);
                }
                AnyRoomBuilder::Joined(joined) => {
                    builder.add_joined_room(joined);
                }
                AnyRoomBuilder::Left(left) => {
                    builder.add_left_room(left);
                }
                AnyRoomBuilder::Knocked(knocked) => {
                    builder.add_knocked_room(knocked);
                }
            })
            .await;

        client.get_room(&room_id).expect("look at me, the room is known now")
    }

    /// Overrides the sync/ endpoint with knowledge that the given room exists
    /// in the joined state, runs a sync and returns the given room.
    ///
    /// # Examples
    ///
    /// ```
    /// # tokio_test::block_on(async {
    /// use matrix_sdk::{ruma::room_id, test_utils::mocks::MatrixMockServer};
    ///
    /// let mock_server = MatrixMockServer::new().await;
    /// let client = mock_server.client_builder().build().await;
    ///
    /// let room = mock_server
    ///     .sync_joined_room(&client, room_id!("!room_id:localhost"))
    ///     .await;
    /// # anyhow::Ok(()) });
    pub async fn sync_joined_room(&self, client: &Client, room_id: &RoomId) -> Room {
        self.sync_room(client, JoinedRoomBuilder::new(room_id)).await
    }

    /// Verify that the previous mocks expected number of requests match
    /// reality, and then cancels all active mocks.
    ///
    /// # Examples
    ///
    /// ```
    /// # tokio_test::block_on(async {
    /// use matrix_sdk::{ruma::{room_id, event_id}, test_utils::mocks::MatrixMockServer};
    /// use serde_json::json;
    ///
    /// let mock_server = MatrixMockServer::new().await;
    /// let client = mock_server.client_builder().build().await;
    ///
    /// mock_server.mock_room_state_encryption().plain().mount().await;
    /// let room = mock_server
    ///     .sync_joined_room(&client, room_id!("!room_id:localhost"))
    ///     .await;
    /// mock_server.mock_room_send().ok(event_id!("$some_id")).mount().await;
    ///
    /// // This will succeed.
    /// let response = room.send_raw("m.room.message", json!({ "body": "Hello world" })).await?;
    ///
    /// // Now we reset the mocks.
    /// mock_server.verify_and_reset().await;
    ///
    /// // And we can't send anymore.
    /// let response = room
    ///     .send_raw("m.room.message", json!({ "body": "Hello world" }))
    ///     .await
    ///     .expect_err("We removed the mock so sending should now fail");
    /// # anyhow::Ok(()) });
    /// ```
    pub async fn verify_and_reset(&self) {
        self.server.verify().await;
        self.server.reset().await;
    }
}

// Specific mount endpoints.
impl MatrixMockServer {
    /// Mocks a sync endpoint.
    ///
    /// # Examples
    ///
    /// ```
    /// # tokio_test::block_on(async {
    /// use matrix_sdk::{ruma::room_id, test_utils::mocks::MatrixMockServer};
    /// use matrix_sdk_test::JoinedRoomBuilder;
    ///
    /// // First create the mock server and client pair.
    /// let mock_server = MatrixMockServer::new().await;
    /// let client = mock_server.client_builder().build().await;
    /// let room_id = room_id!("!room_id:localhost");
    ///
    /// // Let's emulate what `MatrixMockServer::sync_joined_room()` does.
    /// mock_server
    ///     .mock_sync()
    ///     .ok_and_run(&client, |builder| {
    ///         builder.add_joined_room(JoinedRoomBuilder::new(room_id));
    ///     })
    ///     .await;
    ///
    /// let room = client
    ///     .get_room(room_id)
    ///     .expect("The room should be available after we mocked the sync");
    /// # anyhow::Ok(()) });
    /// ```
    pub fn mock_sync(&self) -> MockEndpoint<'_, SyncEndpoint> {
        let mock = Mock::given(method("GET"))
            .and(path("/_matrix/client/v3/sync"))
            .and(header("authorization", "Bearer 1234"));
        MockEndpoint {
            mock,
            server: &self.server,
            endpoint: SyncEndpoint { sync_response_builder: self.sync_response_builder.clone() },
        }
    }

    /// Creates a prebuilt mock for sending an event in a room.
    ///
    /// Note: works with *any* room.
    ///
    /// # Examples
    ///
    /// ```
    /// # tokio_test::block_on(async {
    /// use matrix_sdk::{ruma::{room_id, event_id}, test_utils::mocks::MatrixMockServer};
    /// use serde_json::json;
    ///
    /// let mock_server = MatrixMockServer::new().await;
    /// let client = mock_server.client_builder().build().await;
    ///
    /// mock_server.mock_room_state_encryption().plain().mount().await;
    ///
    /// let room = mock_server
    ///     .sync_joined_room(&client, room_id!("!room_id:localhost"))
    ///     .await;
    ///
    /// let event_id = event_id!("$some_id");
    /// mock_server
    ///     .mock_room_send()
    ///     .ok(event_id)
    ///     .expect(1)
    ///     .mount()
    ///     .await;
    ///
    /// let response = room.send_raw("m.room.message", json!({ "body": "Hello world" })).await?;
    ///
    /// assert_eq!(
    ///     event_id,
    ///     response.event_id,
    ///     "The event ID we mocked should match the one we received when we sent the event"
    /// );
    /// # anyhow::Ok(()) });
    /// ```
    pub fn mock_room_send(&self) -> MockEndpoint<'_, RoomSendEndpoint> {
        let mock = Mock::given(method("PUT"))
            .and(header("authorization", "Bearer 1234"))
            .and(path_regex(r"^/_matrix/client/v3/rooms/.*/send/.*".to_owned()));
        MockEndpoint { mock, server: &self.server, endpoint: RoomSendEndpoint }
    }

    /// Creates a prebuilt mock for sending a state event in a room.
    ///
    /// Similar to: [`MatrixMockServer::mock_room_send`]
    ///
    /// Note: works with *any* room.
    /// Note: works with *any* event type.
    ///
    /// ```
    /// # tokio_test::block_on(async {
    /// use matrix_sdk::{ruma::{room_id, event_id}, test_utils::mocks::MatrixMockServer};
    /// use serde_json::json;
    ///
    /// let mock_server = MatrixMockServer::new().await;
    /// let client = mock_server.client_builder().build().await;
    ///
    /// mock_server.mock_room_state_encryption().plain().mount().await;
    ///
    /// let room = mock_server
    ///     .sync_joined_room(&client, room_id!("!room_id:localhost"))
    ///     .await;
    ///
    /// let event_id = event_id!("$some_id");
    /// mock_server
    ///     .mock_room_send_state()
    ///     .ok(event_id)
    ///     .expect(1)
    ///     .mount()
    ///     .await;
    ///
    /// let response_not_mocked = room.send_raw("m.room.create", json!({ "body": "Hello world" })).await;
    /// // The `/send` endpoint should not be mocked by the server.
    /// assert!(response_not_mocked.is_err());
    ///
    ///
    /// let response = room.send_state_event_raw("m.room.message", "my_key", json!({ "body": "Hello world" })).await?;
    /// // The `/state` endpoint should be mocked by the server.
    /// assert_eq!(
    ///     event_id,
    ///     response.event_id,
    ///     "The event ID we mocked should match the one we received when we sent the event"
    /// );
    /// # anyhow::Ok(()) });
    /// ```
    pub fn mock_room_send_state(&self) -> MockEndpoint<'_, RoomSendStateEndpoint> {
        let mock = Mock::given(method("PUT"))
            .and(header("authorization", "Bearer 1234"))
            .and(path_regex(r"^/_matrix/client/v3/rooms/.*/state/.*/.*"));
        MockEndpoint { mock, server: &self.server, endpoint: RoomSendStateEndpoint::default() }
    }

    /// Creates a prebuilt mock for asking whether *a* room is encrypted or not.
    ///
    /// Note: Applies to all rooms.
    ///
    /// # Examples
    ///
    /// ```
    /// # tokio_test::block_on(async {
    /// use matrix_sdk::{ruma::room_id, test_utils::mocks::MatrixMockServer};
    ///
    /// let mock_server = MatrixMockServer::new().await;
    /// let client = mock_server.client_builder().build().await;
    ///
    /// mock_server.mock_room_state_encryption().encrypted().mount().await;
    ///
    /// let room = mock_server
    ///     .sync_joined_room(&client, room_id!("!room_id:localhost"))
    ///     .await;
    ///
    /// assert!(
    ///     room.is_encrypted().await?,
    ///     "The room should be marked as encrypted."
    /// );
    /// # anyhow::Ok(()) });
    /// ```
    pub fn mock_room_state_encryption(&self) -> MockEndpoint<'_, EncryptionStateEndpoint> {
        let mock = Mock::given(method("GET"))
            .and(header("authorization", "Bearer 1234"))
            .and(path_regex(r"^/_matrix/client/v3/rooms/.*/state/m.*room.*encryption.?"));
        MockEndpoint { mock, server: &self.server, endpoint: EncryptionStateEndpoint }
    }

    /// Creates a prebuilt mock for setting the room encryption state.
    ///
    /// Note: Applies to all rooms.
    ///
    /// # Examples
    ///
    /// ```
    /// # tokio_test::block_on(async {
    /// use matrix_sdk::{
    ///     ruma::{event_id, room_id},
    ///     test_utils::mocks::MatrixMockServer,
    /// };
    ///
    /// let mock_server = MatrixMockServer::new().await;
    /// let client = mock_server.client_builder().build().await;
    ///
    /// mock_server.mock_room_state_encryption().plain().mount().await;
    /// mock_server
    ///     .mock_set_room_state_encryption()
    ///     .ok(event_id!("$id"))
    ///     .mock_once()
    ///     .mount()
    ///     .await;
    ///
    /// let room = mock_server
    ///     .sync_joined_room(&client, room_id!("!room_id:localhost"))
    ///     .await;
    ///
    /// room.enable_encryption()
    ///     .await
    ///     .expect("We should be able to enable encryption in the room");
    /// # anyhow::Ok(()) });
    /// ```
    pub fn mock_set_room_state_encryption(&self) -> MockEndpoint<'_, SetEncryptionStateEndpoint> {
        let mock = Mock::given(method("PUT"))
            .and(header("authorization", "Bearer 1234"))
            .and(path_regex(r"^/_matrix/client/v3/rooms/.*/state/m.*room.*encryption.?"));
        MockEndpoint { mock, server: &self.server, endpoint: SetEncryptionStateEndpoint }
    }

    /// Creates a prebuilt mock for the room redact endpoint.
    ///
    /// # Examples
    ///
    /// ```
    /// # tokio_test::block_on(async {
    /// use matrix_sdk::{
    ///     ruma::{event_id, room_id},
    ///     test_utils::mocks::MatrixMockServer,
    /// };
    ///
    /// let mock_server = MatrixMockServer::new().await;
    /// let client = mock_server.client_builder().build().await;
    /// let event_id = event_id!("$id");
    ///
    /// mock_server.mock_room_redact().ok(event_id).mock_once().mount().await;
    ///
    /// let room = mock_server
    ///     .sync_joined_room(&client, room_id!("!room_id:localhost"))
    ///     .await;
    ///
    /// room.redact(event_id, None, None)
    ///     .await
    ///     .expect("We should be able to redact events in the room");
    /// # anyhow::Ok(()) });
    /// ```
    pub fn mock_room_redact(&self) -> MockEndpoint<'_, RoomRedactEndpoint> {
        let mock = Mock::given(method("PUT"))
            .and(path_regex(r"^/_matrix/client/v3/rooms/.*/redact/.*?/.*?"))
            .and(header("authorization", "Bearer 1234"));
        MockEndpoint { mock, server: &self.server, endpoint: RoomRedactEndpoint }
    }

    /// Creates a prebuilt mock for retrieving an event with /room/.../event.
    pub fn mock_room_event(&self) -> MockEndpoint<'_, RoomEventEndpoint> {
        let mock = Mock::given(method("GET")).and(header("authorization", "Bearer 1234"));
        MockEndpoint {
            mock,
            server: &self.server,
            endpoint: RoomEventEndpoint { room: None, match_event_id: false },
        }
    }

    /// Create a prebuild mock for paginating room message with the `/messages`
    /// endpoint.
    pub fn mock_room_messages(&self) -> MockEndpoint<'_, RoomMessagesEndpoint> {
        let mock = Mock::given(method("GET"))
            .and(path_regex(r"^/_matrix/client/v3/rooms/.*/messages$"))
            .and(header("authorization", "Bearer 1234"));
        MockEndpoint { mock, server: &self.server, endpoint: RoomMessagesEndpoint }
    }

    /// Create a prebuilt mock for uploading media.
    pub fn mock_upload(&self) -> MockEndpoint<'_, UploadEndpoint> {
        let mock = Mock::given(method("POST"))
            .and(path("/_matrix/media/v3/upload"))
            .and(header("authorization", "Bearer 1234"));
        MockEndpoint { mock, server: &self.server, endpoint: UploadEndpoint }
    }

    /// Create a prebuilt mock for resolving room aliases.
    ///
    /// # Examples
    ///
    /// ```
    /// # tokio_test::block_on(async {
    /// use matrix_sdk::{
    ///     ruma::{owned_room_id, room_alias_id},
    ///     test_utils::mocks::MatrixMockServer,
    /// };
    /// let mock_server = MatrixMockServer::new().await;
    /// let client = mock_server.client_builder().build().await;
    ///
    /// mock_server
    ///     .mock_room_directory_resolve_alias()
    ///     .ok("!a:b.c", Vec::new())
    ///     .mock_once()
    ///     .mount()
    ///     .await;
    ///
    /// let res = client
    ///     .resolve_room_alias(room_alias_id!("#a:b.c"))
    ///     .await
    ///     .expect("We should be able to resolve the room alias");
    /// assert_eq!(res.room_id, owned_room_id!("!a:b.c"));
    /// # anyhow::Ok(()) });
    /// ```
    pub fn mock_room_directory_resolve_alias(&self) -> MockEndpoint<'_, ResolveRoomAliasEndpoint> {
        let mock =
            Mock::given(method("GET")).and(path_regex(r"/_matrix/client/v3/directory/room/.*"));
        MockEndpoint { mock, server: &self.server, endpoint: ResolveRoomAliasEndpoint }
    }

    /// Create a prebuilt mock for publishing room aliases in the room
    /// directory.
    ///
    /// # Examples
    ///
    /// ```
    /// # tokio_test::block_on(async {
    /// use matrix_sdk::{
    ///     ruma::{room_alias_id, room_id},
    ///     test_utils::mocks::MatrixMockServer,
    /// };
    ///
    /// let mock_server = MatrixMockServer::new().await;
    /// let client = mock_server.client_builder().build().await;
    ///
    /// mock_server
    ///     .mock_room_directory_create_room_alias()
    ///     .ok()
    ///     .mock_once()
    ///     .mount()
    ///     .await;
    ///
    /// client
    ///     .create_room_alias(room_alias_id!("#a:b.c"), room_id!("!a:b.c"))
    ///     .await
    ///     .expect("We should be able to create a room alias");
    /// # anyhow::Ok(()) });
    /// ```
    pub fn mock_room_directory_create_room_alias(
        &self,
    ) -> MockEndpoint<'_, CreateRoomAliasEndpoint> {
        let mock =
            Mock::given(method("PUT")).and(path_regex(r"/_matrix/client/v3/directory/room/.*"));
        MockEndpoint { mock, server: &self.server, endpoint: CreateRoomAliasEndpoint }
    }

    /// Create a prebuilt mock for removing room aliases from the room
    /// directory.
    ///
    /// # Examples
    ///
    /// ```
    /// # tokio_test::block_on(async {
    /// use matrix_sdk::{
    ///     ruma::room_alias_id, test_utils::mocks::MatrixMockServer,
    /// };
    ///
    /// let mock_server = MatrixMockServer::new().await;
    /// let client = mock_server.client_builder().build().await;
    ///
    /// mock_server
    ///     .mock_room_directory_remove_room_alias()
    ///     .ok()
    ///     .mock_once()
    ///     .mount()
    ///     .await;
    ///
    /// client
    ///     .remove_room_alias(room_alias_id!("#a:b.c"))
    ///     .await
    ///     .expect("We should be able to remove the room alias");
    /// # anyhow::Ok(()) });
    /// ```
    pub fn mock_room_directory_remove_room_alias(
        &self,
    ) -> MockEndpoint<'_, RemoveRoomAliasEndpoint> {
        let mock =
            Mock::given(method("DELETE")).and(path_regex(r"/_matrix/client/v3/directory/room/.*"));
        MockEndpoint { mock, server: &self.server, endpoint: RemoveRoomAliasEndpoint }
    }

    /// Create a prebuilt mock for listing public rooms.
    ///
    /// # Examples
    ///
    /// ```
    /// #
    /// tokio_test::block_on(async {
    /// use js_int::uint;
    /// use ruma::directory::PublicRoomsChunkInit;
    /// use matrix_sdk::room_directory_search::RoomDirectorySearch;
    /// use matrix_sdk::{
    ///     ruma::{event_id, room_id},
    ///     test_utils::mocks::MatrixMockServer,
    /// };
    /// let mock_server = MatrixMockServer::new().await;
    /// let client = mock_server.client_builder().build().await;
    /// let event_id = event_id!("$id");
    /// let room_id = room_id!("!room_id:localhost");
    ///
    /// let chunk = vec![PublicRoomsChunkInit {
    ///     num_joined_members: uint!(0),
    ///     room_id: room_id.to_owned(),
    ///     world_readable: true,
    ///     guest_can_join: true,
    /// }.into()];
    ///
    /// mock_server.mock_public_rooms().ok(chunk, None, None, Some(20)).mock_once().mount().await;
    /// let mut room_directory_search = RoomDirectorySearch::new(client);
    ///
    /// room_directory_search.search(Some("some-alias".to_owned()), 100, None)
    ///     .await
    ///     .expect("Room directory search failed");
    ///
    /// let (results, _) = room_directory_search.results();
    /// assert_eq!(results.len(), 1);
    /// assert_eq!(results.get(0).unwrap().room_id, room_id.to_owned());
    /// # });
    /// ```
    pub fn mock_public_rooms(&self) -> MockEndpoint<'_, PublicRoomsEndpoint> {
        let mock = Mock::given(method("POST")).and(path_regex(r"/_matrix/client/v3/publicRooms"));
        MockEndpoint { mock, server: &self.server, endpoint: PublicRoomsEndpoint }
    }

    /// Create a prebuilt mock for setting a room's visibility in the room
    /// directory.
    ///
    /// # Examples
    ///
    /// ```
    /// # tokio_test::block_on(async {
    /// use matrix_sdk::{ruma::room_id, test_utils::mocks::MatrixMockServer};
    /// use ruma::api::client::room::Visibility;
    ///
    /// let mock_server = MatrixMockServer::new().await;
    /// let client = mock_server.client_builder().build().await;
    ///
    /// mock_server
    ///     .mock_room_directory_set_room_visibility()
    ///     .ok()
    ///     .mock_once()
    ///     .mount()
    ///     .await;
    ///
    /// let room = mock_server
    ///     .sync_joined_room(&client, room_id!("!room_id:localhost"))
    ///     .await;
    ///
    /// room.privacy_settings()
    ///     .update_room_visibility(Visibility::Private)
    ///     .await
    ///     .expect("We should be able to update the room's visibility");
    /// # anyhow::Ok(()) });
    /// ```
    pub fn mock_room_directory_set_room_visibility(
        &self,
    ) -> MockEndpoint<'_, SetRoomVisibilityEndpoint> {
        let mock = Mock::given(method("PUT"))
            .and(path_regex(r"^/_matrix/client/v3/directory/list/room/.*$"));
        MockEndpoint { mock, server: &self.server, endpoint: SetRoomVisibilityEndpoint }
    }

    /// Create a prebuilt mock for getting a room's visibility in the room
    /// directory.
    ///
    /// # Examples
    ///
    /// ```
    /// # tokio_test::block_on(async {
    /// use matrix_sdk::{ruma::room_id, test_utils::mocks::MatrixMockServer};
    /// use ruma::api::client::room::Visibility;
    ///
    /// let mock_server = MatrixMockServer::new().await;
    /// let client = mock_server.client_builder().build().await;
    ///
    /// mock_server
    ///     .mock_room_directory_get_room_visibility()
    ///     .ok(Visibility::Public)
    ///     .mock_once()
    ///     .mount()
    ///     .await;
    ///
    /// let room = mock_server
    ///     .sync_joined_room(&client, room_id!("!room_id:localhost"))
    ///     .await;
    ///
    /// let visibility = room
    ///     .privacy_settings()
    ///     .get_room_visibility()
    ///     .await
    ///     .expect("We should be able to get the room's visibility");
    /// assert_eq!(visibility, Visibility::Public);
    /// # anyhow::Ok(()) });
    /// ```
    pub fn mock_room_directory_get_room_visibility(
        &self,
    ) -> MockEndpoint<'_, GetRoomVisibilityEndpoint> {
        let mock = Mock::given(method("GET"))
            .and(path_regex(r"^/_matrix/client/v3/directory/list/room/.*$"));
        MockEndpoint { mock, server: &self.server, endpoint: GetRoomVisibilityEndpoint }
    }

    /// Create a prebuilt mock for fetching information about key storage
    /// backups.
    ///
    /// # Examples
    ///
    /// ```
    /// # #[cfg(feature = "e2e-encryption")]
    /// # {
    /// # tokio_test::block_on(async {
    /// use matrix_sdk::test_utils::mocks::MatrixMockServer;
    ///
    /// let mock_server = MatrixMockServer::new().await;
    /// let client = mock_server.client_builder().build().await;
    ///
    /// mock_server.mock_room_keys_version().exists().expect(1).mount().await;
    ///
    /// let exists =
    ///     client.encryption().backups().fetch_exists_on_server().await.unwrap();
    ///
    /// assert!(exists);
    /// # });
    /// # }
    /// ```
    pub fn mock_room_keys_version(&self) -> MockEndpoint<'_, RoomKeysVersionEndpoint> {
        let mock = Mock::given(method("GET"))
            .and(path_regex(r"_matrix/client/v3/room_keys/version"))
            .and(header("authorization", "Bearer 1234"));
        MockEndpoint { mock, server: &self.server, endpoint: RoomKeysVersionEndpoint }
    }

    /// Create a prebuilt mock for adding key storage backups via POST
    pub fn mock_add_room_keys_version(&self) -> MockEndpoint<'_, AddRoomKeysVersionEndpoint> {
        let mock = Mock::given(method("POST"))
            .and(path_regex(r"_matrix/client/v3/room_keys/version"))
            .and(header("authorization", "Bearer 1234"));
        MockEndpoint { mock, server: &self.server, endpoint: AddRoomKeysVersionEndpoint }
    }

    /// Create a prebuilt mock for adding key storage backups via POST
    pub fn mock_delete_room_keys_version(&self) -> MockEndpoint<'_, DeleteRoomKeysVersionEndpoint> {
        let mock = Mock::given(method("DELETE"))
            .and(path_regex(r"_matrix/client/v3/room_keys/version/[^/]*"))
            .and(header("authorization", "Bearer 1234"));
        MockEndpoint { mock, server: &self.server, endpoint: DeleteRoomKeysVersionEndpoint }
    }

    /// Create a prebuilt mock for getting the room members in a room.
    ///
    /// # Examples
    ///
    /// ``` #
    /// tokio_test::block_on(async {
    /// use matrix_sdk_base::RoomMemberships;
    /// use ruma::events::room::member::MembershipState;
    /// use ruma::events::room::member::RoomMemberEventContent;
    /// use ruma::user_id;
    /// use matrix_sdk_test::event_factory::EventFactory;
    /// use matrix_sdk::{
    ///     ruma::{event_id, room_id},
    ///     test_utils::mocks::MatrixMockServer,
    /// };
    /// let mock_server = MatrixMockServer::new().await;
    /// let client = mock_server.client_builder().build().await;
    /// let event_id = event_id!("$id");
    /// let room_id = room_id!("!room_id:localhost");
    ///
    /// let f = EventFactory::new().room(room_id);
    /// let alice_user_id = user_id!("@alice:b.c");
    /// let alice_knock_event = f
    ///     .event(RoomMemberEventContent::new(MembershipState::Knock))
    ///     .event_id(event_id)
    ///     .sender(alice_user_id)
    ///     .state_key(alice_user_id)
    ///     .into_raw_timeline()
    ///     .cast();
    ///
    /// mock_server.mock_get_members().ok(vec![alice_knock_event]).mock_once().mount().await;
    /// let room = mock_server.sync_joined_room(&client, room_id).await;
    ///
    /// let members = room.members(RoomMemberships::all()).await.unwrap();
    /// assert_eq!(members.len(), 1);
    /// # });
    /// ```
    pub fn mock_get_members(&self) -> MockEndpoint<'_, GetRoomMembersEndpoint> {
        let mock =
            Mock::given(method("GET")).and(path_regex(r"^/_matrix/client/v3/rooms/.*/members$"));
        MockEndpoint { mock, server: &self.server, endpoint: GetRoomMembersEndpoint }
    }

    /// Creates a prebuilt mock for inviting a user to a room by its id.
    ///
    /// # Examples
    ///
    /// ```
    /// # use ruma::user_id;
    /// tokio_test::block_on(async {
    /// use matrix_sdk::{
    ///     ruma::room_id,
    ///     test_utils::mocks::MatrixMockServer,
    /// };
    ///
    /// let mock_server = MatrixMockServer::new().await;
    /// let client = mock_server.client_builder().build().await;
    ///
    /// mock_server.mock_invite_user_by_id().ok().mock_once().mount().await;
    ///
    /// let room = mock_server
    ///     .sync_joined_room(&client, room_id!("!room_id:localhost"))
    ///     .await;
    ///
    /// room.invite_user_by_id(user_id!("@alice:localhost")).await.unwrap();
    /// # anyhow::Ok(()) });
    /// ```
    pub fn mock_invite_user_by_id(&self) -> MockEndpoint<'_, InviteUserByIdEndpoint> {
        let mock =
            Mock::given(method("POST")).and(path_regex(r"^/_matrix/client/v3/rooms/.*/invite$"));
        MockEndpoint { mock, server: &self.server, endpoint: InviteUserByIdEndpoint }
    }

    /// Creates a prebuilt mock for kicking a user from a room.
    ///
    /// # Examples
    ///
    /// ```
    /// # use ruma::user_id;
    /// tokio_test::block_on(async {
    /// use matrix_sdk::{
    ///     ruma::room_id,
    ///     test_utils::mocks::MatrixMockServer,
    /// };
    ///
    /// let mock_server = MatrixMockServer::new().await;
    /// let client = mock_server.client_builder().build().await;
    ///
    /// mock_server.mock_kick_user().ok().mock_once().mount().await;
    ///
    /// let room = mock_server
    ///     .sync_joined_room(&client, room_id!("!room_id:localhost"))
    ///     .await;
    ///
    /// room.kick_user(user_id!("@alice:localhost"), None).await.unwrap();
    /// # anyhow::Ok(()) });
    /// ```
    pub fn mock_kick_user(&self) -> MockEndpoint<'_, KickUserEndpoint> {
        let mock =
            Mock::given(method("POST")).and(path_regex(r"^/_matrix/client/v3/rooms/.*/kick"));
        MockEndpoint { mock, server: &self.server, endpoint: KickUserEndpoint }
    }

    /// Creates a prebuilt mock for banning a user from a room.
    ///
    /// # Examples
    ///
    /// ```
    /// # use ruma::user_id;
    /// tokio_test::block_on(async {
    /// use matrix_sdk::{
    ///     ruma::room_id,
    ///     test_utils::mocks::MatrixMockServer,
    /// };
    ///
    /// let mock_server = MatrixMockServer::new().await;
    /// let client = mock_server.client_builder().build().await;
    ///
    /// mock_server.mock_ban_user().ok().mock_once().mount().await;
    ///
    /// let room = mock_server
    ///     .sync_joined_room(&client, room_id!("!room_id:localhost"))
    ///     .await;
    ///
    /// room.ban_user(user_id!("@alice:localhost"), None).await.unwrap();
    /// # anyhow::Ok(()) });
    /// ```
    pub fn mock_ban_user(&self) -> MockEndpoint<'_, BanUserEndpoint> {
        let mock = Mock::given(method("POST")).and(path_regex(r"^/_matrix/client/v3/rooms/.*/ban"));
        MockEndpoint { mock, server: &self.server, endpoint: BanUserEndpoint }
    }
}

/// Parameter to [`MatrixMockServer::sync_room`].
pub enum AnyRoomBuilder {
    /// A room we've been invited to.
    Invited(InvitedRoomBuilder),
    /// A room we've joined.
    Joined(JoinedRoomBuilder),
    /// A room we've left.
    Left(LeftRoomBuilder),
    /// A room we've knocked to.
    Knocked(KnockedRoomBuilder),
}

impl AnyRoomBuilder {
    /// Get the [`RoomId`] of the room this [`AnyRoomBuilder`] will create.
    fn room_id(&self) -> &RoomId {
        match self {
            AnyRoomBuilder::Invited(r) => r.room_id(),
            AnyRoomBuilder::Joined(r) => r.room_id(),
            AnyRoomBuilder::Left(r) => r.room_id(),
            AnyRoomBuilder::Knocked(r) => r.room_id(),
        }
    }
}

impl From<InvitedRoomBuilder> for AnyRoomBuilder {
    fn from(val: InvitedRoomBuilder) -> AnyRoomBuilder {
        AnyRoomBuilder::Invited(val)
    }
}

impl From<JoinedRoomBuilder> for AnyRoomBuilder {
    fn from(val: JoinedRoomBuilder) -> AnyRoomBuilder {
        AnyRoomBuilder::Joined(val)
    }
}

impl From<LeftRoomBuilder> for AnyRoomBuilder {
    fn from(val: LeftRoomBuilder) -> AnyRoomBuilder {
        AnyRoomBuilder::Left(val)
    }
}

impl From<KnockedRoomBuilder> for AnyRoomBuilder {
    fn from(val: KnockedRoomBuilder) -> AnyRoomBuilder {
        AnyRoomBuilder::Knocked(val)
    }
}

/// The [path percent-encode set] as defined in the WHATWG URL standard + `/`
/// since we always encode single segments of the path.
///
/// [path percent-encode set]: https://url.spec.whatwg.org/#path-percent-encode-set
///
/// Copied from Ruma:
/// https://github.com/ruma/ruma/blob/e4cb409ff3aaa16f31a7fe1e61fee43b2d144f7b/crates/ruma-common/src/percent_encode.rs#L7
const PATH_PERCENT_ENCODE_SET: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b'"')
    .add(b'#')
    .add(b'<')
    .add(b'>')
    .add(b'?')
    .add(b'`')
    .add(b'{')
    .add(b'}')
    .add(b'/');

fn percent_encoded_path(path: &str) -> String {
    percent_encoding::utf8_percent_encode(path, PATH_PERCENT_ENCODE_SET).to_string()
}

/// A wrapper for a [`Mock`] as well as a [`MockServer`], allowing us to call
/// [`Mock::mount`] or [`Mock::mount_as_scoped`] without having to pass the
/// [`MockServer`] reference (i.e. call `mount()` instead of `mount(&server)`).
pub struct MatrixMock<'a> {
    mock: Mock,
    server: &'a MockServer,
}

impl MatrixMock<'_> {
    /// Set an expectation on the number of times this [`MatrixMock`] should
    /// match in the current test case.
    ///
    /// Expectations are verified when the server is shutting down: if
    /// the expectation is not satisfied, the [`MatrixMockServer`] will panic
    /// and the `error_message` is shown.
    ///
    /// By default, no expectation is set for [`MatrixMock`]s.
    pub fn expect<T: Into<Times>>(self, num_calls: T) -> Self {
        Self { mock: self.mock.expect(num_calls), ..self }
    }

    /// Assign a name to your mock.
    ///
    /// The mock name will be used in error messages (e.g. if the mock
    /// expectation is not satisfied) and debug logs to help you identify
    /// what failed.
    pub fn named(self, name: impl Into<String>) -> Self {
        Self { mock: self.mock.named(name), ..self }
    }

    /// Respond to a response of this endpoint exactly once.
    ///
    /// After it's been called, subsequent responses will hit the next handler
    /// or a 404.
    ///
    /// Also verifies that it's been called once.
    pub fn mock_once(self) -> Self {
        Self { mock: self.mock.up_to_n_times(1).expect(1), ..self }
    }

    /// Makes sure the endpoint is never reached.
    pub fn never(self) -> Self {
        Self { mock: self.mock.expect(0), ..self }
    }

    /// Specify an upper limit to the number of times you would like this
    /// [`MatrixMock`] to respond to incoming requests that satisfy the
    /// conditions imposed by your matchers.
    pub fn up_to_n_times(self, num: u64) -> Self {
        Self { mock: self.mock.up_to_n_times(num), ..self }
    }

    /// Mount a [`MatrixMock`] on the attached server.
    ///
    /// The [`MatrixMock`] will remain active until the [`MatrixMockServer`] is
    /// shut down. If you want to control or limit how long your
    /// [`MatrixMock`] stays active, check out [`Self::mount_as_scoped`].
    pub async fn mount(self) {
        self.mock.mount(self.server).await;
    }

    /// Mount a [`MatrixMock`] as **scoped** on the attached server.
    ///
    /// When using [`Self::mount`], your [`MatrixMock`]s will be active until
    /// the [`MatrixMockServer`] is shut down.
    ///
    /// When using `mount_as_scoped`, your [`MatrixMock`]s will be active as
    /// long as the returned [`MockGuard`] is not dropped.
    ///
    /// When the returned [`MockGuard`] is dropped, [`MatrixMockServer`] will
    /// verify that the expectations set on the scoped [`MatrixMock`] were
    /// verified - if not, it will panic.
    pub async fn mount_as_scoped(self) -> MockGuard {
        self.mock.mount_as_scoped(self.server).await
    }
}

/// Generic mocked endpoint, with useful common helpers.
pub struct MockEndpoint<'a, T> {
    server: &'a MockServer,
    mock: MockBuilder,
    endpoint: T,
}

impl<'a, T> MockEndpoint<'a, T> {
    /// Specify how to respond to a query (viz., like
    /// [`MockBuilder::respond_with`] does), when other predefined responses
    /// aren't sufficient.
    ///
    /// # Examples
    ///
    /// ```
    /// # tokio_test::block_on(async {
    /// use matrix_sdk::{ruma::{room_id, event_id}, test_utils::mocks::MatrixMockServer};
    /// use serde_json::json;
    /// use wiremock::ResponseTemplate;
    ///
    /// let mock_server = MatrixMockServer::new().await;
    /// let client = mock_server.client_builder().build().await;
    ///
    /// mock_server.mock_room_state_encryption().plain().mount().await;
    ///
    /// let room = mock_server
    ///     .sync_joined_room(&client, room_id!("!room_id:localhost"))
    ///     .await;
    ///
    /// let event_id = event_id!("$some_id");
    /// mock_server
    ///     .mock_room_send()
    ///     .respond_with(
    ///         ResponseTemplate::new(429)
    ///             .insert_header("Retry-After", "100")
    ///             .set_body_json(json!({
    ///                 "errcode": "M_LIMIT_EXCEEDED",
    ///                 "custom_field": "with custom data",
    ///     })))
    ///     .expect(1)
    ///     .mount()
    ///     .await;
    ///
    /// room
    ///     .send_raw("m.room.message", json!({ "body": "Hello world" }))
    ///     .await
    ///     .expect_err("The sending of the event should fail");
    /// # anyhow::Ok(()) });
    /// ```
    pub fn respond_with<R: Respond + 'static>(self, func: R) -> MatrixMock<'a> {
        MatrixMock { mock: self.mock.respond_with(func), server: self.server }
    }

    /// Returns a send endpoint that emulates a transient failure, i.e responds
    /// with error 500.
    ///
    /// # Examples
    /// ```
    /// # tokio_test::block_on(async {
    /// use matrix_sdk::{ruma::{room_id, event_id}, test_utils::mocks::MatrixMockServer};
    /// use serde_json::json;
    ///
    /// let mock_server = MatrixMockServer::new().await;
    /// let client = mock_server.client_builder().build().await;
    ///
    /// mock_server.mock_room_state_encryption().plain().mount().await;
    ///
    /// let room = mock_server
    ///     .sync_joined_room(&client, room_id!("!room_id:localhost"))
    ///     .await;
    ///
    /// mock_server
    ///     .mock_room_send()
    ///     .error500()
    ///     .expect(1)
    ///     .mount()
    ///     .await;
    ///
    /// room
    ///     .send_raw("m.room.message", json!({ "body": "Hello world" }))
    ///     .await.expect_err("The sending of the event should have failed");
    /// # anyhow::Ok(()) });
    /// ```
    pub fn error500(self) -> MatrixMock<'a> {
        MatrixMock { mock: self.mock.respond_with(ResponseTemplate::new(500)), server: self.server }
    }

    /// Internal helper to return an `{ event_id }` JSON struct along with a 200
    /// ok response.
    fn ok_with_event_id(self, event_id: OwnedEventId) -> MatrixMock<'a> {
        let mock = self.mock.respond_with(
            ResponseTemplate::new(200).set_body_json(json!({ "event_id": event_id })),
        );
        MatrixMock { server: self.server, mock }
    }

    /// Returns an endpoint that emulates a permanent failure error (e.g. event
    /// is too large).
    ///
    /// # Examples
    /// ```
    /// # tokio_test::block_on(async {
    /// use matrix_sdk::{ruma::{room_id, event_id}, test_utils::mocks::MatrixMockServer};
    /// use serde_json::json;
    ///
    /// let mock_server = MatrixMockServer::new().await;
    /// let client = mock_server.client_builder().build().await;
    ///
    /// mock_server.mock_room_state_encryption().plain().mount().await;
    ///
    /// let room = mock_server
    ///     .sync_joined_room(&client, room_id!("!room_id:localhost"))
    ///     .await;
    ///
    /// mock_server
    ///     .mock_room_send()
    ///     .error_too_large()
    ///     .expect(1)
    ///     .mount()
    ///     .await;
    ///
    /// room
    ///     .send_raw("m.room.message", json!({ "body": "Hello world" }))
    ///     .await.expect_err("The sending of the event should have failed");
    /// # anyhow::Ok(()) });
    /// ```
    pub fn error_too_large(self) -> MatrixMock<'a> {
        MatrixMock {
            mock: self.mock.respond_with(ResponseTemplate::new(413).set_body_json(json!({
                // From https://spec.matrix.org/v1.10/client-server-api/#standard-error-response
                "errcode": "M_TOO_LARGE",
            }))),
            server: self.server,
        }
    }
}

/// A prebuilt mock for sending a message like event in a room.
pub struct RoomSendEndpoint;

impl<'a> MockEndpoint<'a, RoomSendEndpoint> {
    /// Ensures that the body of the request is a superset of the provided
    /// `body` parameter.
    ///
    /// # Examples
    /// ```
    /// # tokio_test::block_on(async {
    /// use matrix_sdk::{
    ///     ruma::{room_id, event_id, events::room::message::RoomMessageEventContent},
    ///     test_utils::mocks::MatrixMockServer
    /// };
    /// use serde_json::json;
    ///
    /// let mock_server = MatrixMockServer::new().await;
    /// let client = mock_server.client_builder().build().await;
    ///
    /// mock_server.mock_room_state_encryption().plain().mount().await;
    ///
    /// let room = mock_server
    ///     .sync_joined_room(&client, room_id!("!room_id:localhost"))
    ///     .await;
    ///
    /// let event_id = event_id!("$some_id");
    /// mock_server
    ///     .mock_room_send()
    ///     .body_matches_partial_json(json!({
    ///         "body": "Hello world",
    ///     }))
    ///     .ok(event_id)
    ///     .expect(1)
    ///     .mount()
    ///     .await;
    ///
    /// let content = RoomMessageEventContent::text_plain("Hello world");
    /// let response = room.send(content).await?;
    ///
    /// assert_eq!(
    ///     event_id,
    ///     response.event_id,
    ///     "The event ID we mocked should match the one we received when we sent the event"
    /// );
    /// # anyhow::Ok(()) });
    /// ```
    pub fn body_matches_partial_json(self, body: Value) -> Self {
        Self { mock: self.mock.and(body_partial_json(body)), ..self }
    }

    /// Ensures that the send endpoint request uses a specific event type.
    ///
    /// # Examples
    ///
    /// see also [`MatrixMockServer::mock_room_send`] for more context.
    ///
    /// ```
    /// # tokio_test::block_on(async {
    /// use matrix_sdk::{ruma::{room_id, event_id}, test_utils::mocks::MatrixMockServer};
    /// use serde_json::json;
    ///
    /// let mock_server = MatrixMockServer::new().await;
    /// let client = mock_server.client_builder().build().await;
    ///
    /// mock_server.mock_room_state_encryption().plain().mount().await;
    ///
    /// let room = mock_server
    ///     .sync_joined_room(&client, room_id!("!room_id:localhost"))
    ///     .await;
    ///
    /// let event_id = event_id!("$some_id");
    /// mock_server
    ///     .mock_room_send()
    ///     .for_type("m.room.message".into())
    ///     .ok(event_id)
    ///     .expect(1)
    ///     .mount()
    ///     .await;
    ///
    /// let response_not_mocked = room.send_raw("m.room.reaction", json!({ "body": "Hello world" })).await;
    /// // The `m.room.reaction` event type should not be mocked by the server.
    /// assert!(response_not_mocked.is_err());
    ///
    /// let response = room.send_raw("m.room.message", json!({ "body": "Hello world" })).await?;
    /// // The `m.room.message` event type should be mocked by the server.
    /// assert_eq!(
    ///     event_id,
    ///     response.event_id,
    ///     "The event ID we mocked should match the one we received when we sent the event"
    /// );
    /// # anyhow::Ok(()) });
    /// ```
    pub fn for_type(self, event_type: MessageLikeEventType) -> Self {
        Self {
            // Note: we already defined a path when constructing the mock builder, but this one
            // ought to be more specialized.
            mock: self
                .mock
                .and(path_regex(format!(r"^/_matrix/client/v3/rooms/.*/send/{event_type}",))),
            ..self
        }
    }

    /// Ensures the event was sent as a delayed event.
    ///
    /// Note: works with *any* room.
    ///
    /// # Examples
    ///
    /// see also [`MatrixMockServer::mock_room_send`] for more context.
    ///
    /// ```
    /// # tokio_test::block_on(async {
    /// use matrix_sdk::{
    ///     ruma::{
    ///         api::client::delayed_events::{delayed_message_event, DelayParameters},
    ///         events::{message::MessageEventContent, AnyMessageLikeEventContent},
    ///         room_id,
    ///         time::Duration,
    ///         TransactionId,
    ///     },
    ///     test_utils::mocks::MatrixMockServer,
    /// };
    /// use serde_json::json;
    /// use wiremock::ResponseTemplate;
    ///
    /// let mock_server = MatrixMockServer::new().await;
    /// let client = mock_server.client_builder().build().await;
    ///
    /// mock_server.mock_room_state_encryption().plain().mount().await;
    ///
    /// let room = mock_server.sync_joined_room(&client, room_id!("!room_id:localhost")).await;
    ///
    /// mock_server
    ///     .mock_room_send()
    ///     .with_delay(Duration::from_millis(500))
    ///     .respond_with(ResponseTemplate::new(200).set_body_json(json!({"delay_id":"$some_id"})))
    ///     .mock_once()
    ///     .mount()
    ///     .await;
    ///
    /// let response_not_mocked =
    ///     room.send_raw("m.room.message", json!({ "body": "Hello world" })).await;
    ///
    /// // A non delayed event should not be mocked by the server.
    /// assert!(response_not_mocked.is_err());
    ///
    /// let r = delayed_message_event::unstable::Request::new(
    ///     room.room_id().to_owned(),
    ///     TransactionId::new(),
    ///     DelayParameters::Timeout { timeout: Duration::from_millis(500) },
    ///     &AnyMessageLikeEventContent::Message(MessageEventContent::plain("hello world")),
    /// )
    /// .unwrap();
    ///
    /// let response = room.client().send(r).await.unwrap();
    /// // The delayed `m.room.message` event type should be mocked by the server.
    /// assert_eq!("$some_id", response.delay_id);
    /// # anyhow::Ok(()) });
    /// ```
    pub fn with_delay(self, delay: Duration) -> Self {
        Self {
            mock: self
                .mock
                .and(query_param("org.matrix.msc4140.delay", delay.as_millis().to_string())),
            ..self
        }
    }

    /// Returns a send endpoint that emulates success, i.e. the event has been
    /// sent with the given event id.
    ///
    /// # Examples
    /// ```
    /// # tokio_test::block_on(async {
    /// use matrix_sdk::{ruma::{room_id, event_id}, test_utils::mocks::MatrixMockServer};
    /// use serde_json::json;
    ///
    /// let mock_server = MatrixMockServer::new().await;
    /// let client = mock_server.client_builder().build().await;
    ///
    /// mock_server.mock_room_state_encryption().plain().mount().await;
    ///
    /// let room = mock_server
    ///     .sync_joined_room(&client, room_id!("!room_id:localhost"))
    ///     .await;
    ///
    /// let event_id = event_id!("$some_id");
    /// let send_guard = mock_server
    ///     .mock_room_send()
    ///     .ok(event_id)
    ///     .expect(1)
    ///     .mount_as_scoped()
    ///     .await;
    ///
    /// let response = room.send_raw("m.room.message", json!({ "body": "Hello world" })).await?;
    ///
    /// assert_eq!(
    ///     event_id,
    ///     response.event_id,
    ///     "The event ID we mocked should match the one we received when we sent the event"
    /// );
    /// # anyhow::Ok(()) });
    /// ```
    pub fn ok(self, returned_event_id: impl Into<OwnedEventId>) -> MatrixMock<'a> {
        self.ok_with_event_id(returned_event_id.into())
    }
}

/// A prebuilt mock for sending a state event in a room.
#[derive(Default)]
pub struct RoomSendStateEndpoint {
    state_key: Option<String>,
    event_type: Option<StateEventType>,
}

impl<'a> MockEndpoint<'a, RoomSendStateEndpoint> {
    fn generate_path_regexp(endpoint: &RoomSendStateEndpoint) -> String {
        format!(
            r"^/_matrix/client/v3/rooms/.*/state/{}/{}",
            endpoint.event_type.as_ref().map_or_else(|| ".*".to_owned(), |t| t.to_string()),
            endpoint.state_key.as_ref().map_or_else(|| ".*".to_owned(), |k| k.to_string())
        )
    }

    /// Ensures that the body of the request is a superset of the provided
    /// `body` parameter.
    ///
    /// # Examples
    /// ```
    /// # tokio_test::block_on(async {
    /// use matrix_sdk::{
    ///     ruma::{room_id, event_id, events::room::power_levels::RoomPowerLevelsEventContent},
    ///     test_utils::mocks::MatrixMockServer
    /// };
    /// use serde_json::json;
    ///
    /// let mock_server = MatrixMockServer::new().await;
    /// let client = mock_server.client_builder().build().await;
    ///
    /// mock_server.mock_room_state_encryption().plain().mount().await;
    ///
    /// let room = mock_server
    ///     .sync_joined_room(&client, room_id!("!room_id:localhost"))
    ///     .await;
    ///
    /// let event_id = event_id!("$some_id");
    /// mock_server
    ///     .mock_room_send_state()
    ///     .body_matches_partial_json(json!({
    ///         "redact": 51,
    ///     }))
    ///     .ok(event_id)
    ///     .expect(1)
    ///     .mount()
    ///     .await;
    ///
    /// let mut content = RoomPowerLevelsEventContent::new();
    /// // Update the power level to a non default value.
    /// // Otherwise it will be skipped from serialization.
    /// content.redact = 51.into();
    ///
    /// let response = room.send_state_event(content).await?;
    ///
    /// assert_eq!(
    ///     event_id,
    ///     response.event_id,
    ///     "The event ID we mocked should match the one we received when we sent the event"
    /// );
    /// # anyhow::Ok(()) });
    /// ```
    pub fn body_matches_partial_json(self, body: Value) -> Self {
        Self { mock: self.mock.and(body_partial_json(body)), ..self }
    }

    /// Ensures that the send endpoint request uses a specific event type.
    ///
    /// Note: works with *any* room.
    ///
    /// # Examples
    ///
    /// see also [`MatrixMockServer::mock_room_send`] for more context.
    ///
    /// ```
    /// # tokio_test::block_on(async {
    /// use matrix_sdk::{
    ///     ruma::{
    ///         event_id,
    ///         events::room::{
    ///             create::RoomCreateEventContent, power_levels::RoomPowerLevelsEventContent,
    ///         },
    ///         events::StateEventType,
    ///         room_id,
    ///     },
    ///     test_utils::mocks::MatrixMockServer,
    /// };
    ///
    /// let mock_server = MatrixMockServer::new().await;
    /// let client = mock_server.client_builder().build().await;
    ///
    /// mock_server.mock_room_state_encryption().plain().mount().await;
    ///
    /// let room = mock_server.sync_joined_room(&client, room_id!("!room_id:localhost")).await;
    ///
    /// let event_id = event_id!("$some_id");
    ///
    /// mock_server
    ///     .mock_room_send_state()
    ///     .for_type(StateEventType::RoomPowerLevels)
    ///     .ok(event_id)
    ///     .expect(1)
    ///     .mount()
    ///     .await;
    ///
    /// let response_not_mocked = room.send_state_event(RoomCreateEventContent::new_v11()).await;
    /// // The `m.room.reaction` event type should not be mocked by the server.
    /// assert!(response_not_mocked.is_err());
    ///
    /// let response = room.send_state_event(RoomPowerLevelsEventContent::new()).await?;
    /// // The `m.room.message` event type should be mocked by the server.
    /// assert_eq!(
    ///     event_id, response.event_id,
    ///     "The event ID we mocked should match the one we received when we sent the event"
    /// );
    ///
    /// # anyhow::Ok(()) });
    /// ```
    pub fn for_type(mut self, event_type: StateEventType) -> Self {
        self.endpoint.event_type = Some(event_type);
        // Note: we may have already defined a path, but this one ought to be more
        // specialized (unless for_key/for_type were called multiple times).
        Self { mock: self.mock.and(path_regex(Self::generate_path_regexp(&self.endpoint))), ..self }
    }

    /// Ensures the event was sent as a delayed event.
    ///
    /// Note: works with *any* room.
    ///
    /// # Examples
    ///
    /// see also [`MatrixMockServer::mock_room_send`] for more context.
    ///
    /// ```
    /// # tokio_test::block_on(async {
    /// use matrix_sdk::{
    ///     ruma::{
    ///         api::client::delayed_events::{delayed_state_event, DelayParameters},
    ///         events::{room::create::RoomCreateEventContent, AnyStateEventContent},
    ///         room_id,
    ///         time::Duration,
    ///     },
    ///     test_utils::mocks::MatrixMockServer,
    /// };
    /// use wiremock::ResponseTemplate;
    /// use serde_json::json;
    ///
    /// let mock_server = MatrixMockServer::new().await;
    /// let client = mock_server.client_builder().build().await;
    ///
    /// mock_server.mock_room_state_encryption().plain().mount().await;
    ///
    /// let room = mock_server.sync_joined_room(&client, room_id!("!room_id:localhost")).await;
    ///
    /// mock_server
    ///     .mock_room_send_state()
    ///     .with_delay(Duration::from_millis(500))
    ///     .respond_with(ResponseTemplate::new(200).set_body_json(json!({"delay_id":"$some_id"})))
    ///     .mock_once()
    ///     .mount()
    ///     .await;
    ///
    /// let response_not_mocked = room.send_state_event(RoomCreateEventContent::new_v11()).await;
    /// // A non delayed event should not be mocked by the server.
    /// assert!(response_not_mocked.is_err());
    ///
    /// let r = delayed_state_event::unstable::Request::new(
    ///     room.room_id().to_owned(),
    ///     "".to_owned(),
    ///     DelayParameters::Timeout { timeout: Duration::from_millis(500) },
    ///     &AnyStateEventContent::RoomCreate(RoomCreateEventContent::new_v11()),
    /// )
    /// .unwrap();
    /// let response = room.client().send(r).await.unwrap();
    /// // The delayed `m.room.message` event type should be mocked by the server.
    /// assert_eq!("$some_id", response.delay_id);
    ///
    /// # anyhow::Ok(()) });
    /// ```
    pub fn with_delay(self, delay: Duration) -> Self {
        Self {
            mock: self
                .mock
                .and(query_param("org.matrix.msc4140.delay", delay.as_millis().to_string())),
            ..self
        }
    }

    ///
    /// ```
    /// # tokio_test::block_on(async {
    /// use matrix_sdk::{
    ///     ruma::{
    ///         event_id,
    ///         events::{call::member::CallMemberEventContent, AnyStateEventContent},
    ///         room_id,
    ///     },
    ///     test_utils::mocks::MatrixMockServer,
    /// };
    ///
    /// let mock_server = MatrixMockServer::new().await;
    /// let client = mock_server.client_builder().build().await;
    ///
    /// mock_server.mock_room_state_encryption().plain().mount().await;
    ///
    /// let room = mock_server.sync_joined_room(&client, room_id!("!room_id:localhost")).await;
    ///
    /// let event_id = event_id!("$some_id");
    ///
    /// mock_server
    ///     .mock_room_send_state()
    ///     .for_key("my_key".to_owned())
    ///     .ok(event_id)
    ///     .expect(1)
    ///     .mount()
    ///     .await;
    ///
    /// let response_not_mocked = room
    ///     .send_state_event_for_key(
    ///         "",
    ///         AnyStateEventContent::CallMember(CallMemberEventContent::new_empty(None)),
    ///     )
    ///     .await;
    /// // The `m.room.reaction` event type should not be mocked by the server.
    /// assert!(response_not_mocked.is_err());
    ///
    /// let response = room
    ///     .send_state_event_for_key(
    ///         "my_key",
    ///         AnyStateEventContent::CallMember(CallMemberEventContent::new_empty(None)),
    ///     )
    ///     .await
    ///     .unwrap();
    ///
    /// // The `m.room.message` event type should be mocked by the server.
    /// assert_eq!(
    ///     event_id, response.event_id,
    ///     "The event ID we mocked should match the one we received when we sent the event"
    /// );
    /// # anyhow::Ok(()) });
    /// ```
    pub fn for_key(mut self, state_key: String) -> Self {
        self.endpoint.state_key = Some(state_key);
        // Note: we may have already defined a path, but this one ought to be more
        // specialized (unless for_key/for_type were called multiple times).
        Self { mock: self.mock.and(path_regex(Self::generate_path_regexp(&self.endpoint))), ..self }
    }

    /// Returns a send endpoint that emulates success, i.e. the event has been
    /// sent with the given event id.
    ///
    /// # Examples
    /// ```
    /// # tokio_test::block_on(async {
    /// use matrix_sdk::{ruma::{room_id, event_id}, test_utils::mocks::MatrixMockServer};
    /// use serde_json::json;
    ///
    /// let mock_server = MatrixMockServer::new().await;
    /// let client = mock_server.client_builder().build().await;
    ///
    /// mock_server.mock_room_state_encryption().plain().mount().await;
    ///
    /// let room = mock_server
    ///     .sync_joined_room(&client, room_id!("!room_id:localhost"))
    ///     .await;
    ///
    /// let event_id = event_id!("$some_id");
    /// let send_guard = mock_server
    ///     .mock_room_send_state()
    ///     .ok(event_id)
    ///     .expect(1)
    ///     .mount_as_scoped()
    ///     .await;
    ///
    /// let response = room.send_state_event_raw("m.room.message", "my_key", json!({ "body": "Hello world" })).await?;
    ///
    /// assert_eq!(
    ///     event_id,
    ///     response.event_id,
    ///     "The event ID we mocked should match the one we received when we sent the event"
    /// );
    /// # anyhow::Ok(()) });
    /// ```
    pub fn ok(self, returned_event_id: impl Into<OwnedEventId>) -> MatrixMock<'a> {
        self.ok_with_event_id(returned_event_id.into())
    }
}

/// A prebuilt mock for running sync v2.
pub struct SyncEndpoint {
    sync_response_builder: Arc<Mutex<SyncResponseBuilder>>,
}

impl MockEndpoint<'_, SyncEndpoint> {
    /// Temporarily mocks the sync with the given endpoint and runs a client
    /// sync with it.
    ///
    /// After calling this function, the sync endpoint isn't mocked anymore.
    ///
    /// # Examples
    ///
    /// ```
    /// # tokio_test::block_on(async {
    /// use matrix_sdk::{ruma::room_id, test_utils::mocks::MatrixMockServer};
    /// use matrix_sdk_test::JoinedRoomBuilder;
    ///
    /// // First create the mock server and client pair.
    /// let mock_server = MatrixMockServer::new().await;
    /// let client = mock_server.client_builder().build().await;
    /// let room_id = room_id!("!room_id:localhost");
    ///
    /// // Let's emulate what `MatrixMockServer::sync_joined_room()` does.
    /// mock_server
    ///     .mock_sync()
    ///     .ok_and_run(&client, |builder| {
    ///         builder.add_joined_room(JoinedRoomBuilder::new(room_id));
    ///     })
    ///     .await;
    ///
    /// let room = client
    ///     .get_room(room_id)
    ///     .expect("The room should be available after we mocked the sync");
    /// # anyhow::Ok(()) });
    /// ```
    pub async fn ok_and_run<F: FnOnce(&mut SyncResponseBuilder)>(self, client: &Client, func: F) {
        let json_response = {
            let mut builder = self.endpoint.sync_response_builder.lock().unwrap();
            func(&mut builder);
            builder.build_json_sync_response()
        };

        let _scope = self
            .mock
            .respond_with(ResponseTemplate::new(200).set_body_json(json_response))
            .mount_as_scoped(self.server)
            .await;

        let _response = client.sync_once(Default::default()).await.unwrap();
    }
}

/// A prebuilt mock for reading the encryption state of a room.
pub struct EncryptionStateEndpoint;

impl<'a> MockEndpoint<'a, EncryptionStateEndpoint> {
    /// Marks the room as encrypted.
    ///
    /// # Examples
    ///
    /// ```
    /// # tokio_test::block_on(async {
    /// use matrix_sdk::{ruma::room_id, test_utils::mocks::MatrixMockServer};
    ///
    /// let mock_server = MatrixMockServer::new().await;
    /// let client = mock_server.client_builder().build().await;
    ///
    /// mock_server.mock_room_state_encryption().encrypted().mount().await;
    ///
    /// let room = mock_server
    ///     .sync_joined_room(&client, room_id!("!room_id:localhost"))
    ///     .await;
    ///
    /// assert!(
    ///     room.is_encrypted().await?,
    ///     "The room should be marked as encrypted."
    /// );
    /// # anyhow::Ok(()) });
    /// ```
    pub fn encrypted(self) -> MatrixMock<'a> {
        let mock = self.mock.respond_with(
            ResponseTemplate::new(200).set_body_json(&*test_json::sync_events::ENCRYPTION_CONTENT),
        );
        MatrixMock { mock, server: self.server }
    }

    /// Marks the room as not encrypted.
    ///
    /// # Examples
    ///
    /// ```
    /// # tokio_test::block_on(async {
    /// use matrix_sdk::{ruma::room_id, test_utils::mocks::MatrixMockServer};
    ///
    /// let mock_server = MatrixMockServer::new().await;
    /// let client = mock_server.client_builder().build().await;
    ///
    /// mock_server.mock_room_state_encryption().plain().mount().await;
    ///
    /// let room = mock_server
    ///     .sync_joined_room(&client, room_id!("!room_id:localhost"))
    ///     .await;
    ///
    /// assert!(
    ///     !room.is_encrypted().await?,
    ///     "The room should not be marked as encrypted."
    /// );
    /// # anyhow::Ok(()) });
    /// ```
    pub fn plain(self) -> MatrixMock<'a> {
        let mock = self
            .mock
            .respond_with(ResponseTemplate::new(404).set_body_json(&*test_json::NOT_FOUND));
        MatrixMock { mock, server: self.server }
    }
}

/// A prebuilt mock for setting the encryption state of a room.
pub struct SetEncryptionStateEndpoint;

impl<'a> MockEndpoint<'a, SetEncryptionStateEndpoint> {
    /// Returns a mock for a successful setting of the encryption state event.
    pub fn ok(self, returned_event_id: impl Into<OwnedEventId>) -> MatrixMock<'a> {
        self.ok_with_event_id(returned_event_id.into())
    }
}

/// A prebuilt mock for redacting an event in a room.
pub struct RoomRedactEndpoint;

impl<'a> MockEndpoint<'a, RoomRedactEndpoint> {
    /// Returns a redact endpoint that emulates success, i.e. the redaction
    /// event has been sent with the given event id.
    pub fn ok(self, returned_event_id: impl Into<OwnedEventId>) -> MatrixMock<'a> {
        self.ok_with_event_id(returned_event_id.into())
    }
}

/// A prebuilt mock for getting a single event in a room.
pub struct RoomEventEndpoint {
    room: Option<OwnedRoomId>,
    match_event_id: bool,
}

impl<'a> MockEndpoint<'a, RoomEventEndpoint> {
    /// Limits the scope of this mock to a specific room.
    pub fn room(mut self, room: impl Into<OwnedRoomId>) -> Self {
        self.endpoint.room = Some(room.into());
        self
    }

    /// Whether the mock checks for the event id from the event.
    pub fn match_event_id(mut self) -> Self {
        self.endpoint.match_event_id = true;
        self
    }

    /// Returns a redact endpoint that emulates success, i.e. the redaction
    /// event has been sent with the given event id.
    pub fn ok(self, event: TimelineEvent) -> MatrixMock<'a> {
        let event_path = if self.endpoint.match_event_id {
            let event_id = event.kind.event_id().expect("an event id is required");
            event_id.to_string()
        } else {
            // Event is at the end, so no need to add anything.
            "".to_owned()
        };

        let room_path = self.endpoint.room.map_or_else(|| ".*".to_owned(), |room| room.to_string());

        let mock = self
            .mock
            .and(path_regex(format!("^/_matrix/client/v3/rooms/{room_path}/event/{event_path}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(event.into_raw().json()));
        MatrixMock { server: self.server, mock }
    }
}

/// A prebuilt mock for the `/messages` endpoint.
pub struct RoomMessagesEndpoint;

/// A prebuilt mock for getting a room messages in a room.
impl<'a> MockEndpoint<'a, RoomMessagesEndpoint> {
    /// Expects an optional limit to be set on the request.
    pub fn limit(self, limit: u32) -> Self {
        Self { mock: self.mock.and(query_param("limit", limit.to_string())), ..self }
    }

    /// Expects an optional `from` to be set on the request.
    pub fn from(self, from: &str) -> Self {
        Self { mock: self.mock.and(query_param("from", from)), ..self }
    }

    /// Returns a messages endpoint that emulates success, i.e. the messages
    /// provided as `response` could be retrieved.
    ///
    /// Note: pass `chunk` in the correct order: topological for forward
    /// pagination, reverse topological for backwards pagination.
    pub fn ok(
        self,
        start: String,
        end: Option<String>,
        chunk: Vec<impl Into<Raw<AnyTimelineEvent>>>,
        state: Vec<Raw<AnyStateEvent>>,
    ) -> MatrixMock<'a> {
        let mock = self.mock.respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "start": start,
            "end": end,
            "chunk": chunk.into_iter().map(|ev| ev.into()).collect::<Vec<_>>(),
            "state": state,
        })));
        MatrixMock { server: self.server, mock }
    }
}

/// A prebuilt mock for uploading media.
pub struct UploadEndpoint;

impl<'a> MockEndpoint<'a, UploadEndpoint> {
    /// Expect that the content type matches what's given here.
    pub fn expect_mime_type(self, content_type: &str) -> Self {
        Self { mock: self.mock.and(header("content-type", content_type)), ..self }
    }

    /// Returns a redact endpoint that emulates success, i.e. the redaction
    /// event has been sent with the given event id.
    pub fn ok(self, mxc_id: &MxcUri) -> MatrixMock<'a> {
        let mock = self.mock.respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "content_uri": mxc_id
        })));
        MatrixMock { server: self.server, mock }
    }
}

/// A prebuilt mock for resolving a room alias.
pub struct ResolveRoomAliasEndpoint;

impl<'a> MockEndpoint<'a, ResolveRoomAliasEndpoint> {
    /// Sets up the endpoint to only intercept requests for the given room
    /// alias.
    pub fn for_alias(self, alias: impl Into<String>) -> Self {
        let alias = alias.into();
        Self {
            mock: self.mock.and(path_regex(format!(
                r"^/_matrix/client/v3/directory/room/{}",
                percent_encoded_path(&alias)
            ))),
            ..self
        }
    }

    /// Returns a data endpoint with a resolved room alias.
    pub fn ok(self, room_id: &str, servers: Vec<String>) -> MatrixMock<'a> {
        let mock = self.mock.respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "room_id": room_id,
            "servers": servers,
        })));
        MatrixMock { server: self.server, mock }
    }

    /// Returns a data endpoint for a room alias that does not exit.
    pub fn not_found(self) -> MatrixMock<'a> {
        let mock = self.mock.respond_with(ResponseTemplate::new(404).set_body_json(json!({
          "errcode": "M_NOT_FOUND",
          "error": "Room alias not found."
        })));
        MatrixMock { server: self.server, mock }
    }
}

/// A prebuilt mock for creating a room alias.
pub struct CreateRoomAliasEndpoint;

impl<'a> MockEndpoint<'a, CreateRoomAliasEndpoint> {
    /// Returns a data endpoint for creating a room alias.
    pub fn ok(self) -> MatrixMock<'a> {
        let mock = self.mock.respond_with(ResponseTemplate::new(200).set_body_json(json!({})));
        MatrixMock { server: self.server, mock }
    }
}

/// A prebuilt mock for removing a room alias.
pub struct RemoveRoomAliasEndpoint;

impl<'a> MockEndpoint<'a, RemoveRoomAliasEndpoint> {
    /// Returns a data endpoint for removing a room alias.
    pub fn ok(self) -> MatrixMock<'a> {
        let mock = self.mock.respond_with(ResponseTemplate::new(200).set_body_json(json!({})));
        MatrixMock { server: self.server, mock }
    }
}

/// A prebuilt mock for paginating the public room list.
pub struct PublicRoomsEndpoint;

impl<'a> MockEndpoint<'a, PublicRoomsEndpoint> {
    /// Returns a data endpoint for paginating the public room list.
    pub fn ok(
        self,
        chunk: Vec<PublicRoomsChunk>,
        next_batch: Option<String>,
        prev_batch: Option<String>,
        total_room_count_estimate: Option<u64>,
    ) -> MatrixMock<'a> {
        let mock = self.mock.respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "chunk": chunk,
            "next_batch": next_batch,
            "prev_batch": prev_batch,
            "total_room_count_estimate": total_room_count_estimate,
        })));
        MatrixMock { server: self.server, mock }
    }

    /// Returns a data endpoint for paginating the public room list with several
    /// `via` params.
    ///
    /// Each `via` param must be in the `server_map` parameter, otherwise it'll
    /// fail.
    pub fn ok_with_via_params(
        self,
        server_map: BTreeMap<OwnedServerName, Vec<PublicRoomsChunk>>,
    ) -> MatrixMock<'a> {
        let mock = self.mock.respond_with(move |req: &Request| {
            #[derive(Deserialize)]
            struct PartialRequest {
                server: Option<OwnedServerName>,
            }

            let (_, server) = req
                .url
                .query_pairs()
                .into_iter()
                .find(|(key, _)| key == "server")
                .expect("Server param not found in request URL");
            let server = ServerName::parse(server).expect("Couldn't parse server name");
            let chunk = server_map.get(&server).expect("Chunk for the server param not found");
            ResponseTemplate::new(200).set_body_json(json!({
                "chunk": chunk,
                "total_room_count_estimate": chunk.len(),
            }))
        });
        MatrixMock { server: self.server, mock }
    }
}

/// A prebuilt mock for getting the room's visibility in the room directory.
pub struct GetRoomVisibilityEndpoint;

impl<'a> MockEndpoint<'a, GetRoomVisibilityEndpoint> {
    /// Returns an endpoint that get the room's public visibility.
    pub fn ok(self, visibility: Visibility) -> MatrixMock<'a> {
        let mock = self.mock.respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "visibility": visibility,
        })));
        MatrixMock { server: self.server, mock }
    }
}

/// A prebuilt mock for setting the room's visibility in the room directory.
pub struct SetRoomVisibilityEndpoint;

impl<'a> MockEndpoint<'a, SetRoomVisibilityEndpoint> {
    /// Returns an endpoint that updates the room's visibility.
    pub fn ok(self) -> MatrixMock<'a> {
        let mock = self.mock.respond_with(ResponseTemplate::new(200).set_body_json(json!({})));
        MatrixMock { server: self.server, mock }
    }
}

/// A prebuilt mock for `GET room_keys/version`: storage ("backup") of room
/// keys.
pub struct RoomKeysVersionEndpoint;

impl<'a> MockEndpoint<'a, RoomKeysVersionEndpoint> {
    /// Returns an endpoint that says there is a single room keys backup
    pub fn exists(self) -> MatrixMock<'a> {
        let mock = self.mock.respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "algorithm": "m.megolm_backup.v1.curve25519-aes-sha2",
            "auth_data": {
                "public_key": "abcdefg",
                "signatures": {},
            },
            "count": 42,
            "etag": "anopaquestring",
            "version": "1",
        })));
        MatrixMock { server: self.server, mock }
    }

    /// Returns an endpoint that says there is no room keys backup
    pub fn none(self) -> MatrixMock<'a> {
        let mock = self.mock.respond_with(ResponseTemplate::new(404).set_body_json(json!({
            "errcode": "M_NOT_FOUND",
            "error": "No current backup version"
        })));
        MatrixMock { server: self.server, mock }
    }

    /// Returns an endpoint that 429 errors when we get it
    pub fn error429(self) -> MatrixMock<'a> {
        let mock = self.mock.respond_with(ResponseTemplate::new(429).set_body_json(json!({
            "errcode": "M_LIMIT_EXCEEDED",
            "error": "Too many requests",
            "retry_after_ms": 2000
        })));
        MatrixMock { server: self.server, mock }
    }

    /// Returns an endpoint that 404 errors when we get it
    pub fn error404(self) -> MatrixMock<'a> {
        let mock = self.mock.respond_with(ResponseTemplate::new(404));
        MatrixMock { server: self.server, mock }
    }
}

/// A prebuilt mock for `POST room_keys/version`: adding room key backups.
pub struct AddRoomKeysVersionEndpoint;

impl<'a> MockEndpoint<'a, AddRoomKeysVersionEndpoint> {
    /// Returns an endpoint that may be used to add room key backups
    pub fn ok(self) -> MatrixMock<'a> {
        let mock = self
            .mock
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
              "version": "1"
            })))
            .named("POST for the backup creation");
        MatrixMock { server: self.server, mock }
    }
}

/// A prebuilt mock for `DELETE room_keys/version/xxx`: deleting room key
/// backups.
pub struct DeleteRoomKeysVersionEndpoint;

impl<'a> MockEndpoint<'a, DeleteRoomKeysVersionEndpoint> {
    /// Returns an endpoint that allows deleting room key backups
    pub fn ok(self) -> MatrixMock<'a> {
        let mock = self
            .mock
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
            .named("DELETE for the backup deletion");
        MatrixMock { server: self.server, mock }
    }
}

/// A prebuilt mock for `GET /members` request.
pub struct GetRoomMembersEndpoint;

impl<'a> MockEndpoint<'a, GetRoomMembersEndpoint> {
    /// Returns a successful get members request with a list of members.
    pub fn ok(self, members: Vec<Raw<RoomMemberEvent>>) -> MatrixMock<'a> {
        let mock = self.mock.respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "chunk": members,
        })));
        MatrixMock { server: self.server, mock }
    }
}

/// A prebuilt mock for `POST /invite` request.
pub struct InviteUserByIdEndpoint;

impl<'a> MockEndpoint<'a, InviteUserByIdEndpoint> {
    /// Returns a successful invite user by id request.
    pub fn ok(self) -> MatrixMock<'a> {
        let mock = self.mock.respond_with(ResponseTemplate::new(200).set_body_json(json!({})));
        MatrixMock { server: self.server, mock }
    }
}

/// A prebuilt mock for `POST /kick` request.
pub struct KickUserEndpoint;

impl<'a> MockEndpoint<'a, KickUserEndpoint> {
    /// Returns a successful kick user request.
    pub fn ok(self) -> MatrixMock<'a> {
        let mock = self.mock.respond_with(ResponseTemplate::new(200).set_body_json(json!({})));
        MatrixMock { server: self.server, mock }
    }
}

/// A prebuilt mock for `POST /ban` request.
pub struct BanUserEndpoint;

impl<'a> MockEndpoint<'a, BanUserEndpoint> {
    /// Returns a successful ban user request.
    pub fn ok(self) -> MatrixMock<'a> {
        let mock = self.mock.respond_with(ResponseTemplate::new(200).set_body_json(json!({})));
        MatrixMock { server: self.server, mock }
    }
}
