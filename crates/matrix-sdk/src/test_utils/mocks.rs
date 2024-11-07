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

use std::sync::{Arc, Mutex};

use matrix_sdk_base::deserialized_responses::TimelineEvent;
use matrix_sdk_test::{
    test_json, InvitedRoomBuilder, JoinedRoomBuilder, KnockedRoomBuilder, LeftRoomBuilder,
    SyncResponseBuilder,
};
use ruma::{MxcUri, OwnedEventId, OwnedRoomId, RoomId};
use serde_json::json;
use wiremock::{
    matchers::{body_partial_json, header, method, path, path_regex},
    Mock, MockBuilder, MockGuard, MockServer, Respond, ResponseTemplate, Times,
};

use super::logged_in_client;
use crate::{Client, Room};

/// A `wiremock` [`MockServer`] along with useful methods to help mocking Matrix
/// client-server API endpoints easily.
///
/// It implements mock endpoints, limiting the shared code as much as possible,
/// so the mocks are still flexible to use as scoped/unscoped mounts, named, and
/// so on.
///
/// It works like this:
///
/// - start by saying which endpoint you'd like to mock, e.g.
///   [`Self::mock_room_send()`]. This returns a specialized [`MockEndpoint`]
///   data structure, with its own impl. For this example, it's
///   `MockEndpoint<RoomSendEndpoint>`.
/// - configure the response on the endpoint-specific mock data structure. For
///   instance, if you want the sending to result in a transient failure, call
///   [`MockEndpoint::error500`]; if you want it to succeed and return the event
///   `$42`, call [`MockEndpoint::ok()`]. It's still possible to call
///   [`MockEndpoint::respond_with()`], as we do with wiremock MockBuilder, for
///   maximum flexibility when the helpers aren't sufficient.
/// - once the endpoint's response is configured, for any mock builder, you get
///   a [`MatrixMock`]; this is a plain [`wiremock::Mock`] with the server
///   curried, so one doesn't have to pass it around when calling
///   [`MatrixMock::mount()`] or [`MatrixMock::mount_as_scoped()`]. As such, it
///   mostly defers its implementations to [`wiremock::Mock`] under the hood.
pub struct MatrixMockServer {
    server: MockServer,

    /// Make the sync response builder stateful, to keep in memory the batch
    /// token and avoid the client ignoring subsequent responses after the first
    /// one.
    sync_response_builder: Arc<Mutex<SyncResponseBuilder>>,
}

impl MatrixMockServer {
    /// Create a new `wiremock` server specialized for Matrix usage.
    pub async fn new() -> Self {
        let server = MockServer::start().await;
        Self { server, sync_response_builder: Default::default() }
    }

    /// Creates a new [`MatrixMockServer`] when both parts have been already
    /// created.
    pub fn from_server(server: MockServer) -> Self {
        Self { server, sync_response_builder: Default::default() }
    }

    /// Creates a new [`Client`] configured to use this server, preconfigured
    /// with a session expected by the server endpoints.
    pub async fn make_client(&self) -> Client {
        logged_in_client(Some(self.server.uri().to_string())).await
    }

    /// Return the underlying server.
    pub fn server(&self) -> &MockServer {
        &self.server
    }

    /// Overrides the sync/ endpoint with knowledge that the given
    /// invited/joined/knocked/left room exists, runs a sync and returns the
    /// given room.
    pub async fn sync_room(
        &self,
        client: &Client,
        room_id: &RoomId,
        room_data: impl Into<AnyRoomBuilder>,
    ) -> Room {
        let any_room = room_data.into();

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

        client.get_room(room_id).expect("look at me, the room is known now")
    }

    /// Overrides the sync/ endpoint with knowledge that the given room exists
    /// in the joined state, runs a sync and returns the given room.
    pub async fn sync_joined_room(&self, client: &Client, room_id: &RoomId) -> Room {
        self.sync_room(client, room_id, JoinedRoomBuilder::new(room_id)).await
    }

    /// Verify that the previous mocks expected number of requests match
    /// reality, and then cancels all active mocks.
    pub async fn verify_and_reset(&self) {
        self.server.verify().await;
        self.server.reset().await;
    }
}

// Specific mount endpoints.
impl MatrixMockServer {
    /// Mocks a sync endpoint.
    pub fn mock_sync(&self) -> MockEndpoint<'_, SyncEndpoint> {
        let mock = Mock::given(method("GET"))
            .and(path("/_matrix/client/r0/sync"))
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
    pub fn mock_room_send(&self) -> MockEndpoint<'_, RoomSendEndpoint> {
        let mock = Mock::given(method("PUT"))
            .and(path_regex(r"^/_matrix/client/r0/rooms/.*/send/.*"))
            .and(header("authorization", "Bearer 1234"));
        MockEndpoint { mock, server: &self.server, endpoint: RoomSendEndpoint }
    }

    /// Creates a prebuilt mock for asking whether *a* room is encrypted or not.
    ///
    /// Note: Applies to all rooms.
    pub fn mock_room_state_encryption(&self) -> MockEndpoint<'_, EncryptionStateEndpoint> {
        let mock = Mock::given(method("GET"))
            .and(header("authorization", "Bearer 1234"))
            .and(path_regex(r"^/_matrix/client/r0/rooms/.*/state/m.*room.*encryption.?"));
        MockEndpoint { mock, server: &self.server, endpoint: EncryptionStateEndpoint }
    }

    /// Creates a prebuilt mock for setting the room encryption state.
    ///
    /// Note: Applies to all rooms.
    pub fn mock_set_room_state_encryption(&self) -> MockEndpoint<'_, SetEncryptionStateEndpoint> {
        let mock = Mock::given(method("PUT"))
            .and(header("authorization", "Bearer 1234"))
            .and(path_regex(r"^/_matrix/client/r0/rooms/.*/state/m.*room.*encryption.?"));
        MockEndpoint { mock, server: &self.server, endpoint: SetEncryptionStateEndpoint }
    }

    /// Creates a prebuilt mock for the room redact endpoint.
    pub fn mock_room_redact(&self) -> MockEndpoint<'_, RoomRedactEndpoint> {
        let mock = Mock::given(method("PUT"))
            .and(path_regex(r"^/_matrix/client/r0/rooms/.*/redact/.*?/.*?"))
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

    /// Create a prebuilt mock for uploading media.
    pub fn mock_upload(&self) -> MockEndpoint<'_, UploadEndpoint> {
        let mock = Mock::given(method("POST"))
            .and(path("/_matrix/media/r0/upload"))
            .and(header("authorization", "Bearer 1234"));
        MockEndpoint { mock, server: &self.server, endpoint: UploadEndpoint }
    }

    /// Create a prebuilt mock for resolving room aliases.
    pub fn mock_room_directory_resolve_alias(&self) -> MockEndpoint<'_, ResolveRoomAliasEndpoint> {
        let mock =
            Mock::given(method("GET")).and(path_regex(r"/_matrix/client/r0/directory/room/.*"));
        MockEndpoint { mock, server: &self.server, endpoint: ResolveRoomAliasEndpoint }
    }

    /// Create a prebuilt mock for creating room aliases.
    pub fn mock_create_room_alias(&self) -> MockEndpoint<'_, CreateRoomAliasEndpoint> {
        let mock =
            Mock::given(method("PUT")).and(path_regex(r"/_matrix/client/r0/directory/room/.*"));
        MockEndpoint { mock, server: &self.server, endpoint: CreateRoomAliasEndpoint }
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

impl<'a> MatrixMock<'a> {
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
    pub fn respond_with<R: Respond + 'static>(self, func: R) -> MatrixMock<'a> {
        MatrixMock { mock: self.mock.respond_with(func), server: self.server }
    }

    /// Returns a send endpoint that emulates a transient failure, i.e responds
    /// with error 500.
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
}

/// A prebuilt mock for sending an event in a room.
pub struct RoomSendEndpoint;

impl<'a> MockEndpoint<'a, RoomSendEndpoint> {
    /// Ensures that the body of the request is a superset of the provided
    /// `body` parameter.
    pub fn body_matches_partial_json(self, body: serde_json::Value) -> Self {
        Self { mock: self.mock.and(body_partial_json(body)), ..self }
    }

    /// Returns a send endpoint that emulates success, i.e. the event has been
    /// sent with the given event id.
    pub fn ok(self, returned_event_id: impl Into<OwnedEventId>) -> MatrixMock<'a> {
        self.ok_with_event_id(returned_event_id.into())
    }

    /// Returns a send endpoint that emulates a permanent failure (event is too
    /// large).
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

/// A prebuilt mock for running sync v2.
pub struct SyncEndpoint {
    sync_response_builder: Arc<Mutex<SyncResponseBuilder>>,
}

impl<'a> MockEndpoint<'a, SyncEndpoint> {
    /// Temporarily mocks the sync with the given endpoint and runs a client
    /// sync with it.
    ///
    /// After calling this function, the sync endpoint isn't mocked anymore.
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
    pub fn encrypted(self) -> MatrixMock<'a> {
        let mock = self.mock.respond_with(
            ResponseTemplate::new(200).set_body_json(&*test_json::sync_events::ENCRYPTION_CONTENT),
        );
        MatrixMock { mock, server: self.server }
    }

    /// Marks the room as not encrypted.
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
            .and(path_regex(format!("^/_matrix/client/r0/rooms/{room_path}/event/{event_path}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(event.into_raw().json()));
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
