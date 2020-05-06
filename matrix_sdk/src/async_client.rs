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
use std::result::Result as StdResult;
use std::sync::Arc;
use std::time::{Duration, Instant};

use uuid::Uuid;

use futures::future::Future;
use tokio::sync::RwLock;
use tokio::time::delay_for as sleep;
#[cfg(feature = "encryption")]
use tracing::{debug, warn};
use tracing::{info, instrument, trace};

use http::Method as HttpMethod;
use http::Response as HttpResponse;
use reqwest::header::{HeaderValue, InvalidHeaderValue};
use url::Url;

use crate::events::room::message::MessageEventContent;
use crate::events::EventType;
use crate::identifiers::{RoomId, RoomIdOrAliasId, UserId};
use crate::Endpoint;

#[cfg(feature = "encryption")]
use crate::identifiers::DeviceId;

use crate::api;
use crate::base_client::Client as BaseClient;
use crate::models::Room;
use crate::session::Session;
use crate::state::StateStore;
use crate::VERSION;
use crate::{Error, EventEmitter, Result};

const DEFAULT_SYNC_TIMEOUT: Duration = Duration::from_secs(30);

/// An async/await enabled Matrix client.
///
/// All of the state is held in an `Arc` so the `AsyncClient` can be cloned freely.
#[derive(Clone)]
pub struct AsyncClient {
    /// The URL of the homeserver to connect to.
    homeserver: Url,
    /// The underlying HTTP client.
    http_client: reqwest::Client,
    /// User session data.
    pub(crate) base_client: BaseClient,
}

impl std::fmt::Debug for AsyncClient {
    fn fmt(&self, fmt: &mut std::fmt::Formatter<'_>) -> StdResult<(), std::fmt::Error> {
        write!(fmt, "AsyncClient {{ homeserver: {} }}", self.homeserver)
    }
}

#[derive(Default)]
/// Configuration for the creation of the `AsyncClient`.
///
/// When setting the `StateStore` it is up to the user to open/connect
/// the storage backend before client creation.
///
/// # Example
///
/// ```
/// # use matrix_sdk::AsyncClientConfig;
/// // To pass all the request through mitmproxy set the proxy and disable SSL
/// // verification
/// let client_config = AsyncClientConfig::new()
///     .proxy("http://localhost:8080")
///     .unwrap()
///     .disable_ssl_verification();
/// ```
/// An example of adding a default `JsonStore` to the `AsyncClient`.
/// ```no_run
///  # use matrix_sdk::{AsyncClientConfig, JsonStore};
///
/// let store = JsonStore::open("path/to/json").unwrap();
/// let client_config = AsyncClientConfig::new()
///     .state_store(Box::new(store));
/// ```
pub struct AsyncClientConfig {
    proxy: Option<reqwest::Proxy>,
    user_agent: Option<HeaderValue>,
    disable_ssl_verification: bool,
    state_store: Option<Box<dyn StateStore>>,
}

impl std::fmt::Debug for AsyncClientConfig {
    fn fmt(&self, fmt: &mut std::fmt::Formatter<'_>) -> StdResult<(), std::fmt::Error> {
        fmt.debug_struct("AsyncClientConfig")
            .field("proxy", &self.proxy)
            .field("user_agent", &self.user_agent)
            .field("disable_ssl_verification", &self.disable_ssl_verification)
            .finish()
    }
}

impl AsyncClientConfig {
    /// Create a new default `AsyncClientConfig`.
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
    /// use matrix_sdk::AsyncClientConfig;
    ///
    /// let client_config = AsyncClientConfig::new()
    ///     .proxy("http://localhost:8080")
    ///     .unwrap();
    /// ```
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
        self.state_store = Some(store);
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

#[cfg(feature = "encryption")]
use api::r0::keys::{claim_keys, get_keys, upload_keys, KeyAlgorithm};
use api::r0::membership::join_room_by_id;
use api::r0::membership::join_room_by_id_or_alias;
use api::r0::membership::kick_user;
use api::r0::membership::leave_room;
use api::r0::membership::{
    invite_user::{self, InvitationRecipient},
    Invite3pid,
};
use api::r0::message::create_message_event;
use api::r0::message::get_message_events;
use api::r0::room::create_room;
use api::r0::session::login;
use api::r0::sync::sync_events;
#[cfg(feature = "encryption")]
use api::r0::to_device::send_event_to_device;

impl AsyncClient {
    /// Creates a new client for making HTTP requests to the given homeserver.
    ///
    /// # Arguments
    ///
    /// * `homeserver_url` - The homeserver that the client should connect to.
    /// * `session` - If a previous login exists, the access token can be
    ///     reused by giving a session object here.
    pub fn new<U: TryInto<Url>>(homeserver_url: U, session: Option<Session>) -> Result<Self> {
        let config = AsyncClientConfig::new();
        AsyncClient::new_with_config(homeserver_url, session, config)
    }

    /// Create a new client with the given configuration.
    ///
    /// # Arguments
    ///
    /// * `homeserver_url` - The homeserver that the client should connect to.
    /// * `session` - If a previous login exists, the access token can be
    ///     reused by giving a session object here.
    /// * `config` - Configuration for the client.
    pub fn new_with_config<U: TryInto<Url>>(
        homeserver_url: U,
        session: Option<Session>,
        config: AsyncClientConfig,
    ) -> Result<Self> {
        #[allow(clippy::match_wild_err_arm)]
        let homeserver: Url = match homeserver_url.try_into() {
            Ok(u) => u,
            Err(_e) => panic!("Error parsing homeserver url"),
        };

        let http_client = reqwest::Client::builder();

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

        let http_client = http_client.default_headers(headers).build()?;

        let base_client = if let Some(store) = config.state_store {
            BaseClient::new_with_state_store(session, store)?
        } else {
            BaseClient::new(session)?
        };

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

    /// Add `EventEmitter` to `AsyncClient`.
    ///
    /// The methods of `EventEmitter` are called when the respective `RoomEvents` occur.
    pub async fn add_event_emitter(&mut self, emitter: Box<dyn EventEmitter>) {
        self.base_client.add_event_emitter(emitter).await;
    }

    /// Returns an `Option` of the room name from a `RoomId`.
    ///
    /// This is a human readable room name.
    pub async fn get_room_name(&self, room_id: &RoomId) -> Option<String> {
        self.base_client.calculate_room_name(room_id).await
    }

    /// Returns a `Vec` of the room names this client knows about.
    ///
    /// This is a human readable list of room names.
    pub async fn get_room_names(&self) -> Vec<String> {
        self.base_client.calculate_room_names().await
    }

    /// Returns the joined rooms this client knows about.
    ///
    /// A `HashMap` of room id to `matrix::models::Room`
    pub fn joined_rooms(&self) -> Arc<RwLock<HashMap<RoomId, Arc<tokio::sync::RwLock<Room>>>>> {
        self.base_client.joined_rooms()
    }

    /// This allows `AsyncClient` to manually sync state with the provided `StateStore`.
    ///
    /// Returns true when a successful `StateStore` sync has completed.
    /// # Examples
    ///
    /// ```no_run
    /// use matrix_sdk::{AsyncClient, AsyncClientConfig, JsonStore, RoomBuilder};
    /// # use matrix_sdk::api::r0::room::Visibility;
    /// # use url::Url;
    ///
    /// # let homeserver = Url::parse("http://example.com").unwrap();
    /// let store = JsonStore::open("path/to/store").unwrap();
    /// let config = AsyncClientConfig::new().state_store(Box::new(store));
    /// let mut cli = AsyncClient::new(homeserver, None).unwrap();
    /// # use futures::executor::block_on;
    /// # block_on(async {
    /// let _ = cli.login("name", "password", None, None).await.unwrap();
    /// // returns true when a state store sync is successful
    /// assert!(cli.sync_with_state_store().await.unwrap());
    /// // now state is restored without a request to the server
    /// assert_eq!(vec!["room".to_string(), "names".to_string()], cli.get_room_names().await)
    /// # });
    /// ```
    pub async fn sync_with_state_store(&self) -> Result<bool> {
        self.base_client.sync_with_state_store().await
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
    pub async fn login<S: Into<String> + std::fmt::Debug>(
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

    /// Join a room by `RoomId`.
    ///
    /// Returns a `join_room_by_id::Response` consisting of the
    /// joined rooms `RoomId`.
    ///
    /// # Arguments
    ///
    /// * room_id - The `RoomId` of the room to be joined.
    pub async fn join_room_by_id(&mut self, room_id: &RoomId) -> Result<join_room_by_id::Response> {
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
    /// * alias - The `RoomId` or `RoomAliasId` of the room to be joined.
    /// An alias looks like this `#name:example.com`
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

    /// Kick a user out of the specified room.
    ///
    /// Returns a `kick_user::Response`, an empty response.
    ///
    /// # Arguments
    ///
    /// * room_id - The `RoomId` of the room the user should be kicked out of.
    ///
    /// * user_id - The `UserId` of the user that should be kicked out of the room.
    ///
    /// * reason - Optional reason why the room member is being kicked out.
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
    /// * room_id - The `RoomId` of the room to leave.
    ///
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
    /// * room_id - The `RoomId` of the room to invite the specified user to.
    ///
    /// * user_id - The `UserId` of the user to invite to the room.
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
    /// * room_id - The `RoomId` of the room to invite the specified user to.
    ///
    /// * invite_id - A third party id of a user to invite to the room.
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
    /// * room - The easiest way to create this request is using the `RoomBuilder`.
    ///
    /// # Examples
    /// ```no_run
    /// use matrix_sdk::{AsyncClient, RoomBuilder};
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
    /// let mut cli = AsyncClient::new(homeserver, None).unwrap();
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
    /// * request - The easiest way to create a `Request` is using the
    /// `MessagesRequestBuilder`.
    ///
    /// # Examples
    /// ```no_run
    /// # use std::convert::TryFrom;
    /// use matrix_sdk::{AsyncClient, MessagesRequestBuilder};
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
    /// let mut cli = AsyncClient::new(homeserver, None).unwrap();
    /// # use futures::executor::block_on;
    /// # block_on(async {
    /// assert!(cli.room_messages(builder).await.is_ok());
    /// # });
    /// ```
    pub async fn room_messages<R: Into<get_message_events::Request>>(
        &self,
        request: R,
    ) -> Result<get_message_events::Response> {
        let req = request.into();
        self.send(req).await
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
    pub async fn sync(&self, mut sync_settings: SyncSettings) -> Result<sync_events::Response> {
        {
            // if the client has been synced from the state store don't sync again
            if !self.base_client.is_state_store_synced() {
                // this will bail out returning false if the store has not been set up
                if let Ok(synced) = self.sync_with_state_store().await {
                    if synced {
                        // once synced, update the sync token to the last known state from `StateStore`.
                        sync_settings.token = self.sync_token().await;
                    }
                }
            }
        }

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
    /// # use matrix_sdk::{AsyncClient, SyncSettings};
    /// # use url::Url;
    /// # use futures::executor::block_on;
    /// # block_on(async {
    /// # let homeserver = Url::parse("http://localhost:8080").unwrap();
    /// # let mut client = AsyncClient::new(homeserver, None).unwrap();
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
        callback: impl Fn(sync_events::Response) -> C + Send,
    ) where
        C: Future<Output = ()>,
    {
        let mut sync_settings = sync_settings;
        let mut last_sync_time: Option<Instant> = None;

        loop {
            let response = self.sync(sync_settings.clone()).await;

            let response = if let Ok(r) = response {
                r
            } else {
                sleep(Duration::from_secs(1)).await;
                continue;
            };

            callback(response).await;

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

            let now = Instant::now();

            // If the last sync happened less than a second ago, sleep for a
            // while to not hammer out requests if the server doesn't respect
            // the sync timeout.
            if let Some(t) = last_sync_time {
                if now - t <= Duration::from_secs(1) {
                    sleep(Duration::from_secs(1)).await;
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

    async fn send<Request: Endpoint<ResponseError = crate::api::Error> + std::fmt::Debug>(
        &self,
        request: Request,
    ) -> Result<Request::Response> {
        let request: http::Request<Vec<u8>> = request.try_into()?;
        let url = request.uri();
        let path_and_query = url.path_and_query().unwrap();
        let mut url = self.homeserver.clone();

        url.set_path(path_and_query.path());
        url.set_query(path_and_query.query());

        trace!("Doing request {:?}", url);

        let request_builder = match Request::METADATA.method {
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
            HttpMethod::DELETE => unimplemented!(),
            _ => panic!("Unsuported method"),
        };

        let request_builder = if Request::METADATA.requires_authentication {
            let session = self.base_client.session().read().await;

            if let Some(session) = session.as_ref() {
                request_builder.bearer_auth(&session.access_token)
            } else {
                return Err(Error::AuthenticationRequired);
            }
        } else {
            request_builder
        };
        let mut response = request_builder.send().await?;

        trace!("Got response: {:?}", response);

        let status = response.status();
        let mut http_builder = HttpResponse::builder().status(status);
        let headers = http_builder.headers_mut().unwrap();

        for (k, v) in response.headers_mut().drain() {
            if let Some(key) = k {
                headers.insert(key, v);
            }
        }
        let body = response.bytes().await?.as_ref().to_owned();
        let http_response = http_builder.body(body).unwrap();

        Ok(<Request::Response>::try_from(http_response)?)
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
    /// # use matrix_sdk::{AsyncClient, SyncSettings};
    /// # use url::Url;
    /// # use futures::executor::block_on;
    /// # use ruma_identifiers::RoomId;
    /// # use std::convert::TryFrom;
    /// use matrix_sdk::events::room::message::{MessageEventContent, TextMessageEventContent};
    /// # block_on(async {
    /// # let homeserver = Url::parse("http://localhost:8080").unwrap();
    /// # let mut client = AsyncClient::new(homeserver, None).unwrap();
    /// # let room_id = RoomId::try_from("!test:localhost").unwrap();
    /// use uuid::Uuid;
    ///
    /// let content = MessageEventContent::Text(TextMessageEventContent {
    ///     body: "Hello world".to_owned(),
    ///     format: None,
    ///     formatted_body: None,
    ///     relates_to: None,
    /// });
    /// let txn_id = Uuid::new_v4();
    /// client.room_send(&room_id, content, Some(txn_id)).await.unwrap();
    /// })
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
                    self.share_group_session(room_id).await?;
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
    use super::{AsyncClient, Url};
    use crate::events::collections::all::RoomEvent;
    use crate::identifiers::{RoomId, UserId};

    use crate::test_builder::EventBuilder;

    use mockito::mock;
    use std::convert::TryFrom;
    use std::str::FromStr;

    #[tokio::test]
    async fn client_runner() {
        let session = crate::Session {
            access_token: "12345".to_owned(),
            user_id: UserId::try_from("@example:localhost").unwrap(),
            device_id: "DEVICEID".to_owned(),
        };
        let homeserver = url::Url::parse(&mockito::server_url()).unwrap();
        let client = AsyncClient::new(homeserver, Some(session)).unwrap();

        let rid = RoomId::try_from("!roomid:room.com").unwrap();
        let uid = UserId::try_from("@example:localhost").unwrap();

        let mut bld = EventBuilder::default()
            .add_room_event_from_file("../test_data/events/member.json", RoomEvent::RoomMember)
            .add_room_event_from_file(
                "../test_data/events/power_levels.json",
                RoomEvent::RoomPowerLevels,
            )
            .build_client_runner(rid, uid);

        let cli = bld.set_client(client).to_client().await;

        assert_eq!(
            cli.homeserver(),
            &Url::parse(&mockito::server_url()).unwrap()
        );
    }

    #[tokio::test]
    async fn mock_runner() {
        use std::convert::TryFrom;

        let session = crate::Session {
            access_token: "12345".to_owned(),
            user_id: UserId::try_from("@example:localhost").unwrap(),
            device_id: "DEVICEID".to_owned(),
        };

        let homeserver = url::Url::parse(&mockito::server_url()).unwrap();
        let client = AsyncClient::new(homeserver, Some(session)).unwrap();

        let mut bld = EventBuilder::default()
            .add_room_event_from_file("../test_data/events/member.json", RoomEvent::RoomMember)
            .add_room_event_from_file(
                "../test_data/events/power_levels.json",
                RoomEvent::RoomPowerLevels,
            )
            .build_mock_runner(
                "GET",
                mockito::Matcher::Regex(r"^/_matrix/client/r0/sync\?.*$".to_string()),
            );

        let cli = bld.set_client(client).to_client().await.unwrap();

        assert_eq!(
            cli.homeserver(),
            &Url::parse(&mockito::server_url()).unwrap()
        );
    }

    #[tokio::test]
    async fn login_error() {
        let homeserver = Url::from_str(&mockito::server_url()).unwrap();

        let _m = mock("POST", "/_matrix/client/r0/login")
            .with_status(403)
            .with_body_from_file("../test_data/login_response_error.json")
            .create();

        let client = AsyncClient::new(homeserver, None).unwrap();

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
}
