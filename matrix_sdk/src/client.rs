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
use std::{
    collections::HashMap,
    convert::{TryFrom, TryInto},
    fmt::{self, Debug},
    future::Future,
    path::Path,
    result::Result as StdResult,
    sync::Arc,
};

#[cfg(feature = "encryption")]
use dashmap::DashMap;
use futures_timer::Delay as sleep;
use reqwest::header::{HeaderValue, InvalidHeaderValue};
use url::Url;

#[cfg(feature = "encryption")]
use tracing::{debug, warn};
use tracing::{error, info, instrument};

use matrix_sdk_base::{BaseClient, BaseClientConfig, Room, Session, StateStore};

#[cfg(feature = "encryption")]
use matrix_sdk_base::{CryptoStoreError, Device, UserDevices};

use matrix_sdk_common::{
    api::r0::{
        account::register,
        directory::{get_public_rooms, get_public_rooms_filtered},
        membership::{
            ban_user, forget_room,
            invite_user::{self, InvitationRecipient},
            join_room_by_id, join_room_by_id_or_alias, kick_user, leave_room, Invite3pid,
        },
        message::{get_message_events, send_message_event},
        read_marker::set_read_marker,
        receipt::create_receipt,
        room::create_room,
        session::login,
        sync::sync_events,
        typing::create_typing_event,
    },
    assign,
    identifiers::ServerName,
    instant::{Duration, Instant},
    js_int::UInt,
    locks::RwLock,
    presence::PresenceState,
    uuid::Uuid,
    FromHttpResponseError,
};

#[cfg(feature = "encryption")]
use matrix_sdk_common::{
    api::r0::{
        keys::{claim_keys, get_keys, upload_keys},
        to_device::send_event_to_device::{
            IncomingRequest as OwnedToDeviceRequest, Request as ToDeviceRequest,
            Response as ToDeviceResponse,
        },
    },
    identifiers::DeviceKeyAlgorithm,
    locks::Mutex,
};

use crate::{
    events::{room::message::MessageEventContent, AnyMessageEventContent},
    http_client::{DefaultHttpClient, HttpClient, HttpSend},
    identifiers::{EventId, RoomId, RoomIdOrAliasId, UserId},
    Error, EventEmitter, OutgoingRequest, Result,
};

#[cfg(feature = "encryption")]
use crate::{identifiers::DeviceId, sas::Sas};

const DEFAULT_SYNC_TIMEOUT: Duration = Duration::from_secs(30);

/// An async/await enabled Matrix client.
///
/// All of the state is held in an `Arc` so the `Client` can be cloned freely.
#[derive(Clone)]
pub struct Client {
    /// The URL of the homeserver to connect to.
    homeserver: Arc<Url>,
    /// The underlying HTTP client.
    http_client: HttpClient,
    /// User session data.
    pub(crate) base_client: BaseClient,
    /// Locks making sure we only have one group session sharing request in
    /// flight per room.
    #[cfg(feature = "encryption")]
    group_session_locks: DashMap<RoomId, Arc<Mutex<()>>>,
}

#[cfg(not(tarpaulin_include))]
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
    pub(crate) proxy: Option<reqwest::Proxy>,
    pub(crate) user_agent: Option<HeaderValue>,
    pub(crate) disable_ssl_verification: bool,
    pub(crate) base_config: BaseClientConfig,
    pub(crate) timeout: Option<Duration>,
    pub(crate) client: Option<Arc<dyn HttpSend>>,
}

#[cfg(not(tarpaulin_include))]
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

    /// Set a timeout duration for all HTTP requests. The default is no timeout.
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    /// Specify a client to handle sending requests and receiving responses.
    ///
    /// Any type that implements the `HttpSend` trait can be used to send/receive
    /// `http` types.
    pub fn client(mut self, client: Arc<dyn HttpSend>) -> Self {
        self.client = Some(client);
        self
    }
}

#[derive(Debug, Default, Clone)]
/// Settings for a sync call.
pub struct SyncSettings<'a> {
    pub(crate) filter: Option<sync_events::Filter<'a>>,
    pub(crate) timeout: Option<Duration>,
    pub(crate) token: Option<String>,
    pub(crate) full_state: bool,
}

impl<'a> SyncSettings<'a> {
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

    /// Set the sync filter.
    /// It can be either the filter ID, or the definition for the filter.
    ///
    /// # Arguments
    ///
    /// * `filter` - The filter configuration that should be used for the sync call.
    pub fn filter(mut self, filter: sync_events::Filter<'a>) -> Self {
        self.filter = Some(filter);
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
        let homeserver = if let Ok(u) = homeserver_url.try_into() {
            Arc::new(u)
        } else {
            panic!("Error parsing homeserver url")
        };

        let client = if let Some(client) = config.client {
            client
        } else {
            Arc::new(DefaultHttpClient::with_config(&config)?)
        };

        let base_client = BaseClient::new_with_config(config.base_config)?;
        let session = base_client.session().clone();

        let http_client = HttpClient {
            homeserver: homeserver.clone(),
            inner: client,
            session,
        };

        Ok(Self {
            homeserver,
            http_client,
            base_client,
            #[cfg(feature = "encryption")]
            group_session_locks: DashMap::new(),
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

    /// Get the user id of the current owner of the client.
    pub async fn user_id(&self) -> Option<UserId> {
        let session = self.base_client.session().read().await;
        session.as_ref().cloned().map(|s| s.user_id)
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
    /// This can be used for the first login as well as for subsequent logins,
    /// note that if the device id isn't provided a new device will be created.
    ///
    /// If this isn't the first login a device id should be provided to restore
    /// the correct stores.
    ///
    /// Alternatively the `restore_login()` method can be used to restore a
    /// logged in client without the password.
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
    pub async fn login(
        &self,
        user: &str,
        password: &str,
        device_id: Option<&str>,
        initial_device_display_name: Option<&str>,
    ) -> Result<login::Response> {
        info!("Logging in to {} as {:?}", self.homeserver, user);

        let request = login::Request {
            user: login::UserInfo::MatrixId(user),
            login_info: login::LoginInfo::Password { password },
            device_id: device_id.map(|d| d.into()),
            initial_device_display_name,
        };

        let response = self.send(request).await?;
        self.base_client.receive_login_response(&response).await?;

        Ok(response)
    }

    /// Restore a previously logged in session.
    ///
    /// This can be used to restore the client to a logged in state, loading all
    /// the stored state and encryption keys.
    ///
    /// Alternatively, if the whole session isn't stored the `login()` method
    /// can be used with a device id.
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
    /// ```no_run
    /// # use std::convert::TryFrom;
    /// # use matrix_sdk::{Client, RegistrationBuilder};
    /// # use matrix_sdk::api::r0::account::register::RegistrationKind;
    /// # use matrix_sdk::identifiers::DeviceId;
    /// # use futures::executor::block_on;
    /// # use url::Url;
    /// # let homeserver = Url::parse("http://example.com").unwrap();
    /// # block_on(async {
    /// let mut builder = RegistrationBuilder::default();
    /// builder.password("pass")
    ///     .username("user")
    ///     .kind(RegistrationKind::User);
    /// let mut client = Client::new(homeserver).unwrap();
    /// client.register_user(builder).await;
    /// # })
    /// ```
    #[instrument(skip(registration))]
    pub async fn register_user<'a, R: Into<register::Request<'a>>>(
        &self,
        registration: R,
    ) -> Result<register::Response> {
        info!("Registering to {}", self.homeserver);

        let request = registration.into();
        self.send(request).await
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
        let request = join_room_by_id::Request::new(room_id);
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
        server_names: &[Box<ServerName>],
    ) -> Result<join_room_by_id_or_alias::Response> {
        let request = join_room_by_id_or_alias::Request {
            room_id_or_alias: alias.clone(),
            server_name: server_names,
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
        let request = forget_room::Request::new(room_id);
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
        reason: Option<&str>,
    ) -> Result<ban_user::Response> {
        let request = assign!(ban_user::Request::new(room_id, user_id), { reason });
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
        reason: Option<&str>,
    ) -> Result<kick_user::Response> {
        let request = assign!(kick_user::Request::new(room_id, user_id), { reason });
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
        let request = leave_room::Request::new(room_id);
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

    /// Search the homeserver's directory of public rooms.
    ///
    /// Sends a request to "_matrix/client/r0/publicRooms", returns
    /// a `get_public_rooms::Response`.
    ///
    /// # Arguments
    ///
    /// * `limit` - The number of `PublicRoomsChunk`s in each response.
    ///
    /// * `since` - Pagination token from a previous request.
    ///
    /// * `server` - The name of the server, if `None` the requested server is used.
    ///
    /// # Examples
    /// ```no_run
    /// use matrix_sdk::Client;
    /// # use url::Url;
    /// # let homeserver = Url::parse("http://example.com").unwrap();
    /// # let limit = Some(10);
    /// # let since = Some("since token");
    /// # let server = Some("server name");
    ///
    /// let mut client = Client::new(homeserver).unwrap();
    /// # use futures::executor::block_on;
    /// # block_on(async {
    ///
    /// client.public_rooms(limit, since, server).await;
    /// # });
    /// ```
    pub async fn public_rooms(
        &self,
        limit: Option<u32>,
        since: Option<&str>,
        server: Option<&str>,
    ) -> Result<get_public_rooms::Response> {
        let limit = limit.map(|n| UInt::try_from(n).ok()).flatten();

        let request = get_public_rooms::Request {
            limit,
            since,
            server,
        };
        self.send(request).await
    }

    /// Search the homeserver's directory of public rooms with a filter.
    ///
    /// Sends a request to "_matrix/client/r0/publicRooms", returns
    /// a `get_public_rooms_filtered::Response`.
    ///
    /// # Arguments
    ///
    /// * `room_search` - The easiest way to create this request is using the `RoomListFilterBuilder`.
    ///
    /// # Examples
    /// ```
    /// # use std::convert::TryFrom;
    /// # use matrix_sdk::{Client, RoomListFilterBuilder};
    /// # use matrix_sdk::directory::{Filter, RoomNetwork};
    /// # use matrix_sdk::api::r0::directory::get_public_rooms_filtered;
    /// # use url::Url;
    /// # use futures::executor::block_on;
    /// # let homeserver = Url::parse("http://example.com").unwrap();
    /// # block_on(async {
    /// # let last_sync_token = "".to_string();
    /// let mut client = Client::new(homeserver).unwrap();
    ///
    /// let generic_search_term = Some("matrix-rust-sdk".to_string());
    /// let mut builder = RoomListFilterBuilder::new();
    /// builder
    ///     .filter(Filter { generic_search_term, })
    ///     .since(&last_sync_token)
    ///     .room_network(RoomNetwork::Matrix);
    ///
    /// client.public_rooms_filtered(builder).await;
    /// # })
    /// ```
    pub async fn public_rooms_filtered<'a, R: Into<get_public_rooms_filtered::Request<'a>>>(
        &self,
        room_search: R,
    ) -> Result<get_public_rooms_filtered::Response> {
        let request = room_search.into();
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
    /// let mut builder = RoomBuilder::new();
    /// builder.federate(false)
    ///     .initial_state(vec![])
    ///     .visibility(Visibility::Public)
    ///     .name("name")
    ///     .room_version("v1.0");
    ///
    /// let mut client = Client::new(homeserver).unwrap();
    /// # use futures::executor::block_on;
    /// # block_on(async {
    /// assert!(client.create_room(builder).await.is_ok());
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
    /// returns a `get_message_events::Response` that contains chunks
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
    /// # use matrix_sdk::identifiers::room_id;
    /// # use matrix_sdk::api::r0::filter::RoomEventFilter;
    /// # use matrix_sdk::api::r0::message::get_message_events::Direction;
    /// # use url::Url;
    /// # use matrix_sdk::js_int::UInt;
    ///
    /// # let homeserver = Url::parse("http://example.com").unwrap();
    /// let room_id = room_id!("!roomid:example.com");
    /// let mut builder = MessagesRequestBuilder::new(&room_id, "t47429-4392820_219380_26003_2265");
    ///
    /// builder.to("t4357353_219380_26003_2265")
    ///     .direction(Direction::Backward)
    ///     .limit(10);
    ///
    /// let mut client = Client::new(homeserver).unwrap();
    /// # use futures::executor::block_on;
    /// # block_on(async {
    /// assert!(client.room_messages(builder).await.is_ok());
    /// # });
    /// ```
    pub async fn room_messages<'a, R: Into<get_message_events::Request<'a>>>(
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

    /// Share a group session for the given room.
    ///
    /// This will create Olm sessions with all the users/device pairs in the
    /// room if necessary and share a group session with them.
    ///
    /// Does nothing if no group session needs to be shared.
    #[cfg(feature = "encryption")]
    #[cfg_attr(feature = "docs", doc(cfg(encryption)))]
    async fn preshare_group_session(&self, room_id: &RoomId) -> Result<()> {
        // TODO expose this publicly so people can pre-share a group session if
        // e.g. a user starts to type a message for a room.
        if self.base_client.should_share_group_session(room_id).await {
            #[allow(clippy::map_clone)]
            if let Some(mutex) = self.group_session_locks.get(room_id).map(|m| m.clone()) {
                // If a group session share request is already going on,
                // await the release of the lock.
                mutex.lock().await;
            } else {
                // Otherwise create a new lock and share the group
                // session.
                let mutex = Arc::new(Mutex::new(()));
                self.group_session_locks
                    .insert(room_id.clone(), mutex.clone());

                let _guard = mutex.lock().await;

                let missing_sessions = {
                    let room = self.base_client.get_joined_room(room_id).await;
                    let room = room.as_ref().unwrap().read().await;
                    let members = room
                        .joined_members
                        .keys()
                        .chain(room.invited_members.keys());
                    self.base_client.get_missing_sessions(members).await?
                };

                if !missing_sessions.is_empty() {
                    self.claim_one_time_keys(missing_sessions).await?;
                }

                let response = self.share_group_session(room_id).await;

                self.group_session_locks.remove(room_id);

                // If one of the responses failed invalidate the group
                // session as using it would end up in undecryptable
                // messages.
                if let Err(r) = response {
                    self.base_client.invalidate_group_session(room_id).await;
                    return Err(r);
                }
            }
        }

        Ok(())
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
    /// in its unsigned field as `transaction_id`. If not given one is created for the
    /// message.
    ///
    /// # Example
    /// ```no_run
    /// # use matrix_sdk::Room;
    /// # use std::sync::{Arc, RwLock};
    /// # use matrix_sdk::{Client, SyncSettings};
    /// # use url::Url;
    /// # use futures::executor::block_on;
    /// # use matrix_sdk::identifiers::room_id;
    /// # use std::convert::TryFrom;
    /// use matrix_sdk::events::room::message::{MessageEventContent, TextMessageEventContent};
    /// # block_on(async {
    /// # let homeserver = Url::parse("http://localhost:8080").unwrap();
    /// # let mut client = Client::new(homeserver).unwrap();
    /// # let room_id = room_id!("!test:localhost");
    /// use matrix_sdk_common::uuid::Uuid;
    ///
    /// let content = MessageEventContent::Text(TextMessageEventContent::plain("Hello world"));
    /// let txn_id = Uuid::new_v4();
    /// client.room_send(&room_id, content, Some(txn_id)).await.unwrap();
    /// # })
    /// ```
    pub async fn room_send(
        &self,
        room_id: &RoomId,
        content: MessageEventContent,
        txn_id: Option<Uuid>,
    ) -> Result<send_message_event::Response> {
        #[cfg(not(feature = "encryption"))]
        let content = AnyMessageEventContent::RoomMessage(content);

        #[cfg(feature = "encryption")]
        let content = {
            let encrypted = {
                let room = self.base_client.get_joined_room(room_id).await;

                match room {
                    Some(r) => r.read().await.is_encrypted(),
                    None => false,
                }
            };

            if encrypted {
                self.preshare_group_session(room_id).await?;
                AnyMessageEventContent::RoomEncrypted(
                    self.base_client.encrypt(room_id, content).await?,
                )
            } else {
                AnyMessageEventContent::RoomMessage(content)
            }
        };

        let txn_id = txn_id.unwrap_or_else(Uuid::new_v4).to_string();
        let request = send_message_event::Request::new(&room_id, &txn_id, &content);

        let response = self.send(request).await?;
        Ok(response)
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
    /// use matrix_sdk::identifiers::user_id;
    ///
    /// // First construct the request you want to make
    /// // See https://docs.rs/ruma-client-api/latest/ruma_client_api/index.html
    /// // for all available Endpoints
    /// let request = profile::get_profile::Request {
    ///     user_id: user_id!("@example:localhost"),
    /// };
    ///
    /// // Start the request using Client::send()
    /// let response = client.send(request).await.unwrap();
    ///
    /// // Check the corresponding Response struct to find out what types are
    /// // returned
    /// # })
    /// ```
    pub async fn send<Request>(&self, request: Request) -> Result<Request::IncomingResponse>
    where
        Request: OutgoingRequest + Debug,
        Error: From<FromHttpResponseError<Request::EndpointError>>,
    {
        self.http_client.send(request).await
    }

    #[cfg(feature = "encryption")]
    async fn send_to_device(&self, request: OwnedToDeviceRequest) -> Result<ToDeviceResponse> {
        let request = ToDeviceRequest {
            event_type: request.event_type,
            txn_id: &request.txn_id,
            messages: request.messages,
        };

        self.send(request).await
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
    pub async fn sync(&self, sync_settings: SyncSettings<'_>) -> Result<sync_events::Response> {
        let request = sync_events::Request {
            filter: sync_settings.filter,
            since: sync_settings.token.as_deref(),
            full_state: sync_settings.full_state,
            set_presence: PresenceState::Online,
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
        sync_settings: SyncSettings<'_>,
        callback: impl Fn(sync_events::Response) -> C,
    ) where
        C: Future<Output = ()>,
    {
        let mut sync_settings = sync_settings;
        let filter = sync_settings.filter.clone();
        let mut last_sync_time: Option<Instant> = None;

        if sync_settings.token.is_none() {
            sync_settings.token = self.sync_token().await;
        }

        loop {
            let response = self.sync(sync_settings.clone()).await;

            let response = match response {
                Ok(r) => r,
                Err(e) => {
                    error!("Received an invalid response: {}", e);
                    sleep::new(Duration::from_secs(1)).await;
                    continue;
                }
            };

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

                for request in self.base_client.outgoing_to_device_requests().await {
                    let txn_id = request.txn_id.clone();

                    if self.send_to_device(request).await.is_ok() {
                        self.base_client
                            .mark_to_device_request_as_sent(&txn_id)
                            .await;
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
            if let Some(f) = filter.as_ref() {
                sync_settings = sync_settings.filter(f.clone());
            }
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
    #[cfg_attr(feature = "docs", doc(cfg(encryption)))]
    #[instrument]
    async fn claim_one_time_keys(
        &self,
        one_time_keys: BTreeMap<UserId, BTreeMap<Box<DeviceId>, DeviceKeyAlgorithm>>,
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
    #[cfg_attr(feature = "docs", doc(cfg(encryption)))]
    #[instrument]
    async fn share_group_session(&self, room_id: &RoomId) -> Result<()> {
        let mut requests = self
            .base_client
            .share_group_session(room_id)
            .await
            .expect("Keys don't need to be uploaded");

        for request in requests.drain(..) {
            self.send_to_device(request).await?;
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
    #[cfg_attr(feature = "docs", doc(cfg(encryption)))]
    #[instrument]
    async fn keys_upload(&self) -> Result<upload_keys::Response> {
        let request = self
            .base_client
            .keys_for_upload()
            .await
            .expect("Keys don't need to be uploaded");

        debug!(
            "Uploading encryption keys device keys: {}, one-time-keys: {}",
            request.device_keys.is_some(),
            request.one_time_keys.as_ref().map_or(0, |k| k.len())
        );

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
    #[cfg_attr(feature = "docs", doc(cfg(encryption)))]
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

        let mut device_keys: BTreeMap<UserId, Vec<Box<DeviceId>>> = BTreeMap::new();

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

    /// Get a `Sas` verification object with the given flow id.
    #[cfg(feature = "encryption")]
    #[cfg_attr(feature = "docs", doc(cfg(encryption)))]
    pub async fn get_verification(&self, flow_id: &str) -> Option<Sas> {
        self.base_client
            .get_verification(flow_id)
            .await
            .map(|sas| Sas {
                inner: sas,
                http_client: self.http_client.clone(),
            })
    }

    /// Start a interactive verification with the given `Device`.
    ///
    /// # Arguments
    ///
    /// * `device` - The device which we would like to start an interactive
    /// verification with.
    ///
    /// Returns a `Sas` object that represents the interactive verification flow.
    #[cfg(feature = "encryption")]
    #[cfg_attr(feature = "docs", doc(cfg(encryption)))]
    pub async fn start_verification(&self, device: Device) -> Result<Sas> {
        let (sas, request) = self
            .base_client
            .start_verification(device)
            .await
            .ok_or(Error::AuthenticationRequired)?;

        self.send_to_device(request).await?;

        Ok(Sas {
            inner: sas,
            http_client: self.http_client.clone(),
        })
    }

    /// Get a specific device of a user.
    ///
    /// # Arguments
    ///
    /// * `user_id` - The unique id of the user that the device belongs to.
    ///
    /// * `device_id` - The unique id of the device.
    ///
    /// Returns a `Device` if one is found and the crypto store didn't throw an
    /// error.
    ///
    /// This will always return None if the client hasn't been logged in.
    ///
    /// # Example
    ///
    /// ```
    /// # use std::convert::TryFrom;
    /// # use matrix_sdk::{Client, identifiers::UserId};
    /// # use url::Url;
    /// # use futures::executor::block_on;
    /// # let alice = UserId::try_from("@alice:example.org").unwrap();
    /// # let homeserver = Url::parse("http://example.com").unwrap();
    /// # let client = Client::new(homeserver).unwrap();
    /// # block_on(async {
    /// let device = client.get_device(&alice, "DEVICEID".into()).await;
    ///
    /// println!("{:?}", device);
    /// # });
    /// ```
    #[cfg(feature = "encryption")]
    #[cfg_attr(feature = "docs", doc(cfg(encryption)))]
    pub async fn get_device(&self, user_id: &UserId, device_id: &DeviceId) -> Option<Device> {
        self.base_client.get_device(user_id, device_id).await
    }

    /// Get a map holding all the devices of an user.
    ///
    /// This will always return an empty map if the client hasn't been logged
    /// in.
    ///
    /// # Arguments
    ///
    /// * `user_id` - The unique id of the user that the devices belong to.
    ///
    /// # Example
    ///
    /// ```
    /// # use std::convert::TryFrom;
    /// # use matrix_sdk::{Client, identifiers::UserId};
    /// # use url::Url;
    /// # use futures::executor::block_on;
    /// # let alice = UserId::try_from("@alice:example.org").unwrap();
    /// # let homeserver = Url::parse("http://example.com").unwrap();
    /// # let client = Client::new(homeserver).unwrap();
    /// # block_on(async {
    /// let devices = client.get_user_devices(&alice).await.unwrap();
    ///
    /// for device in devices.devices() {
    ///     println!("{:?}", device);
    /// }
    /// # });
    /// ```
    #[cfg(feature = "encryption")]
    #[cfg_attr(feature = "docs", doc(cfg(encryption)))]
    pub async fn get_user_devices(
        &self,
        user_id: &UserId,
    ) -> StdResult<UserDevices, CryptoStoreError> {
        self.base_client.get_user_devices(user_id).await
    }
}

#[cfg(test)]
mod test {
    use super::{
        create_typing_event, get_public_rooms, get_public_rooms_filtered,
        register::RegistrationKind, Client, ClientConfig, Invite3pid, MessageEventContent, Session,
        SyncSettings, Url,
    };
    use crate::{RegistrationBuilder, RoomListFilterBuilder};

    use matrix_sdk_base::JsonStore;
    use matrix_sdk_common::{
        api::r0::uiaa::AuthData,
        directory::Filter,
        events::room::message::TextMessageEventContent,
        identifiers::{event_id, room_id, user_id},
        thirdparty,
    };
    use matrix_sdk_test::{test_json, EventBuilder, EventsJson};
    use mockito::{mock, Matcher};
    use tempfile::tempdir;

    use std::{convert::TryInto, path::Path, str::FromStr, time::Duration};

    async fn logged_in_client() -> Client {
        let session = Session {
            access_token: "1234".to_owned(),
            user_id: user_id!("@example:localhost"),
            device_id: "DEVICEID".into(),
        };
        let homeserver = url::Url::parse(&mockito::server_url()).unwrap();
        let client = Client::new(homeserver).unwrap();
        client.restore_login(session).await.unwrap();

        client
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
        assert!(logged_in, "Client should be logged in");
    }

    #[tokio::test]
    async fn test_join_leave_room() {
        let homeserver = Url::from_str(&mockito::server_url()).unwrap();

        let room_id = room_id!("!SVkFJHzfwvuaIEawgC:localhost");

        let session = Session {
            access_token: "1234".to_owned(),
            user_id: user_id!("@example:localhost"),
            device_id: "DEVICEID".into(),
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
        let client = logged_in_client().await;

        let _m = mock(
            "GET",
            Matcher::Regex(r"^/_matrix/client/r0/sync\?.*$".to_string()),
        )
        .with_status(200)
        .with_body(test_json::SYNC.to_string())
        .match_header("authorization", "Bearer 1234")
        .create();

        let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));
        let _response = client.sync(sync_settings).await.unwrap();

        // let bc = &client.base_client;
        // let ignored_users = bc.ignored_users.read().await;
        // assert_eq!(1, ignored_users.len())
    }

    #[tokio::test]
    async fn room_creation() {
        let client = logged_in_client().await;

        let mut response = EventBuilder::default()
            .add_state_event(EventsJson::Member)
            .add_state_event(EventsJson::PowerLevels)
            .build_sync_response();

        client
            .base_client
            .receive_sync_response(&mut response)
            .await
            .unwrap();
        let room_id = room_id!("!SVkFJHzfwvuaIEawgC:localhost");

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
        let client = Client::new(homeserver).unwrap();

        let _m = mock("POST", "/_matrix/client/r0/login")
            .with_status(403)
            .with_body(test_json::LOGIN_RESPONSE_ERR.to_string())
            .create();

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
        let client = Client::new(homeserver).unwrap();

        let _m = mock("POST", "/_matrix/client/r0/register")
            .with_status(403)
            .with_body(test_json::REGISTRATION_RESPONSE_ERR.to_string())
            .create();

        let mut user = RegistrationBuilder::default();

        user.username("user")
            .password("password")
            .auth(AuthData::FallbackAcknowledgement { session: "foobar" })
            .kind(RegistrationKind::User);

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
        let client = logged_in_client().await;

        let _m = mock(
            "POST",
            Matcher::Regex(r"^/_matrix/client/r0/rooms/.*/join".to_string()),
        )
        .with_status(200)
        .with_body(test_json::ROOM_ID.to_string())
        .match_header("authorization", "Bearer 1234")
        .create();

        let room_id = room_id!("!testroom:example.org");

        assert_eq!(
            // this is the `join_by_room_id::Response` but since no PartialEq we check the RoomId field
            client.join_room_by_id(&room_id).await.unwrap().room_id,
            room_id
        );
    }

    #[tokio::test]
    async fn join_room_by_id_or_alias() {
        let client = logged_in_client().await;

        let _m = mock(
            "POST",
            Matcher::Regex(r"^/_matrix/client/r0/join/".to_string()),
        )
        .with_status(200)
        .with_body(test_json::ROOM_ID.to_string())
        .match_header("authorization", "Bearer 1234")
        .create();

        let room_id = room_id!("!testroom:example.org").into();

        assert_eq!(
            // this is the `join_by_room_id::Response` but since no PartialEq we check the RoomId field
            client
                .join_room_by_id_or_alias(&room_id, &["server.com".try_into().unwrap()])
                .await
                .unwrap()
                .room_id,
            room_id!("!testroom:example.org")
        );
    }

    #[tokio::test]
    async fn invite_user_by_id() {
        let client = logged_in_client().await;

        let _m = mock(
            "POST",
            Matcher::Regex(r"^/_matrix/client/r0/rooms/.*/invite".to_string()),
        )
        .with_status(200)
        .with_body(test_json::LOGOUT.to_string())
        .match_header("authorization", "Bearer 1234")
        .create();

        let user = user_id!("@example:localhost");
        let room_id = room_id!("!testroom:example.org");

        client.invite_user_by_id(&room_id, &user).await.unwrap();
    }

    #[tokio::test]
    async fn invite_user_by_3pid() {
        let client = logged_in_client().await;

        let _m = mock(
            "POST",
            Matcher::Regex(r"^/_matrix/client/r0/rooms/.*/invite".to_string()),
        )
        .with_status(200)
        // empty JSON object
        .with_body(test_json::LOGOUT.to_string())
        .match_header("authorization", "Bearer 1234")
        .create();

        let room_id = room_id!("!testroom:example.org");

        client
            .invite_user_by_3pid(
                &room_id,
                &Invite3pid {
                    id_server: "example.org".to_string(),
                    id_access_token: "IdToken".to_string(),
                    medium: thirdparty::Medium::Email,
                    address: "address".to_string(),
                },
            )
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn room_search_all() {
        let homeserver = Url::from_str(&mockito::server_url()).unwrap();
        let client = Client::new(homeserver).unwrap();

        let _m = mock(
            "GET",
            Matcher::Regex(r"^/_matrix/client/r0/publicRooms".to_string()),
        )
        .with_status(200)
        .with_body(test_json::PUBLIC_ROOMS.to_string())
        .create();

        let get_public_rooms::Response { chunk, .. } =
            client.public_rooms(Some(10), None, None).await.unwrap();
        assert_eq!(chunk.len(), 1);
    }

    #[tokio::test]
    async fn room_search_filtered() {
        let client = logged_in_client().await;

        let _m = mock(
            "POST",
            Matcher::Regex(r"^/_matrix/client/r0/publicRooms".to_string()),
        )
        .with_status(200)
        .with_body(test_json::PUBLIC_ROOMS.to_string())
        .match_header("authorization", "Bearer 1234")
        .create();

        let generic_search_term = Some("cheese".to_string());
        let mut request = RoomListFilterBuilder::default();
        request.filter(Filter {
            generic_search_term,
        });

        let get_public_rooms_filtered::Response { chunk, .. } =
            client.public_rooms_filtered(request).await.unwrap();
        assert_eq!(chunk.len(), 1);
    }

    #[tokio::test]
    async fn leave_room() {
        let client = logged_in_client().await;

        let _m = mock(
            "POST",
            Matcher::Regex(r"^/_matrix/client/r0/rooms/.*/leave".to_string()),
        )
        .with_status(200)
        // this is an empty JSON object
        .with_body(test_json::LOGOUT.to_string())
        .match_header("authorization", "Bearer 1234")
        .create();

        let room_id = room_id!("!testroom:example.org");

        client.leave_room(&room_id).await.unwrap();
    }

    #[tokio::test]
    async fn ban_user() {
        let client = logged_in_client().await;

        let _m = mock(
            "POST",
            Matcher::Regex(r"^/_matrix/client/r0/rooms/.*/ban".to_string()),
        )
        .with_status(200)
        // this is an empty JSON object
        .with_body(test_json::LOGOUT.to_string())
        .match_header("authorization", "Bearer 1234")
        .create();

        let user = user_id!("@example:localhost");
        let room_id = room_id!("!testroom:example.org");
        client.ban_user(&room_id, &user, None).await.unwrap();
    }

    #[tokio::test]
    async fn kick_user() {
        let client = logged_in_client().await;

        let _m = mock(
            "POST",
            Matcher::Regex(r"^/_matrix/client/r0/rooms/.*/kick".to_string()),
        )
        .with_status(200)
        // this is an empty JSON object
        .with_body(test_json::LOGOUT.to_string())
        .match_header("authorization", "Bearer 1234")
        .create();

        let user = user_id!("@example:localhost");
        let room_id = room_id!("!testroom:example.org");

        client.kick_user(&room_id, &user, None).await.unwrap();
    }

    #[tokio::test]
    async fn forget_room() {
        let client = logged_in_client().await;

        let _m = mock(
            "POST",
            Matcher::Regex(r"^/_matrix/client/r0/rooms/.*/forget".to_string()),
        )
        .with_status(200)
        // this is an empty JSON object
        .with_body(test_json::LOGOUT.to_string())
        .match_header("authorization", "Bearer 1234")
        .create();

        let room_id = room_id!("!testroom:example.org");

        client.forget_room_by_id(&room_id).await.unwrap();
    }

    #[tokio::test]
    async fn read_receipt() {
        let client = logged_in_client().await;

        let _m = mock(
            "POST",
            Matcher::Regex(r"^/_matrix/client/r0/rooms/.*/receipt".to_string()),
        )
        .with_status(200)
        // this is an empty JSON object
        .with_body(test_json::LOGOUT.to_string())
        .match_header("authorization", "Bearer 1234")
        .create();

        let room_id = room_id!("!testroom:example.org");
        let event_id = event_id!("$xxxxxx:example.org");

        client.read_receipt(&room_id, &event_id).await.unwrap();
    }

    #[tokio::test]
    async fn read_marker() {
        let client = logged_in_client().await;

        let _m = mock(
            "POST",
            Matcher::Regex(r"^/_matrix/client/r0/rooms/.*/read_markers".to_string()),
        )
        .with_status(200)
        // this is an empty JSON object
        .with_body(test_json::LOGOUT.to_string())
        .match_header("authorization", "Bearer 1234")
        .create();

        let room_id = room_id!("!testroom:example.org");
        let event_id = event_id!("$xxxxxx:example.org");

        client.read_marker(&room_id, &event_id, None).await.unwrap();
    }

    #[tokio::test]
    #[allow(irrefutable_let_patterns)]
    async fn typing_notice() {
        let client = logged_in_client().await;

        let _m = mock(
            "PUT",
            Matcher::Regex(r"^/_matrix/client/r0/rooms/.*/typing".to_string()),
        )
        .with_status(200)
        // this is an empty JSON object
        .with_body(test_json::LOGOUT.to_string())
        .match_header("authorization", "Bearer 1234")
        .create();

        let room_id = room_id!("!testroom:example.org");

        let response = client
            .typing_notice(
                &room_id,
                &client.user_id().await.unwrap(),
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

        let client = logged_in_client().await;

        let _m = mock(
            "PUT",
            Matcher::Regex(r"^/_matrix/client/r0/rooms/.*/send/".to_string()),
        )
        .with_status(200)
        .match_header("authorization", "Bearer 1234")
        .with_body(test_json::EVENT_ID.to_string())
        .create();

        let room_id = room_id!("!testroom:example.org");

        let content = MessageEventContent::Text(TextMessageEventContent {
            body: "Hello world".to_owned(),
            relates_to: None,
            formatted: None,
        });
        let txn_id = Uuid::new_v4();
        let response = client
            .room_send(&room_id, content, Some(txn_id))
            .await
            .unwrap();

        assert_eq!(event_id!("$h29iv0s8:example.com"), response.event_id)
    }

    #[tokio::test]
    async fn user_presence() {
        let client = logged_in_client().await;

        let _m = mock(
            "GET",
            Matcher::Regex(r"^/_matrix/client/r0/sync\?.*$".to_string()),
        )
        .with_status(200)
        .match_header("authorization", "Bearer 1234")
        .with_body(test_json::SYNC.to_string())
        .create();

        let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

        let _response = client.sync(sync_settings).await.unwrap();

        let rooms_lock = &client.base_client.joined_rooms();
        let rooms = rooms_lock.read().await;
        let room = &rooms
            .get(&room_id!("!SVkFJHzfwvuaIEawgC:localhost"))
            .unwrap()
            .read()
            .await;

        assert_eq!(1, room.joined_members.len());
        assert!(room.power_levels.is_some())
    }

    #[tokio::test]
    async fn calculate_room_names_from_summary() {
        let client = logged_in_client().await;

        let _m = mock(
            "GET",
            Matcher::Regex(r"^/_matrix/client/r0/sync\?.*$".to_string()),
        )
        .with_status(200)
        .match_header("authorization", "Bearer 1234")
        .with_body(test_json::DEFAULT_SYNC_SUMMARY.to_string())
        .create();

        let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));
        let _response = client.sync(sync_settings).await.unwrap();

        let mut room_names = vec![];
        for room in client.joined_rooms().read().await.values() {
            room_names.push(room.read().await.display_name())
        }

        assert_eq!(vec!["example2"], room_names);
    }

    #[tokio::test]
    async fn invited_rooms() {
        let client = logged_in_client().await;

        let _m = mock(
            "GET",
            Matcher::Regex(r"^/_matrix/client/r0/sync\?.*$".to_string()),
        )
        .with_status(200)
        .match_header("authorization", "Bearer 1234")
        .with_body(test_json::INVITE_SYNC.to_string())
        .create();

        let _response = client.sync(SyncSettings::default()).await.unwrap();

        assert!(client.joined_rooms().read().await.is_empty());
        assert!(client.left_rooms().read().await.is_empty());
        assert!(!client.invited_rooms().read().await.is_empty());

        assert!(client
            .get_invited_room(&room_id!("!696r7674:example.com"))
            .await
            .is_some());
    }

    #[tokio::test]
    async fn left_rooms() {
        let client = logged_in_client().await;

        let _m = mock(
            "GET",
            Matcher::Regex(r"^/_matrix/client/r0/sync\?.*$".to_string()),
        )
        .with_status(200)
        .match_header("authorization", "Bearer 1234")
        .with_body(test_json::LEAVE_SYNC.to_string())
        .create();

        let _response = client.sync(SyncSettings::default()).await.unwrap();

        assert!(client.joined_rooms().read().await.is_empty());
        assert!(!client.left_rooms().read().await.is_empty());
        assert!(client.invited_rooms().read().await.is_empty());

        assert!(client
            .get_left_room(&room_id!("!SVkFJHzfwvuaIEawgC:localhost"))
            .await
            .is_some())
    }

    #[tokio::test]
    async fn test_client_sync_store() {
        let homeserver = url::Url::from_str(&mockito::server_url()).unwrap();

        let session = Session {
            access_token: "1234".to_owned(),
            user_id: user_id!("@cheeky_monkey:matrix.org"),
            device_id: "DEVICEID".into(),
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

        // This is commented out because this field is private...
        // assert_eq!(
        //     *base_client.ignored_users.read().await,
        //     vec![user_id!("@someone:example.org")]
        // );
    }

    #[tokio::test]
    async fn sync() {
        let client = logged_in_client().await;

        let _m = mock(
            "GET",
            Matcher::Regex(r"^/_matrix/client/r0/sync\?.*$".to_string()),
        )
        .with_status(200)
        .with_body(test_json::SYNC.to_string())
        .match_header("authorization", "Bearer 1234")
        .create();

        let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

        let response = client.sync(sync_settings).await.unwrap();

        assert_ne!(response.next_batch, "");

        assert!(client.sync_token().await.is_some());
    }

    #[tokio::test]
    async fn room_names() {
        let client = logged_in_client().await;

        let _m = mock(
            "GET",
            Matcher::Regex(r"^/_matrix/client/r0/sync\?.*$".to_string()),
        )
        .with_status(200)
        .match_header("authorization", "Bearer 1234")
        .with_body(test_json::SYNC.to_string())
        .create();

        let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

        let _response = client.sync(sync_settings).await.unwrap();

        let mut names = vec![];
        for r in client.joined_rooms().read().await.values() {
            names.push(r.read().await.display_name());
        }
        assert_eq!(vec!["tutorial"], names);
        let room = client
            .get_joined_room(&room_id!("!SVkFJHzfwvuaIEawgC:localhost"))
            .await
            .unwrap();

        assert_eq!("tutorial".to_string(), room.read().await.display_name());
    }
}
