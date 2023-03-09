// Copyright 2020 Damir Jelić
// Copyright 2020 The Matrix.org Foundation C.I.C.
// Copyright 2022 Famedly GmbH
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

use std::{
    collections::BTreeMap,
    fmt::{self, Debug},
    future::Future,
    pin::Pin,
    sync::Arc,
};

#[cfg(target_arch = "wasm32")]
use async_once_cell::OnceCell;
use dashmap::DashMap;
use eyeball::Observable;
use futures_core::Stream;
use futures_util::StreamExt;
use matrix_sdk_base::{
    store::DynStateStore, BaseClient, RoomType, SendOutsideWasm, Session, SessionMeta,
    SessionTokens, SyncOutsideWasm,
};
use matrix_sdk_common::{
    instant::Instant,
    locks::{Mutex, RwLock, RwLockReadGuard},
};
#[cfg(feature = "appservice")]
use ruma::TransactionId;
use ruma::{
    api::{
        client::{
            account::{register, whoami},
            alias::get_alias,
            device::{delete_devices, get_devices, update_device},
            directory::{get_public_rooms, get_public_rooms_filtered},
            discovery::{
                get_capabilities::{self, Capabilities},
                get_supported_versions,
            },
            error::ErrorKind,
            filter::{create_filter::v3::Request as FilterUploadRequest, FilterDefinition},
            membership::{join_room_by_id, join_room_by_id_or_alias},
            push::get_notifications::v3::Notification,
            room::create_room,
            session::{
                get_login_types, login, logout, refresh_token, sso_login, sso_login_with_provider,
            },
            sync::sync_events,
            uiaa::{AuthData, UserIdentifier},
        },
        error::FromHttpResponseError,
        MatrixVersion, OutgoingRequest, SendAccessToken,
    },
    assign,
    serde::JsonObject,
    DeviceId, OwnedDeviceId, OwnedRoomId, OwnedServerName, RoomAliasId, RoomId, RoomOrAliasId,
    ServerName, UInt, UserId,
};
use serde::de::DeserializeOwned;
use tokio::sync::broadcast;
#[cfg(not(target_arch = "wasm32"))]
use tokio::sync::OnceCell;
#[cfg(feature = "e2e-encryption")]
use tracing::error;
use tracing::{debug, field::display, info, instrument, trace, Instrument, Span};
use url::Url;

#[cfg(feature = "e2e-encryption")]
use crate::encryption::Encryption;
use crate::{
    config::RequestConfig,
    error::{HttpError, HttpResult},
    event_handler::{
        EventHandler, EventHandlerDropGuard, EventHandlerHandle, EventHandlerStore, SyncEvent,
    },
    http_client::HttpClient,
    room,
    sync::SyncResponse,
    Account, Error, Media, RefreshTokenError, Result, RumaApiError,
};

mod builder;
mod login_builder;

#[cfg(feature = "sso-login")]
pub use self::login_builder::SsoLoginBuilder;
pub use self::{
    builder::{ClientBuildError, ClientBuilder},
    login_builder::LoginBuilder,
};

#[cfg(not(target_arch = "wasm32"))]
type NotificationHandlerFut = Pin<Box<dyn Future<Output = ()> + Send>>;
#[cfg(target_arch = "wasm32")]
type NotificationHandlerFut = Pin<Box<dyn Future<Output = ()>>>;

#[cfg(not(target_arch = "wasm32"))]
type NotificationHandlerFn =
    Box<dyn Fn(Notification, room::Room, Client) -> NotificationHandlerFut + Send + Sync>;
#[cfg(target_arch = "wasm32")]
type NotificationHandlerFn =
    Box<dyn Fn(Notification, room::Room, Client) -> NotificationHandlerFut>;

/// Enum controlling if a loop running callbacks should continue or abort.
///
/// This is mainly used in the [`sync_with_callback`] method, the return value
/// of the provided callback controls if the sync loop should be exited.
///
/// [`sync_with_callback`]: #method.sync_with_callback
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoopCtrl {
    /// Continue running the loop.
    Continue,
    /// Break out of the loop.
    Break,
}

/// Wrapper struct for ErrorKind::UnknownToken
#[derive(Debug, Clone)]
pub struct UnknownToken {
    /// Whether or not the session was soft logged out
    pub soft_logout: bool,
}

/// An async/await enabled Matrix client.
///
/// All of the state is held in an `Arc` so the `Client` can be cloned freely.
#[derive(Clone)]
pub struct Client {
    pub(crate) inner: Arc<ClientInner>,
}

pub(crate) struct ClientInner {
    /// The URL of the homeserver to connect to.
    homeserver: RwLock<Url>,
    /// The OIDC Provider that is trusted by the homeserver.
    authentication_issuer: Option<RwLock<Url>>,
    /// The sliding sync proxy that is trusted by the homeserver.
    #[cfg(feature = "experimental-sliding-sync")]
    sliding_sync_proxy: Option<RwLock<Url>>,
    /// The underlying HTTP client.
    http_client: HttpClient,
    /// User session data.
    base_client: BaseClient,
    /// The Matrix versions the server supports (well-known ones only)
    server_versions: OnceCell<Box<[MatrixVersion]>>,
    /// Locks making sure we only have one group session sharing request in
    /// flight per room.
    #[cfg(feature = "e2e-encryption")]
    pub(crate) group_session_locks: Mutex<BTreeMap<OwnedRoomId, Arc<Mutex<()>>>>,
    /// Lock making sure we're only doing one key claim request at a time.
    #[cfg(feature = "e2e-encryption")]
    pub(crate) key_claim_lock: Mutex<()>,
    pub(crate) members_request_locks: Mutex<BTreeMap<OwnedRoomId, Arc<Mutex<()>>>>,
    /// Locks for requests on the encryption state of rooms.
    pub(crate) encryption_state_request_locks: DashMap<OwnedRoomId, Arc<Mutex<()>>>,
    pub(crate) typing_notice_times: DashMap<OwnedRoomId, Instant>,
    /// Event handlers. See `add_event_handler`.
    pub(crate) event_handlers: EventHandlerStore,
    /// Notification handlers. See `register_notification_handler`.
    notification_handlers: RwLock<Vec<NotificationHandlerFn>>,
    /// Whether the client should operate in application service style mode.
    /// This is low-level functionality. For an high-level API check the
    /// `matrix_sdk_appservice` crate.
    appservice_mode: bool,
    /// Whether the client should update its homeserver URL with the discovery
    /// information present in the login response.
    respect_login_well_known: bool,
    /// Whether to try to refresh the access token automatically when an
    /// `M_UNKNOWN_TOKEN` error is encountered.
    handle_refresh_tokens: bool,
    /// Lock making sure we're only doing one token refresh at a time.
    refresh_token_lock: Mutex<Result<(), RefreshTokenError>>,
    /// An event that can be listened on to wait for a successful sync. The
    /// event will only be fired if a sync loop is running. Can be used for
    /// synchronization, e.g. if we send out a request to create a room, we can
    /// wait for the sync to get the data to fetch a room object from the state
    /// store.
    pub(crate) sync_beat: event_listener::Event,
    /// Client API UnknownToken error publisher. Allows the subscriber logout
    /// the user when any request fails because of an invalid access token
    pub(crate) unknown_token_error_sender: broadcast::Sender<UnknownToken>,
    /// Root span for `tracing`.
    pub(crate) root_span: Span,
}

#[cfg(not(tarpaulin_include))]
impl Debug for Client {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> Result<(), fmt::Error> {
        write!(fmt, "Client")
    }
}

impl Client {
    /// Create a new [`Client`] that will use the given homeserver.
    ///
    /// # Arguments
    ///
    /// * `homeserver_url` - The homeserver that the client should connect to.
    pub async fn new(homeserver_url: Url) -> Result<Self, HttpError> {
        Self::builder()
            .homeserver_url(homeserver_url)
            .build()
            .await
            .map_err(ClientBuildError::assert_valid_builder_args)
    }

    /// Create a new [`ClientBuilder`].
    pub fn builder() -> ClientBuilder {
        ClientBuilder::new()
    }

    pub(crate) fn base_client(&self) -> &BaseClient {
        &self.inner.base_client
    }

    /// Change the homeserver URL used by this client.
    ///
    /// # Arguments
    ///
    /// * `homeserver_url` - The new URL to use.
    async fn set_homeserver(&self, homeserver_url: Url) {
        let mut homeserver = self.inner.homeserver.write().await;
        *homeserver = homeserver_url;
    }

    /// Get the capabilities of the homeserver.
    ///
    /// This method should be used to check what features are supported by the
    /// homeserver.
    ///
    /// # Example
    /// ```no_run
    /// # use futures::executor::block_on;
    /// # use matrix_sdk::Client;
    /// # use url::Url;
    /// # block_on(async {
    /// # let homeserver = Url::parse("http://example.com")?;
    /// let client = Client::new(homeserver).await?;
    ///
    /// let capabilities = client.get_capabilities().await?;
    ///
    /// if capabilities.change_password.enabled {
    ///     // Change password
    /// }
    ///
    /// # anyhow::Ok(()) });
    /// ```
    pub async fn get_capabilities(&self) -> HttpResult<Capabilities> {
        let res = self.send(get_capabilities::v3::Request::new(), None).await?;
        Ok(res.capabilities)
    }

    /// Process a [transaction] received from the homeserver which has been
    /// converted into a sync response.
    ///
    /// # Arguments
    ///
    /// * `transaction_id` - The id of the transaction, used to guard against
    ///   the same transaction being sent twice. This guarding currently isn't
    ///   implemented.
    /// * `sync_response` - The sync response converted from a transaction
    ///   received from the homeserver.
    ///
    /// [transaction]: https://matrix.org/docs/spec/application_service/r0.1.2#put-matrix-app-v1-transactions-txnid
    #[cfg(feature = "appservice")]
    pub async fn receive_transaction(
        &self,
        transaction_id: &TransactionId,
        sync_response: sync_events::v3::Response,
    ) -> Result<()> {
        const TXN_ID_KEY: &[u8] = b"appservice.txn_id";

        let store = self.store();
        let store_tokens = store.get_custom_value(TXN_ID_KEY).await?;
        let mut txn_id_bytes = transaction_id.as_bytes().to_vec();
        if let Some(mut store_tokens) = store_tokens {
            // The data is separated by a NULL byte.
            let mut store_tokens_split = store_tokens.split(|x| *x == b'\0');
            if store_tokens_split.any(|x| x == transaction_id.as_bytes()) {
                // We already encountered this transaction id before, so we exit early instead
                // of processing further.
                //
                // Spec: https://spec.matrix.org/v1.3/application-service-api/#pushing-events
                return Ok(());
            }
            store_tokens.push(b'\0');
            store_tokens.append(&mut txn_id_bytes);
            self.store().set_custom_value(TXN_ID_KEY, store_tokens).await?;
        } else {
            self.store().set_custom_value(TXN_ID_KEY, txn_id_bytes).await?;
        }
        self.process_sync(sync_response).await?;

        Ok(())
    }

    /// Get a copy of the default request config.
    ///
    /// The default request config is what's used when sending requests if no
    /// `RequestConfig` is explicitly passed to [`send`][Self::send] or another
    /// function with such a parameter.
    ///
    /// If the default request config was not customized through
    /// [`ClientBuilder`] when creating this `Client`, the returned value will
    /// be equivalent to [`RequestConfig::default()`].
    pub fn request_config(&self) -> RequestConfig {
        self.inner.http_client.request_config
    }

    /// Is the client logged in.
    pub fn logged_in(&self) -> bool {
        self.inner.base_client.logged_in()
    }

    /// The Homeserver of the client.
    pub async fn homeserver(&self) -> Url {
        self.inner.homeserver.read().await.clone()
    }

    /// The OIDC Provider that is trusted by the homeserver.
    pub async fn authentication_issuer(&self) -> Option<Url> {
        let server = self.inner.authentication_issuer.as_ref()?;
        Some(server.read().await.clone())
    }

    /// The sliding sync proxy that is trusted by the homeserver.
    #[cfg(feature = "experimental-sliding-sync")]
    pub async fn sliding_sync_proxy(&self) -> Option<Url> {
        let server = self.inner.sliding_sync_proxy.as_ref()?;
        Some(server.read().await.clone())
    }

    fn session_meta(&self) -> Option<&SessionMeta> {
        self.base_client().session_meta()
    }

    /// Get the user id of the current owner of the client.
    pub fn user_id(&self) -> Option<&UserId> {
        self.session_meta().map(|s| s.user_id.as_ref())
    }

    /// Get the device ID that identifies the current session.
    pub fn device_id(&self) -> Option<&DeviceId> {
        self.session_meta().map(|s| s.device_id.as_ref())
    }

    /// Get the current access token and optional refresh token for this
    /// session.
    ///
    /// Will be `None` if the client has not been logged in.
    ///
    /// After login, the tokens should only change if support for [refreshing
    /// access tokens] has been enabled.
    ///
    /// [refreshing access tokens]: https://spec.matrix.org/v1.3/client-server-api/#refreshing-access-tokens
    pub fn session_tokens(&self) -> Option<SessionTokens> {
        self.base_client().session_tokens().clone()
    }

    /// Get the current access token for this session.
    ///
    /// Will be `None` if the client has not been logged in.
    ///
    /// After login, this token should only change if support for [refreshing
    /// access tokens] has been enabled.
    ///
    /// [refreshing access tokens]: https://spec.matrix.org/v1.3/client-server-api/#refreshing-access-tokens
    pub fn access_token(&self) -> Option<String> {
        self.session_tokens().map(|tokens| tokens.access_token)
    }

    /// Get the current refresh token for this session.
    ///
    /// Will be `None` if the client has not been logged in, or if the access
    /// token doesn't expire.
    ///
    /// After login, this token should only change if support for [refreshing
    /// access tokens] has been enabled.
    ///
    /// [refreshing access tokens]: https://spec.matrix.org/v1.3/client-server-api/#refreshing-access-tokens
    pub fn refresh_token(&self) -> Option<String> {
        self.session_tokens().and_then(|tokens| tokens.refresh_token)
    }

    /// [`Stream`] to get notified when the current access token and optional
    /// refresh token for this session change.
    ///
    /// This can be used with [`Client::session()`] to persist the [`Session`]
    /// when the tokens change.
    ///
    /// After login, the tokens should only change if support for [refreshing
    /// access tokens] has been enabled.
    ///
    /// # Example
    ///
    /// ```no_run
    /// use futures_util::StreamExt;
    /// use matrix_sdk::Client;
    /// # use matrix_sdk::Session;
    /// # use futures::executor::block_on;
    /// # block_on(async {
    /// # fn persist_session(_: Option<Session>) {};
    ///
    /// let homeserver = "http://example.com";
    /// let client = Client::builder()
    ///     .homeserver_url(homeserver)
    ///     .handle_refresh_tokens()
    ///     .build()
    ///     .await?;
    ///
    /// let response = client
    ///     .login_username("user", "wordpass")
    ///     .initial_device_display_name("My App")
    ///     .request_refresh_token()
    ///     .send()
    ///     .await?;
    ///
    /// persist_session(client.session());
    ///
    /// // Handle when at least one of the tokens changed.
    /// let future = client.session_tokens_changed_stream().for_each(move |_| {
    ///     let client = client.clone();
    ///     async move {
    ///         persist_session(client.session());
    ///     }
    /// });
    ///
    /// tokio::spawn(future);
    ///
    /// # anyhow::Ok(()) });
    /// ```
    ///
    /// [refreshing access tokens]: https://spec.matrix.org/v1.3/client-server-api/#refreshing-access-tokens
    pub fn session_tokens_changed_stream(&self) -> impl Stream<Item = ()> {
        self.session_tokens_stream().map(|_| ())
    }

    /// Get changes to the access token and optional refresh token for this
    /// session as a [`Stream`].
    ///
    /// The value will be `None` if the client has not been logged in.
    ///
    /// After login, the tokens should only change if support for [refreshing
    /// access tokens] has been enabled.
    ///
    /// # Example
    ///
    /// ```no_run
    /// use futures::StreamExt;
    /// use matrix_sdk::Client;
    /// # use matrix_sdk::Session;
    /// # use futures::executor::block_on;
    /// # block_on(async {
    /// # fn persist_session(_: &Session) {};
    ///
    /// let homeserver = "http://example.com";
    /// let client = Client::builder()
    ///     .homeserver_url(homeserver)
    ///     .handle_refresh_tokens()
    ///     .build()
    ///     .await?;
    ///
    /// client
    ///     .login_username("user", "wordpass")
    ///     .initial_device_display_name("My App")
    ///     .request_refresh_token()
    ///     .send()
    ///     .await?;
    ///
    /// let mut session = client.session().expect("Client should be logged in");
    /// persist_session(&session);
    ///
    /// // Handle when at least one of the tokens changed.
    /// let mut tokens_stream = client.session_tokens_stream();
    /// loop {
    ///     if let Some(tokens) = tokens_stream.next().await.flatten() {
    ///         session.access_token = tokens.access_token;
    ///
    ///         if let Some(refresh_token) = tokens.refresh_token {
    ///             session.refresh_token = Some(refresh_token);
    ///         }
    ///
    ///         persist_session(&session);
    ///     }
    /// }
    ///
    /// # anyhow::Ok(()) });
    /// ```
    ///
    /// [refreshing access tokens]: https://spec.matrix.org/v1.3/client-server-api/#refreshing-access-tokens
    pub fn session_tokens_stream(&self) -> impl Stream<Item = Option<SessionTokens>> {
        Observable::subscribe(&self.base_client().session_tokens())
    }

    /// Get the whole session info of this client.
    ///
    /// Will be `None` if the client has not been logged in.
    ///
    /// Can be used with [`Client::restore_session`] to restore a previously
    /// logged-in session.
    pub fn session(&self) -> Option<Session> {
        self.base_client().session()
    }

    /// Get a reference to the state store.
    pub fn store(&self) -> &DynStateStore {
        self.base_client().store()
    }

    /// Get the account of the current owner of the client.
    pub fn account(&self) -> Account {
        Account::new(self.clone())
    }

    /// Get the encryption manager of the client.
    #[cfg(feature = "e2e-encryption")]
    pub fn encryption(&self) -> Encryption {
        Encryption::new(self.clone())
    }

    /// Get the media manager of the client.
    pub fn media(&self) -> Media {
        Media::new(self.clone())
    }

    /// Register a handler for a specific event type.
    ///
    /// The handler is a function or closure with one or more arguments. The
    /// first argument is the event itself. All additional arguments are
    /// "context" arguments: They have to implement [`EventHandlerContext`].
    /// This trait is named that way because most of the types implementing it
    /// give additional context about an event: The room it was in, its raw form
    /// and other similar things. As two exceptions to this,
    /// [`Client`] and [`EventHandlerHandle`] also implement the
    /// `EventHandlerContext` trait so you don't have to clone your client
    /// into the event handler manually and a handler can decide to remove
    /// itself.
    ///
    /// Some context arguments are not universally applicable. A context
    /// argument that isn't available for the given event type will result in
    /// the event handler being skipped and an error being logged. The following
    /// context argument types are only available for a subset of event types:
    ///
    /// * [`Room`][room::Room] is only available for room-specific events, i.e.
    ///   not for events like global account data events or presence events.
    ///
    /// You can provide custom context via
    /// [`add_event_handler_context`](Client::add_event_handler_context) and
    /// then use [`Ctx<T>`](crate::event_handler::Ctx) to extract the context
    /// into the event handler.
    ///
    /// [`EventHandlerContext`]: crate::event_handler::EventHandlerContext
    ///
    /// # Examples
    ///
    /// ```
    /// # use futures::executor::block_on;
    /// # use url::Url;
    /// # let homeserver = Url::parse("http://localhost:8080").unwrap();
    /// use matrix_sdk::{
    ///     deserialized_responses::EncryptionInfo,
    ///     event_handler::Ctx,
    ///     room::Room,
    ///     ruma::{
    ///         events::{
    ///             macros::EventContent,
    ///             push_rules::PushRulesEvent,
    ///             room::{message::SyncRoomMessageEvent, topic::SyncRoomTopicEvent},
    ///         },
    ///         Int, MilliSecondsSinceUnixEpoch,
    ///     },
    ///     Client,
    /// };
    /// use serde::{Deserialize, Serialize};
    ///
    /// # block_on(async {
    /// # let client = matrix_sdk::Client::builder()
    /// #     .homeserver_url(homeserver)
    /// #     .server_versions([ruma::api::MatrixVersion::V1_0])
    /// #     .build()
    /// #     .await
    /// #     .unwrap();
    ///
    /// client.add_event_handler(
    ///     |ev: SyncRoomMessageEvent, room: Room, client: Client| async move {
    ///         // Common usage: Room event plus room and client.
    ///     },
    /// );
    /// client.add_event_handler(
    ///     |ev: SyncRoomMessageEvent, room: Room, encryption_info: Option<EncryptionInfo>| {
    ///         async move {
    ///             // An `Option<EncryptionInfo>` parameter lets you distinguish between
    ///             // unencrypted events and events that were decrypted by the SDK.
    ///         }
    ///     },
    /// );
    /// client.add_event_handler(|ev: SyncRoomTopicEvent| async move {
    ///     // You can omit any or all arguments after the first.
    /// });
    ///
    /// // Registering a temporary event handler:
    /// let handle = client.add_event_handler(|ev: SyncRoomMessageEvent| async move {
    ///     /* Event handler */
    /// });
    /// client.remove_event_handler(handle);
    ///
    /// // Registering custom event handler context:
    /// #[derive(Debug, Clone)] // The context will be cloned for event handler.
    /// struct MyContext {
    ///     number: usize,
    /// }
    /// client.add_event_handler_context(MyContext { number: 5 });
    /// client.add_event_handler(|ev: SyncRoomMessageEvent, context: Ctx<MyContext>| async move {
    ///     // Use the context
    /// });
    ///
    /// // Custom events work exactly the same way, you just need to declare
    /// // the content struct and use the EventContent derive macro on it.
    /// #[derive(Clone, Debug, Deserialize, Serialize, EventContent)]
    /// #[ruma_event(type = "org.shiny_new_2fa.token", kind = MessageLike)]
    /// struct TokenEventContent {
    ///     token: String,
    ///     #[serde(rename = "exp")]
    ///     expires_at: MilliSecondsSinceUnixEpoch,
    /// }
    ///
    /// client.add_event_handler(|ev: SyncTokenEvent, room: Room| async move {
    ///     todo!("Display the token");
    /// });
    ///
    /// // Adding your custom data to the handler can be done as well
    /// let data = "MyCustomIdentifier".to_owned();
    ///
    /// client.add_event_handler({
    ///     let data = data.clone();
    ///     move |ev: SyncRoomMessageEvent | {
    ///         let data = data.clone();
    ///         async move {
    ///             println!("Calling the handler with identifier {data}");
    ///         }
    ///     }
    /// });
    /// # });
    /// ```
    pub fn add_event_handler<Ev, Ctx, H>(&self, handler: H) -> EventHandlerHandle
    where
        Ev: SyncEvent + DeserializeOwned + Send + 'static,
        H: EventHandler<Ev, Ctx>,
    {
        self.add_event_handler_impl(handler, None)
    }

    /// Register a handler for a specific room, and event type.
    ///
    /// This method works the same way as
    /// [`add_event_handler`][Self::add_event_handler], except that the handler
    /// will only be called for events in the room with the specified ID. See
    /// that method for more details on event handler functions.
    ///
    /// `client.add_room_event_handler(room_id, hdl)` is equivalent to
    /// `room.add_event_handler(hdl)`. Use whichever one is more convenient in
    /// your use case.
    pub fn add_room_event_handler<Ev, Ctx, H>(
        &self,
        room_id: &RoomId,
        handler: H,
    ) -> EventHandlerHandle
    where
        Ev: SyncEvent + DeserializeOwned + Send + 'static,
        H: EventHandler<Ev, Ctx>,
    {
        self.add_event_handler_impl(handler, Some(room_id.to_owned()))
    }

    /// Remove the event handler associated with the handle.
    ///
    /// Note that you **must not** call `remove_event_handler` from the
    /// non-async part of an event handler, that is:
    ///
    /// ```ignore
    /// client.add_event_handler(|ev: SomeEvent, client: Client, handle: EventHandlerHandle| {
    ///     // ⚠ this will cause a deadlock ⚠
    ///     client.remove_event_handler(handle);
    ///
    ///     async move {
    ///         // removing the event handler here is fine
    ///         client.remove_event_handler(handle);
    ///     }
    /// })
    /// ```
    ///
    /// Note also that handlers that remove themselves will still execute with
    /// events received in the same sync cycle.
    ///
    /// # Arguments
    ///
    /// `handle` - The [`EventHandlerHandle`] that is returned when
    /// registering the event handler with [`Client::add_event_handler`].
    ///
    /// # Examples
    ///
    /// ```
    /// # use futures::executor::block_on;
    /// # use url::Url;
    /// # use tokio::sync::mpsc;
    /// #
    /// # let homeserver = Url::parse("http://localhost:8080").unwrap();
    /// #
    /// use matrix_sdk::{
    ///     event_handler::EventHandlerHandle,
    ///     ruma::events::room::member::SyncRoomMemberEvent, Client,
    /// };
    /// #
    /// # block_on(async {
    /// # let client = matrix_sdk::Client::builder()
    /// #     .homeserver_url(homeserver)
    /// #     .server_versions([ruma::api::MatrixVersion::V1_0])
    /// #     .build()
    /// #     .await
    /// #     .unwrap();
    ///
    /// client.add_event_handler(
    ///     |ev: SyncRoomMemberEvent,
    ///      client: Client,
    ///      handle: EventHandlerHandle| async move {
    ///         // Common usage: Check arriving Event is the expected one
    ///         println!("Expected RoomMemberEvent received!");
    ///         client.remove_event_handler(handle);
    ///     },
    /// );
    /// # });
    /// ```
    pub fn remove_event_handler(&self, handle: EventHandlerHandle) {
        self.inner.event_handlers.remove(handle);
    }

    /// Create an [`EventHandlerDropGuard`] for the event handler identified by
    /// the given handle.
    ///
    /// When the returned value is dropped, the event handler will be removed.
    pub fn event_handler_drop_guard(&self, handle: EventHandlerHandle) -> EventHandlerDropGuard {
        EventHandlerDropGuard::new(handle, self.clone())
    }

    /// Add an arbitrary value for use as event handler context.
    ///
    /// The value can be obtained in an event handler by adding an argument of
    /// the type [`Ctx<T>`][crate::event_handler::Ctx].
    ///
    /// If a value of the same type has been added before, it will be
    /// overwritten.
    ///
    /// # Example
    ///
    /// ```
    /// # use futures::executor::block_on;
    /// use matrix_sdk::{
    ///     event_handler::Ctx, room::Room,
    ///     ruma::events::room::message::SyncRoomMessageEvent,
    /// };
    /// # #[derive(Clone)]
    /// # struct SomeType;
    /// # fn obtain_gui_handle() -> SomeType { SomeType }
    /// # let homeserver = url::Url::parse("http://localhost:8080").unwrap();
    /// # block_on(async {
    /// # let client = matrix_sdk::Client::builder()
    /// #     .homeserver_url(homeserver)
    /// #     .server_versions([ruma::api::MatrixVersion::V1_0])
    /// #     .build()
    /// #     .await
    /// #     .unwrap();
    ///
    /// // Handle used to send messages to the UI part of the app
    /// let my_gui_handle: SomeType = obtain_gui_handle();
    ///
    /// client.add_event_handler_context(my_gui_handle.clone());
    /// client.add_event_handler(
    ///     |ev: SyncRoomMessageEvent, room: Room, gui_handle: Ctx<SomeType>| {
    ///         async move {
    ///             // gui_handle.send(DisplayMessage { message: ev });
    ///         }
    ///     },
    /// );
    /// # });
    /// ```
    pub fn add_event_handler_context<T>(&self, ctx: T)
    where
        T: Clone + Send + Sync + 'static,
    {
        self.inner.event_handlers.add_context(ctx);
    }

    /// Register a handler for a notification.
    ///
    /// Similar to [`Client::add_event_handler`], but only allows functions
    /// or closures with exactly the three arguments [`Notification`],
    /// [`room::Room`], [`Client`] for now.
    pub async fn register_notification_handler<H, Fut>(&self, handler: H) -> &Self
    where
        H: Fn(Notification, room::Room, Client) -> Fut
            + SendOutsideWasm
            + SyncOutsideWasm
            + 'static,
        Fut: Future<Output = ()> + SendOutsideWasm + 'static,
    {
        self.inner.notification_handlers.write().await.push(Box::new(
            move |notification, room, client| Box::pin((handler)(notification, room, client)),
        ));

        self
    }

    pub(crate) async fn notification_handlers(
        &self,
    ) -> RwLockReadGuard<'_, Vec<NotificationHandlerFn>> {
        self.inner.notification_handlers.read().await
    }

    /// Get all the rooms the client knows about.
    ///
    /// This will return the list of joined, invited, and left rooms.
    pub fn rooms(&self) -> Vec<room::Room> {
        self.base_client()
            .get_rooms()
            .into_iter()
            .map(|room| room::Common::new(self.clone(), room).into())
            .collect()
    }

    /// Returns the joined rooms this client knows about.
    pub fn joined_rooms(&self) -> Vec<room::Joined> {
        self.base_client()
            .get_rooms()
            .into_iter()
            .filter_map(|room| room::Joined::new(self, room))
            .collect()
    }

    /// Returns the invited rooms this client knows about.
    pub fn invited_rooms(&self) -> Vec<room::Invited> {
        self.base_client()
            .get_stripped_rooms()
            .into_iter()
            .filter_map(|room| room::Invited::new(self, room))
            .collect()
    }

    /// Returns the left rooms this client knows about.
    pub fn left_rooms(&self) -> Vec<room::Left> {
        self.base_client()
            .get_rooms()
            .into_iter()
            .filter_map(|room| room::Left::new(self, room))
            .collect()
    }

    /// Get a room with the given room id.
    ///
    /// # Arguments
    ///
    /// `room_id` - The unique id of the room that should be fetched.
    pub fn get_room(&self, room_id: &RoomId) -> Option<room::Room> {
        self.base_client()
            .get_room(room_id)
            .map(|room| room::Common::new(self.clone(), room).into())
    }

    /// Get a joined room with the given room id.
    ///
    /// # Arguments
    ///
    /// `room_id` - The unique id of the room that should be fetched.
    pub fn get_joined_room(&self, room_id: &RoomId) -> Option<room::Joined> {
        self.base_client().get_room(room_id).and_then(|room| room::Joined::new(self, room))
    }

    /// Get an invited room with the given room id.
    ///
    /// # Arguments
    ///
    /// `room_id` - The unique id of the room that should be fetched.
    pub fn get_invited_room(&self, room_id: &RoomId) -> Option<room::Invited> {
        self.base_client().get_room(room_id).and_then(|room| room::Invited::new(self, room))
    }

    /// Get a left room with the given room id.
    ///
    /// # Arguments
    ///
    /// `room_id` - The unique id of the room that should be fetched.
    pub fn get_left_room(&self, room_id: &RoomId) -> Option<room::Left> {
        self.base_client().get_room(room_id).and_then(|room| room::Left::new(self, room))
    }

    /// Resolve a room alias to a room id and a list of servers which know
    /// about it.
    ///
    /// # Arguments
    ///
    /// `room_alias` - The room alias to be resolved.
    pub async fn resolve_room_alias(
        &self,
        room_alias: &RoomAliasId,
    ) -> HttpResult<get_alias::v3::Response> {
        let request = get_alias::v3::Request::new(room_alias.to_owned());
        self.send(request, None).await
    }

    /// Gets the homeserver’s supported login types.
    ///
    /// This should be the first step when trying to login so you can call the
    /// appropriate method for the next step.
    pub async fn get_login_types(&self) -> HttpResult<get_login_types::v3::Response> {
        let request = get_login_types::v3::Request::new();
        self.send(request, None).await
    }

    /// Get the URL to use to login via Single Sign-On.
    ///
    /// Returns a URL that should be opened in a web browser to let the user
    /// login.
    ///
    /// After a successful login, the loginToken received at the redirect URL
    /// should be used to login with [`login_with_token`].
    ///
    /// # Arguments
    ///
    /// * `redirect_url` - The URL that will receive a `loginToken` after a
    ///   successful SSO login.
    ///
    /// * `idp_id` - The optional ID of the identity provider to login with.
    ///
    /// [`login_with_token`]: #method.login_with_token
    pub async fn get_sso_login_url(
        &self,
        redirect_url: &str,
        idp_id: Option<&str>,
    ) -> Result<String> {
        let homeserver = self.homeserver().await;
        let server_versions = self.server_versions().await?;

        let request = if let Some(id) = idp_id {
            sso_login_with_provider::v3::Request::new(id.to_owned(), redirect_url.to_owned())
                .try_into_http_request::<Vec<u8>>(
                    homeserver.as_str(),
                    SendAccessToken::None,
                    server_versions,
                )
        } else {
            sso_login::v3::Request::new(redirect_url.to_owned()).try_into_http_request::<Vec<u8>>(
                homeserver.as_str(),
                SendAccessToken::None,
                server_versions,
            )
        };

        match request {
            Ok(req) => Ok(req.uri().to_string()),
            Err(err) => Err(Error::from(HttpError::from(err))),
        }
    }

    /// Login to the server with a username and password.
    ///
    /// This can be used for the first login as well as for subsequent logins,
    /// note that if the device ID isn't provided a new device will be created.
    ///
    /// If this isn't the first login, a device ID should be provided through
    /// [`LoginBuilder::device_id`] to restore the correct stores.
    ///
    /// Alternatively the [`restore_session`] method can be used to restore a
    /// logged-in client without the password.
    ///
    /// # Arguments
    ///
    /// * `user` - The user ID or user ID localpart of the user that should be
    ///   logged into the homeserver.
    ///
    /// * `password` - The password of the user.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use futures::executor::block_on;
    /// # use url::Url;
    /// # let homeserver = Url::parse("http://example.com").unwrap();
    /// # block_on(async {
    /// use matrix_sdk::Client;
    ///
    /// let client = Client::new(homeserver).await?;
    /// let user = "example";
    ///
    /// let response = client
    ///     .login_username(user, "wordpass")
    ///     .initial_device_display_name("My bot")
    ///     .await?;
    ///
    /// println!(
    ///     "Logged in as {user}, got device_id {} and access_token {}",
    ///     response.device_id, response.access_token,
    /// );
    /// # anyhow::Ok(()) });
    /// ```
    ///
    /// [`restore_session`]: #method.restore_session
    pub fn login_username(&self, id: impl AsRef<str>, password: &str) -> LoginBuilder {
        self.login_identifier(UserIdentifier::UserIdOrLocalpart(id.as_ref().to_owned()), password)
    }

    /// Login to the server with a user identifier and password.
    ///
    /// This is more general form of [`login_username`][Self::login_username]
    /// that also accepts third-party identifiers instead of just the user ID or
    /// its localpart.
    pub fn login_identifier(&self, id: UserIdentifier, password: &str) -> LoginBuilder {
        LoginBuilder::new_password(self.clone(), id, password.to_owned())
    }

    /// Login to the server with a custom login type
    ///
    /// # Arguments
    ///
    /// * `login_type` - Identifier of the custom login type, e.g.
    ///   `org.matrix.login.jwt`
    ///
    /// * `data` - The additional data which should be attached to the login
    ///   request.
    ///
    /// ```no_run
    /// # use futures::executor::block_on;
    /// # use url::Url;
    /// # let homeserver = Url::parse("http://example.com").unwrap();
    /// # block_on(async {
    /// use matrix_sdk::Client;
    ///
    /// let client = Client::new(homeserver).await?;
    /// let user = "example";
    ///
    /// let response = client
    ///     .login_custom(
    ///         "org.matrix.login.jwt",
    ///         [("token".to_owned(), "jwt_token_content".into())]
    ///             .into_iter()
    ///             .collect(),
    ///     )?
    ///     .initial_device_display_name("My bot")
    ///     .await?;
    ///
    /// println!(
    ///     "Logged in as {user}, got device_id {} and access_token {}",
    ///     response.device_id, response.access_token,
    /// );
    /// # anyhow::Ok(()) });
    /// ```
    pub fn login_custom(
        &self,
        login_type: &str,
        data: JsonObject,
    ) -> serde_json::Result<LoginBuilder> {
        LoginBuilder::new_custom(self.clone(), login_type, data)
    }

    /// Login to the server with a token.
    ///
    /// This token is usually received in the SSO flow after following the URL
    /// provided by [`get_sso_login_url`], note that this is not the access
    /// token of a session.
    ///
    /// This should only be used for the first login.
    ///
    /// The [`restore_session`] method should be used to restore a logged-in
    /// client after the first login.
    ///
    /// A device ID should be provided through [`LoginBuilder::device_id`] to
    /// restore the correct stores, if the device ID isn't provided a new
    /// device will be created.
    ///
    /// # Arguments
    ///
    /// * `token` - A login token.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use matrix_sdk::Client;
    /// # use matrix_sdk::ruma::{assign, DeviceId};
    /// # use futures::executor::block_on;
    /// # use url::Url;
    /// # let homeserver = Url::parse("https://example.com").unwrap();
    /// # let redirect_url = "http://localhost:1234";
    /// # let login_token = "token";
    /// # block_on(async {
    /// let client = Client::new(homeserver).await.unwrap();
    /// let sso_url = client.get_sso_login_url(redirect_url, None);
    ///
    /// // Let the user authenticate at the SSO URL
    /// // Receive the loginToken param at redirect_url
    ///
    /// let response = client
    ///     .login_token(login_token)
    ///     .initial_device_display_name("My app")
    ///     .await
    ///     .unwrap();
    ///
    /// println!(
    ///     "Logged in as {}, got device_id {} and access_token {}",
    ///     response.user_id, response.device_id, response.access_token,
    /// );
    /// # })
    /// ```
    ///
    /// [`get_sso_login_url`]: #method.get_sso_login_url
    /// [`restore_session`]: #method.restore_session
    pub fn login_token(&self, token: &str) -> LoginBuilder {
        LoginBuilder::new_token(self.clone(), token.to_owned())
    }

    /// Login to the server via Single Sign-On.
    ///
    /// This takes care of the whole SSO flow:
    ///   * Spawn a local http server
    ///   * Provide a callback to open the SSO login URL in a web browser
    ///   * Wait for the local http server to get the loginToken
    ///   * Call [`login_token`]
    ///
    /// If cancellation is needed the method should be wrapped in a cancellable
    /// task. **Note** that users with root access to the system have the
    /// ability to snoop in on the data/token that is passed to the local
    /// HTTP server that will be spawned.
    ///
    /// If you need more control over the SSO login process, you should use
    /// [`get_sso_login_url`] and [`login_token`] directly.
    ///
    /// This should only be used for the first login.
    ///
    /// The [`restore_session`] method should be used to restore a logged-in
    /// client after the first login.
    ///
    /// # Arguments
    ///
    /// * `use_sso_login_url` - A callback that will receive the SSO Login URL.
    ///   It should usually be used to open the SSO URL in a browser and must
    ///   return `Ok(())` if the URL was successfully opened. If it returns
    ///   `Err`, the error will be forwarded.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use matrix_sdk::Client;
    /// # use futures::executor::block_on;
    /// # use url::Url;
    /// # let homeserver = Url::parse("https://example.com").unwrap();
    /// # block_on(async {
    /// let client = Client::new(homeserver).await.unwrap();
    ///
    /// let response = client
    ///     .login_sso(|sso_url| async move {
    ///         // Open sso_url
    ///         Ok(())
    ///     })
    ///     .initial_device_display_name("My app")
    ///     .await
    ///     .unwrap();
    ///
    /// println!(
    ///     "Logged in as {}, got device_id {} and access_token {}",
    ///     response.user_id, response.device_id, response.access_token
    /// );
    /// # })
    /// ```
    ///
    /// [`get_sso_login_url`]: #method.get_sso_login_url
    /// [`login_token`]: #method.login_token
    /// [`restore_session`]: #method.restore_session
    #[cfg(feature = "sso-login")]
    pub fn login_sso<F, Fut>(&self, use_sso_login_url: F) -> SsoLoginBuilder<F>
    where
        F: FnOnce(String) -> Fut + Send,
        Fut: Future<Output = Result<()>> + Send,
    {
        SsoLoginBuilder::new(self.clone(), use_sso_login_url)
    }

    /// Receive a login response and update the homeserver and the base client
    /// if needed.
    ///
    /// # Arguments
    ///
    /// * `response` - A successful login response.
    async fn receive_login_response(&self, response: &login::v3::Response) -> Result<()> {
        if self.inner.respect_login_well_known {
            if let Some(well_known) = &response.well_known {
                if let Ok(homeserver) = Url::parse(&well_known.homeserver.base_url) {
                    self.set_homeserver(homeserver).await;
                }
            }
        }

        self.inner
            .root_span
            .record("user_id", display(&response.user_id))
            .record("device_id", display(&response.device_id));

        #[cfg(feature = "e2e-encryption")]
        if let Some(key) = self.encryption().ed25519_key().await {
            self.inner.root_span.record("ed25519_key", key);
        }

        self.inner.base_client.receive_login_response(response).await?;

        Ok(())
    }

    /// Restore a previously logged in session.
    ///
    /// This can be used to restore the client to a logged in state, loading all
    /// the stored state and encryption keys.
    ///
    /// Alternatively, if the whole session isn't stored the [`login`] method
    /// can be used with a device ID.
    ///
    /// # Arguments
    ///
    /// * `session` - A session that the user already has from a
    /// previous login call.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use matrix_sdk::{
    ///     ruma::{device_id, user_id},
    ///     Client, Session,
    /// };
    /// # use url::Url;
    /// # use futures::executor::block_on;
    /// # block_on(async {
    ///
    /// let homeserver = Url::parse("http://example.com")?;
    /// let client = Client::new(homeserver).await?;
    ///
    /// let session = Session {
    ///     access_token: "My-Token".to_owned(),
    ///     refresh_token: None,
    ///     user_id: user_id!("@example:localhost").to_owned(),
    ///     device_id: device_id!("MYDEVICEID").to_owned(),
    /// };
    ///
    /// client.restore_session(session).await?;
    /// # anyhow::Ok(()) });
    /// ```
    ///
    /// The `Session` object can also be created from the response the
    /// [`LoginBuilder::send()`] method returns:
    ///
    /// ```no_run
    /// use matrix_sdk::{Client, Session};
    /// # use url::Url;
    /// # use futures::executor::block_on;
    /// # block_on(async {
    ///
    /// let homeserver = Url::parse("http://example.com")?;
    /// let client = Client::new(homeserver).await?;
    ///
    /// let session: Session =
    ///     client.login_username("example", "my-password").send().await?.into();
    ///
    /// // Persist the `Session` so it can later be used to restore the login.
    /// client.restore_session(session).await?;
    /// # anyhow::Ok(()) });
    /// ```
    ///
    /// [`login`]: #method.login
    #[instrument(skip_all, parent = &self.inner.root_span)]
    pub async fn restore_session(&self, session: Session) -> Result<()> {
        debug!("Restoring session");

        let (meta, tokens) = session.into_parts();

        self.inner
            .root_span
            .record("user_id", display(&meta.user_id))
            .record("device_id", display(&meta.device_id));

        self.base_client().set_session_tokens(tokens);
        self.base_client().set_session_meta(meta).await?;

        #[cfg(feature = "e2e-encryption")]
        if let Some(key) = self.encryption().ed25519_key().await {
            self.inner.root_span.record("ed25519_key", key);
        }

        debug!("Done restoring session");

        Ok(())
    }

    /// Refresh the access token.
    ///
    /// When support for [refreshing access tokens] is activated on both the
    /// homeserver and the client, access tokens have an expiration date and
    /// need to be refreshed periodically. To activate support for refresh
    /// tokens in the [`Client`], it needs to be done at login with the
    /// [`LoginBuilder::request_refresh_token()`] method, or during account
    /// registration.
    ///
    /// This method doesn't need to be called if
    /// [`ClientBuilder::handle_refresh_tokens()`] is called during construction
    /// of the `Client`. Otherwise, it should be called once when a refresh
    /// token is available and an [`UnknownToken`] error is received.
    /// If this call fails with another [`UnknownToken`] error, it means that
    /// the session needs to be logged in again.
    ///
    /// It can also be called at any time when a refresh token is available, it
    /// will invalidate the previous access token.
    ///
    /// The new tokens in the response will be used by the `Client` and should
    /// be persisted to be able to [restore the session]. The response will
    /// always contain an access token that replaces the previous one. It
    /// can also contain a refresh token, in which case it will also replace
    /// the previous one.
    ///
    /// This method is protected behind a lock, so calling this method several
    /// times at once will only call the endpoint once and all subsequent calls
    /// will wait for the result of the first call. The first call will
    /// return `Ok(Some(response))` or the [`HttpError`] returned by the
    /// endpoint, while the others will return `Ok(None)` if the token was
    /// refreshed by the first call or a [`RefreshTokenError`] error, if it
    /// failed.
    ///
    /// # Example
    ///
    /// ```no_run
    /// use matrix_sdk::{Client, Error, Session};
    /// use url::Url;
    /// # use futures::executor::block_on;
    /// # block_on(async {
    /// # fn get_credentials() -> (&'static str, &'static str) { ("", "") };
    /// # fn persist_session(_: Option<Session>) {};
    ///
    /// let homeserver = Url::parse("http://example.com")?;
    /// let client = Client::new(homeserver).await?;
    ///
    /// let (user, password) = get_credentials();
    /// let response = client
    ///     .login_username(user, password)
    ///     .initial_device_display_name("My App")
    ///     .request_refresh_token()
    ///     .send()
    ///     .await?;
    ///
    /// persist_session(client.session());
    ///
    /// // Handle when an `M_UNKNOWN_TOKEN` error is encountered.
    /// async fn on_unknown_token_err(client: &Client) -> Result<(), Error> {
    ///     if client.refresh_token().is_some()
    ///         && client.refresh_access_token().await.is_ok()
    ///     {
    ///         persist_session(client.session());
    ///         return Ok(());
    ///     }
    ///
    ///     let (user, password) = get_credentials();
    ///     client
    ///         .login_username(user, password)
    ///         .request_refresh_token()
    ///         .send()
    ///         .await?;
    ///
    ///     persist_session(client.session());
    ///
    ///     Ok(())
    /// }
    ///
    /// # anyhow::Ok(()) });
    /// ```
    ///
    /// [refreshing access tokens]: https://spec.matrix.org/v1.3/client-server-api/#refreshing-access-tokens
    /// [`UnknownToken`]: ruma::api::client::error::ErrorKind::UnknownToken
    /// [restore the session]: Client::restore_session
    pub async fn refresh_access_token(&self) -> HttpResult<Option<refresh_token::v3::Response>> {
        #[cfg(not(target_arch = "wasm32"))]
        let lock = self.inner.refresh_token_lock.try_lock().ok();
        #[cfg(target_arch = "wasm32")]
        let lock = self.inner.refresh_token_lock.try_lock();

        if let Some(mut guard) = lock {
            let Some(mut session_tokens) = self.session_tokens() else {
                *guard = Err(RefreshTokenError::RefreshTokenRequired);
                return Err(RefreshTokenError::RefreshTokenRequired.into());
            };

            let refresh_token = session_tokens
                .refresh_token
                .clone()
                .ok_or(RefreshTokenError::RefreshTokenRequired)?;
            let request = refresh_token::v3::Request::new(refresh_token);

            let res = self
                .inner
                .http_client
                .send(
                    request,
                    None,
                    self.homeserver().await.to_string(),
                    self.access_token().as_deref(),
                    self.user_id(),
                    self.server_versions().await?,
                )
                .await;

            match res {
                Ok(res) => {
                    *guard = Ok(());

                    session_tokens.update_with_refresh_response(&res);

                    self.base_client().set_session_tokens(session_tokens);

                    // TODO: Let ffi client to know that tokens have changed

                    Ok(Some(res))
                }
                Err(error) => {
                    *guard = match error.as_ruma_api_error() {
                        Some(RumaApiError::ClientApi(api_error)) => {
                            Err(RefreshTokenError::ClientApi(api_error.to_owned()))
                        }
                        _ => Err(RefreshTokenError::UnableToRefreshToken),
                    };

                    Err(error)
                }
            }
        } else {
            match *self.inner.refresh_token_lock.lock().await {
                Ok(_) => Ok(None),
                Err(_) => Err(RefreshTokenError::UnableToRefreshToken.into()),
            }
        }
    }

    /// Register a user to the server.
    ///
    /// # Arguments
    ///
    /// * `registration` - The easiest way to create this request is using the
    ///   [`register::v3::Request`] itself.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use matrix_sdk::Client;
    /// # use matrix_sdk::ruma::{
    /// #     api::client::{
    /// #         account::register::{v3::Request as RegistrationRequest, RegistrationKind},
    /// #         uiaa,
    /// #     },
    /// #     DeviceId,
    /// # };
    /// # use futures::executor::block_on;
    /// # use url::Url;
    /// # let homeserver = Url::parse("http://example.com").unwrap();
    /// # block_on(async {
    ///
    /// let mut request = RegistrationRequest::new();
    /// request.username = Some("user".to_owned());
    /// request.password = Some("password".to_owned());
    /// request.auth = Some(uiaa::AuthData::FallbackAcknowledgement(
    ///     uiaa::FallbackAcknowledgement::new("foobar".to_owned()),
    /// ));
    ///
    /// let client = Client::new(homeserver).await.unwrap();
    /// client.register(request).await;
    /// # })
    /// ```
    #[instrument(skip_all, parent = &self.inner.root_span)]
    pub async fn register(
        &self,
        request: register::v3::Request,
    ) -> HttpResult<register::v3::Response> {
        let homeserver = self.homeserver().await;
        info!("Registering to {homeserver}");

        let config = if self.inner.appservice_mode {
            Some(RequestConfig::short_retry().force_auth())
        } else {
            None
        };

        self.send(request, config).await
    }

    /// Get or upload a sync filter.
    ///
    /// This method will either get a filter ID from the store or upload the
    /// filter definition to the homeserver and return the new filter ID.
    ///
    /// # Arguments
    ///
    /// * `filter_name` - The unique name of the filter, this name will be used
    /// locally to store and identify the filter ID returned by the server.
    ///
    /// * `definition` - The filter definition that should be uploaded to the
    /// server if no filter ID can be found in the store.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use matrix_sdk::{
    /// #    Client, config::SyncSettings,
    /// #    ruma::api::client::{
    /// #        filter::{
    /// #           FilterDefinition, LazyLoadOptions, RoomEventFilter, RoomFilter,
    /// #        },
    /// #        sync::sync_events::v3::Filter,
    /// #    }
    /// # };
    /// # use futures::executor::block_on;
    /// # use url::Url;
    /// # block_on(async {
    /// # let homeserver = Url::parse("http://example.com").unwrap();
    /// # let client = Client::new(homeserver).await.unwrap();
    /// let mut filter = FilterDefinition::default();
    ///
    /// // Let's enable member lazy loading.
    /// filter.room.state.lazy_load_options =
    ///     LazyLoadOptions::Enabled { include_redundant_members: false };
    ///
    /// let filter_id = client
    ///     .get_or_upload_filter("sync", filter)
    ///     .await
    ///     .unwrap();
    ///
    /// let sync_settings = SyncSettings::new()
    ///     .filter(Filter::FilterId(filter_id));
    ///
    /// let response = client.sync_once(sync_settings).await.unwrap();
    /// # });
    #[instrument(skip(self, definition), parent = &self.inner.root_span)]
    pub async fn get_or_upload_filter(
        &self,
        filter_name: &str,
        definition: FilterDefinition,
    ) -> Result<String> {
        if let Some(filter) = self.inner.base_client.get_filter(filter_name).await? {
            debug!("Found filter locally");
            Ok(filter)
        } else {
            debug!("Didn't find filter locally");
            let user_id = self.user_id().ok_or(Error::AuthenticationRequired)?;
            let request = FilterUploadRequest::new(user_id.to_owned(), definition);
            let response = self.send(request, None).await?;

            self.inner.base_client.receive_filter_upload(filter_name, &response).await?;

            Ok(response.filter_id)
        }
    }

    /// Join a room by `RoomId`.
    ///
    /// Returns a `join_room_by_id::Response` consisting of the
    /// joined rooms `RoomId`.
    ///
    /// # Arguments
    ///
    /// * `room_id` - The `RoomId` of the room to be joined.
    pub async fn join_room_by_id(&self, room_id: &RoomId) -> Result<room::Joined> {
        let request = join_room_by_id::v3::Request::new(room_id.to_owned());
        let response = self.send(request, None).await?;
        let base_room = self.base_client().room_joined(&response.room_id).await?;
        room::Joined::new(self, base_room).ok_or(Error::InconsistentState)
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
        alias: &RoomOrAliasId,
        server_names: &[OwnedServerName],
    ) -> Result<room::Joined> {
        let request = assign!(join_room_by_id_or_alias::v3::Request::new(alias.to_owned()), {
            server_name: server_names.to_owned(),
        });
        let response = self.send(request, None).await?;
        let base_room = self.base_client().room_joined(&response.room_id).await?;
        room::Joined::new(self, base_room).ok_or(Error::InconsistentState)
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
    /// * `server` - The name of the server, if `None` the requested server is
    ///   used.
    ///
    /// # Examples
    /// ```no_run
    /// use matrix_sdk::Client;
    /// # use url::Url;
    /// # let homeserver = Url::parse("http://example.com").unwrap();
    /// # let limit = Some(10);
    /// # let since = Some("since token");
    /// # let server = Some("servername.com".try_into().unwrap());
    /// # use futures::executor::block_on;
    /// # block_on(async {
    ///
    /// let mut client = Client::new(homeserver).await.unwrap();
    ///
    /// client.public_rooms(limit, since, server).await;
    /// # });
    /// ```
    #[cfg_attr(not(target_arch = "wasm32"), deny(clippy::future_not_send))]
    pub async fn public_rooms(
        &self,
        limit: Option<u32>,
        since: Option<&str>,
        server: Option<&ServerName>,
    ) -> HttpResult<get_public_rooms::v3::Response> {
        let limit = limit.map(UInt::from);

        let request = assign!(get_public_rooms::v3::Request::new(), {
            limit,
            since: since.map(ToOwned::to_owned),
            server: server.map(ToOwned::to_owned),
        });
        self.send(request, None).await
    }

    /// Create a room using the `RoomBuilder` and send the request.
    ///
    /// Sends a request to `/_matrix/client/r0/createRoom`, returns a
    /// `create_room::Response`, this is an empty response.
    ///
    /// # Arguments
    ///
    /// * `room` - The easiest way to create this request is using the
    /// `create_room::Request` itself.
    ///
    /// # Examples
    /// ```no_run
    /// use matrix_sdk::Client;
    /// # use matrix_sdk::ruma::api::client::room::{
    /// #     create_room::v3::Request as CreateRoomRequest,
    /// #     Visibility,
    /// # };
    /// # use url::Url;
    ///
    /// # use futures::executor::block_on;
    /// # block_on(async {
    /// # let homeserver = Url::parse("http://example.com").unwrap();
    /// let request = CreateRoomRequest::new();
    /// let client = Client::new(homeserver).await.unwrap();
    /// assert!(client.create_room(request).await.is_ok());
    /// # });
    /// ```
    pub async fn create_room(&self, request: create_room::v3::Request) -> HttpResult<room::Joined> {
        let response = self.send(request, None).await?;

        let base_room =
            self.base_client().get_or_create_room(&response.room_id, RoomType::Joined).await;
        Ok(room::Joined::new(self, base_room).unwrap())
    }

    /// Search the homeserver's directory for public rooms with a filter.
    ///
    /// # Arguments
    ///
    /// * `room_search` - The easiest way to create this request is using the
    /// `get_public_rooms_filtered::Request` itself.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use url::Url;
    /// # use matrix_sdk::Client;
    /// # use futures::executor::block_on;
    /// # block_on(async {
    /// # let homeserver = Url::parse("http://example.com")?;
    /// use matrix_sdk::ruma::{
    ///     api::client::directory::get_public_rooms_filtered, directory::Filter,
    /// };
    /// # let mut client = Client::new(homeserver).await?;
    ///
    /// let mut filter = Filter::new();
    /// filter.generic_search_term = Some("rust".to_owned());
    /// let mut request = get_public_rooms_filtered::v3::Request::new();
    /// request.filter = filter;
    ///
    /// let response = client.public_rooms_filtered(request).await?;
    ///
    /// for room in response.chunk {
    ///     println!("Found room {:?}", room);
    /// }
    /// # anyhow::Ok(()) });
    /// ```
    pub async fn public_rooms_filtered(
        &self,
        request: get_public_rooms_filtered::v3::Request,
    ) -> HttpResult<get_public_rooms_filtered::v3::Response> {
        self.send(request, None).await
    }

    /// Send an arbitrary request to the server, without updating client state.
    ///
    /// **Warning:** Because this method *does not* update the client state, it
    /// is important to make sure that you account for this yourself, and
    /// use wrapper methods where available.  This method should *only* be
    /// used if a wrapper method for the endpoint you'd like to use is not
    /// available.
    ///
    /// # Arguments
    ///
    /// * `request` - A filled out and valid request for the endpoint to be hit
    ///
    /// * `timeout` - An optional request timeout setting, this overrides the
    /// default request setting if one was set.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use matrix_sdk::{Client, config::SyncSettings};
    /// # use futures::executor::block_on;
    /// # use url::Url;
    /// # block_on(async {
    /// # let homeserver = Url::parse("http://localhost:8080")?;
    /// # let mut client = Client::new(homeserver).await?;
    /// use matrix_sdk::ruma::{api::client::profile, user_id};
    ///
    /// // First construct the request you want to make
    /// // See https://docs.rs/ruma-client-api/latest/ruma_client_api/index.html
    /// // for all available Endpoints
    /// let user_id = user_id!("@example:localhost").to_owned();
    /// let request = profile::get_profile::v3::Request::new(user_id);
    ///
    /// // Start the request using Client::send()
    /// let response = client.send(request, None).await?;
    ///
    /// // Check the corresponding Response struct to find out what types are
    /// // returned
    /// # anyhow::Ok(()) });
    /// ```
    pub async fn send<Request>(
        &self,
        request: Request,
        config: Option<RequestConfig>,
    ) -> HttpResult<Request::IncomingResponse>
    where
        Request: OutgoingRequest + Clone + Debug,
        HttpError: From<FromHttpResponseError<Request::EndpointError>>,
    {
        let res = self.send_inner(request.clone(), config, None).await;

        // If this is an `M_UNKNOWN_TOKEN` error and refresh token handling is active,
        // try to refresh the token and retry the request.
        if self.inner.handle_refresh_tokens {
            if let Err(Some(ErrorKind::UnknownToken { .. })) =
                res.as_ref().map_err(HttpError::client_api_error_kind)
            {
                if let Err(refresh_error) = self.refresh_access_token().await {
                    match &refresh_error {
                        HttpError::RefreshToken(RefreshTokenError::RefreshTokenRequired) => {
                            // Refreshing access tokens is not supported by
                            // this `Session`, ignore.
                        }
                        _ => {
                            return Err(refresh_error);
                        }
                    }
                } else {
                    return self.send_inner(request, config, None).await;
                }
            }
        }

        res
    }

    #[cfg(feature = "experimental-sliding-sync")]
    // FIXME: remove this as soon as Sliding-Sync isn't needing an external server
    // anymore
    pub(crate) async fn send_with_homeserver<Request>(
        &self,
        request: Request,
        config: Option<RequestConfig>,
        homeserver: Option<String>,
    ) -> HttpResult<Request::IncomingResponse>
    where
        Request: OutgoingRequest + Clone + Debug,
        HttpError: From<FromHttpResponseError<Request::EndpointError>>,
    {
        let res = self.send_inner(request.clone(), config, homeserver.clone()).await;

        // If this is an `M_UNKNOWN_TOKEN` error and refresh token handling is active,
        // try to refresh the token and retry the request.
        if self.inner.handle_refresh_tokens {
            if let Err(Some(ErrorKind::UnknownToken { .. })) =
                res.as_ref().map_err(HttpError::client_api_error_kind)
            {
                if let Err(refresh_error) = self.refresh_access_token().await {
                    match &refresh_error {
                        HttpError::RefreshToken(RefreshTokenError::RefreshTokenRequired) => {
                            // Refreshing access tokens is not supported by
                            // this `Session`, ignore.
                        }
                        _ => {
                            return Err(refresh_error);
                        }
                    }
                } else {
                    return self.send_inner(request, config, homeserver).await;
                }
            }
        }

        res
    }

    async fn send_inner<Request>(
        &self,
        request: Request,
        config: Option<RequestConfig>,
        homeserver: Option<String>,
    ) -> HttpResult<Request::IncomingResponse>
    where
        Request: OutgoingRequest + Debug,
        HttpError: From<FromHttpResponseError<Request::EndpointError>>,
    {
        let homeserver = match homeserver {
            Some(hs) => hs,
            None => self.homeserver().await.to_string(),
        };

        let response = self
            .inner
            .http_client
            .send(
                request,
                config,
                homeserver,
                self.access_token().as_deref(),
                self.user_id(),
                self.server_versions().await?,
            )
            .await;

        if let Err(http_error) = &response {
            if let Some(ErrorKind::UnknownToken { soft_logout }) =
                http_error.client_api_error_kind()
            {
                _ = self
                    .inner
                    .unknown_token_error_sender
                    .send(UnknownToken { soft_logout: *soft_logout });
            }
        }

        response
    }

    async fn request_server_versions(&self) -> HttpResult<Box<[MatrixVersion]>> {
        let server_versions: Box<[MatrixVersion]> = self
            .inner
            .http_client
            .send(
                get_supported_versions::Request::new(),
                None,
                self.homeserver().await.to_string(),
                None,
                None,
                &[MatrixVersion::V1_0],
            )
            .await?
            .known_versions()
            .collect();

        if server_versions.is_empty() {
            Ok(vec![MatrixVersion::V1_0].into())
        } else {
            Ok(server_versions)
        }
    }

    pub(crate) async fn server_versions(&self) -> HttpResult<&[MatrixVersion]> {
        #[cfg(target_arch = "wasm32")]
        let server_versions =
            self.inner.server_versions.get_or_try_init(self.request_server_versions()).await?;

        #[cfg(not(target_arch = "wasm32"))]
        let server_versions =
            self.inner.server_versions.get_or_try_init(|| self.request_server_versions()).await?;

        Ok(server_versions)
    }

    /// Get information of all our own devices.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use matrix_sdk::{Client, config::SyncSettings};
    /// # use futures::executor::block_on;
    /// # use url::Url;
    /// # block_on(async {
    /// # let homeserver = Url::parse("http://localhost:8080")?;
    /// # let mut client = Client::new(homeserver).await?;
    /// let response = client.devices().await?;
    ///
    /// for device in response.devices {
    ///     println!(
    ///         "Device: {} {}",
    ///         device.device_id,
    ///         device.display_name.as_deref().unwrap_or("")
    ///     );
    /// }
    /// # anyhow::Ok(()) });
    /// ```
    pub async fn devices(&self) -> HttpResult<get_devices::v3::Response> {
        let request = get_devices::v3::Request::new();

        self.send(request, None).await
    }

    /// Delete the given devices from the server.
    ///
    /// # Arguments
    ///
    /// * `devices` - The list of devices that should be deleted from the
    /// server.
    ///
    /// * `auth_data` - This request requires user interactive auth, the first
    /// request needs to set this to `None` and will always fail with an
    /// `UiaaResponse`. The response will contain information for the
    /// interactive auth and the same request needs to be made but this time
    /// with some `auth_data` provided.
    ///
    /// ```no_run
    /// # use matrix_sdk::{
    /// #    ruma::{api::client::uiaa, device_id},
    /// #    Client, Error, config::SyncSettings,
    /// # };
    /// # use futures::executor::block_on;
    /// # use serde_json::json;
    /// # use url::Url;
    /// # use std::collections::BTreeMap;
    /// # block_on(async {
    /// # let homeserver = Url::parse("http://localhost:8080")?;
    /// # let mut client = Client::new(homeserver).await?;
    /// let devices = &[device_id!("DEVICEID").to_owned()];
    ///
    /// if let Err(e) = client.delete_devices(devices, None).await {
    ///     if let Some(info) = e.as_uiaa_response() {
    ///         let mut password = uiaa::Password::new(
    ///             uiaa::UserIdentifier::UserIdOrLocalpart("example".to_owned()),
    ///             "wordpass".to_owned(),
    ///         );
    ///         password.session = info.session.clone();
    ///
    ///         client
    ///             .delete_devices(devices, Some(uiaa::AuthData::Password(password)))
    ///             .await?;
    ///     }
    /// }
    /// # anyhow::Ok(()) });
    pub async fn delete_devices(
        &self,
        devices: &[OwnedDeviceId],
        auth_data: Option<AuthData>,
    ) -> HttpResult<delete_devices::v3::Response> {
        let mut request = delete_devices::v3::Request::new(devices.to_owned());
        request.auth = auth_data;

        self.send(request, None).await
    }

    /// Change the display name of a device owned by the current user.
    ///
    /// Returns a `update_device::Response` which specifies the result
    /// of the operation.
    ///
    /// # Arguments
    ///
    /// * `device_id` - The ID of the device to change the display name of.
    /// * `display_name` - The new display name to set.
    pub async fn rename_device(
        &self,
        device_id: &DeviceId,
        display_name: &str,
    ) -> HttpResult<update_device::v3::Response> {
        let mut request = update_device::v3::Request::new(device_id.to_owned());
        request.display_name = Some(display_name.to_owned());

        self.send(request, None).await
    }

    /// Synchronize the client's state with the latest state on the server.
    ///
    /// ## Syncing Events
    ///
    /// Messages or any other type of event need to be periodically fetched from
    /// the server, this is achieved by sending a `/sync` request to the server.
    ///
    /// The first sync is sent out without a [`token`]. The response of the
    /// first sync will contain a [`next_batch`] field which should then be
    /// used in the subsequent sync calls as the [`token`]. This ensures that we
    /// don't receive the same events multiple times.
    ///
    /// ## Long Polling
    ///
    /// A sync should in the usual case always be in flight. The
    /// [`SyncSettings`] have a  [`timeout`] option, which controls how
    /// long the server will wait for new events before it will respond.
    /// The server will respond immediately if some new events arrive before the
    /// timeout has expired. If no changes arrive and the timeout expires an
    /// empty sync response will be sent to the client.
    ///
    /// This method of sending a request that may not receive a response
    /// immediately is called long polling.
    ///
    /// ## Filtering Events
    ///
    /// The number or type of messages and events that the client should receive
    /// from the server can be altered using a [`Filter`].
    ///
    /// Filters can be non-trivial and, since they will be sent with every sync
    /// request, they may take up a bunch of unnecessary bandwidth.
    ///
    /// Luckily filters can be uploaded to the server and reused using an unique
    /// identifier, this can be achieved using the [`get_or_upload_filter()`]
    /// method.
    ///
    /// # Arguments
    ///
    /// * `sync_settings` - Settings for the sync call, this allows us to set
    /// various options to configure the sync:
    ///     * [`filter`] - To configure which events we receive and which get
    ///       [filtered] by the server
    ///     * [`timeout`] - To configure our [long polling] setup.
    ///     * [`token`] - To tell the server which events we already received
    ///       and where we wish to continue syncing.
    ///     * [`full_state`] - To tell the server that we wish to receive all
    ///       state events, regardless of our configured [`token`].
    ///     * [`set_presence`] - To tell the server to set the presence and to
    ///       which state.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use url::Url;
    /// # use futures::executor::block_on;
    /// # block_on(async {
    /// # let homeserver = Url::parse("http://localhost:8080")?;
    /// # let username = "";
    /// # let password = "";
    /// use matrix_sdk::{
    ///     config::SyncSettings,
    ///     ruma::events::room::message::OriginalSyncRoomMessageEvent, Client,
    /// };
    ///
    /// let client = Client::new(homeserver).await?;
    /// client.login_username(username, password).send().await?;
    ///
    /// // Sync once so we receive the client state and old messages.
    /// client.sync_once(SyncSettings::default()).await?;
    ///
    /// // Register our handler so we start responding once we receive a new
    /// // event.
    /// client.add_event_handler(|ev: OriginalSyncRoomMessageEvent| async move {
    ///     println!("Received event {}: {:?}", ev.sender, ev.content);
    /// });
    ///
    /// // Now keep on syncing forever. `sync()` will use the stored sync token
    /// // from our `sync_once()` call automatically.
    /// client.sync(SyncSettings::default()).await;
    /// # anyhow::Ok(()) });
    /// ```
    ///
    /// [`sync`]: #method.sync
    /// [`SyncSettings`]: crate::config::SyncSettings
    /// [`token`]: crate::config::SyncSettings#method.token
    /// [`timeout`]: crate::config::SyncSettings#method.timeout
    /// [`full_state`]: crate::config::SyncSettings#method.full_state
    /// [`set_presence`]: ruma::presence::PresenceState
    /// [`filter`]: crate::config::SyncSettings#method.filter
    /// [`Filter`]: ruma::api::client::sync::sync_events::v3::Filter
    /// [`next_batch`]: SyncResponse#structfield.next_batch
    /// [`get_or_upload_filter()`]: #method.get_or_upload_filter
    /// [long polling]: #long-polling
    /// [filtered]: #filtering-events
    #[instrument(skip(self))]
    pub async fn sync_once(
        &self,
        sync_settings: crate::config::SyncSettings,
    ) -> Result<SyncResponse> {
        // The sync might not return for quite a while due to the timeout.
        // We'll see if there's anything crypto related to send out before we
        // sync, i.e. if we closed our client after a sync but before the
        // crypto requests were sent out.
        //
        // This will mostly be a no-op.
        #[cfg(feature = "e2e-encryption")]
        if let Err(e) = self.send_outgoing_requests().await {
            error!(error = ?e, "Error while sending outgoing E2EE requests");
        }

        let request = assign!(sync_events::v3::Request::new(), {
            filter: sync_settings.filter,
            since: sync_settings.token,
            full_state: sync_settings.full_state,
            set_presence: sync_settings.set_presence,
            timeout: sync_settings.timeout,
        });
        let mut request_config = self.request_config();
        if let Some(timeout) = sync_settings.timeout {
            request_config.timeout += timeout;
        }

        let response = self.send(request, Some(request_config)).await?;
        let next_batch = response.next_batch.clone();
        let response = self.process_sync(response).await?;

        #[cfg(feature = "e2e-encryption")]
        if let Err(e) = self.send_outgoing_requests().await {
            error!(error = ?e, "Error while sending outgoing E2EE requests");
        }

        self.inner.sync_beat.notify(usize::MAX);

        Ok(SyncResponse::new(next_batch, response))
    }

    /// Repeatedly synchronize the client state with the server.
    ///
    /// This method will only return on error, if cancellation is needed
    /// the method should be wrapped in a cancelable task or the
    /// [`Client::sync_with_callback`] method can be used or
    /// [`Client::sync_with_result_callback`] if you want to handle error
    /// cases in the loop, too.
    ///
    /// This method will internally call [`Client::sync_once`] in a loop.
    ///
    /// This method can be used with the [`Client::add_event_handler`]
    /// method to react to individual events. If you instead wish to handle
    /// events in a bulk manner the [`Client::sync_with_callback`],
    /// [`Client::sync_with_result_callback`] and
    /// [`Client::sync_stream`] methods can be used instead. Those methods
    /// repeatedly return the whole sync response.
    ///
    /// # Arguments
    ///
    /// * `sync_settings` - Settings for the sync call. *Note* that those
    ///   settings will be only used for the first sync call. See the argument
    ///   docs for [`Client::sync_once`] for more info.
    ///
    /// # Return
    /// The sync runs until an error occurs, returning with `Err(Error)`. It is
    /// up to the user of the API to check the error and decide whether the sync
    /// should continue or not.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use url::Url;
    /// # use futures::executor::block_on;
    /// # block_on(async {
    /// # let homeserver = Url::parse("http://localhost:8080")?;
    /// # let username = "";
    /// # let password = "";
    /// use matrix_sdk::{
    ///     config::SyncSettings,
    ///     ruma::events::room::message::OriginalSyncRoomMessageEvent, Client,
    /// };
    ///
    /// let client = Client::new(homeserver).await?;
    /// client.login_username(&username, &password).send().await?;
    ///
    /// // Register our handler so we start responding once we receive a new
    /// // event.
    /// client.add_event_handler(|ev: OriginalSyncRoomMessageEvent| async move {
    ///     println!("Received event {}: {:?}", ev.sender, ev.content);
    /// });
    ///
    /// // Now keep on syncing forever. `sync()` will use the latest sync token
    /// // automatically.
    /// client.sync(SyncSettings::default()).await?;
    /// # anyhow::Ok(()) });
    /// ```
    ///
    /// [argument docs]: #method.sync_once
    /// [`sync_with_callback`]: #method.sync_with_callback
    pub async fn sync(&self, sync_settings: crate::config::SyncSettings) -> Result<(), Error> {
        self.sync_with_callback(sync_settings, |_| async { LoopCtrl::Continue }).await
    }

    /// Repeatedly call sync to synchronize the client state with the server.
    ///
    /// # Arguments
    ///
    /// * `sync_settings` - Settings for the sync call. *Note* that those
    ///   settings will be only used for the first sync call. See the argument
    ///   docs for [`Client::sync_once`] for more info.
    ///
    /// * `callback` - A callback that will be called every time a successful
    ///   response has been fetched from the server. The callback must return a
    ///   boolean which signalizes if the method should stop syncing. If the
    ///   callback returns `LoopCtrl::Continue` the sync will continue, if the
    ///   callback returns `LoopCtrl::Break` the sync will be stopped.
    ///
    /// # Return
    /// The sync runs until an error occurs or the
    /// callback indicates that the Loop should stop. If the callback asked for
    /// a regular stop, the result will be `Ok(())` otherwise the
    /// `Err(Error)` is returned.
    ///
    /// # Examples
    ///
    /// The following example demonstrates how to sync forever while sending all
    /// the interesting events through a mpsc channel to another thread e.g. a
    /// UI thread.
    ///
    /// ```no_run
    /// # use std::time::Duration;
    /// # use matrix_sdk::{Client, config::SyncSettings, LoopCtrl};
    /// # use url::Url;
    /// # use futures::executor::block_on;
    /// # block_on(async {
    /// # let homeserver = Url::parse("http://localhost:8080").unwrap();
    /// # let mut client = Client::new(homeserver).await.unwrap();
    ///
    /// use tokio::sync::mpsc::channel;
    ///
    /// let (tx, rx) = channel(100);
    ///
    /// let sync_channel = &tx;
    /// let sync_settings = SyncSettings::new()
    ///     .timeout(Duration::from_secs(30));
    ///
    /// client
    ///     .sync_with_callback(sync_settings, |response| async move {
    ///         let channel = sync_channel;
    ///         for (room_id, room) in response.rooms.join {
    ///             for event in room.timeline.events {
    ///                 channel.send(event).await.unwrap();
    ///             }
    ///         }
    ///
    ///         LoopCtrl::Continue
    ///     })
    ///     .await;
    /// })
    /// ```
    #[instrument(skip_all, parent = &self.inner.root_span)]
    pub async fn sync_with_callback<C>(
        &self,
        sync_settings: crate::config::SyncSettings,
        callback: impl Fn(SyncResponse) -> C,
    ) -> Result<(), Error>
    where
        C: Future<Output = LoopCtrl>,
    {
        self.sync_with_result_callback(sync_settings, |result| async {
            Ok(callback(result?).await)
        })
        .await
    }

    /// Repeatedly call sync to synchronize the client state with the server.
    ///
    /// # Arguments
    ///
    /// * `sync_settings` - Settings for the sync call. *Note* that those
    ///   settings will be only used for the first sync call. See the argument
    ///   docs for [`Client::sync_once`] for more info.
    ///
    /// * `callback` - A callback that will be called every time after a
    ///   response has been received, failure or not. The callback returns a
    ///   `Result<LoopCtrl, Error>, too. When returning `Ok(LoopCtrl::Continue)`
    ///   the sync will continue, if the callback returns `Ok(LoopCtrl::Break)`
    ///   the sync will be stopped and the function returns `Ok(())`. In case
    ///   the callback can't handle the `Error` or has a different malfunction,
    ///   it can return an `Err(Error)`, which results in the sync ending and
    ///   the `Err(Error)` being returned.
    ///
    /// # Return
    /// The sync runs until an error occurs that the callback can't handle or
    /// the callback indicates that the Loop should stop. If the callback
    /// asked for a regular stop, the result will be `Ok(())` otherwise the
    /// `Err(Error)` is returned.
    ///
    /// _Note_: Lower-level configuration (e.g. for retries) are not changed by
    /// this, and are handled first without sending the result to the
    /// callback. Only after they have exceeded is the `Result` handed to
    /// the callback.
    ///
    /// # Examples
    ///
    /// The following example demonstrates how to sync forever while sending all
    /// the interesting events through a mpsc channel to another thread e.g. a
    /// UI thread.
    ///
    /// ```no_run
    /// # use std::time::Duration;
    /// # use matrix_sdk::{Client, config::SyncSettings, LoopCtrl};
    /// # use url::Url;
    /// # use futures::executor::block_on;
    /// # block_on(async {
    /// # let homeserver = Url::parse("http://localhost:8080").unwrap();
    /// # let mut client = Client::new(homeserver).await.unwrap();
    ///
    /// use tokio::sync::mpsc::channel;
    ///
    /// let (tx, rx) = channel(100);
    ///
    /// let sync_channel = &tx;
    /// let sync_settings = SyncSettings::new()
    ///     .timeout(Duration::from_secs(30));
    ///
    /// client
    ///     .sync_with_result_callback(sync_settings, |response| async move {
    ///         let channel = sync_channel;
    ///         let sync_response = response?;
    ///         for (room_id, room) in sync_response.rooms.join {
    ///              for event in room.timeline.events {
    ///                  channel.send(event).await.unwrap();
    ///               }
    ///         }
    ///
    ///         Ok(LoopCtrl::Continue)
    ///     })
    ///     .await;
    /// })
    /// ```
    #[instrument(skip(self, callback))]
    pub async fn sync_with_result_callback<C>(
        &self,
        mut sync_settings: crate::config::SyncSettings,
        callback: impl Fn(Result<SyncResponse, Error>) -> C,
    ) -> Result<(), Error>
    where
        C: Future<Output = Result<LoopCtrl, Error>>,
    {
        let mut last_sync_time: Option<Instant> = None;

        if sync_settings.token.is_none() {
            sync_settings.token = self.sync_token().await;
        }

        loop {
            trace!("Syncing");
            let result = self.sync_loop_helper(&mut sync_settings).await;

            trace!("Running callback");
            if callback(result).await? == LoopCtrl::Break {
                trace!("Callback told us to stop");
                break;
            }
            trace!("Done running callback");

            Client::delay_sync(&mut last_sync_time).await
        }

        Ok(())
    }

    //// Repeatedly synchronize the client state with the server.
    ///
    /// This method will internally call [`Client::sync_once`] in a loop and is
    /// equivalent to the [`Client::sync`] method but the responses are provided
    /// as an async stream.
    ///
    /// # Arguments
    ///
    /// * `sync_settings` - Settings for the sync call. *Note* that those
    ///   settings will be only used for the first sync call. See the argument
    ///   docs for [`Client::sync_once`] for more info.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use url::Url;
    /// # use futures::executor::block_on;
    /// # block_on(async {
    /// # let homeserver = Url::parse("http://localhost:8080")?;
    /// # let username = "";
    /// # let password = "";
    /// use futures::StreamExt;
    /// use matrix_sdk::{config::SyncSettings, Client};
    ///
    /// let client = Client::new(homeserver).await?;
    /// client.login_username(&username, &password).send().await?;
    ///
    /// let mut sync_stream =
    ///     Box::pin(client.sync_stream(SyncSettings::default()).await);
    ///
    /// while let Some(Ok(response)) = sync_stream.next().await {
    ///     for room in response.rooms.join.values() {
    ///         for e in &room.timeline.events {
    ///             if let Ok(event) = e.event.deserialize() {
    ///                 println!("Received event {:?}", event);
    ///             }
    ///         }
    ///     }
    /// }
    ///
    /// # anyhow::Ok(()) });
    /// ```
    #[instrument(skip(self), parent = &self.inner.root_span)]
    pub async fn sync_stream(
        &self,
        mut sync_settings: crate::config::SyncSettings,
    ) -> impl Stream<Item = Result<SyncResponse>> + '_ {
        let mut last_sync_time: Option<Instant> = None;

        if sync_settings.token.is_none() {
            sync_settings.token = self.sync_token().await;
        }

        let parent_span = Span::current();

        async_stream::stream! {
            loop {
                yield self.sync_loop_helper(&mut sync_settings).instrument(parent_span.clone()).await;

                Client::delay_sync(&mut last_sync_time).await
            }
        }
    }

    /// Get the current, if any, sync token of the client.
    /// This will be None if the client didn't sync at least once.
    pub(crate) async fn sync_token(&self) -> Option<String> {
        self.inner.base_client.sync_token().await
    }

    /// Gets information about the owner of a given access token.
    pub async fn whoami(&self) -> HttpResult<whoami::v3::Response> {
        let request = whoami::v3::Request::new();
        self.send(request, None).await
    }

    /// Log out the current user
    pub async fn logout(&self) -> HttpResult<logout::v3::Response> {
        let request = logout::v3::Request::new();
        self.send(request, None).await
    }

    /// Subscribes a new receiver to client UnknownToken errors
    pub fn subscribe_to_unknown_token_errors(&self) -> broadcast::Receiver<UnknownToken> {
        let broadcast = &self.inner.unknown_token_error_sender;
        broadcast.subscribe()
    }
}

// The http mocking library is not supported for wasm32
#[cfg(all(test, not(target_arch = "wasm32")))]
pub(crate) mod tests {
    use std::time::Duration;

    use matrix_sdk_test::{async_test, test_json, EventBuilder, JoinedRoomBuilder, StateTestEvent};
    #[cfg(target_arch = "wasm32")]
    wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_browser);

    use ruma::{events::ignored_user_list::IgnoredUserListEventContent, UserId};
    use url::Url;
    use wiremock::{
        matchers::{header, method, path},
        Mock, MockServer, ResponseTemplate,
    };

    use super::Client;
    use crate::{
        config::{RequestConfig, SyncSettings},
        test_utils::{logged_in_client, no_retry_test_client, test_client_builder},
    };

    #[async_test]
    async fn account_data() {
        let server = MockServer::start().await;
        let client = logged_in_client(Some(server.uri())).await;

        Mock::given(method("GET"))
            .and(path("/_matrix/client/r0/sync".to_owned()))
            .and(header("authorization", "Bearer 1234"))
            .respond_with(ResponseTemplate::new(200).set_body_json(&*test_json::SYNC))
            .mount(&server)
            .await;

        let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));
        let _response = client.sync_once(sync_settings).await.unwrap();

        let content = client
            .account()
            .account_data::<IgnoredUserListEventContent>()
            .await
            .unwrap()
            .unwrap()
            .deserialize()
            .unwrap();

        assert_eq!(content.ignored_users.len(), 1);
    }

    #[async_test]
    async fn successful_discovery() {
        let server = MockServer::start().await;
        let server_url = server.uri();
        let domain = server_url.strip_prefix("http://").unwrap();
        let alice = UserId::parse("@alice:".to_owned() + domain).unwrap();

        Mock::given(method("GET"))
            .and(path("/.well-known/matrix/client"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(
                test_json::WELL_KNOWN.to_string().replace("HOMESERVER_URL", server_url.as_ref()),
                "application/json",
            ))
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path("/_matrix/client/versions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(&*test_json::VERSIONS))
            .mount(&server)
            .await;
        let client = Client::builder().server_name(alice.server_name()).build().await.unwrap();

        assert_eq!(client.homeserver().await, Url::parse(server_url.as_ref()).unwrap());
    }

    #[async_test]
    async fn discovery_broken_server() {
        let server = MockServer::start().await;
        let server_url = server.uri();
        let domain = server_url.strip_prefix("http://").unwrap();
        let alice = UserId::parse("@alice:".to_owned() + domain).unwrap();

        Mock::given(method("GET"))
            .and(path("/.well-known/matrix/client"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;

        assert!(
            Client::builder().server_name(alice.server_name()).build().await.is_err(),
            "Creating a client from a user ID should fail when the .well-known request fails."
        );
    }

    #[async_test]
    async fn room_creation() {
        let server = MockServer::start().await;
        let client = logged_in_client(Some(server.uri())).await;

        let response = EventBuilder::default()
            .add_joined_room(
                JoinedRoomBuilder::default()
                    .add_state_event(StateTestEvent::Member)
                    .add_state_event(StateTestEvent::PowerLevels),
            )
            .build_sync_response();

        client.inner.base_client.receive_sync_response(response).await.unwrap();
        let room_id = &test_json::DEFAULT_SYNC_ROOM_ID;

        assert_eq!(client.homeserver().await, Url::parse(&server.uri()).unwrap());

        let room = client.get_joined_room(room_id);
        assert!(room.is_some());
    }

    #[async_test]
    async fn retry_limit_http_requests() {
        let server = MockServer::start().await;
        let client = test_client_builder(Some(server.uri()))
            .request_config(RequestConfig::new().retry_limit(3))
            .build()
            .await
            .unwrap();

        assert!(client.request_config().retry_limit.unwrap() == 3);

        Mock::given(method("POST"))
            .and(path("/_matrix/client/r0/login"))
            .respond_with(ResponseTemplate::new(501))
            .expect(3)
            .mount(&server)
            .await;

        client.login_username("example", "wordpass").send().await.unwrap_err();
    }

    #[async_test]
    async fn retry_timeout_http_requests() {
        // Keep this timeout small so that the test doesn't take long
        let retry_timeout = Duration::from_secs(5);
        let server = MockServer::start().await;
        let client = test_client_builder(Some(server.uri()))
            .request_config(RequestConfig::new().retry_timeout(retry_timeout))
            .build()
            .await
            .unwrap();

        assert!(client.request_config().retry_timeout.unwrap() == retry_timeout);

        Mock::given(method("POST"))
            .and(path("/_matrix/client/r0/login"))
            .respond_with(ResponseTemplate::new(501))
            .expect(2..)
            .mount(&server)
            .await;

        client.login_username("example", "wordpass").send().await.unwrap_err();
    }

    #[async_test]
    async fn short_retry_initial_http_requests() {
        let server = MockServer::start().await;
        let client = test_client_builder(Some(server.uri())).build().await.unwrap();

        Mock::given(method("POST"))
            .and(path("/_matrix/client/r0/login"))
            .respond_with(ResponseTemplate::new(501))
            .expect(3..)
            .mount(&server)
            .await;

        client.login_username("example", "wordpass").send().await.unwrap_err();
    }

    #[async_test]
    async fn no_retry_http_requests() {
        let server = MockServer::start().await;
        let client = logged_in_client(Some(server.uri())).await;

        Mock::given(method("GET"))
            .and(path("/_matrix/client/r0/devices"))
            .respond_with(ResponseTemplate::new(501))
            .expect(1)
            .mount(&server)
            .await;

        client.devices().await.unwrap_err();
    }

    #[async_test]
    async fn set_homeserver() {
        let client = no_retry_test_client(Some("http://localhost".to_owned())).await;
        assert_eq!(client.homeserver().await.as_ref(), "http://localhost/");

        let homeserver = Url::parse("http://example.com/").unwrap();
        client.set_homeserver(homeserver.clone()).await;
        assert_eq!(client.homeserver().await, homeserver);
    }
}
