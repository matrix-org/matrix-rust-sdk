// Copyright 2020 Damir Jelić
// Copyright 2020 The Matrix.org Foundation C.I.C.
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

#[cfg(feature = "encryption")]
use std::collections::BTreeMap;
use std::collections::HashMap;
use std::convert::{TryFrom, TryInto};
use std::fmt::{self, Debug};
use std::path::Path;
use std::result::Result as StdResult;
use std::sync::Arc;

use matrix_sdk_common::instant::{Duration, Instant};
use matrix_sdk_common::locks::RwLock;
use matrix_sdk_common::uuid::Uuid;

use futures_timer::Delay as sleep;
use std::future::Future;
#[cfg(feature = "encryption")]
use tracing::{debug, warn};
use tracing::{info, instrument, trace};

use http::Method as HttpMethod;
use http::Response as HttpResponse;
use reqwest::header::{HeaderValue, InvalidHeaderValue, AUTHORIZATION};
use url::Url;

use crate::events::room::message::MessageEventContent;
use crate::events::EventType;
use crate::identifiers::{EventId, RoomId, RoomIdOrAliasId, UserId};
use crate::Endpoint;

#[cfg(feature = "encryption")]
use crate::identifiers::DeviceId;

use crate::api;
#[cfg(not(target_arch = "wasm32"))]
use crate::VERSION;
use crate::{Error, EventEmitter, Result};
use matrix_sdk_base::{BaseClient, BaseClientConfig, Room, Session, StateStore};

const DEFAULT_SYNC_TIMEOUT: Duration = Duration::from_secs(30);

/// An async/await enabled Matrix client.
///
/// All of the state is held in an `Arc` so the `Client` can be cloned freely.
#[derive(Clone)]
pub struct Client {
    /// The URL of the homeserver to connect to.
    homeserver: Url,
    /// The underlying HTTP client.
    http_client: reqwest::Client,
    /// User session data.
    pub(crate) base_client: BaseClient,
}

#[cfg_attr(tarpaulin, skip)]
impl Debug for Client {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> StdResult<(), fmt::Error> {
        write!(fmt, "Client {{ homeserver: {} }}", self.homeserver)
    }
}

/// Configuration for the creation of the `Client`.
///
/// When setting the `StateStore` it is up to the user to open/connect
/// the storage backend before client creation.
///
/// # Example
///
/// ```
/// # use matrix_sdk::ClientConfig;
/// // To pass all the request through mitmproxy set the proxy and disable SSL
/// // verification
/// let client_config = ClientConfig::new()
///     .proxy("http://localhost:8080")
///     .unwrap()
///     .disable_ssl_verification();
/// ```
/// An example of adding a default `JsonStore` to the `Client`.
/// ```no_run
///  # use matrix_sdk::{ClientConfig, JsonStore};
///
/// let store = JsonStore::open("path/to/json").unwrap();
/// let client_config = ClientConfig::new()
///     .state_store(Box::new(store));
/// ```
#[derive(Default)]
pub struct ClientConfig {
    #[cfg(not(target_arch = "wasm32"))]
    proxy: Option<reqwest::Proxy>,
    user_agent: Option<HeaderValue>,
    disable_ssl_verification: bool,
    base_config: BaseClientConfig,
}

#[cfg_attr(tarpaulin, skip)]
impl Debug for ClientConfig {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut res = fmt.debug_struct("ClientConfig");

        #[cfg(not(target_arch = "wasm32"))]
        let res = res.field("proxy", &self.proxy);

        res.field("user_agent", &self.user_agent)
            .field("disable_ssl_verification", &self.disable_ssl_verification)
            .finish()
    }
}

impl ClientConfig {
    /// Create a new default `ClientConfig`.
    pub fn new() -> Self {
        Default::default()
    }

    /// Set the proxy through which all the HTTP requests should go.
    ///
    /// Note, only HTTP proxies are supported.
    ///
    /// # Arguments
    ///
    /// * `proxy` - The HTTP URL of the proxy.
    ///
    /// # Example
    ///
    /// ```
    /// use matrix_sdk::ClientConfig;
    ///
    /// let client_config = ClientConfig::new()
    ///     .proxy("http://localhost:8080")
    ///     .unwrap();
    /// ```
    #[cfg(not(target_arch = "wasm32"))]
    pub fn proxy(mut self, proxy: &str) -> Result<Self> {
        self.proxy = Some(reqwest::Proxy::all(proxy)?);
        Ok(self)
    }

    /// Disable SSL verification for the HTTP requests.
    pub fn disable_ssl_verification(mut self) -> Self {
        self.disable_ssl_verification = true;
        self
    }

    /// Set a custom HTTP user agent for the client.
    pub fn user_agent(mut self, user_agent: &str) -> StdResult<Self, InvalidHeaderValue> {
        self.user_agent = Some(HeaderValue::from_str(user_agent)?);
        Ok(self)
    }

    /// Set a custom implementation of a `StateStore`.
    ///
    /// The state store should be opened before being set.
    pub fn state_store(mut self, store: Box<dyn StateStore>) -> Self {
        self.base_config = self.base_config.state_store(store);
        self
    }

    /// Set the path for storage.
    ///
    /// # Arguments
    ///
    /// * `path` - The path where the stores should save data in. It is the
    /// callers responsibility to make sure that the path exists.
    ///
    /// In the default configuration the client will open default
    /// implementations for the crypto store and the state store. It will use
    /// the given path to open the stores. If no path is provided no store will
    /// be opened
    pub fn store_path<P: AsRef<Path>>(mut self, path: P) -> Self {
        self.base_config = self.base_config.store_path(path);
        self
    }

    /// Set the passphrase to encrypt the crypto store.
    ///
    /// # Argument
    ///
    /// * `passphrase` - The passphrase that will be used to encrypt the data in
    /// the cryptostore.
    ///
    /// This is only used if no custom cryptostore is set.
    pub fn passphrase(mut self, passphrase: String) -> Self {
        self.base_config = self.base_config.passphrase(passphrase);
        self
    }
}

#[derive(Debug, Default, Clone)]
/// Settings for a sync call.
pub struct SyncSettings {
    pub(crate) timeout: Option<Duration>,
    pub(crate) token: Option<String>,
    pub(crate) full_state: bool,
}

impl SyncSettings {
    /// Create new default sync settings.
    pub fn new() -> Self {
        Default::default()
    }

    /// Set the sync token.
    ///
    /// # Arguments
    ///
    /// * `token` - The sync token that should be used for the sync call.
    pub fn token<S: Into<String>>(mut self, token: S) -> Self {
        self.token = Some(token.into());
        self
    }

    /// Set the maximum time the server can wait, in milliseconds, before
    /// responding to the sync request.
    ///
    /// # Arguments
    ///
    /// * `timeout` - The time the server is allowed to wait.
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    /// Should the server return the full state from the start of the timeline.
    ///
    /// This does nothing if no sync token is set.
    ///
    /// # Arguments
    /// * `full_state` - A boolean deciding if the server should return the full
    ///     state or not.
    pub fn full_state(mut self, full_state: bool) -> Self {
        self.full_state = full_state;
        self
    }
}

use api::r0::account::register;
#[cfg(feature = "encryption")]
use api::r0::keys::{claim_keys, get_keys, upload_keys, KeyAlgorithm};
use api::r0::membership::{
    ban_user, forget_room,
    invite_user::{self, InvitationRecipient},
    join_room_by_id, join_room_by_id_or_alias, kick_user, leave_room, Invite3pid,
};
use api::r0::message::create_message_event;
use api::r0::message::get_message_events;
use api::r0::read_marker::set_read_marker;
use api::r0::receipt::create_receipt;
use api::r0::room::create_room;
use api::r0::session::login;
use api::r0::sync::sync_events;
#[cfg(feature = "encryption")]
use api::r0::to_device::send_event_to_device;
use api::r0::typing::create_typing_event;
use api::r0::uiaa::UiaaResponse;

impl Client {
    /// Creates a new client for making HTTP requests to the given homeserver.
    ///
    /// # Arguments
    ///
    /// * `homeserver_url` - The homeserver that the client should connect to.
    pub fn new<U: TryInto<Url>>(homeserver_url: U) -> Result<Self> {
        let config = ClientConfig::new();
        Client::new_with_config(homeserver_url, config)
    }

    /// Create a new client with the given configuration.
    ///
    /// # Arguments
    ///
    /// * `homeserver_url` - The homeserver that the client should connect to.
    ///
    /// * `config` - Configuration for the client.
    pub fn new_with_config<U: TryInto<Url>>(
        homeserver_url: U,
        config: ClientConfig,
    ) -> Result<Self> {
        #[allow(clippy::match_wild_err_arm)]
        let homeserver: Url = match homeserver_url.try_into() {
            Ok(u) => u,
            Err(_e) => panic!("Error parsing homeserver url"),
        };

        let http_client = reqwest::Client::builder();

        #[cfg(not(target_arch = "wasm32"))]
        let http_client = {
            let http_client = if config.disable_ssl_verification {
                http_client.danger_accept_invalid_certs(true)
            } else {
                http_client
            };

            let http_client = match config.proxy {
                Some(p) => http_client.proxy(p),
                None => http_client,
            };

            let mut headers = reqwest::header::HeaderMap::new();

            let user_agent = match config.user_agent {
                Some(a) => a,
                None => HeaderValue::from_str(&format!("matrix-rust-sdk {}", VERSION)).unwrap(),
            };

            headers.insert(reqwest::header::USER_AGENT, user_agent);

            http_client.default_headers(headers)
        };

        let http_client = http_client.build()?;

        let base_client = BaseClient::new_with_config(config.base_config)?;

        Ok(Self {
            homeserver,
            http_client,
            base_client,
        })
    }

    /// Is the client logged in.
    pub async fn logged_in(&self) -> bool {
        self.base_client.logged_in().await
    }

    /// The Homeserver of the client.
    pub fn homeserver(&self) -> &Url {
        &self.homeserver
    }

    /// Add `EventEmitter` to `Client`.
    ///
    /// The methods of `EventEmitter` are called when the respective `RoomEvents` occur.
    pub async fn add_event_emitter(&mut self, emitter: Box<dyn EventEmitter>) {
        self.base_client.add_event_emitter(emitter).await;
    }

    /// Returns the joined rooms this client knows about.
    pub fn joined_rooms(&self) -> Arc<RwLock<HashMap<RoomId, Arc<RwLock<Room>>>>> {
        self.base_client.joined_rooms()
    }

    /// Returns the invited rooms this client knows about.
    pub fn invited_rooms(&self) -> Arc<RwLock<HashMap<RoomId, Arc<RwLock<Room>>>>> {
        self.base_client.invited_rooms()
    }

    /// Returns the left rooms this client knows about.
    pub fn left_rooms(&self) -> Arc<RwLock<HashMap<RoomId, Arc<RwLock<Room>>>>> {
        self.base_client.left_rooms()
    }

    /// Get a joined room with the given room id.
    ///
    /// # Arguments
    ///
    /// `room_id` - The unique id of the room that should be fetched.
    pub async fn get_joined_room(&self, room_id: &RoomId) -> Option<Arc<RwLock<Room>>> {
        self.base_client.get_joined_room(room_id).await
    }

    /// Get an invited room with the given room id.
    ///
    /// # Arguments
    ///
    /// `room_id` - The unique id of the room that should be fetched.
    pub async fn get_invited_room(&self, room_id: &RoomId) -> Option<Arc<RwLock<Room>>> {
        self.base_client.get_invited_room(room_id).await
    }

    /// Get a left room with the given room id.
    ///
    /// # Arguments
    ///
    /// `room_id` - The unique id of the room that should be fetched.
    pub async fn get_left_room(&self, room_id: &RoomId) -> Option<Arc<RwLock<Room>>> {
        self.base_client.get_left_room(room_id).await
    }

    /// This allows `Client` to manually store `Room` state with the provided
    /// `StateStore`.
    ///
    /// Returns Ok when a successful `Room` store occurs.
    pub async fn store_room_state(&self, room_id: &RoomId) -> Result<()> {
        self.base_client
            .store_room_state(room_id)
            .await
            .map_err(Into::into)
    }

    /// Login to the server.
    ///
    /// # Arguments
    ///
    /// * `user` - The user that should be logged in to the homeserver.
    ///
    /// * `password` - The password of the user.
    ///
    /// * `device_id` - A unique id that will be associated with this session. If
    ///     not given the homeserver will create one. Can be an existing
    ///     device_id from a previous login call. Note that this should be done
    ///     only if the client also holds the encryption keys for this device.
    #[instrument(skip(password))]
    pub async fn login<S: Into<String> + Debug>(
        &self,
        user: S,
        password: S,
        device_id: Option<S>,
        initial_device_display_name: Option<S>,
    ) -> Result<login::Response> {
        info!("Logging in to {} as {:?}", self.homeserver, user);

        let request = login::Request {
            user: login::UserInfo::MatrixId(user.into()),
            login_info: login::LoginInfo::Password {
                password: password.into(),
            },
            device_id: device_id.map(|d| d.into()),
            initial_device_display_name: initial_device_display_name.map(|d| d.into()),
        };

        let response = self.send(request).await?;
        self.base_client.receive_login_response(&response).await?;

        Ok(response)
    }

    /// Restore a previously logged in session.
    ///
    /// # Arguments
    ///
    /// * `session` - A session that the user already has from a
    /// previous login call.
    pub async fn restore_login(&self, session: Session) -> Result<()> {
        Ok(self.base_client.restore_login(session).await?)
    }

    /// Register a user to the server.
    ///
    /// # Arguments
    ///
    /// * `registration` - The easiest way to create this request is using the `RegistrationBuilder`.
    ///
    ///
    /// # Examples
    /// ```
    /// # use std::convert::TryFrom;
    /// # use matrix_sdk::{Client, RegistrationBuilder};
    /// # use matrix_sdk::api::r0::account::register::RegistrationKind;
    /// # use matrix_sdk::identifiers::DeviceId;
    /// # use url::Url;
    /// # let homeserver = Url::parse("http://example.com").unwrap();
    /// # let mut rt = tokio::runtime::Runtime::new().unwrap();
    /// # rt.block_on(async {
    /// let mut builder = RegistrationBuilder::default();
    /// builder.password("pass")
    ///     .username("user")
    ///     .kind(RegistrationKind::User);
    /// let mut client = Client::new(homeserver).unwrap();
    /// client.register_user(builder).await;
    /// # })
    /// ```
    #[instrument(skip(registration))]
    pub async fn register_user<R: Into<register::Request>>(
        &self,
        registration: R,
    ) -> Result<register::Response> {
        info!("Registering to {}", self.homeserver);

        let request = registration.into();
        self.send_uiaa(request).await
    }

    /// Join a room by `RoomId`.
    ///
    /// Returns a `join_room_by_id::Response` consisting of the
    /// joined rooms `RoomId`.
    ///
    /// # Arguments
    ///
    /// * `room_id` - The `RoomId` of the room to be joined.
    pub async fn join_room_by_id(&self, room_id: &RoomId) -> Result<join_room_by_id::Response> {
        let request = join_room_by_id::Request {
            room_id: room_id.clone(),
            third_party_signed: None,
        };
        self.send(request).await
    }

    /// Join a room by `RoomId`.
    ///
    /// Returns a `join_room_by_id_or_alias::Response` consisting of the
    /// joined rooms `RoomId`.
    ///
    /// # Arguments
    ///
    /// * `alias` - The `RoomId` or `RoomAliasId` of the room to be joined.
    /// An alias looks like `#name:example.com`.
    pub async fn join_room_by_id_or_alias(
        &self,
        alias: &RoomIdOrAliasId,
        server_names: &[String],
    ) -> Result<join_room_by_id_or_alias::Response> {
        let request = join_room_by_id_or_alias::Request {
            room_id_or_alias: alias.clone(),
            server_name: server_names.to_owned(),
            third_party_signed: None,
        };
        self.send(request).await
    }

    /// Forget a room by `RoomId`.
    ///
    /// Returns a `forget_room::Response`, an empty response.
    ///
    /// # Arguments
    ///
    /// * `room_id` - The `RoomId` of the room to be forget.
    pub async fn forget_room_by_id(&self, room_id: &RoomId) -> Result<forget_room::Response> {
        let request = forget_room::Request {
            room_id: room_id.clone(),
        };
        self.send(request).await
    }

    /// Ban a user from a room by `RoomId` and `UserId`.
    ///
    /// Returns a `ban_user::Response`, an empty response.
    ///
    /// # Arguments
    ///
    /// * `room_id` - The `RoomId` of the room to ban the user from.
    ///
    /// * `user_id` - The user to ban by `UserId`.
    ///
    /// * `reason` - The reason for banning this user.
    pub async fn ban_user(
        &self,
        room_id: &RoomId,
        user_id: &UserId,
        reason: Option<String>,
    ) -> Result<ban_user::Response> {
        let request = ban_user::Request {
            reason,
            room_id: room_id.clone(),
            user_id: user_id.clone(),
        };
        self.send(request).await
    }

    /// Kick a user out of the specified room.
    ///
    /// Returns a `kick_user::Response`, an empty response.
    ///
    /// # Arguments
    ///
    /// * `room_id` - The `RoomId` of the room the user should be kicked out of.
    ///
    /// * `user_id` - The `UserId` of the user that should be kicked out of the room.
    ///
    /// * `reason` - Optional reason why the room member is being kicked out.
    pub async fn kick_user(
        &self,
        room_id: &RoomId,
        user_id: &UserId,
        reason: Option<String>,
    ) -> Result<kick_user::Response> {
        let request = kick_user::Request {
            reason,
            room_id: room_id.clone(),
            user_id: user_id.clone(),
        };
        self.send(request).await
    }

    /// Leave the specified room.
    ///
    /// Returns a `leave_room::Response`, an empty response.
    ///
    /// # Arguments
    ///
    /// * `room_id` - The `RoomId` of the room to leave.
    pub async fn leave_room(&self, room_id: &RoomId) -> Result<leave_room::Response> {
        let request = leave_room::Request {
            room_id: room_id.clone(),
        };
        self.send(request).await
    }

    /// Invite the specified user by `UserId` to the given room.
    ///
    /// Returns a `invite_user::Response`, an empty response.
    ///
    /// # Arguments
    ///
    /// * `room_id` - The `RoomId` of the room to invite the specified user to.
    ///
    /// * `user_id` - The `UserId` of the user to invite to the room.
    pub async fn invite_user_by_id(
        &self,
        room_id: &RoomId,
        user_id: &UserId,
    ) -> Result<invite_user::Response> {
        let request = invite_user::Request {
            room_id: room_id.clone(),
            recipient: InvitationRecipient::UserId {
                user_id: user_id.clone(),
            },
        };
        self.send(request).await
    }

    /// Invite the specified user by third party id to the given room.
    ///
    /// Returns a `invite_user::Response`, an empty response.
    ///
    /// # Arguments
    ///
    /// * `room_id` - The `RoomId` of the room to invite the specified user to.
    ///
    /// * `invite_id` - A third party id of a user to invite to the room.
    pub async fn invite_user_by_3pid(
        &self,
        room_id: &RoomId,
        invite_id: &Invite3pid,
    ) -> Result<invite_user::Response> {
        let request = invite_user::Request {
            room_id: room_id.clone(),
            recipient: InvitationRecipient::ThirdPartyId(invite_id.clone()),
        };
        self.send(request).await
    }

    /// Create a room using the `RoomBuilder` and send the request.
    ///
    /// Sends a request to `/_matrix/client/r0/createRoom`, returns a `create_room::Response`,
    /// this is an empty response.
    ///
    /// # Arguments
    ///
    /// * `room` - The easiest way to create this request is using the `RoomBuilder`.
    ///
    /// # Examples
    /// ```no_run
    /// use matrix_sdk::{Client, RoomBuilder};
    /// # use matrix_sdk::api::r0::room::Visibility;
    /// # use url::Url;
    ///
    /// # let homeserver = Url::parse("http://example.com").unwrap();
    /// let mut builder = RoomBuilder::default();
    /// builder.creation_content(false)
    ///     .initial_state(vec![])
    ///     .visibility(Visibility::Public)
    ///     .name("name")
    ///     .room_version("v1.0");
    ///
    /// let mut cli = Client::new(homeserver).unwrap();
    /// # use futures::executor::block_on;
    /// # block_on(async {
    /// assert!(cli.create_room(builder).await.is_ok());
    /// # });
    /// ```
    pub async fn create_room<R: Into<create_room::Request>>(
        &self,
        room: R,
    ) -> Result<create_room::Response> {
        let request = room.into();
        self.send(request).await
    }

    /// Get messages starting at a specific sync point using the
    /// `MessagesRequestBuilder`s `from` field as a starting point.
    ///
    /// Sends a request to `/_matrix/client/r0/rooms/{room_id}/messages` and
    /// returns a `get_message_events::IncomingResponse` that contains chunks
    /// of `RoomEvents`.
    ///
    /// # Arguments
    ///
    /// * `request` - The easiest way to create this request is using the
    /// `MessagesRequestBuilder`.
    ///
    /// # Examples
    /// ```no_run
    /// # use std::convert::TryFrom;
    /// use matrix_sdk::{Client, MessagesRequestBuilder};
    /// # use matrix_sdk::identifiers::RoomId;
    /// # use matrix_sdk::api::r0::filter::RoomEventFilter;
    /// # use matrix_sdk::api::r0::message::get_message_events::Direction;
    /// # use url::Url;
    /// # use matrix_sdk::js_int::UInt;
    ///
    /// # let homeserver = Url::parse("http://example.com").unwrap();
    /// let mut builder = MessagesRequestBuilder::new();
    /// builder.room_id(RoomId::try_from("!roomid:example.com").unwrap())
    ///     .from("t47429-4392820_219380_26003_2265".to_string())
    ///     .to("t4357353_219380_26003_2265".to_string())
    ///     .direction(Direction::Backward)
    ///     .limit(UInt::new(10).unwrap());
    ///
    /// let mut client = Client::new(homeserver).unwrap();
    /// # use futures::executor::block_on;
    /// # block_on(async {
    /// assert!(client.room_messages(builder).await.is_ok());
    /// # });
    /// ```
    pub async fn room_messages<R: Into<get_message_events::Request>>(
        &self,
        request: R,
    ) -> Result<get_message_events::Response> {
        let req = request.into();
        self.send(req).await
    }

    /// Send a request to notify the room of a user typing.
    ///
    /// Returns a `create_typing_event::Response`, an empty response.
    ///
    /// # Arguments
    ///
    /// * `room_id` - The `RoomId` the user is typing in.
    ///
    /// * `user_id` - The `UserId` of the user that is typing.
    ///
    /// * `typing` - Whether the user is typing, if false `timeout` is not needed.
    ///
    /// * `timeout` - Length of time in milliseconds to mark user is typing.
    pub async fn typing_notice(
        &self,
        room_id: &RoomId,
        user_id: &UserId,
        typing: bool,
        timeout: Option<Duration>,
    ) -> Result<create_typing_event::Response> {
        let request = create_typing_event::Request {
            room_id: room_id.clone(),
            user_id: user_id.clone(),
            timeout,
            typing,
        };
        self.send(request).await
    }

    /// Send a request to notify the room the user has read specific event.
    ///
    /// Returns a `create_receipt::Response`, an empty response.
    ///
    /// # Arguments
    ///
    /// * `room_id` - The `RoomId` the user is currently in.
    ///
    /// * `event_id` - The `EventId` specifies the event to set the read receipt on.
    pub async fn read_receipt(
        &self,
        room_id: &RoomId,
        event_id: &EventId,
    ) -> Result<create_receipt::Response> {
        let request = create_receipt::Request {
            room_id: room_id.clone(),
            event_id: event_id.clone(),
            receipt_type: create_receipt::ReceiptType::Read,
        };
        self.send(request).await
    }

    /// Send a request to notify the room user has read up to specific event.
    ///
    /// Returns a `set_read_marker::Response`, an empty response.
    ///
    /// # Arguments
    ///
    /// * room_id - The `RoomId` the user is currently in.
    ///
    /// * fully_read - The `EventId` of the event the user has read to.
    ///
    /// * read_receipt - An `EventId` to specify the event to set the read receipt on.
    pub async fn read_marker(
        &self,
        room_id: &RoomId,
        fully_read: &EventId,
        read_receipt: Option<&EventId>,
    ) -> Result<set_read_marker::Response> {
        let request = set_read_marker::Request {
            room_id: room_id.clone(),
            fully_read: fully_read.clone(),
            read_receipt: read_receipt.cloned(),
        };
        self.send(request).await
    }

    /// Send a room message to the homeserver.
    ///
    /// Returns the parsed response from the server.
    ///
    /// If the encryption feature is enabled this method will transparently
    /// encrypt the room message if the given room is encrypted.
    ///
    /// # Arguments
    ///
    /// * `room_id` -  The id of the room that should receive the message.
    ///
    /// * `content` - The content of the message event.
    ///
    /// * `txn_id` - A unique `Uuid` that can be attached to a `MessageEvent` held
    /// in it's unsigned field as `transaction_id`. If not given one is created for the
    /// message.
    ///
    /// # Example
    /// ```no_run
    /// # use matrix_sdk::Room;
    /// # use std::sync::{Arc, RwLock};
    /// # use matrix_sdk::{Client, SyncSettings};
    /// # use url::Url;
    /// # use futures::executor::block_on;
    /// # use ruma_identifiers::RoomId;
    /// # use std::convert::TryFrom;
    /// use matrix_sdk::events::room::message::{MessageEventContent, TextMessageEventContent};
    /// # block_on(async {
    /// # let homeserver = Url::parse("http://localhost:8080").unwrap();
    /// # let mut client = Client::new(homeserver).unwrap();
    /// # let room_id = RoomId::try_from("!test:localhost").unwrap();
    /// use matrix_sdk_common::uuid::Uuid;
    ///
    /// let content = MessageEventContent::Text(TextMessageEventContent {
    ///     body: "Hello world".to_owned(),
    ///     format: None,
    ///     formatted_body: None,
    ///     relates_to: None,
    /// });
    /// let txn_id = Uuid::new_v4();
    /// client.room_send(&room_id, content, Some(txn_id)).await.unwrap();
    /// # })
    /// ```
    pub async fn room_send(
        &self,
        room_id: &RoomId,
        content: MessageEventContent,
        txn_id: Option<Uuid>,
    ) -> Result<create_message_event::Response> {
        #[allow(unused_mut)]
        let mut event_type = EventType::RoomMessage;
        #[allow(unused_mut)]
        let mut raw_content = serde_json::value::to_raw_value(&content)?;

        #[cfg(feature = "encryption")]
        {
            let encrypted = {
                let room = self.base_client.get_joined_room(room_id).await;

                match room {
                    Some(r) => r.read().await.is_encrypted(),
                    None => false,
                }
            };

            if encrypted {
                let missing_sessions = {
                    let room = self.base_client.get_joined_room(room_id).await;
                    let room = room.as_ref().unwrap().read().await;
                    let users = room.members.keys();
                    self.base_client.get_missing_sessions(users).await?
                };

                if !missing_sessions.is_empty() {
                    self.claim_one_time_keys(missing_sessions).await?;
                }

                if self.base_client.should_share_group_session(room_id).await {
                    // TODO we need to make sure that only one such request is
                    // in flight per room at a time.
                    let response = self.share_group_session(room_id).await;

                    // If one of the responses failed invalidate the group
                    // session as using it would end up in undecryptable
                    // messages.
                    if let Err(r) = response {
                        self.base_client.invalidate_group_session(room_id).await;
                        return Err(r);
                    }
                }

                raw_content = serde_json::value::to_raw_value(
                    &self.base_client.encrypt(room_id, content).await?,
                )?;
                event_type = EventType::RoomEncrypted;
            }
        }

        let request = create_message_event::Request {
            room_id: room_id.clone(),
            event_type,
            txn_id: txn_id.unwrap_or_else(Uuid::new_v4).to_string(),
            data: raw_content,
        };

        let response = self.send(request).await?;
        Ok(response)
    }

    async fn send_request(
        &self,
        requires_auth: bool,
        method: HttpMethod,
        request: http::Request<Vec<u8>>,
    ) -> Result<reqwest::Response> {
        let url = request.uri();
        let path_and_query = url.path_and_query().unwrap();
        let mut url = self.homeserver.clone();

        url.set_path(path_and_query.path());
        url.set_query(path_and_query.query());

        let request_builder = match method {
            HttpMethod::GET => self.http_client.get(url),
            HttpMethod::POST => {
                let body = request.body().clone();
                self.http_client
                    .post(url)
                    .body(body)
                    .header(reqwest::header::CONTENT_TYPE, "application/json")
            }
            HttpMethod::PUT => {
                let body = request.body().clone();
                self.http_client
                    .put(url)
                    .body(body)
                    .header(reqwest::header::CONTENT_TYPE, "application/json")
            }
            HttpMethod::DELETE => {
                let body = request.body().clone();
                self.http_client
                    .delete(url)
                    .body(body)
                    .header(reqwest::header::CONTENT_TYPE, "application/json")
            }
            method => panic!("Unsuported method {}", method),
        };

        let request_builder = if requires_auth {
            let session = self.base_client.session().read().await;

            if let Some(session) = session.as_ref() {
                let header_value = format!("Bearer {}", &session.access_token);
                request_builder.header(AUTHORIZATION, header_value)
            } else {
                return Err(Error::AuthenticationRequired);
            }
        } else {
            request_builder
        };

        Ok(request_builder.send().await?)
    }

    async fn response_to_http_response(
        &self,
        mut response: reqwest::Response,
    ) -> Result<http::Response<Vec<u8>>> {
        let status = response.status();
        let mut http_builder = HttpResponse::builder().status(status);
        let headers = http_builder.headers_mut().unwrap();

        for (k, v) in response.headers_mut().drain() {
            if let Some(key) = k {
                headers.insert(key, v);
            }
        }
        let body = response.bytes().await?.as_ref().to_owned();
        Ok(http_builder.body(body).unwrap())
    }

    /// Send an arbitrary request to the server, without updating client state.
    ///
    /// **Warning:** Because this method *does not* update the client state, it is
    /// important to make sure than you account for this yourself, and use wrapper methods
    /// where available.  This method should *only* be used if a wrapper method for the
    /// endpoint you'd like to use is not available.
    ///
    /// # Arguments
    ///
    /// * `request` - A filled out and valid request for the endpoint to be hit
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use matrix_sdk::{Client, SyncSettings};
    /// # use futures::executor::block_on;
    /// # use url::Url;
    /// # use std::convert::TryFrom;
    /// # block_on(async {
    /// # let homeserver = Url::parse("http://localhost:8080").unwrap();
    /// # let mut client = Client::new(homeserver).unwrap();
    /// use matrix_sdk::api::r0::profile;
    /// use matrix_sdk::identifiers::UserId;
    ///
    /// // First construct the request you want to make
    /// // See https://docs.rs/ruma-client-api/latest/ruma_client_api/index.html
    /// // for all available Endpoints
    /// let request = profile::get_profile::Request {
    ///     user_id: UserId::try_from("@example:localhost").unwrap(),
    /// };
    ///
    /// // Start the request using Client::send()
    /// let response = client.send(request).await.unwrap();
    ///
    /// // Check the corresponding Response struct to find out what types are
    /// // returned
    /// # })
    /// ```
    pub async fn send<Request: Endpoint<ResponseError = crate::api::Error> + Debug>(
        &self,
        request: Request,
    ) -> Result<Request::Response> {
        let request: http::Request<Vec<u8>> = request.try_into()?;
        let response = self
            .send_request(
                Request::METADATA.requires_authentication,
                Request::METADATA.method,
                request,
            )
            .await?;

        trace!("Got response: {:?}", response);

        let response = self.response_to_http_response(response).await?;

        Ok(<Request::Response>::try_from(response)?)
    }

    /// Send an arbitrary request to the server, without updating client state.
    ///
    /// This version allows the client to make registration requests.
    ///
    /// **Warning:** Because this method *does not* update the client state, it is
    /// important to make sure than you account for this yourself, and use wrapper methods
    /// where available.  This method should *only* be used if a wrapper method for the
    /// endpoint you'd like to use is not available.
    ///
    /// # Arguments
    ///
    /// * `request` - This version of send is for dealing with types that return
    /// a `UiaaResponse` as the `Endpoint<ResponseError = UiaaResponse>` associated type.
    ///
    /// # Examples
    /// ```
    /// # use std::convert::TryFrom;
    /// # use matrix_sdk::{Client, RegistrationBuilder};
    /// # use matrix_sdk::api::r0::account::register::{RegistrationKind, Request};
    /// # use matrix_sdk::identifiers::DeviceId;
    /// # use url::Url;
    /// # let homeserver = Url::parse("http://example.com").unwrap();
    /// # let mut rt = tokio::runtime::Runtime::new().unwrap();
    /// # rt.block_on(async {
    /// let mut builder = RegistrationBuilder::default();
    /// builder.password("pass")
    ///     .username("user")
    ///     .kind(RegistrationKind::User);
    /// let mut client = Client::new(homeserver).unwrap();
    /// let req: Request = builder.into();
    /// client.send_uiaa(req).await;
    /// # })
    /// ```
    pub async fn send_uiaa<Request: Endpoint<ResponseError = UiaaResponse> + Debug>(
        &self,
        request: Request,
    ) -> Result<Request::Response> {
        let request: http::Request<Vec<u8>> = request.try_into()?;
        let response = self
            .send_request(
                Request::METADATA.requires_authentication,
                Request::METADATA.method,
                request,
            )
            .await?;

        trace!("Got response: {:?}", response);

        let response = self.response_to_http_response(response).await?;

        let uiaa: Result<_> = <Request::Response>::try_from(response).map_err(Into::into);

        Ok(uiaa?)
    }

    /// Synchronize the client's state with the latest state on the server.
    ///
    /// If a `StateStore` is provided and this is the initial sync state will
    /// be loaded from the state store.
    ///
    /// # Arguments
    ///
    /// * `sync_settings` - Settings for the sync call.
    #[instrument]
    pub async fn sync(&self, sync_settings: SyncSettings) -> Result<sync_events::Response> {
        let request = sync_events::Request {
            filter: None,
            since: sync_settings.token,
            full_state: sync_settings.full_state,
            set_presence: sync_events::SetPresence::Online,
            timeout: sync_settings.timeout,
        };

        let mut response = self.send(request).await?;

        self.base_client
            .receive_sync_response(&mut response)
            .await?;

        Ok(response)
    }

    /// Repeatedly call sync to synchronize the client state with the server.
    ///
    /// # Arguments
    ///
    /// * `sync_settings` - Settings for the sync call. Note that those settings
    ///     will be only used for the first sync call.
    ///
    /// * `callback` - A callback that will be called every time a successful
    ///     response has been fetched from the server.
    ///
    /// # Examples
    ///
    /// The following example demonstrates how to sync forever while sending all
    /// the interesting events through a mpsc channel to another thread e.g. a
    /// UI thread.
    ///
    /// ```compile_fail,E0658
    /// # use matrix_sdk::events::{
    /// #     collections::all::RoomEvent,
    /// #     room::message::{MessageEvent, MessageEventContent, TextMessageEventContent},
    /// #     EventResult,
    /// # };
    /// # use matrix_sdk::Room;
    /// # use std::sync::{Arc, RwLock};
    /// # use matrix_sdk::{Client, SyncSettings};
    /// # use url::Url;
    /// # use futures::executor::block_on;
    /// # block_on(async {
    /// # let homeserver = Url::parse("http://localhost:8080").unwrap();
    /// # let mut client = Client::new(homeserver, None).unwrap();
    ///
    /// use async_std::sync::channel;
    ///
    /// let (tx, rx) = channel(100);
    ///
    /// let sync_channel = &tx;
    /// let sync_settings = SyncSettings::new()
    ///     .timeout(30_000)
    ///     .unwrap();
    ///
    /// client
    ///     .sync_forever(sync_settings, async move |response| {
    ///         let channel = sync_channel;
    ///
    ///         for (room_id, room) in response.rooms.join {
    ///             for event in room.timeline.events {
    ///                 if let EventResult::Ok(e) = event {
    ///                     channel.send(e).await;
    ///                 }
    ///             }
    ///         }
    ///     })
    ///     .await;
    /// })
    /// ```
    #[instrument(skip(callback))]
    pub async fn sync_forever<C>(
        &self,
        sync_settings: SyncSettings,
        callback: impl Fn(sync_events::Response) -> C,
    ) where
        C: Future<Output = ()>,
    {
        let mut sync_settings = sync_settings;
        let mut last_sync_time: Option<Instant> = None;

        if sync_settings.token.is_none() {
            sync_settings.token = self.sync_token().await;
        }

        loop {
            let response = self.sync(sync_settings.clone()).await;

            let response = if let Ok(r) = response {
                r
            } else {
                sleep::new(Duration::from_secs(1)).await;

                continue;
            };

            // TODO send out to-device messages here

            #[cfg(feature = "encryption")]
            {
                if self.base_client.should_upload_keys().await {
                    let response = self.keys_upload().await;

                    if let Err(e) = response {
                        warn!("Error while uploading E2EE keys {:?}", e);
                    }
                }

                if self.base_client.should_query_keys().await {
                    let response = self.keys_query().await;

                    if let Err(e) = response {
                        warn!("Error while querying device keys {:?}", e);
                    }
                }
            }

            callback(response).await;

            let now = Instant::now();

            // If the last sync happened less than a second ago, sleep for a
            // while to not hammer out requests if the server doesn't respect
            // the sync timeout.
            if let Some(t) = last_sync_time {
                if now - t <= Duration::from_secs(1) {
                    sleep::new(Duration::from_secs(1)).await;
                }
            }

            last_sync_time = Some(now);

            sync_settings = SyncSettings::new().timeout(DEFAULT_SYNC_TIMEOUT).token(
                self.sync_token()
                    .await
                    .expect("No sync token found after initial sync"),
            );
        }
    }

    /// Claim one-time keys creating new Olm sessions.
    ///
    /// # Arguments
    ///
    /// * `users` - The list of user/device pairs that we should claim keys for.
    ///
    /// # Panics
    ///
    /// Panics if the client isn't logged in, or if no encryption keys need to
    /// be uploaded.
    #[cfg(feature = "encryption")]
    #[cfg_attr(docsrs, doc(cfg(feature = "encryption")))]
    #[instrument]
    async fn claim_one_time_keys(
        &self,
        one_time_keys: BTreeMap<UserId, BTreeMap<DeviceId, KeyAlgorithm>>,
    ) -> Result<claim_keys::Response> {
        let request = claim_keys::Request {
            timeout: None,
            one_time_keys,
        };

        let response = self.send(request).await?;
        self.base_client
            .receive_keys_claim_response(&response)
            .await?;
        Ok(response)
    }

    /// Share a group session for a room.
    ///
    /// # Arguments
    ///
    /// * `room_id` - The ID of the room for which we want to share a group
    /// session.
    ///
    /// # Panics
    ///
    /// Panics if the client isn't logged in.
    #[cfg(feature = "encryption")]
    #[cfg_attr(docsrs, doc(cfg(feature = "encryption")))]
    #[instrument]
    async fn share_group_session(&self, room_id: &RoomId) -> Result<()> {
        let mut requests = self
            .base_client
            .share_group_session(room_id)
            .await
            .expect("Keys don't need to be uploaded");

        for request in requests.drain(..) {
            let _response: send_event_to_device::Response = self.send(request).await?;
        }

        Ok(())
    }

    /// Upload the E2E encryption keys.
    ///
    /// This uploads the long lived device keys as well as the required amount
    /// of one-time keys.
    ///
    /// # Panics
    ///
    /// Panics if the client isn't logged in, or if no encryption keys need to
    /// be uploaded.
    #[cfg(feature = "encryption")]
    #[cfg_attr(docsrs, doc(cfg(feature = "encryption")))]
    #[instrument]
    async fn keys_upload(&self) -> Result<upload_keys::Response> {
        let (device_keys, one_time_keys) = self
            .base_client
            .keys_for_upload()
            .await
            .expect("Keys don't need to be uploaded");

        debug!(
            "Uploading encryption keys device keys: {}, one-time-keys: {}",
            device_keys.is_some(),
            one_time_keys.as_ref().map_or(0, |k| k.len())
        );

        let request = upload_keys::Request {
            device_keys,
            one_time_keys,
        };

        let response = self.send(request).await?;
        self.base_client
            .receive_keys_upload_response(&response)
            .await?;
        Ok(response)
    }

    /// Get the current, if any, sync token of the client.
    /// This will be None if the client didn't sync at least once.
    pub async fn sync_token(&self) -> Option<String> {
        self.base_client.sync_token().await
    }

    /// Query the server for users device keys.
    ///
    /// # Panics
    ///
    /// Panics if no key query needs to be done.
    #[cfg(feature = "encryption")]
    #[cfg_attr(docsrs, doc(cfg(feature = "encryption")))]
    #[instrument]
    async fn keys_query(&self) -> Result<get_keys::Response> {
        let mut users_for_query = self
            .base_client
            .users_for_key_query()
            .await
            .expect("Keys don't need to be uploaded");

        debug!(
            "Querying device keys device for users: {:?}",
            users_for_query
        );

        let mut device_keys: BTreeMap<UserId, Vec<DeviceId>> = BTreeMap::new();

        for user in users_for_query.drain() {
            device_keys.insert(user, Vec::new());
        }

        let request = get_keys::Request {
            timeout: None,
            device_keys,
            token: None,
        };

        let response = self.send(request).await?;
        self.base_client
            .receive_keys_query_response(&response)
            .await?;

        Ok(response)
    }
}

#[cfg(test)]
mod test {
    use super::{
        api::r0::uiaa::AuthData, ban_user, create_receipt, create_typing_event, forget_room,
        invite_user, kick_user, leave_room, register::RegistrationKind, set_read_marker,
        Invite3pid, MessageEventContent,
    };
    use super::{Client, ClientConfig, Session, SyncSettings, Url};
    use crate::events::collections::all::RoomEvent;
    use crate::events::room::member::MembershipState;
    use crate::events::room::message::TextMessageEventContent;
    use crate::identifiers::{EventId, RoomId, RoomIdOrAliasId, UserId};
    use crate::RegistrationBuilder;

    use matrix_sdk_base::JsonStore;
    use matrix_sdk_test::{test_json, EventBuilder, EventsJson};
    use mockito::{mock, Matcher};
    use tempfile::tempdir;

    use std::convert::TryFrom;
    use std::path::Path;
    use std::str::FromStr;
    use std::time::Duration;

    #[tokio::test]
    async fn test_join_leave_room() {
        let homeserver = Url::from_str(&mockito::server_url()).unwrap();

        let room_id = RoomId::try_from("!SVkFJHzfwvuaIEawgC:localhost").unwrap();

        let session = Session {
            access_token: "1234".to_owned(),
            user_id: UserId::try_from("@example:localhost").unwrap(),
            device_id: "DEVICEID".to_owned(),
        };

        let _m = mock(
            "GET",
            Matcher::Regex(r"^/_matrix/client/r0/sync\?.*$".to_string()),
        )
        .with_status(200)
        .with_body(test_json::SYNC.to_string())
        .create();

        let dir = tempdir().unwrap();
        let path: &Path = dir.path();
        let store = Box::new(JsonStore::open(path).unwrap());

        let config = ClientConfig::default().state_store(store);
        let client = Client::new_with_config(homeserver.clone(), config).unwrap();
        client.restore_login(session.clone()).await.unwrap();

        let room = client.get_joined_room(&room_id).await;
        assert!(room.is_none());

        client.sync(SyncSettings::default()).await.unwrap();

        let room = client.get_left_room(&room_id).await;
        assert!(room.is_none());

        let room = client.get_joined_room(&room_id).await;
        assert!(room.is_some());

        // test store reloads with correct room state from JsonStore
        let store = Box::new(JsonStore::open(path).unwrap());
        let config = ClientConfig::default().state_store(store);
        let joined_client = Client::new_with_config(homeserver, config).unwrap();
        joined_client.restore_login(session).await.unwrap();

        // joined room reloaded from state store
        joined_client.sync(SyncSettings::default()).await.unwrap();
        let room = joined_client.get_joined_room(&room_id).await;
        assert!(room.is_some());

        let _m = mock(
            "GET",
            Matcher::Regex(r"^/_matrix/client/r0/sync\?.*$".to_string()),
        )
        .with_status(200)
        .with_body(test_json::LEAVE_SYNC_EVENT.to_string())
        .create();

        joined_client.sync(SyncSettings::default()).await.unwrap();

        let room = joined_client.get_joined_room(&room_id).await;
        assert!(room.is_none());

        let room = joined_client.get_left_room(&room_id).await;
        assert!(room.is_some());
    }

    #[tokio::test]
    async fn account_data() {
        let homeserver = Url::from_str(&mockito::server_url()).unwrap();

        let session = Session {
            access_token: "1234".to_owned(),
            user_id: UserId::try_from("@example:example.com").unwrap(),
            device_id: "DEVICEID".to_owned(),
        };

        let _m = mock(
            "GET",
            Matcher::Regex(r"^/_matrix/client/r0/sync\?.*$".to_string()),
        )
        .with_status(200)
        .with_body(test_json::SYNC.to_string())
        .create();

        let client = Client::new(homeserver).unwrap();
        client.restore_login(session).await.unwrap();

        let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

        let _response = client.sync(sync_settings).await.unwrap();

        // let bc = &client.base_client;
        // let ignored_users = bc.ignored_users.read().await;
        // assert_eq!(1, ignored_users.len())
    }

    #[tokio::test]
    async fn room_creation() {
        let session = Session {
            access_token: "12345".to_owned(),
            user_id: UserId::try_from("@example:localhost").unwrap(),
            device_id: "DEVICEID".to_owned(),
        };
        let homeserver = url::Url::parse(&mockito::server_url()).unwrap();
        let client = Client::new(homeserver).unwrap();
        client.restore_login(session).await.unwrap();

        let mut response = EventBuilder::default()
            .add_room_event(EventsJson::Member, RoomEvent::RoomMember)
            .add_room_event(EventsJson::PowerLevels, RoomEvent::RoomPowerLevels)
            .build_sync_response();

        client
            .base_client
            .receive_sync_response(&mut response)
            .await
            .unwrap();
        let room_id = RoomId::try_from("!SVkFJHzfwvuaIEawgC:localhost").unwrap();

        assert_eq!(
            client.homeserver(),
            &Url::parse(&mockito::server_url()).unwrap()
        );

        let room = client.get_joined_room(&room_id).await;
        assert!(room.is_some());
    }

    #[tokio::test]
    async fn login_error() {
        let homeserver = Url::from_str(&mockito::server_url()).unwrap();

        let _m = mock("POST", "/_matrix/client/r0/login")
            .with_status(403)
            .with_body(test_json::LOGIN_RESPONSE_ERR.to_string())
            .create();

        let client = Client::new(homeserver).unwrap();

        if let Err(err) = client.login("example", "wordpass", None, None).await {
            if let crate::Error::RumaResponse(crate::FromHttpResponseError::Http(
                crate::ServerError::Known(crate::api::Error {
                    kind,
                    message,
                    status_code,
                }),
            )) = err
            {
                if let crate::api::error::ErrorKind::Forbidden = kind {
                } else {
                    panic!(
                        "found the wrong `ErrorKind` {:?}, expected `Forbidden",
                        kind
                    );
                }
                assert_eq!(message, "Invalid password".to_string());
                assert_eq!(status_code, http::StatusCode::from_u16(403).unwrap());
            } else {
                panic!(
                    "found the wrong `Error` type {:?}, expected `Error::RumaResponse",
                    err
                );
            }
        } else {
            panic!("this request should return an `Err` variant")
        }
    }

    #[tokio::test]
    async fn register_error() {
        let homeserver = Url::from_str(&mockito::server_url()).unwrap();

        let _m = mock("POST", "/_matrix/client/r0/register")
            .with_status(403)
            .with_body(test_json::REGISTRATION_RESPONSE_ERR.to_string())
            .with_body_from_file("../test_data/registration_response_error.json")
            .create();

        let mut user = RegistrationBuilder::default();

        user.username("user")
            .password("password")
            .auth(AuthData::FallbackAcknowledgement {
                session: "foobar".to_string(),
            })
            .kind(RegistrationKind::User);

        let client = Client::new(homeserver).unwrap();

        if let Err(err) = client.register_user(user).await {
            if let crate::Error::UiaaError(crate::FromHttpResponseError::Http(
                // TODO this should be a UiaaError need to investigate
                crate::ServerError::Unknown(e),
            )) = err
            {
                assert!(e.to_string().starts_with("EOF while parsing"))
            } else {
                panic!(
                    "found the wrong `Error` type {:#?}, expected `ServerError::Unknown",
                    err
                );
            }
        } else {
            panic!("this request should return an `Err` variant")
        }
    }

    #[tokio::test]
    async fn join_room_by_id() {
        let homeserver = Url::from_str(&mockito::server_url()).unwrap();

        let session = Session {
            access_token: "1234".to_owned(),
            user_id: UserId::try_from("@example:localhost").unwrap(),
            device_id: "DEVICEID".to_owned(),
        };

        let _m = mock(
            "POST",
            Matcher::Regex(r"^/_matrix/client/r0/rooms/.*/join".to_string()),
        )
        .with_status(200)
        .with_body(test_json::ROOM_ID.to_string())
        .create();

        let client = Client::new(homeserver).unwrap();
        client.restore_login(session).await.unwrap();
        let room_id = RoomId::try_from("!testroom:example.org").unwrap();

        assert_eq!(
            // this is the `join_by_room_id::Response` but since no PartialEq we check the RoomId field
            client.join_room_by_id(&room_id).await.unwrap().room_id,
            room_id
        );
    }

    #[tokio::test]
    async fn join_room_by_id_or_alias() {
        let homeserver = Url::from_str(&mockito::server_url()).unwrap();

        let session = Session {
            access_token: "1234".to_owned(),
            user_id: UserId::try_from("@example:localhost").unwrap(),
            device_id: "DEVICEID".to_owned(),
        };

        let _m = mock(
            "POST",
            Matcher::Regex(r"^/_matrix/client/r0/join/".to_string()),
        )
        .with_status(200)
        .with_body(test_json::ROOM_ID.to_string())
        .create();

        let client = Client::new(homeserver).unwrap();
        client.restore_login(session).await.unwrap();
        let room_id = RoomIdOrAliasId::try_from("!testroom:example.org").unwrap();

        assert_eq!(
            // this is the `join_by_room_id::Response` but since no PartialEq we check the RoomId field
            client
                .join_room_by_id_or_alias(&room_id, &["server.com".to_string()])
                .await
                .unwrap()
                .room_id,
            RoomId::try_from("!testroom:example.org").unwrap()
        );
    }

    #[tokio::test]
    #[allow(irrefutable_let_patterns)]
    async fn invite_user_by_id() {
        let homeserver = Url::from_str(&mockito::server_url()).unwrap();
        let user = UserId::try_from("@example:localhost").unwrap();
        let room_id = RoomId::try_from("!testroom:example.org").unwrap();

        let session = Session {
            access_token: "1234".to_owned(),
            user_id: user.clone(),
            device_id: "DEVICEID".to_owned(),
        };

        let _m = mock(
            "POST",
            Matcher::Regex(r"^/_matrix/client/r0/rooms/.*/invite".to_string()),
        )
        .with_status(200)
        .with_body(test_json::LOGOUT.to_string())
        .create();

        let client = Client::new(homeserver).unwrap();
        client.restore_login(session).await.unwrap();

        if let invite_user::Response = client.invite_user_by_id(&room_id, &user).await.unwrap() {}
    }

    #[tokio::test]
    #[allow(irrefutable_let_patterns)]
    async fn invite_user_by_3pid() {
        let homeserver = Url::from_str(&mockito::server_url()).unwrap();
        let user = UserId::try_from("@example:localhost").unwrap();
        let room_id = RoomId::try_from("!testroom:example.org").unwrap();

        let session = Session {
            access_token: "1234".to_owned(),
            user_id: user.clone(),
            device_id: "DEVICEID".to_owned(),
        };

        let _m = mock(
            "POST",
            Matcher::Regex(r"^/_matrix/client/r0/rooms/.*/invite".to_string()),
        )
        .with_status(200)
        // empty JSON object
        .with_body(test_json::LOGOUT.to_string())
        .create();

        let client = Client::new(homeserver).unwrap();
        client.restore_login(session).await.unwrap();

        if let invite_user::Response = client
            .invite_user_by_3pid(
                &room_id,
                &Invite3pid {
                    id_server: "example.org".to_string(),
                    id_access_token: "IdToken".to_string(),
                    medium: crate::api::r0::thirdparty::Medium::Email,
                    address: "address".to_string(),
                },
            )
            .await
            .unwrap()
        {}
    }

    #[tokio::test]
    #[allow(irrefutable_let_patterns)]
    async fn leave_room() {
        let homeserver = Url::from_str(&mockito::server_url()).unwrap();

        let session = Session {
            access_token: "1234".to_owned(),
            user_id: UserId::try_from("@example:localhost").unwrap(),
            device_id: "DEVICEID".to_owned(),
        };

        let _m = mock(
            "POST",
            Matcher::Regex(r"^/_matrix/client/r0/rooms/.*/leave".to_string()),
        )
        .with_status(200)
        // this is an empty JSON object
        .with_body(test_json::LOGOUT.to_string())
        .create();

        let client = Client::new(homeserver).unwrap();
        client.restore_login(session).await.unwrap();
        let room_id = RoomId::try_from("!testroom:example.org").unwrap();

        let response = client.leave_room(&room_id).await.unwrap();
        if let leave_room::Response = response {
        } else {
            panic!(
                "expected `ruma_client_api::leave_room::Response` found {:?}",
                response
            )
        }
    }

    #[tokio::test]
    #[allow(irrefutable_let_patterns)]
    async fn ban_user() {
        let homeserver = Url::from_str(&mockito::server_url()).unwrap();
        let user = UserId::try_from("@example:localhost").unwrap();
        let room_id = RoomId::try_from("!testroom:example.org").unwrap();

        let session = Session {
            access_token: "1234".to_owned(),
            user_id: user.clone(),
            device_id: "DEVICEID".to_owned(),
        };

        let _m = mock(
            "POST",
            Matcher::Regex(r"^/_matrix/client/r0/rooms/.*/ban".to_string()),
        )
        .with_status(200)
        // this is an empty JSON object
        .with_body(test_json::LOGOUT.to_string())
        .create();

        let client = Client::new(homeserver).unwrap();
        client.restore_login(session).await.unwrap();

        let response = client.ban_user(&room_id, &user, None).await.unwrap();
        if let ban_user::Response = response {
        } else {
            panic!(
                "expected `ruma_client_api::ban_user::Response` found {:?}",
                response
            )
        }
    }

    #[tokio::test]
    #[allow(irrefutable_let_patterns)]
    async fn kick_user() {
        let homeserver = Url::from_str(&mockito::server_url()).unwrap();
        let user = UserId::try_from("@example:localhost").unwrap();
        let room_id = RoomId::try_from("!testroom:example.org").unwrap();

        let session = Session {
            access_token: "1234".to_owned(),
            user_id: user.clone(),
            device_id: "DEVICEID".to_owned(),
        };

        let _m = mock(
            "POST",
            Matcher::Regex(r"^/_matrix/client/r0/rooms/.*/kick".to_string()),
        )
        .with_status(200)
        // this is an empty JSON object
        .with_body(test_json::LOGOUT.to_string())
        .create();

        let client = Client::new(homeserver).unwrap();
        client.restore_login(session).await.unwrap();

        let response = client.kick_user(&room_id, &user, None).await.unwrap();
        if let kick_user::Response = response {
        } else {
            panic!(
                "expected `ruma_client_api::kick_user::Response` found {:?}",
                response
            )
        }
    }

    #[tokio::test]
    #[allow(irrefutable_let_patterns)]
    async fn forget_room() {
        let homeserver = Url::from_str(&mockito::server_url()).unwrap();
        let user = UserId::try_from("@example:localhost").unwrap();
        let room_id = RoomId::try_from("!testroom:example.org").unwrap();

        let session = Session {
            access_token: "1234".to_owned(),
            user_id: user.clone(),
            device_id: "DEVICEID".to_owned(),
        };

        let _m = mock(
            "POST",
            Matcher::Regex(r"^/_matrix/client/r0/rooms/.*/forget".to_string()),
        )
        .with_status(200)
        // this is an empty JSON object
        .with_body(test_json::LOGOUT.to_string())
        .create();

        let client = Client::new(homeserver).unwrap();
        client.restore_login(session).await.unwrap();

        let response = client.forget_room_by_id(&room_id).await.unwrap();
        if let forget_room::Response = response {
        } else {
            panic!(
                "expected `ruma_client_api::forget_room::Response` found {:?}",
                response
            )
        }
    }

    #[tokio::test]
    #[allow(irrefutable_let_patterns)]
    async fn read_receipt() {
        let homeserver = Url::from_str(&mockito::server_url()).unwrap();
        let user_id = UserId::try_from("@example:localhost").unwrap();
        let room_id = RoomId::try_from("!testroom:example.org").unwrap();
        let event_id = EventId::new("example.org").unwrap();

        let session = Session {
            access_token: "1234".to_owned(),
            user_id,
            device_id: "DEVICEID".to_owned(),
        };

        let _m = mock(
            "POST",
            Matcher::Regex(r"^/_matrix/client/r0/rooms/.*/receipt".to_string()),
        )
        .with_status(200)
        // this is an empty JSON object
        .with_body(test_json::LOGOUT.to_string())
        .create();

        let client = Client::new(homeserver).unwrap();
        client.restore_login(session).await.unwrap();

        let response = client.read_receipt(&room_id, &event_id).await.unwrap();
        if let create_receipt::Response = response {
        } else {
            panic!(
                "expected `ruma_client_api::create_receipt::Response` found {:?}",
                response
            )
        }
    }

    #[tokio::test]
    #[allow(irrefutable_let_patterns)]
    async fn read_marker() {
        let homeserver = Url::from_str(&mockito::server_url()).unwrap();
        let user_id = UserId::try_from("@example:localhost").unwrap();
        let room_id = RoomId::try_from("!testroom:example.org").unwrap();
        let event_id = EventId::new("example.org").unwrap();

        let session = Session {
            access_token: "1234".to_owned(),
            user_id,
            device_id: "DEVICEID".to_owned(),
        };

        let _m = mock(
            "POST",
            Matcher::Regex(r"^/_matrix/client/r0/rooms/.*/read_markers".to_string()),
        )
        .with_status(200)
        // this is an empty JSON object
        .with_body(test_json::LOGOUT.to_string())
        .create();

        let client = Client::new(homeserver).unwrap();
        client.restore_login(session).await.unwrap();

        let response = client.read_marker(&room_id, &event_id, None).await.unwrap();
        if let set_read_marker::Response = response {
        } else {
            panic!(
                "expected `ruma_client_api::set_read_marker::Response` found {:?}",
                response
            )
        }
    }

    #[tokio::test]
    #[allow(irrefutable_let_patterns)]
    async fn typing_notice() {
        let homeserver = Url::from_str(&mockito::server_url()).unwrap();
        let user = UserId::try_from("@example:localhost").unwrap();
        let room_id = RoomId::try_from("!testroom:example.org").unwrap();

        let session = Session {
            access_token: "1234".to_owned(),
            user_id: user.clone(),
            device_id: "DEVICEID".to_owned(),
        };

        let _m = mock(
            "PUT",
            Matcher::Regex(r"^/_matrix/client/r0/rooms/.*/typing".to_string()),
        )
        .with_status(200)
        // this is an empty JSON object
        .with_body(test_json::LOGOUT.to_string())
        .create();

        let client = Client::new(homeserver).unwrap();
        client.restore_login(session).await.unwrap();

        let response = client
            .typing_notice(
                &room_id,
                &user,
                true,
                Some(std::time::Duration::from_secs(1)),
            )
            .await
            .unwrap();
        if let create_typing_event::Response = response {
        } else {
            panic!(
                "expected `ruma_client_api::create_typing_event::Response` found {:?}",
                response
            )
        }
    }

    #[tokio::test]
    async fn room_message_send() {
        use matrix_sdk_common::uuid::Uuid;

        let homeserver = Url::from_str(&mockito::server_url()).unwrap();
        let user = UserId::try_from("@example:localhost").unwrap();
        let room_id = RoomId::try_from("!testroom:example.org").unwrap();

        let session = Session {
            access_token: "1234".to_owned(),
            user_id: user.clone(),
            device_id: "DEVICEID".to_owned(),
        };

        let _m = mock(
            "PUT",
            Matcher::Regex(r"^/_matrix/client/r0/rooms/.*/send/".to_string()),
        )
        .with_status(200)
        .with_body(test_json::EVENT_ID.to_string())
        .create();

        let client = Client::new(homeserver).unwrap();
        client.restore_login(session).await.unwrap();

        let content = MessageEventContent::Text(TextMessageEventContent {
            body: "Hello world".to_owned(),
            format: None,
            formatted_body: None,
            relates_to: None,
        });
        let txn_id = Uuid::new_v4();
        let response = client
            .room_send(&room_id, content, Some(txn_id))
            .await
            .unwrap();

        assert_eq!(
            EventId::try_from("$h29iv0s8:example.com").unwrap(),
            response.event_id
        )
    }

    #[tokio::test]
    async fn user_presence() {
        let homeserver = Url::from_str(&mockito::server_url()).unwrap();

        let session = Session {
            access_token: "1234".to_owned(),
            user_id: UserId::try_from("@example:localhost").unwrap(),
            device_id: "DEVICEID".to_owned(),
        };

        let _m = mock(
            "GET",
            Matcher::Regex(r"^/_matrix/client/r0/sync\?.*$".to_string()),
        )
        .with_status(200)
        .with_body(test_json::SYNC.to_string())
        .create();

        let client = Client::new(homeserver).unwrap();
        client.restore_login(session).await.unwrap();

        let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

        let _response = client.sync(sync_settings).await.unwrap();

        let rooms_lock = &client.base_client.joined_rooms();
        let rooms = rooms_lock.read().await;
        let room = &rooms
            .get(&RoomId::try_from("!SVkFJHzfwvuaIEawgC:localhost").unwrap())
            .unwrap()
            .read()
            .await;

        assert_eq!(2, room.members.len());
        for member in room.members.values() {
            assert_eq!(MembershipState::Join, member.membership);
        }

        assert!(room.power_levels.is_some())
    }

    #[tokio::test]
    async fn calculate_room_names_from_summary() {
        let homeserver = Url::from_str(&mockito::server_url()).unwrap();

        let session = Session {
            access_token: "1234".to_owned(),
            user_id: UserId::try_from("@example:localhost").unwrap(),
            device_id: "DEVICEID".to_owned(),
        };

        let _m = mock(
            "GET",
            Matcher::Regex(r"^/_matrix/client/r0/sync\?.*$".to_string()),
        )
        .with_status(200)
        .with_body(test_json::DEFAULT_SYNC_SUMMARY.to_string())
        .create();

        let client = Client::new(homeserver).unwrap();
        client.restore_login(session).await.unwrap();

        let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));
        let _response = client.sync(sync_settings).await.unwrap();

        let mut room_names = vec![];
        for room in client.joined_rooms().read().await.values() {
            room_names.push(room.read().await.display_name())
        }

        assert_eq!(vec!["example, example2"], room_names);
    }

    #[tokio::test]
    async fn invited_rooms() {
        use std::convert::TryFrom;

        let session = Session {
            access_token: "12345".to_owned(),
            user_id: UserId::try_from("@example:localhost").unwrap(),
            device_id: "DEVICEID".to_owned(),
        };

        let homeserver = url::Url::parse(&mockito::server_url()).unwrap();
        let client = Client::new(homeserver).unwrap();
        client.restore_login(session).await.unwrap();

        let _m = mock(
            "GET",
            Matcher::Regex(r"^/_matrix/client/r0/sync\?.*$".to_string()),
        )
        .with_status(200)
        .with_body(test_json::INVITE_SYNC.to_string())
        .create();

        let _response = client.sync(SyncSettings::default()).await.unwrap();

        assert!(client.joined_rooms().read().await.is_empty());
        assert!(client.left_rooms().read().await.is_empty());
        assert!(!client.invited_rooms().read().await.is_empty());

        assert!(client
            .get_invited_room(&RoomId::try_from("!696r7674:example.com").unwrap())
            .await
            .is_some());
    }

    #[tokio::test]
    async fn left_rooms() {
        use std::convert::TryFrom;

        let session = Session {
            access_token: "12345".to_owned(),
            user_id: UserId::try_from("@example:localhost").unwrap(),
            device_id: "DEVICEID".to_owned(),
        };

        let homeserver = url::Url::parse(&mockito::server_url()).unwrap();
        let client = Client::new(homeserver).unwrap();
        client.restore_login(session).await.unwrap();

        let _m = mock(
            "GET",
            Matcher::Regex(r"^/_matrix/client/r0/sync\?.*$".to_string()),
        )
        .with_status(200)
        .with_body(test_json::LEAVE_SYNC.to_string())
        .create();

        let _response = client.sync(SyncSettings::default()).await.unwrap();

        assert!(client.joined_rooms().read().await.is_empty());
        assert!(!client.left_rooms().read().await.is_empty());
        assert!(client.invited_rooms().read().await.is_empty());

        assert!(client
            .get_left_room(&RoomId::try_from("!SVkFJHzfwvuaIEawgC:localhost").unwrap())
            .await
            .is_some())
    }

    #[tokio::test]
    async fn test_client_sync_store() {
        let homeserver = url::Url::from_str(&mockito::server_url()).unwrap();

        let session = Session {
            access_token: "1234".to_owned(),
            user_id: UserId::try_from("@cheeky_monkey:matrix.org").unwrap(),
            device_id: "DEVICEID".to_owned(),
        };

        let _m = mock(
            "GET",
            Matcher::Regex(r"^/_matrix/client/r0/sync\?.*$".to_string()),
        )
        .with_status(200)
        .with_body(test_json::SYNC.to_string())
        .create();

        let _m = mock("POST", "/_matrix/client/r0/login")
            .with_status(200)
            .with_body(test_json::LOGIN.to_string())
            .create();

        let dir = tempdir().unwrap();
        // a sync response to populate our JSON store
        let config =
            ClientConfig::default().state_store(Box::new(JsonStore::open(dir.path()).unwrap()));
        let client = Client::new_with_config(homeserver.clone(), config).unwrap();
        client.restore_login(session.clone()).await.unwrap();
        let sync_settings = SyncSettings::new().timeout(std::time::Duration::from_millis(3000));

        // gather state to save to the db, the first time through loading will be skipped
        let _ = client.sync(sync_settings.clone()).await.unwrap();

        // now syncing the client will update from the state store
        let config =
            ClientConfig::default().state_store(Box::new(JsonStore::open(dir.path()).unwrap()));
        let client = Client::new_with_config(homeserver, config).unwrap();
        client.restore_login(session.clone()).await.unwrap();
        client.sync(sync_settings).await.unwrap();

        let base_client = &client.base_client;

        // assert the synced client and the logged in client are equal
        assert_eq!(*base_client.session().read().await, Some(session));
        assert_eq!(
            base_client.sync_token().await,
            Some("s526_47314_0_7_1_1_1_11444_1".to_string())
        );
        // assert_eq!(
        //     *base_client.ignored_users.read().await,
        //     vec![UserId::try_from("@someone:example.org").unwrap()]
        // );
    }

    #[tokio::test]
    async fn login() {
        let homeserver = Url::from_str(&mockito::server_url()).unwrap();

        let _m = mock("POST", "/_matrix/client/r0/login")
            .with_status(200)
            .with_body(test_json::LOGIN.to_string())
            .create();

        let client = Client::new(homeserver).unwrap();

        client
            .login("example", "wordpass", None, None)
            .await
            .unwrap();

        let logged_in = client.logged_in().await;
        assert!(logged_in, "Clint should be logged in");
    }

    #[tokio::test]
    async fn sync() {
        let homeserver = Url::from_str(&mockito::server_url()).unwrap();

        let session = Session {
            access_token: "1234".to_owned(),
            user_id: UserId::try_from("@example:localhost").unwrap(),
            device_id: "DEVICEID".to_owned(),
        };

        let _m = mock(
            "GET",
            Matcher::Regex(r"^/_matrix/client/r0/sync\?.*$".to_string()),
        )
        .with_status(200)
        .with_body(test_json::SYNC.to_string())
        .create();

        let client = Client::new(homeserver).unwrap();
        client.restore_login(session).await.unwrap();

        let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

        let response = client.sync(sync_settings).await.unwrap();

        assert_ne!(response.next_batch, "");

        assert!(client.sync_token().await.is_some());
    }

    #[tokio::test]
    async fn room_names() {
        let homeserver = Url::from_str(&mockito::server_url()).unwrap();

        let session = Session {
            access_token: "1234".to_owned(),
            user_id: UserId::try_from("@example:localhost").unwrap(),
            device_id: "DEVICEID".to_owned(),
        };

        let _m = mock(
            "GET",
            Matcher::Regex(r"^/_matrix/client/r0/sync\?.*$".to_string()),
        )
        .with_status(200)
        .with_body(test_json::SYNC.to_string())
        .create();

        let client = Client::new(homeserver).unwrap();
        client.restore_login(session).await.unwrap();

        let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

        let _response = client.sync(sync_settings).await.unwrap();

        let mut names = vec![];
        for r in client.joined_rooms().read().await.values() {
            names.push(r.read().await.display_name());
        }
        assert_eq!(vec!["tutorial"], names);
        let room = client
            .get_joined_room(&RoomId::try_from("!SVkFJHzfwvuaIEawgC:localhost").unwrap())
            .await
            .unwrap();

        assert_eq!("tutorial".to_string(), room.read().await.display_name());
    }
}
