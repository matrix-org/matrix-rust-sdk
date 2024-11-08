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
use ruma::{
    directory::PublicRoomsChunk,
    events::{MessageLikeEventType, StateEventType},
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
            .and(path_regex(r"^/_matrix/client/v3/rooms/.*/send/.*"))
            .and(header("authorization", "Bearer 1234"));
        MockEndpoint { mock, server: &self.server, endpoint: RoomSendEndpoint }
    }

    /// Creates a prebuilt mock for sending an event with a specific type in a
    /// room.
    ///
    /// Note: works with *any* room.
    ///
    /// Similar to: [mock_room_send]
    ///
    /// # Examples
    ///
    /// see also [mock_room_send] for more context.
    ///
    /// ```
    /// let event_id = event_id!("$some_id");
    /// mock_server
    ///     .mock_room_send_for_type("m.room.message")
    ///     .ok(event_id)
    ///     .expect(1)
    ///     .mount()
    ///     .await;
    /// ```
    pub fn mock_room_send_for_type(
        &self,
        event_type: MessageLikeEventType,
    ) -> MockEndpoint<'_, RoomSendEndpoint> {
        let mock = Mock::given(method("PUT"))
            .and(path_regex(format!(r"^/_matrix/client/r0/rooms/.*/send/{}", event_type)))
            .and(header("authorization", "Bearer 1234"));
        MockEndpoint { mock, server: &self.server, endpoint: RoomSendEndpoint }
    }

    /// Creates a prebuilt mock for sending a state event in a room.
    ///
    /// Similar to: [mock_room_send]
    ///
    /// Note: works with *any* room.
    /// Note: works with *any* event type.
    pub fn mock_room_state(&self) -> MockEndpoint<'_, RoomSendEndpoint> {
        let mock = Mock::given(method("PUT"))
            .and(path_regex(r"^/_matrix/client/r0/rooms/.*/state/.*"))
            .and(header("authorization", "Bearer 1234"));
        MockEndpoint { mock, server: &self.server, endpoint: RoomSendEndpoint }
    }

    /// Creates a prebuilt mock for sending a state event with a specific type
    /// in a room.
    ///
    /// Similar to: [mock_room_send]
    ///
    /// Note: works with *any* room.
    pub fn mock_room_state_for_type(
        &self,
        state_type: StateEventType,
    ) -> MockEndpoint<'_, RoomSendEndpoint> {
        let mock = Mock::given(method("PUT"))
            .and(path_regex(format!(r"^/_matrix/client/r0/rooms/.*/state/{}", state_type)))
            .and(header("authorization", "Bearer 1234"));
        MockEndpoint { mock, server: &self.server, endpoint: RoomSendEndpoint }
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

    /// Create a prebuild mock for reading room message with the `/messages` endpoint.
    pub fn mock_room_messages(&self, limit: Option<u32>) -> MockEndpoint<'_, RoomMessagesEndpoint> {
        let mut mock = Mock::given(method("GET"))
            .and(path_regex(r"^/_matrix/client/r0/rooms/.*/messages$"))
            .and(header("authorization", "Bearer 1234"));
        if let Some(l) = limit {
            mock = mock.and(query_param("limit", l.to_string()));
        }
        MockEndpoint { mock, server: &self.server, endpoint: RoomMessagesEndpoint {} }
    }

    /// Create a prebuilt mock for uploading media.
    pub fn mock_upload(&self) -> MockEndpoint<'_, UploadEndpoint> {
        let mock = Mock::given(method("POST"))
            .and(path("/_matrix/media/v3/upload"))
            .and(header("authorization", "Bearer 1234"));
        MockEndpoint { mock, server: &self.server, endpoint: UploadEndpoint }
    }

    /// Create a prebuilt mock for resolving room aliases.
    pub fn mock_room_directory_resolve_alias(&self) -> MockEndpoint<'_, ResolveRoomAliasEndpoint> {
        let mock =
            Mock::given(method("GET")).and(path_regex(r"/_matrix/client/v3/directory/room/.*"));
        MockEndpoint { mock, server: &self.server, endpoint: ResolveRoomAliasEndpoint }
    }

    /// Create a prebuilt mock for creating room aliases.
    pub fn mock_create_room_alias(&self) -> MockEndpoint<'_, CreateRoomAliasEndpoint> {
        let mock =
            Mock::given(method("PUT")).and(path_regex(r"/_matrix/client/v3/directory/room/.*"));
        MockEndpoint { mock, server: &self.server, endpoint: CreateRoomAliasEndpoint }
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

/// A prebuilt mock for sending an event in a room.
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
    /// let send_guard = mock_server
    ///     .mock_room_send()
    ///     .body_matches_partial_json(json!({
    ///         "body": "Hello world",
    ///     }))
    ///     .ok(event_id)
    ///     .expect(1)
    ///     .mount_as_scoped()
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
    /// Returns a messages endpoint that emulates success, i.e. the messages
    /// provided as `response` could be retrieved.
    pub fn ok(self, response: impl Into<Value>) -> MatrixMock<'a> {
        let body: Value = response.into();
        let mock = self.mock.respond_with(ResponseTemplate::new(200).set_body_json(body));
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
