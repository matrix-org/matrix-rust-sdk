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
    sync::{Arc, Mutex, atomic::AtomicU32},
};

use js_int::UInt;
use matrix_sdk_base::deserialized_responses::TimelineEvent;
#[cfg(feature = "experimental-element-recent-emojis")]
use matrix_sdk_base::recent_emojis::RecentEmojisContent;
use matrix_sdk_test::{
    InvitedRoomBuilder, JoinedRoomBuilder, KnockedRoomBuilder, LeftRoomBuilder,
    SyncResponseBuilder, test_json,
};
use percent_encoding::{AsciiSet, CONTROLS};
use ruma::{
    DeviceId, EventId, MilliSecondsSinceUnixEpoch, MxcUri, OwnedDeviceId, OwnedEventId,
    OwnedOneTimeKeyId, OwnedRoomId, OwnedUserId, RoomId, ServerName, UserId,
    api::client::{
        profile::{ProfileFieldName, ProfileFieldValue},
        receipt::create_receipt::v3::ReceiptType,
        room::Visibility,
        sync::sync_events::v5,
        threads::get_thread_subscriptions_changes::unstable::{
            ThreadSubscription, ThreadUnsubscription,
        },
    },
    device_id,
    directory::PublicRoomsChunk,
    encryption::{CrossSigningKey, DeviceKeys, OneTimeKey},
    events::{
        AnyStateEvent, AnySyncTimelineEvent, AnyTimelineEvent, GlobalAccountDataEventType,
        MessageLikeEventType, RoomAccountDataEventType, StateEventType, receipt::ReceiptThread,
        room::member::RoomMemberEvent,
    },
    media::Method,
    push::RuleKind,
    serde::Raw,
    time::Duration,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, from_value, json};
use tokio::sync::oneshot::{self, Receiver};
use wiremock::{
    Mock, MockBuilder, MockGuard, MockServer, Request, Respond, ResponseTemplate, Times,
    matchers::{
        body_json, body_partial_json, header, method, path, path_regex, query_param,
        query_param_is_missing,
    },
};

#[cfg(feature = "e2e-encryption")]
pub mod encryption;
pub mod oauth;

use super::client::MockClientBuilder;
use crate::{Client, OwnedServerName, Room, SlidingSyncBuilder, room::IncludeRelations};

/// Structure used to store the crypto keys uploaded to the server.
/// They will be served back to clients when requested.
#[derive(Debug, Default)]
struct Keys {
    device: BTreeMap<OwnedUserId, BTreeMap<String, Raw<DeviceKeys>>>,
    master: BTreeMap<OwnedUserId, Raw<CrossSigningKey>>,
    self_signing: BTreeMap<OwnedUserId, Raw<CrossSigningKey>>,
    user_signing: BTreeMap<OwnedUserId, Raw<CrossSigningKey>>,
    one_time_keys: BTreeMap<
        OwnedUserId,
        BTreeMap<OwnedDeviceId, BTreeMap<OwnedOneTimeKeyId, Raw<OneTimeKey>>>,
    >,
}

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
/// let result = room.send_raw("m.room.message", json!({ "body": "Hello world" })).await?;
///
/// assert_eq!(
///     event_id,
///     result.response.event_id,
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

    /// Make this mock server capable of mocking real end to end communications
    keys: Arc<Mutex<Keys>>,

    /// For crypto API end-points to work we need to be able to recognise
    /// what client is doing the request by mapping the token to the user_id
    token_to_user_id_map: Arc<Mutex<BTreeMap<String, OwnedUserId>>>,
    token_counter: AtomicU32,
}

impl MatrixMockServer {
    /// Create a new [`wiremock`] server specialized for Matrix usage.
    pub async fn new() -> Self {
        let server = MockServer::start().await;
        let keys: Arc<Mutex<Keys>> = Default::default();
        Self {
            server,
            sync_response_builder: Default::default(),
            keys,
            token_to_user_id_map: Default::default(),
            token_counter: AtomicU32::new(0),
        }
    }

    /// Creates a new [`MatrixMockServer`] from a [`wiremock`] server.
    pub fn from_server(server: MockServer) -> Self {
        let keys: Arc<Mutex<Keys>> = Default::default();
        Self {
            server,
            sync_response_builder: Default::default(),
            keys,
            token_to_user_id_map: Default::default(),
            token_counter: AtomicU32::new(0),
        }
    }

    /// Creates a new [`MockClientBuilder`] configured to use this server,
    /// preconfigured with a session expected by the server endpoints.
    pub fn client_builder(&self) -> MockClientBuilder {
        MockClientBuilder::new(Some(&self.server.uri()))
    }

    /// Return the underlying [`wiremock`] server.
    pub fn server(&self) -> &MockServer {
        &self.server
    }

    /// Return the URI of this server.
    pub fn uri(&self) -> String {
        self.server.uri()
    }

    /// Get an `OAuthMockServer` that uses the same mock server as this one.
    pub fn oauth(&self) -> oauth::OAuthMockServer<'_> {
        oauth::OAuthMockServer::new(self)
    }

    /// Mock the given endpoint.
    fn mock_endpoint<T>(&self, mock: MockBuilder, endpoint: T) -> MockEndpoint<'_, T> {
        MockEndpoint::new(&self.server, mock, endpoint)
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
        let mock = Mock::given(method("GET")).and(path("/_matrix/client/v3/sync"));
        self.mock_endpoint(
            mock,
            SyncEndpoint { sync_response_builder: self.sync_response_builder.clone() },
        )
    }

    /// Mocks the sliding sync endpoint.
    pub fn mock_sliding_sync(&self) -> MockEndpoint<'_, SlidingSyncEndpoint> {
        let mock = Mock::given(method("POST"))
            .and(path("/_matrix/client/unstable/org.matrix.simplified_msc3575/sync"));
        self.mock_endpoint(mock, SlidingSyncEndpoint)
    }

    /// Creates a prebuilt mock for joining a room.
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
    /// let room_id = room_id!("!test:localhost");
    ///
    /// mock_server.mock_room_join(room_id).ok().mount();
    ///
    /// let room = client.join_room_by_id(room_id).await?;
    ///
    /// assert_eq!(
    ///     room_id,
    ///     room.room_id(),
    ///     "The room ID we mocked should match the one we received when we joined the room"
    /// );
    /// # anyhow::Ok(()) });
    /// ```
    pub fn mock_room_join(&self, room_id: &RoomId) -> MockEndpoint<'_, JoinRoomEndpoint> {
        let mock = Mock::given(method("POST"))
            .and(path_regex(format!("^/_matrix/client/v3/rooms/{room_id}/join")));
        self.mock_endpoint(mock, JoinRoomEndpoint { room_id: room_id.to_owned() })
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
    /// let result = room.send_raw("m.room.message", json!({ "body": "Hello world" })).await?;
    ///
    /// assert_eq!(
    ///     event_id,
    ///     result.response.event_id,
    ///     "The event ID we mocked should match the one we received when we sent the event"
    /// );
    /// # anyhow::Ok(()) });
    /// ```
    pub fn mock_room_send(&self) -> MockEndpoint<'_, RoomSendEndpoint> {
        let mock = Mock::given(method("PUT"))
            .and(path_regex(r"^/_matrix/client/v3/rooms/.*/send/.*".to_owned()));
        self.mock_endpoint(mock, RoomSendEndpoint)
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
        let mock =
            Mock::given(method("PUT")).and(path_regex(r"^/_matrix/client/v3/rooms/.*/state/.*/.*"));
        self.mock_endpoint(mock, RoomSendStateEndpoint::default()).expect_default_access_token()
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
    ///     room.latest_encryption_state().await?.is_encrypted(),
    ///     "The room should be marked as encrypted."
    /// );
    /// # anyhow::Ok(()) });
    /// ```
    pub fn mock_room_state_encryption(&self) -> MockEndpoint<'_, EncryptionStateEndpoint> {
        let mock = Mock::given(method("GET"))
            .and(path_regex(r"^/_matrix/client/v3/rooms/.*/state/m.*room.*encryption.?"));
        self.mock_endpoint(mock, EncryptionStateEndpoint).expect_default_access_token()
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
            .and(path_regex(r"^/_matrix/client/v3/rooms/.*/state/m.*room.*encryption.?"));
        self.mock_endpoint(mock, SetEncryptionStateEndpoint).expect_default_access_token()
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
            .and(path_regex(r"^/_matrix/client/v3/rooms/.*/redact/.*?/.*?"));
        self.mock_endpoint(mock, RoomRedactEndpoint).expect_default_access_token()
    }

    /// Creates a prebuilt mock for retrieving an event with /room/.../event.
    pub fn mock_room_event(&self) -> MockEndpoint<'_, RoomEventEndpoint> {
        let mock = Mock::given(method("GET"));
        self.mock_endpoint(mock, RoomEventEndpoint { room: None, match_event_id: false })
            .expect_default_access_token()
    }

    /// Creates a prebuilt mock for retrieving an event with /room/.../context.
    pub fn mock_room_event_context(&self) -> MockEndpoint<'_, RoomEventContextEndpoint> {
        let mock = Mock::given(method("GET"));
        self.mock_endpoint(mock, RoomEventContextEndpoint { room: None, match_event_id: false })
            .expect_default_access_token()
    }

    /// Create a prebuild mock for paginating room message with the `/messages`
    /// endpoint.
    pub fn mock_room_messages(&self) -> MockEndpoint<'_, RoomMessagesEndpoint> {
        let mock =
            Mock::given(method("GET")).and(path_regex(r"^/_matrix/client/v3/rooms/.*/messages$"));
        self.mock_endpoint(mock, RoomMessagesEndpoint).expect_default_access_token()
    }

    /// Create a prebuilt mock for uploading media.
    pub fn mock_upload(&self) -> MockEndpoint<'_, UploadEndpoint> {
        let mock = Mock::given(method("POST")).and(path("/_matrix/media/v3/upload"));
        self.mock_endpoint(mock, UploadEndpoint)
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
        self.mock_endpoint(mock, ResolveRoomAliasEndpoint)
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
        self.mock_endpoint(mock, CreateRoomAliasEndpoint)
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
        self.mock_endpoint(mock, RemoveRoomAliasEndpoint)
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
        self.mock_endpoint(mock, PublicRoomsEndpoint)
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
        self.mock_endpoint(mock, SetRoomVisibilityEndpoint)
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
        self.mock_endpoint(mock, GetRoomVisibilityEndpoint)
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
        let mock =
            Mock::given(method("GET")).and(path_regex(r"_matrix/client/v3/room_keys/version"));
        self.mock_endpoint(mock, RoomKeysVersionEndpoint).expect_default_access_token()
    }

    /// Create a prebuilt mock for adding key storage backups via POST
    pub fn mock_add_room_keys_version(&self) -> MockEndpoint<'_, AddRoomKeysVersionEndpoint> {
        let mock =
            Mock::given(method("POST")).and(path_regex(r"_matrix/client/v3/room_keys/version"));
        self.mock_endpoint(mock, AddRoomKeysVersionEndpoint).expect_default_access_token()
    }

    /// Create a prebuilt mock for adding key storage backups via POST
    pub fn mock_delete_room_keys_version(&self) -> MockEndpoint<'_, DeleteRoomKeysVersionEndpoint> {
        let mock = Mock::given(method("DELETE"))
            .and(path_regex(r"_matrix/client/v3/room_keys/version/[^/]*"));
        self.mock_endpoint(mock, DeleteRoomKeysVersionEndpoint).expect_default_access_token()
    }

    /// Creates a prebuilt mock for the `/sendToDevice` endpoint.
    ///
    /// This mock can be used to simulate sending to-device messages in tests.
    /// # Examples
    ///
    /// ```
    /// # #[cfg(feature = "e2e-encryption")]
    /// # {
    /// # tokio_test::block_on(async {
    /// use std::collections::BTreeMap;
    /// use matrix_sdk::{
    ///     ruma::{
    ///         serde::Raw,
    ///         api::client::to_device::send_event_to_device::v3::Request as ToDeviceRequest,
    ///         to_device::DeviceIdOrAllDevices,
    ///         user_id,owned_device_id
    ///     },
    ///     test_utils::mocks::MatrixMockServer,
    /// };
    /// use serde_json::json;
    ///
    /// let mock_server = MatrixMockServer::new().await;
    /// let client = mock_server.client_builder().build().await;
    ///
    /// mock_server.mock_send_to_device().ok().mock_once().mount().await;
    ///
    /// let request = ToDeviceRequest::new_raw(
    ///     "m.custom.event".into(),
    ///     "txn_id".into(),
    /// BTreeMap::from([
    /// (user_id!("@alice:localhost").to_owned(), BTreeMap::from([(
    ///     DeviceIdOrAllDevices::AllDevices,
    ///     Raw::new(&ruma::events::AnyToDeviceEventContent::Dummy(ruma::events::dummy::ToDeviceDummyEventContent {})).unwrap(),
    /// )])),
    /// ])
    /// );
    ///
    /// client
    ///     .send(request)
    ///     .await
    ///     .expect("We should be able to send a to-device message");
    /// # anyhow::Ok(()) });
    /// # }
    /// ```
    pub fn mock_send_to_device(&self) -> MockEndpoint<'_, SendToDeviceEndpoint> {
        let mock =
            Mock::given(method("PUT")).and(path_regex(r"^/_matrix/client/v3/sendToDevice/.*/.*"));
        self.mock_endpoint(mock, SendToDeviceEndpoint).expect_default_access_token()
    }

    /// Create a prebuilt mock for getting the room members in a room.
    ///
    /// # Examples
    ///
    /// ```
    /// # tokio_test::block_on(async {
    /// use matrix_sdk::{
    ///     ruma::{event_id, room_id},
    ///     test_utils::mocks::MatrixMockServer,
    /// };
    /// use matrix_sdk_base::RoomMemberships;
    /// use matrix_sdk_test::event_factory::EventFactory;
    /// use ruma::{
    ///     events::room::member::{MembershipState, RoomMemberEventContent},
    ///     user_id,
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
    ///     .into_raw();
    ///
    /// mock_server
    ///     .mock_get_members()
    ///     .ok(vec![alice_knock_event])
    ///     .mock_once()
    ///     .mount()
    ///     .await;
    /// let room = mock_server.sync_joined_room(&client, room_id).await;
    ///
    /// let members = room.members(RoomMemberships::all()).await.unwrap();
    /// assert_eq!(members.len(), 1);
    /// # });
    /// ```
    pub fn mock_get_members(&self) -> MockEndpoint<'_, GetRoomMembersEndpoint> {
        let mock =
            Mock::given(method("GET")).and(path_regex(r"^/_matrix/client/v3/rooms/.*/members$"));
        self.mock_endpoint(mock, GetRoomMembersEndpoint)
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
        self.mock_endpoint(mock, InviteUserByIdEndpoint)
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
        self.mock_endpoint(mock, KickUserEndpoint)
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
        self.mock_endpoint(mock, BanUserEndpoint)
    }

    /// Creates a prebuilt mock for the `/_matrix/client/versions` endpoint.
    pub fn mock_versions(&self) -> MockEndpoint<'_, VersionsEndpoint> {
        let mock = Mock::given(method("GET")).and(path_regex(r"^/_matrix/client/versions"));
        self.mock_endpoint(mock, VersionsEndpoint)
    }

    /// Creates a prebuilt mock for the room summary endpoint [MSC3266](https://github.com/matrix-org/matrix-spec-proposals/pull/3266).
    pub fn mock_room_summary(&self) -> MockEndpoint<'_, RoomSummaryEndpoint> {
        let mock = Mock::given(method("GET"))
            .and(path_regex(r"^/_matrix/client/unstable/im.nheko.summary/rooms/.*/summary"));
        self.mock_endpoint(mock, RoomSummaryEndpoint)
    }

    /// Creates a prebuilt mock for the endpoint used to set a room's pinned
    /// events.
    pub fn mock_set_room_pinned_events(&self) -> MockEndpoint<'_, SetRoomPinnedEventsEndpoint> {
        let mock = Mock::given(method("PUT"))
            .and(path_regex(r"^/_matrix/client/v3/rooms/.*/state/m.room.pinned_events/.*?"));
        self.mock_endpoint(mock, SetRoomPinnedEventsEndpoint).expect_default_access_token()
    }

    /// Creates a prebuilt mock for the endpoint used to get information about
    /// the owner of the given access token.
    ///
    /// If no access token is provided, the access token to match is `"1234"`,
    /// which matches the default value in the mock data.
    pub fn mock_who_am_i(&self) -> MockEndpoint<'_, WhoAmIEndpoint> {
        let mock =
            Mock::given(method("GET")).and(path_regex(r"^/_matrix/client/v3/account/whoami"));
        self.mock_endpoint(mock, WhoAmIEndpoint).expect_default_access_token()
    }

    /// Creates a prebuilt mock for the endpoint used to publish end-to-end
    /// encryption keys.
    pub fn mock_upload_keys(&self) -> MockEndpoint<'_, UploadKeysEndpoint> {
        let mock = Mock::given(method("POST")).and(path_regex(r"^/_matrix/client/v3/keys/upload"));
        self.mock_endpoint(mock, UploadKeysEndpoint).expect_default_access_token()
    }

    /// Creates a prebuilt mock for the endpoint used to query end-to-end
    /// encryption keys.
    pub fn mock_query_keys(&self) -> MockEndpoint<'_, QueryKeysEndpoint> {
        let mock = Mock::given(method("POST")).and(path_regex(r"^/_matrix/client/v3/keys/query"));
        self.mock_endpoint(mock, QueryKeysEndpoint).expect_default_access_token()
    }

    /// Creates a prebuilt mock for the endpoint used to discover the URL of a
    /// homeserver.
    pub fn mock_well_known(&self) -> MockEndpoint<'_, WellKnownEndpoint> {
        let mock = Mock::given(method("GET")).and(path_regex(r"^/.well-known/matrix/client"));
        self.mock_endpoint(mock, WellKnownEndpoint)
    }

    /// Creates a prebuilt mock for the endpoint used to publish cross-signing
    /// keys.
    pub fn mock_upload_cross_signing_keys(
        &self,
    ) -> MockEndpoint<'_, UploadCrossSigningKeysEndpoint> {
        let mock = Mock::given(method("POST"))
            .and(path_regex(r"^/_matrix/client/v3/keys/device_signing/upload"));
        self.mock_endpoint(mock, UploadCrossSigningKeysEndpoint).expect_default_access_token()
    }

    /// Creates a prebuilt mock for the endpoint used to publish cross-signing
    /// signatures.
    pub fn mock_upload_cross_signing_signatures(
        &self,
    ) -> MockEndpoint<'_, UploadCrossSigningSignaturesEndpoint> {
        let mock = Mock::given(method("POST"))
            .and(path_regex(r"^/_matrix/client/v3/keys/signatures/upload"));
        self.mock_endpoint(mock, UploadCrossSigningSignaturesEndpoint).expect_default_access_token()
    }

    /// Creates a prebuilt mock for the endpoint used to leave a room.
    pub fn mock_room_leave(&self) -> MockEndpoint<'_, RoomLeaveEndpoint> {
        let mock =
            Mock::given(method("POST")).and(path_regex(r"^/_matrix/client/v3/rooms/.*/leave"));
        self.mock_endpoint(mock, RoomLeaveEndpoint).expect_default_access_token()
    }

    /// Creates a prebuilt mock for the endpoint used to forget a room.
    pub fn mock_room_forget(&self) -> MockEndpoint<'_, RoomForgetEndpoint> {
        let mock =
            Mock::given(method("POST")).and(path_regex(r"^/_matrix/client/v3/rooms/.*/forget"));
        self.mock_endpoint(mock, RoomForgetEndpoint).expect_default_access_token()
    }

    /// Create a prebuilt mock for the endpoint use to log out a session.
    pub fn mock_logout(&self) -> MockEndpoint<'_, LogoutEndpoint> {
        let mock = Mock::given(method("POST")).and(path("/_matrix/client/v3/logout"));
        self.mock_endpoint(mock, LogoutEndpoint).expect_default_access_token()
    }

    /// Create a prebuilt mock for the endpoint used to get the list of thread
    /// roots.
    pub fn mock_room_threads(&self) -> MockEndpoint<'_, RoomThreadsEndpoint> {
        let mock =
            Mock::given(method("GET")).and(path_regex(r"^/_matrix/client/v1/rooms/.*/threads$"));
        self.mock_endpoint(mock, RoomThreadsEndpoint).expect_default_access_token()
    }

    /// Create a prebuilt mock for the endpoint used to get the related events.
    pub fn mock_room_relations(&self) -> MockEndpoint<'_, RoomRelationsEndpoint> {
        // Routing happens in the final method ok(), since it can get complicated.
        let mock = Mock::given(method("GET"));
        self.mock_endpoint(mock, RoomRelationsEndpoint::default()).expect_default_access_token()
    }

    /// Create a prebuilt mock for the endpoint used to get the global account
    /// data.
    ///
    /// # Examples
    ///
    /// ```
    /// tokio_test::block_on(async {
    /// use js_int::uint;
    /// use matrix_sdk::test_utils::mocks::MatrixMockServer;
    ///
    /// let mock_server = MatrixMockServer::new().await;
    /// let client = mock_server.client_builder().build().await;
    ///
    /// mock_server.mock_get_recent_emojis().ok(
    ///     client.user_id().unwrap(),
    ///     vec![(":)".to_string(), uint!(1))]
    /// )
    /// .mock_once()
    /// .mount()
    /// .await;
    ///
    /// client.account().fetch_media_preview_config_event_content().await.unwrap();
    ///
    /// # anyhow::Ok(()) });
    /// ```
    #[cfg(feature = "experimental-element-recent-emojis")]
    pub fn mock_get_recent_emojis(&self) -> MockEndpoint<'_, GetRecentEmojisEndpoint> {
        let mock = Mock::given(method("GET"));
        self.mock_endpoint(mock, GetRecentEmojisEndpoint).expect_default_access_token()
    }

    /// Create a prebuilt mock for the endpoint that updates the global account
    /// data.
    ///
    /// # Examples
    ///
    /// ```
    /// tokio_test::block_on(async {
    /// use js_int::uint;
    /// use matrix_sdk::test_utils::mocks::MatrixMockServer;
    /// use ruma::user_id;
    ///
    /// let mock_server = MatrixMockServer::new().await;
    /// let client = mock_server.client_builder().build().await;
    /// let user_id = client.user_id().unwrap();
    ///
    /// mock.server.mock_get_recent_emojis()
    /// .ok(user_id, vec![(":D".to_string(), uint!(1))])
    /// .mock_once()
    /// .mount()
    /// .await;
    /// mock_server.mock_add_recent_emojis()
    /// .ok(user_id)
    /// .mock_once()
    /// .mount()
    /// .await;
    /// // Calls both get and update recent emoji endpoints, with an update value
    /// client.account().add_recent_emoji(":D").await.unwrap();
    ///
    /// # anyhow::Ok(()) });
    /// ```
    #[cfg(feature = "experimental-element-recent-emojis")]
    pub fn mock_add_recent_emojis(&self) -> MockEndpoint<'_, UpdateRecentEmojisEndpoint> {
        let mock = Mock::given(method("PUT"));
        self.mock_endpoint(mock, UpdateRecentEmojisEndpoint::new()).expect_default_access_token()
    }

    /// Create a prebuilt mock for the endpoint used to get the default secret
    /// storage key.
    ///
    /// # Examples
    ///
    /// ```
    /// tokio_test::block_on(async {
    /// use js_int::uint;
    /// use matrix_sdk::{
    ///     encryption::secret_storage::SecretStorage,
    ///     test_utils::mocks::MatrixMockServer,
    /// };
    ///
    /// let mock_server = MatrixMockServer::new().await;
    /// let client = mock_server.client_builder().build().await;
    ///
    /// mock_server.mock_get_default_secret_storage_key().ok(
    ///     client.user_id().unwrap(),
    ///     "abc", // key ID of default secret storage key
    /// )
    ///     .mount()
    ///     .await;
    ///
    /// client.encryption()
    ///     .secret_storage()
    ///     .fetch_default_key_id()
    ///     .await
    ///     .unwrap();
    ///
    /// # anyhow::Ok(()) });
    /// ```
    #[cfg(feature = "e2e-encryption")]
    pub fn mock_get_default_secret_storage_key(
        &self,
    ) -> MockEndpoint<'_, GetDefaultSecretStorageKeyEndpoint> {
        let mock = Mock::given(method("GET"));
        self.mock_endpoint(mock, GetDefaultSecretStorageKeyEndpoint).expect_default_access_token()
    }

    /// Create a prebuilt mock for the endpoint used to get a secret storage
    /// key.
    ///
    /// # Examples
    ///
    /// ```
    /// tokio_test::block_on(async {
    /// use js_int::uint;
    /// use ruma::events::secret_storage::key;
    /// use ruma::serde::Base64;
    /// use matrix_sdk::{
    ///     encryption::secret_storage::SecretStorage,
    ///     test_utils::mocks::MatrixMockServer,
    /// };
    ///
    /// let mock_server = MatrixMockServer::new().await;
    /// let client = mock_server.client_builder().build().await;
    ///
    /// mock_server.mock_get_default_secret_storage_key().ok(
    ///     client.user_id().unwrap(),
    ///     "abc",
    /// )
    ///     .mount()
    ///     .await;
    /// mock_server.mock_get_secret_storage_key().ok(
    ///     client.user_id().unwrap(),
    ///     &key::SecretStorageKeyEventContent::new(
    ///         "abc".into(),
    ///         key::SecretStorageEncryptionAlgorithm::V1AesHmacSha2(key::SecretStorageV1AesHmacSha2Properties::new(
    ///             Some(Base64::parse("xv5b6/p3ExEw++wTyfSHEg==").unwrap()),
    ///             Some(Base64::parse("ujBBbXahnTAMkmPUX2/0+VTfUh63pGyVRuBcDMgmJC8=").unwrap()),
    ///         )),
    ///     ),
    /// )
    ///     .mount()
    ///     .await;
    ///
    /// client.encryption()
    ///     .secret_storage()
    ///     .open_secret_store("EsTj 3yST y93F SLpB jJsz eAXc 2XzA ygD3 w69H fGaN TKBj jXEd")
    ///     .await
    ///     .unwrap();
    ///
    /// # anyhow::Ok(()) });
    /// ```
    #[cfg(feature = "e2e-encryption")]
    pub fn mock_get_secret_storage_key(&self) -> MockEndpoint<'_, GetSecretStorageKeyEndpoint> {
        let mock = Mock::given(method("GET"));
        self.mock_endpoint(mock, GetSecretStorageKeyEndpoint).expect_default_access_token()
    }

    /// Create a prebuilt mock for the endpoint used to get the default secret
    /// storage key.
    ///
    /// # Examples
    ///
    /// ```
    /// tokio_test::block_on(async {
    /// use js_int::uint;
    /// use serde_json::json;
    /// use ruma::events::GlobalAccountDataEventType;
    /// use matrix_sdk::test_utils::mocks::MatrixMockServer;
    ///
    /// let mock_server = MatrixMockServer::new().await;
    /// let client = mock_server.client_builder().build().await;
    ///
    /// mock_server.mock_get_master_signing_key().ok(
    ///     client.user_id().unwrap(),
    ///     json!({})
    /// )
    /// .mount()
    /// .await;
    ///
    /// client.account()
    ///     .fetch_account_data(GlobalAccountDataEventType::from("m.cross_signing.master".to_owned()))
    ///     .await
    ///     .unwrap();
    ///
    /// # anyhow::Ok(()) });
    /// ```
    #[cfg(feature = "e2e-encryption")]
    pub fn mock_get_master_signing_key(&self) -> MockEndpoint<'_, GetMasterSigningKeyEndpoint> {
        let mock = Mock::given(method("GET"));
        self.mock_endpoint(mock, GetMasterSigningKeyEndpoint).expect_default_access_token()
    }

    /// Create a prebuilt mock for the endpoint used to send a single receipt.
    pub fn mock_send_receipt(
        &self,
        receipt_type: ReceiptType,
    ) -> MockEndpoint<'_, ReceiptEndpoint> {
        let mock = Mock::given(method("POST"))
            .and(path_regex(format!("^/_matrix/client/v3/rooms/.*/receipt/{receipt_type}/")));
        self.mock_endpoint(mock, ReceiptEndpoint).expect_default_access_token()
    }

    /// Create a prebuilt mock for the endpoint used to send multiple receipts.
    pub fn mock_send_read_markers(&self) -> MockEndpoint<'_, ReadMarkersEndpoint> {
        let mock = Mock::given(method("POST"))
            .and(path_regex(r"^/_matrix/client/v3/rooms/.*/read_markers"));
        self.mock_endpoint(mock, ReadMarkersEndpoint).expect_default_access_token()
    }

    /// Create a prebuilt mock for the endpoint used to set room account data.
    pub fn mock_set_room_account_data(
        &self,
        data_type: RoomAccountDataEventType,
    ) -> MockEndpoint<'_, RoomAccountDataEndpoint> {
        let mock = Mock::given(method("PUT")).and(path_regex(format!(
            "^/_matrix/client/v3/user/.*/rooms/.*/account_data/{data_type}"
        )));
        self.mock_endpoint(mock, RoomAccountDataEndpoint).expect_default_access_token()
    }

    /// Create a prebuilt mock for the endpoint used to get the media config of
    /// the homeserver that requires authentication.
    pub fn mock_authenticated_media_config(
        &self,
    ) -> MockEndpoint<'_, AuthenticatedMediaConfigEndpoint> {
        let mock = Mock::given(method("GET")).and(path("/_matrix/client/v1/media/config"));
        self.mock_endpoint(mock, AuthenticatedMediaConfigEndpoint)
    }

    /// Create a prebuilt mock for the endpoint used to get the media config of
    /// the homeserver without requiring authentication.
    pub fn mock_media_config(&self) -> MockEndpoint<'_, MediaConfigEndpoint> {
        let mock = Mock::given(method("GET")).and(path("/_matrix/media/v3/config"));
        self.mock_endpoint(mock, MediaConfigEndpoint)
    }

    /// Create a prebuilt mock for the endpoint used to log into a session.
    pub fn mock_login(&self) -> MockEndpoint<'_, LoginEndpoint> {
        let mock = Mock::given(method("POST")).and(path("/_matrix/client/v3/login"));
        self.mock_endpoint(mock, LoginEndpoint)
    }

    /// Create a prebuilt mock for the endpoint used to list the devices of a
    /// user.
    pub fn mock_devices(&self) -> MockEndpoint<'_, DevicesEndpoint> {
        let mock = Mock::given(method("GET")).and(path("/_matrix/client/v3/devices"));
        self.mock_endpoint(mock, DevicesEndpoint).expect_default_access_token()
    }

    /// Create a prebuilt mock for the endpoint used to query a single device.
    pub fn mock_get_device(&self) -> MockEndpoint<'_, GetDeviceEndpoint> {
        let mock = Mock::given(method("GET")).and(path_regex("/_matrix/client/v3/devices/.*"));
        self.mock_endpoint(mock, GetDeviceEndpoint).expect_default_access_token()
    }

    /// Create a prebuilt mock for the endpoint used to search in the user
    /// directory.
    pub fn mock_user_directory(&self) -> MockEndpoint<'_, UserDirectoryEndpoint> {
        let mock = Mock::given(method("POST"))
            .and(path("/_matrix/client/v3/user_directory/search"))
            .and(body_json(&*test_json::search_users::SEARCH_USERS_REQUEST));
        self.mock_endpoint(mock, UserDirectoryEndpoint).expect_default_access_token()
    }

    /// Create a prebuilt mock for the endpoint used to create a new room.
    pub fn mock_create_room(&self) -> MockEndpoint<'_, CreateRoomEndpoint> {
        let mock = Mock::given(method("POST")).and(path("/_matrix/client/v3/createRoom"));
        self.mock_endpoint(mock, CreateRoomEndpoint).expect_default_access_token()
    }

    /// Create a prebuilt mock for the endpoint used to upgrade a room.
    pub fn mock_upgrade_room(&self) -> MockEndpoint<'_, UpgradeRoomEndpoint> {
        let mock =
            Mock::given(method("POST")).and(path_regex("/_matrix/client/v3/rooms/.*/upgrade"));
        self.mock_endpoint(mock, UpgradeRoomEndpoint).expect_default_access_token()
    }

    /// Create a prebuilt mock for the endpoint used to pre-allocate a MXC URI
    /// for a media file.
    pub fn mock_media_allocate(&self) -> MockEndpoint<'_, MediaAllocateEndpoint> {
        let mock = Mock::given(method("POST")).and(path("/_matrix/media/v1/create"));
        self.mock_endpoint(mock, MediaAllocateEndpoint)
    }

    /// Create a prebuilt mock for the endpoint used to upload a media file with
    /// a pre-allocated MXC URI.
    pub fn mock_media_allocated_upload(
        &self,
        server_name: &str,
        media_id: &str,
    ) -> MockEndpoint<'_, MediaAllocatedUploadEndpoint> {
        let mock = Mock::given(method("PUT"))
            .and(path(format!("/_matrix/media/v3/upload/{server_name}/{media_id}")));
        self.mock_endpoint(mock, MediaAllocatedUploadEndpoint)
    }

    /// Create a prebuilt mock for the endpoint used to download a media file
    /// without requiring authentication.
    pub fn mock_media_download(&self) -> MockEndpoint<'_, MediaDownloadEndpoint> {
        let mock = Mock::given(method("GET")).and(path_regex("^/_matrix/media/v3/download/"));
        self.mock_endpoint(mock, MediaDownloadEndpoint)
    }

    /// Create a prebuilt mock for the endpoint used to download a thumbnail of
    /// a media file without requiring authentication.
    pub fn mock_media_thumbnail(
        &self,
        resize_method: Method,
        width: u16,
        height: u16,
        animated: bool,
    ) -> MockEndpoint<'_, MediaThumbnailEndpoint> {
        let mock = Mock::given(method("GET"))
            .and(path_regex("^/_matrix/media/v3/thumbnail/"))
            .and(query_param("method", resize_method.as_str()))
            .and(query_param("width", width.to_string()))
            .and(query_param("height", height.to_string()))
            .and(query_param("animated", animated.to_string()));
        self.mock_endpoint(mock, MediaThumbnailEndpoint)
    }

    /// Create a prebuilt mock for the endpoint used to download a media file
    /// that requires authentication.
    pub fn mock_authed_media_download(&self) -> MockEndpoint<'_, AuthedMediaDownloadEndpoint> {
        let mock =
            Mock::given(method("GET")).and(path_regex("^/_matrix/client/v1/media/download/"));
        self.mock_endpoint(mock, AuthedMediaDownloadEndpoint).expect_default_access_token()
    }

    /// Create a prebuilt mock for the endpoint used to download a thumbnail of
    /// a media file that requires authentication.
    pub fn mock_authed_media_thumbnail(
        &self,
        resize_method: Method,
        width: u16,
        height: u16,
        animated: bool,
    ) -> MockEndpoint<'_, AuthedMediaThumbnailEndpoint> {
        let mock = Mock::given(method("GET"))
            .and(path_regex("^/_matrix/client/v1/media/thumbnail/"))
            .and(query_param("method", resize_method.as_str()))
            .and(query_param("width", width.to_string()))
            .and(query_param("height", height.to_string()))
            .and(query_param("animated", animated.to_string()));
        self.mock_endpoint(mock, AuthedMediaThumbnailEndpoint).expect_default_access_token()
    }

    /// Create a prebuilt mock for the endpoint used to get a single thread
    /// subscription status in a given room.
    pub fn mock_room_get_thread_subscription(
        &self,
    ) -> MockEndpoint<'_, RoomGetThreadSubscriptionEndpoint> {
        let mock = Mock::given(method("GET"));
        self.mock_endpoint(mock, RoomGetThreadSubscriptionEndpoint::default())
            .expect_default_access_token()
    }

    /// Create a prebuilt mock for the endpoint used to define a thread
    /// subscription in a given room.
    pub fn mock_room_put_thread_subscription(
        &self,
    ) -> MockEndpoint<'_, RoomPutThreadSubscriptionEndpoint> {
        let mock = Mock::given(method("PUT"));
        self.mock_endpoint(mock, RoomPutThreadSubscriptionEndpoint::default())
            .expect_default_access_token()
    }

    /// Create a prebuilt mock for the endpoint used to delete a thread
    /// subscription in a given room.
    pub fn mock_room_delete_thread_subscription(
        &self,
    ) -> MockEndpoint<'_, RoomDeleteThreadSubscriptionEndpoint> {
        let mock = Mock::given(method("DELETE"));
        self.mock_endpoint(mock, RoomDeleteThreadSubscriptionEndpoint::default())
            .expect_default_access_token()
    }

    /// Create a prebuilt mock for the endpoint used to enable a push rule.
    pub fn mock_enable_push_rule(
        &self,
        kind: RuleKind,
        rule_id: impl AsRef<str>,
    ) -> MockEndpoint<'_, EnablePushRuleEndpoint> {
        let rule_id = rule_id.as_ref();
        let mock = Mock::given(method("PUT")).and(path_regex(format!(
            "^/_matrix/client/v3/pushrules/global/{kind}/{rule_id}/enabled",
        )));
        self.mock_endpoint(mock, EnablePushRuleEndpoint).expect_default_access_token()
    }

    /// Create a prebuilt mock for the endpoint used to set push rules actions.
    pub fn mock_set_push_rules_actions(
        &self,
        kind: RuleKind,
        rule_id: PushRuleIdSpec<'_>,
    ) -> MockEndpoint<'_, SetPushRulesActionsEndpoint> {
        let rule_id = rule_id.to_path();
        let mock = Mock::given(method("PUT")).and(path_regex(format!(
            "^/_matrix/client/v3/pushrules/global/{kind}/{rule_id}/actions",
        )));
        self.mock_endpoint(mock, SetPushRulesActionsEndpoint).expect_default_access_token()
    }

    /// Create a prebuilt mock for the endpoint used to set push rules.
    pub fn mock_set_push_rules(
        &self,
        kind: RuleKind,
        rule_id: PushRuleIdSpec<'_>,
    ) -> MockEndpoint<'_, SetPushRulesEndpoint> {
        let rule_id = rule_id.to_path();
        let mock = Mock::given(method("PUT"))
            .and(path_regex(format!("^/_matrix/client/v3/pushrules/global/{kind}/{rule_id}$",)));
        self.mock_endpoint(mock, SetPushRulesEndpoint).expect_default_access_token()
    }

    /// Create a prebuilt mock for the endpoint used to delete push rules.
    pub fn mock_delete_push_rules(
        &self,
        kind: RuleKind,
        rule_id: PushRuleIdSpec<'_>,
    ) -> MockEndpoint<'_, DeletePushRulesEndpoint> {
        let rule_id = rule_id.to_path();
        let mock = Mock::given(method("DELETE"))
            .and(path_regex(format!("^/_matrix/client/v3/pushrules/global/{kind}/{rule_id}$",)));
        self.mock_endpoint(mock, DeletePushRulesEndpoint).expect_default_access_token()
    }

    /// Create a prebuilt mock for the federation version endpoint.
    pub fn mock_federation_version(&self) -> MockEndpoint<'_, FederationVersionEndpoint> {
        let mock = Mock::given(method("GET")).and(path("/_matrix/federation/v1/version"));
        self.mock_endpoint(mock, FederationVersionEndpoint)
    }

    /// Create a prebuilt mock for the endpoint used to get all thread
    /// subscriptions across all rooms.
    pub fn mock_get_thread_subscriptions(
        &self,
    ) -> MockEndpoint<'_, GetThreadSubscriptionsEndpoint> {
        let mock = Mock::given(method("GET"))
            .and(path_regex(r"^/_matrix/client/unstable/io.element.msc4308/thread_subscriptions$"));
        self.mock_endpoint(mock, GetThreadSubscriptionsEndpoint::default())
            .expect_default_access_token()
    }

    /// Create a prebuilt mock for the endpoint used to retrieve a space tree
    pub fn mock_get_hierarchy(&self) -> MockEndpoint<'_, GetHierarchyEndpoint> {
        let mock =
            Mock::given(method("GET")).and(path_regex(r"^/_matrix/client/v1/rooms/.*/hierarchy"));
        self.mock_endpoint(mock, GetHierarchyEndpoint).expect_default_access_token()
    }

    /// Create a prebuilt mock for the endpoint used to set a space child.
    pub fn mock_set_space_child(&self) -> MockEndpoint<'_, SetSpaceChildEndpoint> {
        let mock = Mock::given(method("PUT"))
            .and(path_regex(r"^/_matrix/client/v3/rooms/.*/state/m.space.child/.*?"));
        self.mock_endpoint(mock, SetSpaceChildEndpoint).expect_default_access_token()
    }

    /// Create a prebuilt mock for the endpoint used to set a space parent.
    pub fn mock_set_space_parent(&self) -> MockEndpoint<'_, SetSpaceParentEndpoint> {
        let mock = Mock::given(method("PUT"))
            .and(path_regex(r"^/_matrix/client/v3/rooms/.*/state/m.space.parent"));
        self.mock_endpoint(mock, SetSpaceParentEndpoint).expect_default_access_token()
    }

    /// Create a prebuilt mock for the endpoint used to get a profile field.
    pub fn mock_get_profile_field(
        &self,
        user_id: &UserId,
        field: ProfileFieldName,
    ) -> MockEndpoint<'_, GetProfileFieldEndpoint> {
        let mock = Mock::given(method("GET"))
            .and(path(format!("/_matrix/client/v3/profile/{user_id}/{field}")));
        self.mock_endpoint(mock, GetProfileFieldEndpoint { field })
    }

    /// Create a prebuilt mock for the endpoint used to set a profile field.
    pub fn mock_set_profile_field(
        &self,
        user_id: &UserId,
        field: ProfileFieldName,
    ) -> MockEndpoint<'_, SetProfileFieldEndpoint> {
        let mock = Mock::given(method("PUT"))
            .and(path(format!("/_matrix/client/v3/profile/{user_id}/{field}")));
        self.mock_endpoint(mock, SetProfileFieldEndpoint).expect_default_access_token()
    }

    /// Create a prebuilt mock for the endpoint used to delete a profile field.
    pub fn mock_delete_profile_field(
        &self,
        user_id: &UserId,
        field: ProfileFieldName,
    ) -> MockEndpoint<'_, DeleteProfileFieldEndpoint> {
        let mock = Mock::given(method("DELETE"))
            .and(path(format!("/_matrix/client/v3/profile/{user_id}/{field}")));
        self.mock_endpoint(mock, DeleteProfileFieldEndpoint).expect_default_access_token()
    }

    /// Create a prebuilt mock for the endpoint used to get a profile.
    pub fn mock_get_profile(&self, user_id: &UserId) -> MockEndpoint<'_, GetProfileEndpoint> {
        let mock =
            Mock::given(method("GET")).and(path(format!("/_matrix/client/v3/profile/{user_id}")));
        self.mock_endpoint(mock, GetProfileEndpoint)
    }
}

/// A specification for a push rule ID.
pub enum PushRuleIdSpec<'a> {
    /// A precise rule ID.
    Some(&'a str),
    /// Any rule ID should match.
    Any,
}

impl<'a> PushRuleIdSpec<'a> {
    /// Convert this [`PushRuleIdSpec`] to a path.
    pub fn to_path(&self) -> &str {
        match self {
            PushRuleIdSpec::Some(id) => id,
            PushRuleIdSpec::Any => "[^/]*",
        }
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
    pub(super) mock: Mock,
    pub(super) server: &'a MockServer,
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
    expected_access_token: ExpectedAccessToken,
}

impl<'a, T> MockEndpoint<'a, T> {
    fn new(server: &'a MockServer, mock: MockBuilder, endpoint: T) -> Self {
        Self { server, mock, endpoint, expected_access_token: ExpectedAccessToken::Ignore }
    }

    /// Expect authentication with the default access token on this endpoint.
    pub fn expect_default_access_token(mut self) -> Self {
        self.expected_access_token = ExpectedAccessToken::Default;
        self
    }

    /// Expect authentication with the given access token on this endpoint.
    pub fn expect_access_token(mut self, access_token: &'static str) -> Self {
        self.expected_access_token = ExpectedAccessToken::Custom(access_token);
        self
    }

    /// Expect authentication with any access token on this endpoint, regardless
    /// of its value.
    ///
    /// This is useful if we don't want to track the value of the access token.
    pub fn expect_any_access_token(mut self) -> Self {
        self.expected_access_token = ExpectedAccessToken::Any;
        self
    }

    /// Expect no authentication on this endpoint.
    ///
    /// This means that the endpoint will not match if an `AUTHENTICATION`
    /// header is present.
    pub fn expect_missing_access_token(mut self) -> Self {
        self.expected_access_token = ExpectedAccessToken::Missing;
        self
    }

    /// Ignore the access token on this endpoint.
    ///
    /// This should be used to override the default behavior of an endpoint that
    /// requires access tokens.
    pub fn ignore_access_token(mut self) -> Self {
        self.expected_access_token = ExpectedAccessToken::Ignore;
        self
    }

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
        let mock = self.mock.and(self.expected_access_token).respond_with(func);
        MatrixMock { mock, server: self.server }
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
        self.respond_with(ResponseTemplate::new(500))
    }

    /// Returns a mocked endpoint that emulates an unimplemented endpoint, i.e
    /// responds with a 404 HTTP status code and an `M_UNRECOGNIZED` Matrix
    /// error code.
    ///
    /// Note that the default behavior of the mock server is to return a 404
    /// status code for endpoints that are not mocked with an empty response.
    ///
    /// This can be useful to check if an endpoint is called, even if it is not
    /// implemented by the server.
    pub fn error_unrecognized(self) -> MatrixMock<'a> {
        self.respond_with(ResponseTemplate::new(404).set_body_json(json!({
            "errcode": "M_UNRECOGNIZED",
            "error": "Unrecognized request",
        })))
    }

    /// Returns a mocked endpoint that emulates an unknown token error, i.e
    /// responds with a 401 HTTP status code and an `M_UNKNOWN_TOKEN` Matrix
    /// error code.
    pub fn error_unknown_token(self, soft_logout: bool) -> MatrixMock<'a> {
        self.respond_with(ResponseTemplate::new(401).set_body_json(json!({
            "errcode": "M_UNKNOWN_TOKEN",
            "error": "Unrecognized access token",
            "soft_logout": soft_logout,
        })))
    }

    /// Internal helper to return an `{ event_id }` JSON struct along with a 200
    /// ok response.
    fn ok_with_event_id(self, event_id: OwnedEventId) -> MatrixMock<'a> {
        self.respond_with(ResponseTemplate::new(200).set_body_json(json!({ "event_id": event_id })))
    }

    /// Internal helper to return a 200 OK response with an empty JSON object in
    /// the body.
    fn ok_empty_json(self) -> MatrixMock<'a> {
        self.respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
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
        self.respond_with(ResponseTemplate::new(413).set_body_json(json!({
            // From https://spec.matrix.org/v1.10/client-server-api/#standard-error-response
            "errcode": "M_TOO_LARGE",
        })))
    }
}

/// The access token to expect on an endpoint.
enum ExpectedAccessToken {
    /// Ignore any access token or lack thereof.
    Ignore,

    /// We expect the default access token.
    Default,

    /// We expect the given access token.
    Custom(&'static str),

    /// We expect any access token.
    Any,

    /// We expect that there is no access token.
    Missing,
}

impl ExpectedAccessToken {
    /// Get the access token from the given request.
    fn access_token(request: &Request) -> Option<&str> {
        request
            .headers
            .get(&http::header::AUTHORIZATION)?
            .to_str()
            .ok()?
            .strip_prefix("Bearer ")
            .filter(|token| !token.is_empty())
    }
}

impl wiremock::Match for ExpectedAccessToken {
    fn matches(&self, request: &Request) -> bool {
        match self {
            Self::Ignore => true,
            Self::Default => Self::access_token(request) == Some("1234"),
            Self::Custom(token) => Self::access_token(request) == Some(token),
            Self::Any => Self::access_token(request).is_some(),
            Self::Missing => request.headers.get(&http::header::AUTHORIZATION).is_none(),
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
    /// let result = room.send(content).await?;
    ///
    /// assert_eq!(
    ///     event_id,
    ///     result.response.event_id,
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
    /// let result = room.send_raw("m.room.message", json!({ "body": "Hello world" })).await?;
    /// // The `m.room.message` event type should be mocked by the server.
    /// assert_eq!(
    ///     event_id,
    ///     result.response.event_id,
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
    /// See also [the MSC](https://github.com/matrix-org/matrix-spec-proposals/pull/4140).
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
    ///     .match_delayed_event(Duration::from_millis(500))
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
    pub fn match_delayed_event(self, delay: Duration) -> Self {
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
    /// let result = room.send_raw("m.room.message", json!({ "body": "Hello world" })).await?;
    ///
    /// assert_eq!(
    ///     event_id,
    ///     result.response.event_id,
    ///     "The event ID we mocked should match the one we received when we sent the event"
    /// );
    /// # anyhow::Ok(()) });
    /// ```
    pub fn ok(self, returned_event_id: impl Into<OwnedEventId>) -> MatrixMock<'a> {
        self.ok_with_event_id(returned_event_id.into())
    }

    /// Returns a send endpoint that emulates success, i.e. the event has been
    /// sent with the given event id.
    ///
    /// The sent event is captured and can be accessed using the returned
    /// [`Receiver`]. The [`Receiver`] is valid only for a send call. The given
    /// `event_sender` are added to the event JSON.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # tokio_test::block_on(async {
    /// use matrix_sdk::{
    ///     ruma::{
    ///         event_id, events::room::message::RoomMessageEventContent, room_id,
    ///     },
    ///     test_utils::mocks::MatrixMockServer,
    /// };
    /// use matrix_sdk_test::JoinedRoomBuilder;
    ///
    /// let room_id = room_id!("!room_id:localhost");
    /// let event_id = event_id!("$some_id");
    ///
    /// let server = MatrixMockServer::new().await;
    /// let client = server.client_builder().build().await;
    ///
    /// let user_id = client.user_id().expect("We should have a user ID by now");
    ///
    /// let (receiver, mock) =
    ///     server.mock_room_send().ok_with_capture(event_id, user_id);
    ///
    /// server
    ///     .mock_sync()
    ///     .ok_and_run(&client, |builder| {
    ///         builder.add_joined_room(JoinedRoomBuilder::new(room_id));
    ///     })
    ///     .await;
    ///
    /// // Mock any additional endpoints that might be needed to send the message.
    ///
    /// let room = client
    ///     .get_room(room_id)
    ///     .expect("We should have access to our room now");
    ///
    /// let event_id = room
    ///     .send(RoomMessageEventContent::text_plain("It's a secret to everybody"))
    ///     .await
    ///     .expect("We should be able to send an initial message")
    ///     .response
    ///     .event_id;
    ///
    /// let event = receiver.await?;
    /// # anyhow::Ok(()) });
    /// ```
    pub fn ok_with_capture(
        self,
        returned_event_id: impl Into<OwnedEventId>,
        event_sender: impl Into<OwnedUserId>,
    ) -> (Receiver<Raw<AnySyncTimelineEvent>>, MatrixMock<'a>) {
        let event_id = returned_event_id.into();
        let event_sender = event_sender.into();

        let (sender, receiver) = oneshot::channel();
        let sender = Arc::new(Mutex::new(Some(sender)));

        let ret = self.respond_with(move |request: &Request| {
            if let Some(sender) = sender.lock().unwrap().take() {
                let uri = &request.url;
                let path_segments = uri.path_segments();
                let maybe_event_type = path_segments.and_then(|mut s| s.nth_back(1));
                let event_type = maybe_event_type
                    .as_ref()
                    .map(|&e| e.to_owned())
                    .unwrap_or("m.room.message".to_owned());

                let body: Value =
                    request.body_json().expect("The received body should be valid JSON");

                let event = json!({
                    "event_id": event_id.clone(),
                    "sender": event_sender,
                    "type": event_type,
                    "origin_server_ts": MilliSecondsSinceUnixEpoch::now(),
                    "content": body,
                });

                let event: Raw<AnySyncTimelineEvent> = from_value(event)
                    .expect("We should be able to create a raw event from the content");

                sender.send(event).expect("We should be able to send the event to the receiver");
            }

            ResponseTemplate::new(200).set_body_json(json!({ "event_id": event_id.clone() }))
        });

        (receiver, ret)
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
    ///     ruma::{
    ///         room_id, event_id,
    ///         events::room::power_levels::RoomPowerLevelsEventContent,
    ///         room_version_rules::AuthorizationRules
    ///     },
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
    /// let mut content = RoomPowerLevelsEventContent::new(&AuthorizationRules::V1);
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
    ///         room_version_rules::AuthorizationRules,
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
    /// let response = room.send_state_event(RoomPowerLevelsEventContent::new(&AuthorizationRules::V1)).await?;
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
    /// See also [the MSC](https://github.com/matrix-org/matrix-spec-proposals/pull/4140).
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
    ///     .match_delayed_event(Duration::from_millis(500))
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
    pub fn match_delayed_event(self, delay: Duration) -> Self {
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

impl<'a> MockEndpoint<'a, SyncEndpoint> {
    /// Expect the given timeout, or lack thereof, in the request.
    pub fn timeout(mut self, timeout: Option<Duration>) -> Self {
        if let Some(timeout) = timeout {
            self.mock = self.mock.and(query_param("timeout", timeout.as_millis().to_string()));
        } else {
            self.mock = self.mock.and(query_param_is_missing("timeout"));
        }

        self
    }

    /// Mocks the sync endpoint, using the given function to generate the
    /// response.
    pub fn ok<F: FnOnce(&mut SyncResponseBuilder)>(self, func: F) -> MatrixMock<'a> {
        let json_response = {
            let mut builder = self.endpoint.sync_response_builder.lock().unwrap();
            func(&mut builder);
            builder.build_json_sync_response()
        };

        self.respond_with(ResponseTemplate::new(200).set_body_json(json_response))
    }

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
        let _scope = self.ok(func).mount_as_scoped().await;

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
    ///     room.latest_encryption_state().await?.is_encrypted(),
    ///     "The room should be marked as encrypted."
    /// );
    /// # anyhow::Ok(()) });
    /// ```
    pub fn encrypted(self) -> MatrixMock<'a> {
        self.respond_with(
            ResponseTemplate::new(200).set_body_json(&*test_json::sync_events::ENCRYPTION_CONTENT),
        )
    }

    /// Marks the room as encrypted, opting into experimental state event
    /// encryption.
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
    /// mock_server.mock_room_state_encryption().state_encrypted().mount().await;
    ///
    /// let room = mock_server
    ///     .sync_joined_room(&client, room_id!("!room_id:localhost"))
    ///     .await;
    ///
    /// assert!(
    ///     room.latest_encryption_state().await?.is_state_encrypted(),
    ///     "The room should be marked as state encrypted."
    /// );
    /// # anyhow::Ok(()) });
    #[cfg(feature = "experimental-encrypted-state-events")]
    pub fn state_encrypted(self) -> MatrixMock<'a> {
        self.respond_with(ResponseTemplate::new(200).set_body_json(
            &*test_json::sync_events::ENCRYPTION_WITH_ENCRYPTED_STATE_EVENTS_CONTENT,
        ))
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
    ///     !room.latest_encryption_state().await?.is_encrypted(),
    ///     "The room should not be marked as encrypted."
    /// );
    /// # anyhow::Ok(()) });
    /// ```
    pub fn plain(self) -> MatrixMock<'a> {
        self.respond_with(ResponseTemplate::new(404).set_body_json(&*test_json::NOT_FOUND))
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
            // The event id should begin with `$`, which would be taken as the end of the
            // regex so we need to escape it
            event_id.as_str().replace("$", "\\$")
        } else {
            // Event is at the end, so no need to add anything.
            "".to_owned()
        };

        let room_path = self.endpoint.room.map_or_else(|| ".*".to_owned(), |room| room.to_string());

        let mock = self
            .mock
            .and(path_regex(format!(r"^/_matrix/client/v3/rooms/{room_path}/event/{event_path}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(event.into_raw().json()));
        MatrixMock { server: self.server, mock }
    }
}

/// A builder pattern for the response to a [`RoomEventContextEndpoint`]
/// request.
pub struct RoomContextResponseTemplate {
    event: TimelineEvent,
    events_before: Vec<TimelineEvent>,
    events_after: Vec<TimelineEvent>,
    start: Option<String>,
    end: Option<String>,
    state_events: Vec<Raw<AnyStateEvent>>,
}

impl RoomContextResponseTemplate {
    /// Creates a new context response with the given focused event.
    pub fn new(event: TimelineEvent) -> Self {
        Self {
            event,
            events_before: Vec::new(),
            events_after: Vec::new(),
            start: None,
            end: None,
            state_events: Vec::new(),
        }
    }

    /// Add some events before the target event.
    pub fn events_before(mut self, events: Vec<TimelineEvent>) -> Self {
        self.events_before = events;
        self
    }

    /// Add some events after the target event.
    pub fn events_after(mut self, events: Vec<TimelineEvent>) -> Self {
        self.events_after = events;
        self
    }

    /// Set the start token that could be used for paginating backwards.
    pub fn start(mut self, start: impl Into<String>) -> Self {
        self.start = Some(start.into());
        self
    }

    /// Set the end token that could be used for paginating forwards.
    pub fn end(mut self, end: impl Into<String>) -> Self {
        self.end = Some(end.into());
        self
    }

    /// Pass some extra state events to this response.
    pub fn state_events(mut self, state_events: Vec<Raw<AnyStateEvent>>) -> Self {
        self.state_events = state_events;
        self
    }
}

/// A prebuilt mock for getting a single event with its context in a room.
pub struct RoomEventContextEndpoint {
    room: Option<OwnedRoomId>,
    match_event_id: bool,
}

impl<'a> MockEndpoint<'a, RoomEventContextEndpoint> {
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

    /// Returns an endpoint that emulates a successful response.
    pub fn ok(self, response: RoomContextResponseTemplate) -> MatrixMock<'a> {
        let event_path = if self.endpoint.match_event_id {
            let event_id = response.event.event_id().expect("an event id is required");
            // The event id should begin with `$`, which would be taken as the end of the
            // regex so we need to escape it
            event_id.as_str().replace("$", "\\$")
        } else {
            // Event is at the end, so no need to add anything.
            "".to_owned()
        };

        let room_path = self.endpoint.room.map_or_else(|| ".*".to_owned(), |room| room.to_string());

        let mock = self
            .mock
            .and(path_regex(format!(r"^/_matrix/client/v3/rooms/{room_path}/context/{event_path}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "event": response.event.into_raw().json(),
                "events_before": response.events_before.into_iter().map(|event| event.into_raw().json().to_owned()).collect::<Vec<_>>(),
                "events_after": response.events_after.into_iter().map(|event| event.into_raw().json().to_owned()).collect::<Vec<_>>(),
                "end": response.end,
                "start": response.start,
                "state": response.state_events,
            })));
        MatrixMock { server: self.server, mock }
    }
}

/// A prebuilt mock for the `/messages` endpoint.
pub struct RoomMessagesEndpoint;

/// A prebuilt mock for getting a room messages in a room.
impl<'a> MockEndpoint<'a, RoomMessagesEndpoint> {
    /// Expects an optional limit to be set on the request.
    pub fn match_limit(self, limit: u32) -> Self {
        Self { mock: self.mock.and(query_param("limit", limit.to_string())), ..self }
    }

    /// Expects an optional `from` to be set on the request.
    pub fn match_from(self, from: &str) -> Self {
        Self { mock: self.mock.and(query_param("from", from)), ..self }
    }

    /// Returns a messages endpoint that emulates success, i.e. the messages
    /// provided as `response` could be retrieved.
    ///
    /// Note: pass `chunk` in the correct order: topological for forward
    /// pagination, reverse topological for backwards pagination.
    pub fn ok(self, response: RoomMessagesResponseTemplate) -> MatrixMock<'a> {
        let mut template = ResponseTemplate::new(200).set_body_json(json!({
            "start": response.start,
            "end": response.end,
            "chunk": response.chunk,
            "state": response.state,
        }));

        if let Some(delay) = response.delay {
            template = template.set_delay(delay);
        }

        self.respond_with(template)
    }
}

/// A response to a [`RoomMessagesEndpoint`] query.
pub struct RoomMessagesResponseTemplate {
    /// The start token for this /messages query.
    pub start: String,
    /// The end token for this /messages query (previous batch for back
    /// paginations, next batch for forward paginations).
    pub end: Option<String>,
    /// The set of timeline events returned by this query.
    pub chunk: Vec<Raw<AnyTimelineEvent>>,
    /// The set of state events returned by this query.
    pub state: Vec<Raw<AnyStateEvent>>,
    /// Optional delay to respond to the query.
    pub delay: Option<Duration>,
}

impl RoomMessagesResponseTemplate {
    /// Fill the events returned as part of this response.
    pub fn events(mut self, chunk: Vec<impl Into<Raw<AnyTimelineEvent>>>) -> Self {
        self.chunk = chunk.into_iter().map(Into::into).collect();
        self
    }

    /// Fill the end token.
    pub fn end_token(mut self, token: impl Into<String>) -> Self {
        self.end = Some(token.into());
        self
    }

    /// Respond with a given delay to the query.
    pub fn with_delay(mut self, delay: Duration) -> Self {
        self.delay = Some(delay);
        self
    }
}

impl Default for RoomMessagesResponseTemplate {
    fn default() -> Self {
        Self {
            start: "start-token-unused".to_owned(),
            end: Default::default(),
            chunk: Default::default(),
            state: Default::default(),
            delay: None,
        }
    }
}

/// A prebuilt mock for uploading media.
pub struct UploadEndpoint;

impl<'a> MockEndpoint<'a, UploadEndpoint> {
    /// Expect that the content type matches what's given here.
    pub fn expect_mime_type(self, content_type: &str) -> Self {
        Self { mock: self.mock.and(header("content-type", content_type)), ..self }
    }

    /// Returns a upload endpoint that emulates success, i.e. the media has been
    /// uploaded to the media server and can be accessed using the given
    /// event has been sent with the given [`MxcUri`].
    ///
    /// The uploaded content is captured and can be accessed using the returned
    /// [`Receiver`]. The [`Receiver`] is valid only for a single media
    /// upload.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # tokio_test::block_on(async {
    /// use matrix_sdk::{
    ///     ruma::{event_id, mxc_uri, room_id},
    ///     test_utils::mocks::MatrixMockServer,
    /// };
    ///
    /// let mxid = mxc_uri!("mxc://localhost/12345");
    ///
    /// let server = MatrixMockServer::new().await;
    /// let (receiver, upload_mock) = server.mock_upload().ok_with_capture(mxid);
    /// let client = server.client_builder().build().await;
    ///
    /// client.media().upload(&mime::TEXT_PLAIN, vec![1, 2, 3, 4, 5], None).await?;
    ///
    /// let uploaded = receiver.await?;
    ///
    /// assert_eq!(uploaded, vec![1, 2, 3, 4, 5]);
    /// # anyhow::Ok(()) });
    /// ```
    pub fn ok_with_capture(self, mxc_id: &MxcUri) -> (Receiver<Vec<u8>>, MatrixMock<'a>) {
        let (sender, receiver) = oneshot::channel();
        let sender = Arc::new(Mutex::new(Some(sender)));
        let response_body = json!({"content_uri": mxc_id});

        let ret = self.respond_with(move |request: &Request| {
            let maybe_sender = sender.lock().unwrap().take();

            if let Some(sender) = maybe_sender {
                let body = request.body.clone();
                let _ = sender.send(body);
            }

            ResponseTemplate::new(200).set_body_json(response_body.clone())
        });

        (receiver, ret)
    }

    /// Returns a upload endpoint that emulates success, i.e. the media has been
    /// uploaded to the media server and can be accessed using the given
    /// event has been sent with the given [`MxcUri`].
    pub fn ok(self, mxc_id: &MxcUri) -> MatrixMock<'a> {
        self.respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "content_uri": mxc_id
        })))
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
        self.respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "room_id": room_id,
            "servers": servers,
        })))
    }

    /// Returns a data endpoint for a room alias that does not exit.
    pub fn not_found(self) -> MatrixMock<'a> {
        self.respond_with(ResponseTemplate::new(404).set_body_json(json!({
          "errcode": "M_NOT_FOUND",
          "error": "Room alias not found."
        })))
    }
}

/// A prebuilt mock for creating a room alias.
pub struct CreateRoomAliasEndpoint;

impl<'a> MockEndpoint<'a, CreateRoomAliasEndpoint> {
    /// Returns a data endpoint for creating a room alias.
    pub fn ok(self) -> MatrixMock<'a> {
        self.respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
    }
}

/// A prebuilt mock for removing a room alias.
pub struct RemoveRoomAliasEndpoint;

impl<'a> MockEndpoint<'a, RemoveRoomAliasEndpoint> {
    /// Returns a data endpoint for removing a room alias.
    pub fn ok(self) -> MatrixMock<'a> {
        self.respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
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
        self.respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "chunk": chunk,
            "next_batch": next_batch,
            "prev_batch": prev_batch,
            "total_room_count_estimate": total_room_count_estimate,
        })))
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
        self.respond_with(move |req: &Request| {
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
        })
    }
}

/// A prebuilt mock for getting the room's visibility in the room directory.
pub struct GetRoomVisibilityEndpoint;

impl<'a> MockEndpoint<'a, GetRoomVisibilityEndpoint> {
    /// Returns an endpoint that get the room's public visibility.
    pub fn ok(self, visibility: Visibility) -> MatrixMock<'a> {
        self.respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "visibility": visibility,
        })))
    }
}

/// A prebuilt mock for setting the room's visibility in the room directory.
pub struct SetRoomVisibilityEndpoint;

impl<'a> MockEndpoint<'a, SetRoomVisibilityEndpoint> {
    /// Returns an endpoint that updates the room's visibility.
    pub fn ok(self) -> MatrixMock<'a> {
        self.respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
    }
}

/// A prebuilt mock for `GET room_keys/version`: storage ("backup") of room
/// keys.
pub struct RoomKeysVersionEndpoint;

impl<'a> MockEndpoint<'a, RoomKeysVersionEndpoint> {
    /// Returns an endpoint that says there is a single room keys backup
    pub fn exists(self) -> MatrixMock<'a> {
        self.respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "algorithm": "m.megolm_backup.v1.curve25519-aes-sha2",
            "auth_data": {
                "public_key": "abcdefg",
                "signatures": {},
            },
            "count": 42,
            "etag": "anopaquestring",
            "version": "1",
        })))
    }

    /// Returns an endpoint that says there is no room keys backup
    pub fn none(self) -> MatrixMock<'a> {
        self.respond_with(ResponseTemplate::new(404).set_body_json(json!({
            "errcode": "M_NOT_FOUND",
            "error": "No current backup version"
        })))
    }

    /// Returns an endpoint that 429 errors when we get it
    pub fn error429(self) -> MatrixMock<'a> {
        self.respond_with(ResponseTemplate::new(429).set_body_json(json!({
            "errcode": "M_LIMIT_EXCEEDED",
            "error": "Too many requests",
            "retry_after_ms": 2000
        })))
    }

    /// Returns an endpoint that 404 errors when we get it
    pub fn error404(self) -> MatrixMock<'a> {
        self.respond_with(ResponseTemplate::new(404))
    }
}

/// A prebuilt mock for `POST room_keys/version`: adding room key backups.
pub struct AddRoomKeysVersionEndpoint;

impl<'a> MockEndpoint<'a, AddRoomKeysVersionEndpoint> {
    /// Returns an endpoint that may be used to add room key backups
    pub fn ok(self) -> MatrixMock<'a> {
        self.respond_with(ResponseTemplate::new(200).set_body_json(json!({
          "version": "1"
        })))
        .named("POST for the backup creation")
    }
}

/// A prebuilt mock for `DELETE room_keys/version/xxx`: deleting room key
/// backups.
pub struct DeleteRoomKeysVersionEndpoint;

impl<'a> MockEndpoint<'a, DeleteRoomKeysVersionEndpoint> {
    /// Returns an endpoint that allows deleting room key backups
    pub fn ok(self) -> MatrixMock<'a> {
        self.respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
            .named("DELETE for the backup deletion")
    }
}

/// A prebuilt mock for the `/sendToDevice` endpoint.
///
/// This mock can be used to simulate sending to-device messages in tests.
pub struct SendToDeviceEndpoint;
impl<'a> MockEndpoint<'a, SendToDeviceEndpoint> {
    /// Returns a successful response with default data.
    pub fn ok(self) -> MatrixMock<'a> {
        self.respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
    }
}

/// A prebuilt mock for `GET /members` request.
pub struct GetRoomMembersEndpoint;

impl<'a> MockEndpoint<'a, GetRoomMembersEndpoint> {
    /// Returns a successful get members request with a list of members.
    pub fn ok(self, members: Vec<Raw<RoomMemberEvent>>) -> MatrixMock<'a> {
        self.respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "chunk": members,
        })))
    }
}

/// A prebuilt mock for `POST /invite` request.
pub struct InviteUserByIdEndpoint;

impl<'a> MockEndpoint<'a, InviteUserByIdEndpoint> {
    /// Returns a successful invite user by id request.
    pub fn ok(self) -> MatrixMock<'a> {
        self.respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
    }
}

/// A prebuilt mock for `POST /kick` request.
pub struct KickUserEndpoint;

impl<'a> MockEndpoint<'a, KickUserEndpoint> {
    /// Returns a successful kick user request.
    pub fn ok(self) -> MatrixMock<'a> {
        self.respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
    }
}

/// A prebuilt mock for `POST /ban` request.
pub struct BanUserEndpoint;

impl<'a> MockEndpoint<'a, BanUserEndpoint> {
    /// Returns a successful ban user request.
    pub fn ok(self) -> MatrixMock<'a> {
        self.respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
    }
}

/// A prebuilt mock for `GET /versions` request.
pub struct VersionsEndpoint;

impl<'a> MockEndpoint<'a, VersionsEndpoint> {
    // Get a JSON array of commonly supported versions.
    fn versions() -> Value {
        json!([
            "r0.0.1", "r0.2.0", "r0.3.0", "r0.4.0", "r0.5.0", "r0.6.0", "r0.6.1", "v1.1", "v1.2",
            "v1.3", "v1.4", "v1.5", "v1.6", "v1.7", "v1.8", "v1.9", "v1.10", "v1.11"
        ])
    }

    /// Returns a successful `/_matrix/client/versions` request.
    ///
    /// The response will return some commonly supported versions.
    pub fn ok(self) -> MatrixMock<'a> {
        self.respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "unstable_features": {},
            "versions": Self::versions()
        })))
    }

    /// Returns a successful `/_matrix/client/versions` request.
    ///
    /// The response will return some commonly supported versions and unstable
    /// features supported by the SDK.
    pub fn ok_with_unstable_features(self) -> MatrixMock<'a> {
        self.respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "unstable_features": {
                "org.matrix.label_based_filtering": true,
                "org.matrix.e2e_cross_signing": true,
                "org.matrix.msc4028": true,
                "org.matrix.simplified_msc3575": true,
            },
            "versions": Self::versions()
        })))
    }

    /// Returns a successful `/_matrix/client/versions` request with the given
    /// versions and unstable features.
    pub fn ok_custom(
        self,
        versions: &[&str],
        unstable_features: &BTreeMap<&str, bool>,
    ) -> MatrixMock<'a> {
        self.respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "unstable_features": unstable_features,
            "versions": versions,
        })))
    }
}

/// A prebuilt mock for the room summary endpoint.
pub struct RoomSummaryEndpoint;

impl<'a> MockEndpoint<'a, RoomSummaryEndpoint> {
    /// Returns a successful response with some default data for the given room
    /// id.
    pub fn ok(self, room_id: &RoomId) -> MatrixMock<'a> {
        self.respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "room_id": room_id,
            "guest_can_join": true,
            "num_joined_members": 1,
            "world_readable": true,
            "join_rule": "public",
        })))
    }
}

/// A prebuilt mock to set a room's pinned events.
pub struct SetRoomPinnedEventsEndpoint;

impl<'a> MockEndpoint<'a, SetRoomPinnedEventsEndpoint> {
    /// Returns a successful response with a given event id.
    /// id.
    pub fn ok(self, event_id: OwnedEventId) -> MatrixMock<'a> {
        self.ok_with_event_id(event_id)
    }

    /// Returns an error response with a generic error code indicating the
    /// client is not authorized to set pinned events.
    pub fn unauthorized(self) -> MatrixMock<'a> {
        self.respond_with(ResponseTemplate::new(400))
    }
}

/// A prebuilt mock for `GET /account/whoami` request.
pub struct WhoAmIEndpoint;

impl<'a> MockEndpoint<'a, WhoAmIEndpoint> {
    /// Returns a successful response with the default device ID.
    pub fn ok(self) -> MatrixMock<'a> {
        self.ok_with_device_id(device_id!("D3V1C31D"))
    }

    /// Returns a successful response with the given device ID.
    pub fn ok_with_device_id(self, device_id: &DeviceId) -> MatrixMock<'a> {
        self.respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "user_id": "@joe:example.org",
            "device_id": device_id,
        })))
    }
}

/// A prebuilt mock for `POST /keys/upload` request.
pub struct UploadKeysEndpoint;

impl<'a> MockEndpoint<'a, UploadKeysEndpoint> {
    /// Returns a successful response with counts of 10 curve25519 keys and 20
    /// signed curve25519 keys.
    pub fn ok(self) -> MatrixMock<'a> {
        self.respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "one_time_key_counts": {
                "curve25519": 10,
                "signed_curve25519": 20,
            },
        })))
    }
}

/// A prebuilt mock for `POST /keys/query` request.
pub struct QueryKeysEndpoint;

impl<'a> MockEndpoint<'a, QueryKeysEndpoint> {
    /// Returns a successful empty response.
    pub fn ok(self) -> MatrixMock<'a> {
        self.respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
    }
}

/// A prebuilt mock for `GET /.well-known/matrix/client` request.
pub struct WellKnownEndpoint;

impl<'a> MockEndpoint<'a, WellKnownEndpoint> {
    /// Returns a successful response with the URL for this homeserver.
    pub fn ok(self) -> MatrixMock<'a> {
        let server_uri = self.server.uri();
        self.ok_with_homeserver_url(&server_uri)
    }

    /// Returns a successful response with the given homeserver URL.
    pub fn ok_with_homeserver_url(self, homeserver_url: &str) -> MatrixMock<'a> {
        self.respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "m.homeserver": {
                "base_url": homeserver_url,
            },
            "m.rtc_foci": [
                {
                    "type": "livekit",
                    "livekit_service_url": "https://livekit.example.com",
                },
            ],
        })))
    }

    /// Returns a 404 error response.
    pub fn error404(self) -> MatrixMock<'a> {
        self.respond_with(ResponseTemplate::new(404))
    }
}

/// A prebuilt mock for `POST /keys/device_signing/upload` request.
pub struct UploadCrossSigningKeysEndpoint;

impl<'a> MockEndpoint<'a, UploadCrossSigningKeysEndpoint> {
    /// Returns a successful empty response.
    pub fn ok(self) -> MatrixMock<'a> {
        self.respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
    }

    /// Returns an error response with a UIAA stage that failed to authenticate
    /// because of an invalid password.
    pub fn uiaa_invalid_password(self) -> MatrixMock<'a> {
        self.respond_with(ResponseTemplate::new(401).set_body_json(json!({
            "errcode": "M_FORBIDDEN",
            "error": "Invalid password",
            "flows": [
                {
                    "stages": [
                        "m.login.password"
                    ]
                }
            ],
            "params": {},
            "session": "oFIJVvtEOCKmRUTYKTYIIPHL"
        })))
    }

    /// Returns an error response with a UIAA stage.
    pub fn uiaa(self) -> MatrixMock<'a> {
        self.respond_with(ResponseTemplate::new(401).set_body_json(json!({
            "flows": [
                {
                    "stages": [
                        "m.login.password"
                    ]
                }
            ],
            "params": {},
            "session": "oFIJVvtEOCKmRUTYKTYIIPHL"
        })))
    }

    /// Returns an error response with an unstable OAuth 2.0 UIAA stage.
    pub fn uiaa_unstable_oauth(self) -> MatrixMock<'a> {
        let server_uri = self.server.uri();
        self.respond_with(ResponseTemplate::new(401).set_body_json(json!({
            "session": "dummy",
            "flows": [{
                "stages": [ "org.matrix.cross_signing_reset" ]
            }],
            "params": {
                "org.matrix.cross_signing_reset": {
                    "url": format!("{server_uri}/account/?action=org.matrix.cross_signing_reset"),
                }
            },
            "msg": "To reset your end-to-end encryption cross-signing identity, you first need to approve it and then try again."
        })))
    }

    /// Returns an error response with a stable OAuth 2.0 UIAA stage.
    pub fn uiaa_stable_oauth(self) -> MatrixMock<'a> {
        let server_uri = self.server.uri();
        self.respond_with(ResponseTemplate::new(401).set_body_json(json!({
            "session": "dummy",
            "flows": [{
                "stages": [ "m.oauth" ]
            }],
            "params": {
                "m.oauth": {
                    "url": format!("{server_uri}/account/?action=org.matrix.cross_signing_reset"),
                }
            },
            "msg": "To reset your end-to-end encryption cross-signing identity, you first need to approve it and then try again."
        })))
    }
}

/// A prebuilt mock for `POST /keys/signatures/upload` request.
pub struct UploadCrossSigningSignaturesEndpoint;

impl<'a> MockEndpoint<'a, UploadCrossSigningSignaturesEndpoint> {
    /// Returns a successful empty response.
    pub fn ok(self) -> MatrixMock<'a> {
        self.respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
    }
}

/// A prebuilt mock for the room leave endpoint.
pub struct RoomLeaveEndpoint;

impl<'a> MockEndpoint<'a, RoomLeaveEndpoint> {
    /// Returns a successful response with some default data for the given room
    /// id.
    pub fn ok(self, room_id: &RoomId) -> MatrixMock<'a> {
        self.respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "room_id": room_id,
        })))
    }

    /// Returns a `M_FORBIDDEN` response.
    pub fn forbidden(self) -> MatrixMock<'a> {
        self.respond_with(ResponseTemplate::new(403).set_body_json(json!({
            "errcode": "M_FORBIDDEN",
            "error": "sowwy",
        })))
    }
}

/// A prebuilt mock for the room forget endpoint.
pub struct RoomForgetEndpoint;

impl<'a> MockEndpoint<'a, RoomForgetEndpoint> {
    /// Returns a successful response with some default data for the given room
    /// id.
    pub fn ok(self) -> MatrixMock<'a> {
        self.respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
    }
}

/// A prebuilt mock for `POST /logout` request.
pub struct LogoutEndpoint;

impl<'a> MockEndpoint<'a, LogoutEndpoint> {
    /// Returns a successful empty response.
    pub fn ok(self) -> MatrixMock<'a> {
        self.respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
    }
}

/// A prebuilt mock for a `GET /rooms/{roomId}/threads` request.
pub struct RoomThreadsEndpoint;

impl<'a> MockEndpoint<'a, RoomThreadsEndpoint> {
    /// Expects an optional `from` to be set on the request.
    pub fn match_from(self, from: &str) -> Self {
        Self { mock: self.mock.and(query_param("from", from)), ..self }
    }

    /// Returns a successful response with some optional events and previous
    /// batch token.
    pub fn ok(
        self,
        chunk: Vec<Raw<AnyTimelineEvent>>,
        next_batch: Option<String>,
    ) -> MatrixMock<'a> {
        self.respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "chunk": chunk,
            "next_batch": next_batch
        })))
    }
}

/// A prebuilt mock for a `GET /rooms/{roomId}/relations/{eventId}` family of
/// requests.
#[derive(Default)]
pub struct RoomRelationsEndpoint {
    event_id: Option<OwnedEventId>,
    spec: Option<IncludeRelations>,
}

impl<'a> MockEndpoint<'a, RoomRelationsEndpoint> {
    /// Expects an optional `from` to be set on the request.
    pub fn match_from(self, from: &str) -> Self {
        Self { mock: self.mock.and(query_param("from", from)), ..self }
    }

    /// Expects an optional `limit` to be set on the request.
    pub fn match_limit(self, limit: u32) -> Self {
        Self { mock: self.mock.and(query_param("limit", limit.to_string())), ..self }
    }

    /// Match the given subrequest, according to the given specification.
    pub fn match_subrequest(mut self, spec: IncludeRelations) -> Self {
        self.endpoint.spec = Some(spec);
        self
    }

    /// Expects the request to match a specific event id.
    pub fn match_target_event(mut self, event_id: OwnedEventId) -> Self {
        self.endpoint.event_id = Some(event_id);
        self
    }

    /// Returns a successful response with some optional events and pagination
    /// tokens.
    pub fn ok(mut self, response: RoomRelationsResponseTemplate) -> MatrixMock<'a> {
        // Escape the leading $ to not confuse the regular expression engine.
        let event_spec = self
            .endpoint
            .event_id
            .take()
            .map(|event_id| event_id.as_str().replace("$", "\\$"))
            .unwrap_or_else(|| ".*".to_owned());

        match self.endpoint.spec.take() {
            Some(IncludeRelations::RelationsOfType(rel_type)) => {
                self.mock = self.mock.and(path_regex(format!(
                    r"^/_matrix/client/v1/rooms/.*/relations/{event_spec}/{rel_type}$"
                )));
            }
            Some(IncludeRelations::RelationsOfTypeAndEventType(rel_type, event_type)) => {
                self.mock = self.mock.and(path_regex(format!(
                    r"^/_matrix/client/v1/rooms/.*/relations/{event_spec}/{rel_type}/{event_type}$"
                )));
            }
            _ => {
                self.mock = self.mock.and(path_regex(format!(
                    r"^/_matrix/client/v1/rooms/.*/relations/{event_spec}",
                )));
            }
        }

        self.respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "chunk": response.chunk,
            "next_batch": response.next_batch,
            "prev_batch": response.prev_batch,
            "recursion_depth": response.recursion_depth,
        })))
    }
}

/// Helper function to set up a [`MockBuilder`] so it intercepts the account
/// data URLs.
fn global_account_data_mock_builder(
    builder: MockBuilder,
    user_id: &UserId,
    event_type: GlobalAccountDataEventType,
) -> MockBuilder {
    builder
        .and(path_regex(format!(r"^/_matrix/client/v3/user/{user_id}/account_data/{event_type}",)))
}

/// A prebuilt mock for a `GET
/// /_matrix/client/v3/user/{userId}/account_data/io.element.recent_emoji`
/// request, which fetches the recently used emojis in the account data.
#[cfg(feature = "experimental-element-recent-emojis")]
pub struct GetRecentEmojisEndpoint;

#[cfg(feature = "experimental-element-recent-emojis")]
impl<'a> MockEndpoint<'a, GetRecentEmojisEndpoint> {
    /// Returns a mock for a successful fetch of the recently used emojis in the
    /// account data.
    pub fn ok(self, user_id: &UserId, emojis: Vec<(String, UInt)>) -> MatrixMock<'a> {
        let mock =
            global_account_data_mock_builder(self.mock, user_id, "io.element.recent_emoji".into())
                .respond_with(
                    ResponseTemplate::new(200).set_body_json(json!({ "recent_emoji": emojis })),
                );
        MatrixMock { server: self.server, mock }
    }
}

/// A prebuilt mock for a `PUT
/// /_matrix/client/v3/user/{userId}/account_data/io.element.recent_emoji`
/// request, which updates the recently used emojis in the account data.
#[cfg(feature = "experimental-element-recent-emojis")]
pub struct UpdateRecentEmojisEndpoint {
    pub(crate) request_body: Option<Vec<(String, UInt)>>,
}

#[cfg(feature = "experimental-element-recent-emojis")]
impl UpdateRecentEmojisEndpoint {
    /// Creates a new instance of the recent update recent emojis mock endpoint.
    fn new() -> Self {
        Self { request_body: None }
    }
}

#[cfg(feature = "experimental-element-recent-emojis")]
impl<'a> MockEndpoint<'a, UpdateRecentEmojisEndpoint> {
    /// Returns a mock that will check the body of the request, making sure its
    /// contents match the provided list of emojis.
    pub fn match_emojis_in_request_body(self, emojis: Vec<(String, UInt)>) -> Self {
        Self::new(
            self.server,
            self.mock.and(body_json(json!(RecentEmojisContent::new(emojis)))),
            self.endpoint,
        )
    }

    /// Returns a mock for a successful update of the recent emojis account data
    /// event. The request body contents should match the provided emoji
    /// list.
    #[cfg(feature = "experimental-element-recent-emojis")]
    pub fn ok(self, user_id: &UserId) -> MatrixMock<'a> {
        let mock =
            global_account_data_mock_builder(self.mock, user_id, "io.element.recent_emoji".into())
                .respond_with(ResponseTemplate::new(200).set_body_json(()));
        MatrixMock { server: self.server, mock }
    }
}

/// A prebuilt mock for a `GET
/// /_matrix/client/v3/user/{userId}/account_data/m.secret_storage.default_key`
/// request, which fetches the ID of the default secret storage key.
#[cfg(feature = "e2e-encryption")]
pub struct GetDefaultSecretStorageKeyEndpoint;

#[cfg(feature = "e2e-encryption")]
impl<'a> MockEndpoint<'a, GetDefaultSecretStorageKeyEndpoint> {
    /// Returns a mock for a successful fetch of the default secret storage key.
    pub fn ok(self, user_id: &UserId, key_id: &str) -> MatrixMock<'a> {
        let mock = global_account_data_mock_builder(
            self.mock,
            user_id,
            GlobalAccountDataEventType::SecretStorageDefaultKey,
        )
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "key": key_id
        })));
        MatrixMock { server: self.server, mock }
    }
}

/// A prebuilt mock for a `GET
/// /_matrix/client/v3/user/{userId}/account_data/m.secret_storage.key.{keyId}`
/// request, which fetches information about a secret storage key.
#[cfg(feature = "e2e-encryption")]
pub struct GetSecretStorageKeyEndpoint;

#[cfg(feature = "e2e-encryption")]
impl<'a> MockEndpoint<'a, GetSecretStorageKeyEndpoint> {
    /// Returns a mock for a successful fetch of the secret storage key
    pub fn ok(
        self,
        user_id: &UserId,
        secret_storage_key_event_content: &ruma::events::secret_storage::key::SecretStorageKeyEventContent,
    ) -> MatrixMock<'a> {
        let mock = global_account_data_mock_builder(
            self.mock,
            user_id,
            GlobalAccountDataEventType::SecretStorageKey(
                secret_storage_key_event_content.key_id.clone(),
            ),
        )
        .respond_with(ResponseTemplate::new(200).set_body_json(secret_storage_key_event_content));
        MatrixMock { server: self.server, mock }
    }
}

/// A prebuilt mock for a `GET
/// /_matrix/client/v3/user/{userId}/account_data/m.cross_signing.master`
/// request, which fetches information about the master signing key.
#[cfg(feature = "e2e-encryption")]
pub struct GetMasterSigningKeyEndpoint;

#[cfg(feature = "e2e-encryption")]
impl<'a> MockEndpoint<'a, GetMasterSigningKeyEndpoint> {
    /// Returns a mock for a successful fetch of the master signing key
    pub fn ok<B: Serialize>(self, user_id: &UserId, key_json: B) -> MatrixMock<'a> {
        let mock = global_account_data_mock_builder(
            self.mock,
            user_id,
            GlobalAccountDataEventType::from("m.cross_signing.master".to_owned()),
        )
        .respond_with(ResponseTemplate::new(200).set_body_json(key_json));
        MatrixMock { server: self.server, mock }
    }
}

/// A response to a [`RoomRelationsEndpoint`] query.
#[derive(Default)]
pub struct RoomRelationsResponseTemplate {
    /// The set of timeline events returned by this query.
    pub chunk: Vec<Raw<AnyTimelineEvent>>,

    /// An opaque string representing a pagination token, which semantics depend
    /// on the direction used in the request.
    pub next_batch: Option<String>,

    /// An opaque string representing a pagination token, which semantics depend
    /// on the direction used in the request.
    pub prev_batch: Option<String>,

    /// If `recurse` was set on the request, the depth to which the server
    /// recursed.
    ///
    /// If `recurse` was not set, this field must be absent.
    pub recursion_depth: Option<u32>,
}

impl RoomRelationsResponseTemplate {
    /// Fill the events returned as part of this response.
    pub fn events(mut self, chunk: Vec<impl Into<Raw<AnyTimelineEvent>>>) -> Self {
        self.chunk = chunk.into_iter().map(Into::into).collect();
        self
    }

    /// Fill the `next_batch` token returned as part of this response.
    pub fn next_batch(mut self, token: impl Into<String>) -> Self {
        self.next_batch = Some(token.into());
        self
    }

    /// Fill the `prev_batch` token returned as part of this response.
    pub fn prev_batch(mut self, token: impl Into<String>) -> Self {
        self.prev_batch = Some(token.into());
        self
    }

    /// Fill the recursion depth returned in this response.
    pub fn recursion_depth(mut self, depth: u32) -> Self {
        self.recursion_depth = Some(depth);
        self
    }
}

/// A prebuilt mock for `POST /rooms/{roomId}/receipt/{receiptType}/{eventId}`
/// request.
pub struct ReceiptEndpoint;

impl<'a> MockEndpoint<'a, ReceiptEndpoint> {
    /// Returns a successful empty response.
    pub fn ok(self) -> MatrixMock<'a> {
        self.respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
    }

    /// Ensures that the body of the request is a superset of the provided
    /// `body` parameter.
    pub fn body_matches_partial_json(self, body: Value) -> Self {
        Self { mock: self.mock.and(body_partial_json(body)), ..self }
    }

    /// Ensures that the body of the request is the exact provided `body`
    /// parameter.
    pub fn body_json(self, body: Value) -> Self {
        Self { mock: self.mock.and(body_json(body)), ..self }
    }

    /// Ensures that the request matches a specific receipt thread.
    pub fn match_thread(self, thread: ReceiptThread) -> Self {
        if let Some(thread_str) = thread.as_str() {
            self.body_matches_partial_json(json!({
                "thread_id": thread_str
            }))
        } else {
            self
        }
    }

    /// Ensures that the request matches a specific event id.
    pub fn match_event_id(self, event_id: &EventId) -> Self {
        Self {
            mock: self.mock.and(path_regex(format!(
                r"^/_matrix/client/v3/rooms/.*/receipt/.*/{}$",
                event_id.as_str().replace("$", "\\$")
            ))),
            ..self
        }
    }
}

/// A prebuilt mock for `POST /rooms/{roomId}/read_markers` request.
pub struct ReadMarkersEndpoint;

impl<'a> MockEndpoint<'a, ReadMarkersEndpoint> {
    /// Returns a successful empty response.
    pub fn ok(self) -> MatrixMock<'a> {
        self.respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
    }
}

/// A prebuilt mock for `PUT /user/{userId}/rooms/{roomId}/account_data/{type}`
/// request.
pub struct RoomAccountDataEndpoint;

impl<'a> MockEndpoint<'a, RoomAccountDataEndpoint> {
    /// Returns a successful empty response.
    pub fn ok(self) -> MatrixMock<'a> {
        self.respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
    }
}

/// A prebuilt mock for `GET /_matrix/client/v1/media/config` request.
pub struct AuthenticatedMediaConfigEndpoint;

impl<'a> MockEndpoint<'a, AuthenticatedMediaConfigEndpoint> {
    /// Returns a successful response with the provided max upload size.
    pub fn ok(self, max_upload_size: UInt) -> MatrixMock<'a> {
        self.respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "m.upload.size": max_upload_size,
        })))
    }

    /// Returns a successful response with a maxed out max upload size.
    pub fn ok_default(self) -> MatrixMock<'a> {
        self.respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "m.upload.size": UInt::MAX,
        })))
    }
}

/// A prebuilt mock for `GET /_matrix/media/v3/config` request.
pub struct MediaConfigEndpoint;

impl<'a> MockEndpoint<'a, MediaConfigEndpoint> {
    /// Returns a successful response with the provided max upload size.
    pub fn ok(self, max_upload_size: UInt) -> MatrixMock<'a> {
        self.respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "m.upload.size": max_upload_size,
        })))
    }
}

/// A prebuilt mock for `POST /login` requests.
pub struct LoginEndpoint;

impl<'a> MockEndpoint<'a, LoginEndpoint> {
    /// Returns a successful response.
    pub fn ok(self) -> MatrixMock<'a> {
        self.respond_with(ResponseTemplate::new(200).set_body_json(&*test_json::LOGIN))
    }

    /// Returns a given response on POST /login requests
    ///
    /// # Arguments
    ///
    /// * `response` - The response that the mock server sends on POST /login
    ///   requests.
    ///
    /// # Returns
    ///
    /// Returns a [`MatrixMock`] which can be mounted.
    ///
    /// # Examples
    ///
    /// ```
    /// use matrix_sdk::test_utils::mocks::{
    ///     LoginResponseTemplate200, MatrixMockServer,
    /// };
    /// use matrix_sdk_test::async_test;
    /// use ruma::{device_id, time::Duration, user_id};
    ///
    /// #[async_test]
    /// async fn test_ok_with() {
    ///     let server = MatrixMockServer::new().await;
    ///     server
    ///         .mock_login()
    ///         .ok_with(LoginResponseTemplate200::new(
    ///             "qwerty",
    ///             device_id!("DEADBEEF"),
    ///             user_id!("@cheeky_monkey:matrix.org"),
    ///         ))
    ///         .mount()
    ///         .await;
    ///
    ///     let client = server.client_builder().unlogged().build().await;
    ///
    ///     let result = client
    ///         .matrix_auth()
    ///         .login_username("example", "wordpass")
    ///         .send()
    ///         .await
    ///         .unwrap();
    ///
    ///     assert!(
    ///         result.access_tokesn.unwrap() == "qwerty",
    ///         "wrong access token in response"
    ///     );
    ///     assert!(
    ///         result.device_id.unwrap() == "DEADBEEF",
    ///         "wrong device id in response"
    ///     );
    ///     assert!(
    ///         result.user_id.unwrap() == "@cheeky_monkey:matrix.org",
    ///         "wrong user id in response"
    ///     );
    /// }
    /// ```
    pub fn ok_with(self, response: LoginResponseTemplate200) -> MatrixMock<'a> {
        self.respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": response.access_token,
            "device_id": response.device_id,
            "user_id": response.user_id,
            "expires_in": response.expires_in.map(|duration| { duration.as_millis() }),
            "refresh_token": response.refresh_token,
            "well_known": response.well_known.map(|vals| {
                json!({
                    "m.homeserver": {
                        "base_url": vals.homeserver_url
                    },
                    "m.identity_server": vals.identity_url.map(|url| {
                        json!({
                            "base_url": url
                        })
                    })
                })
            }),
        })))
    }

    /// Ensures that the body of the request is a superset of the provided
    /// `body` parameter.
    pub fn body_matches_partial_json(self, body: Value) -> Self {
        Self { mock: self.mock.and(body_partial_json(body)), ..self }
    }
}

#[derive(Default)]
struct LoginResponseWellKnown {
    /// Required if well_known is used: The base URL for the homeserver for
    /// client-server connections.
    homeserver_url: String,

    /// Required if well_known and m.identity_server are used: The base URL for
    /// the identity server for client-server connections.
    identity_url: Option<String>,
}

/// A response to a [`LoginEndpoint`] query with status code 200.
#[derive(Default)]
pub struct LoginResponseTemplate200 {
    /// Required: An access token for the account. This access token can then be
    /// used to authorize other requests.
    access_token: Option<String>,

    /// Required: ID of the logged-in device. Will be the same as the
    /// corresponding parameter in the request, if one was specified.
    device_id: Option<OwnedDeviceId>,

    /// The lifetime of the access token, in milliseconds. Once the access token
    /// has expired a new access token can be obtained by using the provided
    /// refresh token. If no refresh token is provided, the client will need
    /// to re-log in to obtain a new access token. If not given, the client
    /// can assume that the access token will not expire.
    expires_in: Option<Duration>,

    /// A refresh token for the account. This token can be used to obtain a new
    /// access token when it expires by calling the /refresh endpoint.
    refresh_token: Option<String>,

    /// Required: The fully-qualified Matrix ID for the account.
    user_id: Option<OwnedUserId>,

    /// Optional client configuration provided by the server.
    well_known: Option<LoginResponseWellKnown>,
}

impl LoginResponseTemplate200 {
    /// Constructor for empty response
    pub fn new<T1: Into<OwnedDeviceId>, T2: Into<OwnedUserId>>(
        access_token: &str,
        device_id: T1,
        user_id: T2,
    ) -> Self {
        Self {
            access_token: Some(access_token.to_owned()),
            device_id: Some(device_id.into()),
            user_id: Some(user_id.into()),
            ..Default::default()
        }
    }

    /// sets expires_in
    pub fn expires_in(mut self, value: Duration) -> Self {
        self.expires_in = Some(value);
        self
    }

    /// sets refresh_token
    pub fn refresh_token(mut self, value: &str) -> Self {
        self.refresh_token = Some(value.to_owned());
        self
    }

    /// sets well_known which takes a homeserver_url and an optional
    /// identity_url
    pub fn well_known(mut self, homeserver_url: String, identity_url: Option<String>) -> Self {
        self.well_known = Some(LoginResponseWellKnown { homeserver_url, identity_url });
        self
    }
}

/// A prebuilt mock for `GET /devices` requests.
pub struct DevicesEndpoint;

impl<'a> MockEndpoint<'a, DevicesEndpoint> {
    /// Returns a successful response.
    pub fn ok(self) -> MatrixMock<'a> {
        self.respond_with(ResponseTemplate::new(200).set_body_json(&*test_json::DEVICES))
    }
}

/// A prebuilt mock for `GET /devices/{deviceId}` requests.
pub struct GetDeviceEndpoint;

impl<'a> MockEndpoint<'a, GetDeviceEndpoint> {
    /// Returns a successful response.
    pub fn ok(self) -> MatrixMock<'a> {
        self.respond_with(ResponseTemplate::new(200).set_body_json(&*test_json::DEVICE))
    }
}

/// A prebuilt mock for `POST /user_directory/search` requests.
pub struct UserDirectoryEndpoint;

impl<'a> MockEndpoint<'a, UserDirectoryEndpoint> {
    /// Returns a successful response.
    pub fn ok(self) -> MatrixMock<'a> {
        self.respond_with(
            ResponseTemplate::new(200)
                .set_body_json(&*test_json::search_users::SEARCH_USERS_RESPONSE),
        )
    }
}

/// A prebuilt mock for `POST /createRoom` requests.
pub struct CreateRoomEndpoint;

impl<'a> MockEndpoint<'a, CreateRoomEndpoint> {
    /// Returns a successful response.
    pub fn ok(self) -> MatrixMock<'a> {
        self.respond_with(
            ResponseTemplate::new(200).set_body_json(json!({ "room_id": "!room:example.org"})),
        )
    }
}

/// A prebuilt mock for `POST /rooms/{roomId}/upgrade` requests.
pub struct UpgradeRoomEndpoint;

impl<'a> MockEndpoint<'a, UpgradeRoomEndpoint> {
    /// Returns a successful response with desired replacement_room ID.
    pub fn ok_with(self, new_room_id: &RoomId) -> MatrixMock<'a> {
        self.respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({ "replacement_room": new_room_id.as_str()})),
        )
    }
}

/// A prebuilt mock for `POST /media/v1/create` requests.
pub struct MediaAllocateEndpoint;

impl<'a> MockEndpoint<'a, MediaAllocateEndpoint> {
    /// Returns a successful response.
    pub fn ok(self) -> MatrixMock<'a> {
        self.respond_with(ResponseTemplate::new(200).set_body_json(json!({
          "content_uri": "mxc://example.com/AQwafuaFswefuhsfAFAgsw"
        })))
    }
}

/// A prebuilt mock for `PUT /media/v3/upload/{server_name}/{media_id}`
/// requests.
pub struct MediaAllocatedUploadEndpoint;

impl<'a> MockEndpoint<'a, MediaAllocatedUploadEndpoint> {
    /// Returns a successful response.
    pub fn ok(self) -> MatrixMock<'a> {
        self.respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
    }
}

/// A prebuilt mock for `GET /media/v3/download` requests.
pub struct MediaDownloadEndpoint;

impl<'a> MockEndpoint<'a, MediaDownloadEndpoint> {
    /// Returns a successful response with a plain text content.
    pub fn ok_plain_text(self) -> MatrixMock<'a> {
        self.respond_with(ResponseTemplate::new(200).set_body_string("Hello, World!"))
    }

    /// Returns a successful response with a fake image content.
    pub fn ok_image(self) -> MatrixMock<'a> {
        self.respond_with(
            ResponseTemplate::new(200).set_body_raw(b"binaryjpegfullimagedata", "image/jpeg"),
        )
    }
}

/// A prebuilt mock for `GET /media/v3/thumbnail` requests.
pub struct MediaThumbnailEndpoint;

impl<'a> MockEndpoint<'a, MediaThumbnailEndpoint> {
    /// Returns a successful response with a fake image content.
    pub fn ok(self) -> MatrixMock<'a> {
        self.respond_with(
            ResponseTemplate::new(200).set_body_raw(b"binaryjpegthumbnaildata", "image/jpeg"),
        )
    }
}

/// A prebuilt mock for `GET /client/v1/media/download` requests.
pub struct AuthedMediaDownloadEndpoint;

impl<'a> MockEndpoint<'a, AuthedMediaDownloadEndpoint> {
    /// Returns a successful response with a plain text content.
    pub fn ok_plain_text(self) -> MatrixMock<'a> {
        self.respond_with(ResponseTemplate::new(200).set_body_string("Hello, World!"))
    }

    /// Returns a successful response with the given bytes.
    pub fn ok_bytes(self, bytes: Vec<u8>) -> MatrixMock<'a> {
        self.respond_with(
            ResponseTemplate::new(200).set_body_raw(bytes, "application/octet-stream"),
        )
    }

    /// Returns a successful response with a fake image content.
    pub fn ok_image(self) -> MatrixMock<'a> {
        self.respond_with(
            ResponseTemplate::new(200).set_body_raw(b"binaryjpegfullimagedata", "image/jpeg"),
        )
    }
}

/// A prebuilt mock for `GET /client/v1/media/thumbnail` requests.
pub struct AuthedMediaThumbnailEndpoint;

impl<'a> MockEndpoint<'a, AuthedMediaThumbnailEndpoint> {
    /// Returns a successful response with a fake image content.
    pub fn ok(self) -> MatrixMock<'a> {
        self.respond_with(
            ResponseTemplate::new(200).set_body_raw(b"binaryjpegthumbnaildata", "image/jpeg"),
        )
    }
}

/// A prebuilt mock for `GET /client/v3/rooms/{room_id}/join` requests.
pub struct JoinRoomEndpoint {
    room_id: OwnedRoomId,
}

impl<'a> MockEndpoint<'a, JoinRoomEndpoint> {
    /// Returns a successful response using the provided [`RoomId`].
    pub fn ok(self) -> MatrixMock<'a> {
        let room_id = self.endpoint.room_id.to_owned();

        self.respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "room_id": room_id,
        })))
    }
}

#[derive(Default)]
struct ThreadSubscriptionMatchers {
    /// Optional room id to match in the query.
    room_id: Option<OwnedRoomId>,
    /// Optional thread root event id to match in the query.
    thread_root: Option<OwnedEventId>,
}

impl ThreadSubscriptionMatchers {
    /// Match the request parameter against a specific room id.
    fn match_room_id(mut self, room_id: OwnedRoomId) -> Self {
        self.room_id = Some(room_id);
        self
    }

    /// Match the request parameter against a specific thread root event id.
    fn match_thread_id(mut self, thread_root: OwnedEventId) -> Self {
        self.thread_root = Some(thread_root);
        self
    }

    /// Compute the final URI for the thread subscription endpoint.
    fn endpoint_regexp_uri(&self) -> String {
        if self.room_id.is_some() || self.thread_root.is_some() {
            format!(
                "^/_matrix/client/unstable/io.element.msc4306/rooms/{}/thread/{}/subscription$",
                self.room_id.as_deref().map(|s| s.as_str()).unwrap_or(".*"),
                self.thread_root.as_deref().map(|s| s.as_str()).unwrap_or(".*").replace("$", "\\$")
            )
        } else {
            "^/_matrix/client/unstable/io.element.msc4306/rooms/.*/thread/.*/subscription$"
                .to_owned()
        }
    }
}

/// A prebuilt mock for `GET
/// /client/*/rooms/{room_id}/threads/{thread_root}/subscription`
#[derive(Default)]
pub struct RoomGetThreadSubscriptionEndpoint {
    matchers: ThreadSubscriptionMatchers,
}

impl<'a> MockEndpoint<'a, RoomGetThreadSubscriptionEndpoint> {
    /// Returns a successful response for the given thread subscription.
    pub fn ok(mut self, automatic: bool) -> MatrixMock<'a> {
        self.mock = self.mock.and(path_regex(self.endpoint.matchers.endpoint_regexp_uri()));
        self.respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "automatic": automatic
        })))
    }

    /// Match the request parameter against a specific room id.
    pub fn match_room_id(mut self, room_id: OwnedRoomId) -> Self {
        self.endpoint.matchers = self.endpoint.matchers.match_room_id(room_id);
        self
    }
    /// Match the request parameter against a specific thread root event id.
    pub fn match_thread_id(mut self, thread_root: OwnedEventId) -> Self {
        self.endpoint.matchers = self.endpoint.matchers.match_thread_id(thread_root);
        self
    }
}

/// A prebuilt mock for `PUT
/// /client/*/rooms/{room_id}/threads/{thread_root}/subscription`
#[derive(Default)]
pub struct RoomPutThreadSubscriptionEndpoint {
    matchers: ThreadSubscriptionMatchers,
}

impl<'a> MockEndpoint<'a, RoomPutThreadSubscriptionEndpoint> {
    /// Returns a successful response for the given setting of thread
    /// subscription.
    pub fn ok(mut self) -> MatrixMock<'a> {
        self.mock = self.mock.and(path_regex(self.endpoint.matchers.endpoint_regexp_uri()));
        self.respond_with(ResponseTemplate::new(200))
    }

    /// Returns that the server skipped an automated thread subscription,
    /// because the user unsubscribed to the thread after the event id passed in
    /// the automatic subscription.
    pub fn conflicting_unsubscription(mut self) -> MatrixMock<'a> {
        self.mock = self.mock.and(path_regex(self.endpoint.matchers.endpoint_regexp_uri()));
        self.respond_with(ResponseTemplate::new(409).set_body_json(json!({
            "errcode": "IO.ELEMENT.MSC4306.M_CONFLICTING_UNSUBSCRIPTION",
            "error": "the user unsubscribed after the subscription event id"
        })))
    }

    /// Match the request parameter against a specific room id.
    pub fn match_room_id(mut self, room_id: OwnedRoomId) -> Self {
        self.endpoint.matchers = self.endpoint.matchers.match_room_id(room_id);
        self
    }
    /// Match the request parameter against a specific thread root event id.
    pub fn match_thread_id(mut self, thread_root: OwnedEventId) -> Self {
        self.endpoint.matchers = self.endpoint.matchers.match_thread_id(thread_root);
        self
    }
    /// Match the request body's `automatic` field against a specific event id.
    pub fn match_automatic_event_id(mut self, up_to_event_id: &EventId) -> Self {
        self.mock = self.mock.and(body_json(json!({
            "automatic": up_to_event_id
        })));
        self
    }
}

/// A prebuilt mock for `DELETE
/// /client/*/rooms/{room_id}/threads/{thread_root}/subscription`
#[derive(Default)]
pub struct RoomDeleteThreadSubscriptionEndpoint {
    matchers: ThreadSubscriptionMatchers,
}

impl<'a> MockEndpoint<'a, RoomDeleteThreadSubscriptionEndpoint> {
    /// Returns a successful response for the deletion of a given thread
    /// subscription.
    pub fn ok(mut self) -> MatrixMock<'a> {
        self.mock = self.mock.and(path_regex(self.endpoint.matchers.endpoint_regexp_uri()));
        self.respond_with(ResponseTemplate::new(200))
    }

    /// Match the request parameter against a specific room id.
    pub fn match_room_id(mut self, room_id: OwnedRoomId) -> Self {
        self.endpoint.matchers = self.endpoint.matchers.match_room_id(room_id);
        self
    }
    /// Match the request parameter against a specific thread root event id.
    pub fn match_thread_id(mut self, thread_root: OwnedEventId) -> Self {
        self.endpoint.matchers = self.endpoint.matchers.match_thread_id(thread_root);
        self
    }
}

/// A prebuilt mock for `PUT
/// /_matrix/client/v3/pushrules/global/{kind}/{ruleId}/enabled`.
pub struct EnablePushRuleEndpoint;

impl<'a> MockEndpoint<'a, EnablePushRuleEndpoint> {
    /// Returns a successful empty JSON response.
    pub fn ok(self) -> MatrixMock<'a> {
        self.ok_empty_json()
    }
}

/// A prebuilt mock for `PUT
/// /_matrix/client/v3/pushrules/global/{kind}/{ruleId}/actions`.
pub struct SetPushRulesActionsEndpoint;

impl<'a> MockEndpoint<'a, SetPushRulesActionsEndpoint> {
    /// Returns a successful empty JSON response.
    pub fn ok(self) -> MatrixMock<'a> {
        self.ok_empty_json()
    }
}

/// A prebuilt mock for `PUT
/// /_matrix/client/v3/pushrules/global/{kind}/{ruleId}`.
pub struct SetPushRulesEndpoint;

impl<'a> MockEndpoint<'a, SetPushRulesEndpoint> {
    /// Returns a successful empty JSON response.
    pub fn ok(self) -> MatrixMock<'a> {
        self.ok_empty_json()
    }
}

/// A prebuilt mock for `DELETE
/// /_matrix/client/v3/pushrules/global/{kind}/{ruleId}`.
pub struct DeletePushRulesEndpoint;

impl<'a> MockEndpoint<'a, DeletePushRulesEndpoint> {
    /// Returns a successful empty JSON response.
    pub fn ok(self) -> MatrixMock<'a> {
        self.ok_empty_json()
    }
}

/// A prebuilt mock for the federation version endpoint.
pub struct FederationVersionEndpoint;

impl<'a> MockEndpoint<'a, FederationVersionEndpoint> {
    /// Returns a successful response with the given server name and version.
    pub fn ok(self, server_name: &str, version: &str) -> MatrixMock<'a> {
        let response_body = json!({
            "server": {
                "name": server_name,
                "version": version
            }
        });
        self.respond_with(ResponseTemplate::new(200).set_body_json(response_body))
    }

    /// Returns a successful response with empty/missing server information.
    pub fn ok_empty(self) -> MatrixMock<'a> {
        let response_body = json!({});
        self.respond_with(ResponseTemplate::new(200).set_body_json(response_body))
    }
}

/// A prebuilt mock for `GET ^/_matrix/client/v3/thread_subscriptions`.
#[derive(Default)]
pub struct GetThreadSubscriptionsEndpoint {
    /// New thread subscriptions per (room id, thread root event id).
    subscribed: BTreeMap<OwnedRoomId, BTreeMap<OwnedEventId, ThreadSubscription>>,
    /// New thread unsubscriptions per (room id, thread root event id).
    unsubscribed: BTreeMap<OwnedRoomId, BTreeMap<OwnedEventId, ThreadUnsubscription>>,
    /// Optional delay to respond to the query.
    delay: Option<Duration>,
}

impl<'a> MockEndpoint<'a, GetThreadSubscriptionsEndpoint> {
    /// Add a single thread subscription to the response.
    pub fn add_subscription(
        mut self,
        room_id: OwnedRoomId,
        thread_root: OwnedEventId,
        subscription: ThreadSubscription,
    ) -> Self {
        self.endpoint.subscribed.entry(room_id).or_default().insert(thread_root, subscription);
        self
    }

    /// Add a single thread unsubscription to the response.
    pub fn add_unsubscription(
        mut self,
        room_id: OwnedRoomId,
        thread_root: OwnedEventId,
        unsubscription: ThreadUnsubscription,
    ) -> Self {
        self.endpoint.unsubscribed.entry(room_id).or_default().insert(thread_root, unsubscription);
        self
    }

    /// Respond with a given delay to the query.
    pub fn with_delay(mut self, delay: Duration) -> Self {
        self.endpoint.delay = Some(delay);
        self
    }

    /// Match the `from` query parameter to a given value.
    pub fn match_from(self, from: &str) -> Self {
        Self { mock: self.mock.and(query_param("from", from)), ..self }
    }
    /// Match the `to` query parameter to a given value.
    pub fn match_to(self, to: &str) -> Self {
        Self { mock: self.mock.and(query_param("to", to)), ..self }
    }

    /// Returns a successful response with the given thread subscriptions, and
    /// "end" parameter to be used in the next query.
    pub fn ok(self, end: Option<String>) -> MatrixMock<'a> {
        let response_body = json!({
            "subscribed": self.endpoint.subscribed,
            "unsubscribed": self.endpoint.unsubscribed,
            "end": end,
        });

        let mut template = ResponseTemplate::new(200).set_body_json(response_body);

        if let Some(delay) = self.endpoint.delay {
            template = template.set_delay(delay);
        }

        self.respond_with(template)
    }
}

/// A prebuilt mock for `GET /client/*/rooms/{roomId}/hierarchy`
#[derive(Default)]
pub struct GetHierarchyEndpoint;

impl<'a> MockEndpoint<'a, GetHierarchyEndpoint> {
    /// Returns a successful response containing the given room IDs.
    pub fn ok_with_room_ids(self, room_ids: Vec<&RoomId>) -> MatrixMock<'a> {
        let rooms = room_ids
            .iter()
            .map(|id| {
                json!({
                  "room_id": id,
                  "num_joined_members": 1,
                  "world_readable": false,
                  "guest_can_join": false,
                  "children_state": []
                })
            })
            .collect::<Vec<_>>();

        self.respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "rooms": rooms,
        })))
    }

    /// Returns a successful response containing the given room IDs and children
    /// states
    pub fn ok_with_room_ids_and_children_state(
        self,
        room_ids: Vec<&RoomId>,
        children_state: Vec<(&RoomId, Vec<&ServerName>)>,
    ) -> MatrixMock<'a> {
        let children_state = children_state
            .into_iter()
            .map(|(id, via)| {
                json!({
                    "type":
                    "m.space.child",
                    "state_key": id,
                    "content": { "via": via },
                    "sender": "@bob:matrix.org",
                    "origin_server_ts": MilliSecondsSinceUnixEpoch::now()
                })
            })
            .collect::<Vec<_>>();

        let rooms = room_ids
            .iter()
            .map(|id| {
                json!({
                  "room_id": id,
                  "num_joined_members": 1,
                  "world_readable": false,
                  "guest_can_join": false,
                  "children_state": children_state
                })
            })
            .collect::<Vec<_>>();

        self.respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "rooms": rooms,
        })))
    }

    /// Returns a successful response with an empty list of rooms.
    pub fn ok(self) -> MatrixMock<'a> {
        self.respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "rooms": []
        })))
    }
}

/// A prebuilt mock for `PUT
/// /_matrix/client/v3/rooms/{roomId}/state/m.space.child/{stateKey}`
pub struct SetSpaceChildEndpoint;

impl<'a> MockEndpoint<'a, SetSpaceChildEndpoint> {
    /// Returns a successful response with a given event id.
    pub fn ok(self, event_id: OwnedEventId) -> MatrixMock<'a> {
        self.ok_with_event_id(event_id)
    }

    /// Returns an error response with a generic error code indicating the
    /// client is not authorized to set space children.
    pub fn unauthorized(self) -> MatrixMock<'a> {
        self.respond_with(ResponseTemplate::new(400))
    }
}

/// A prebuilt mock for `PUT
/// /_matrix/client/v3/rooms/{roomId}/state/m.space.parent/{stateKey}`
pub struct SetSpaceParentEndpoint;

impl<'a> MockEndpoint<'a, SetSpaceParentEndpoint> {
    /// Returns a successful response with a given event id.
    pub fn ok(self, event_id: OwnedEventId) -> MatrixMock<'a> {
        self.ok_with_event_id(event_id)
    }

    /// Returns an error response with a generic error code indicating the
    /// client is not authorized to set space parents.
    pub fn unauthorized(self) -> MatrixMock<'a> {
        self.respond_with(ResponseTemplate::new(400))
    }
}

/// A prebuilt mock for running simplified sliding sync.
pub struct SlidingSyncEndpoint;

impl<'a> MockEndpoint<'a, SlidingSyncEndpoint> {
    /// Mocks the sliding sync endpoint with the given response.
    pub fn ok(self, response: v5::Response) -> MatrixMock<'a> {
        // A bit silly that we need to destructure all the fields ourselves, but
        // Response isn't serializable :'(
        self.respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "txn_id": response.txn_id,
            "pos": response.pos,
            "lists": response.lists,
            "rooms": response.rooms,
            "extensions": response.extensions,
        })))
    }

    /// Temporarily mocks the sync with the given endpoint and runs a client
    /// sync with it.
    ///
    /// After calling this function, the sync endpoint isn't mocked anymore.
    pub async fn ok_and_run<F: FnOnce(SlidingSyncBuilder) -> SlidingSyncBuilder>(
        self,
        client: &Client,
        on_builder: F,
        response: v5::Response,
    ) {
        let _scope = self.ok(response).mount_as_scoped().await;

        let sliding_sync =
            on_builder(client.sliding_sync("test_id").unwrap()).build().await.unwrap();

        let _summary = sliding_sync.sync_once().await.unwrap();
    }
}

/// A prebuilt mock for `GET /_matrix/client/*/profile/{user_id}/{key_name}`.
pub struct GetProfileFieldEndpoint {
    field: ProfileFieldName,
}

impl<'a> MockEndpoint<'a, GetProfileFieldEndpoint> {
    /// Returns a successful response containing the given value, if any.
    pub fn ok_with_value(self, value: Option<Value>) -> MatrixMock<'a> {
        if let Some(value) = value {
            let field = self.endpoint.field.to_string();
            self.respond_with(ResponseTemplate::new(200).set_body_json(json!({
                field: value,
            })))
        } else {
            self.ok_empty_json()
        }
    }
}

/// A prebuilt mock for `PUT /_matrix/client/*/profile/{user_id}/{key_name}`.
pub struct SetProfileFieldEndpoint;

impl<'a> MockEndpoint<'a, SetProfileFieldEndpoint> {
    /// Returns a successful empty response.
    pub fn ok(self) -> MatrixMock<'a> {
        self.ok_empty_json()
    }
}

/// A prebuilt mock for `DELETE /_matrix/client/*/profile/{user_id}/{key_name}`.
pub struct DeleteProfileFieldEndpoint;

impl<'a> MockEndpoint<'a, DeleteProfileFieldEndpoint> {
    /// Returns a successful empty response.
    pub fn ok(self) -> MatrixMock<'a> {
        self.ok_empty_json()
    }
}

/// A prebuilt mock for `GET /_matrix/client/*/profile/{user_id}`.
pub struct GetProfileEndpoint;

impl<'a> MockEndpoint<'a, GetProfileEndpoint> {
    /// Returns a successful empty response.
    pub fn ok_with_fields(self, fields: Vec<ProfileFieldValue>) -> MatrixMock<'a> {
        let profile = fields
            .iter()
            .map(|field| (field.field_name(), field.value()))
            .collect::<BTreeMap<_, _>>();
        self.respond_with(ResponseTemplate::new(200).set_body_json(profile))
    }
}
