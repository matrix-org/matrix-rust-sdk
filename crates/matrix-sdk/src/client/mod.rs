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

#[cfg(feature = "experimental-sliding-sync")]
use std::sync::RwLock as StdRwLock;
use std::{
    collections::{btree_map, hash_map::DefaultHasher, BTreeMap},
    fmt::{self, Debug},
    future::Future,
    hash::{Hash, Hasher},
    pin::Pin,
    sync::{Arc, Mutex as StdMutex},
};

use dashmap::DashMap;
use eyeball::{Observable, SharedObservable, Subscriber};
use futures_core::Stream;
#[cfg(feature = "experimental-oidc")]
use mas_oidc_client::{
    error::{
        Error as OidcClientError, ErrorBody as OidcErrorBody, HttpError as OidcHttpError,
        TokenRefreshError, TokenRequestError,
    },
    types::errors::ClientErrorCode,
};
#[cfg(feature = "e2e-encryption")]
use matrix_sdk_base::crypto::store::locks::CryptoStoreLock;
use matrix_sdk_base::{
    store::DynStateStore, BaseClient, RoomState, RoomStateFilter, SendOutsideWasm, SessionMeta,
    SyncOutsideWasm,
};
use matrix_sdk_common::instant::Instant;
#[cfg(feature = "experimental-sliding-sync")]
use ruma::api::client::error::ErrorKind;
use ruma::{
    api::{
        client::{
            account::whoami,
            alias::get_alias,
            device::{delete_devices, get_devices, update_device},
            directory::{get_public_rooms, get_public_rooms_filtered},
            discovery::{
                get_capabilities::{self, Capabilities},
                get_supported_versions,
            },
            filter::{create_filter::v3::Request as FilterUploadRequest, FilterDefinition},
            membership::{join_room_by_id, join_room_by_id_or_alias},
            profile::get_profile,
            push::{get_notifications::v3::Notification, set_pusher, Pusher},
            room::create_room,
            session::login::v3::DiscoveryInfo,
            sync::sync_events,
            uiaa,
            user_directory::search_users,
        },
        error::FromHttpResponseError,
        MatrixVersion, OutgoingRequest,
    },
    assign,
    push::Ruleset,
    DeviceId, OwnedDeviceId, OwnedRoomId, OwnedServerName, RoomAliasId, RoomId, RoomOrAliasId,
    ServerName, UInt, UserId,
};
use serde::de::DeserializeOwned;
use tokio::sync::{broadcast, Mutex, OnceCell, RwLock, RwLockReadGuard};
use tracing::{debug, error, info, instrument, trace, Instrument, Span};
use url::Url;

#[cfg(feature = "e2e-encryption")]
use crate::encryption::Encryption;
#[cfg(feature = "experimental-oidc")]
use crate::oidc::{Oidc, OidcError};
use crate::{
    authentication::{AuthCtx, AuthData},
    config::RequestConfig,
    error::{HttpError, HttpResult},
    event_handler::{
        EventHandler, EventHandlerDropGuard, EventHandlerHandle, EventHandlerStore, SyncEvent,
    },
    http_client::HttpClient,
    matrix_auth::MatrixAuth,
    notification_settings::NotificationSettings,
    sync::{RoomUpdate, SyncResponse},
    Account, AuthApi, AuthSession, Error, Media, RefreshTokenError, Result, Room,
    TransmissionProgress,
};

mod builder;
mod futures;

pub use self::{
    builder::{ClientBuildError, ClientBuilder},
    futures::SendRequest,
};

#[cfg(not(target_arch = "wasm32"))]
type NotificationHandlerFut = Pin<Box<dyn Future<Output = ()> + Send>>;
#[cfg(target_arch = "wasm32")]
type NotificationHandlerFut = Pin<Box<dyn Future<Output = ()>>>;

#[cfg(not(target_arch = "wasm32"))]
type NotificationHandlerFn =
    Box<dyn Fn(Notification, Room, Client) -> NotificationHandlerFut + Send + Sync>;
#[cfg(target_arch = "wasm32")]
type NotificationHandlerFn = Box<dyn Fn(Notification, Room, Client) -> NotificationHandlerFut>;

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

/// Represents changes that can occur to a `Client`s `Session`.
#[derive(Debug, Clone)]
pub enum SessionChange {
    /// The session's token is no longer valid.
    UnknownToken {
        /// Whether or not the session was soft logged out
        soft_logout: bool,
    },
    /// The session's tokens have been refreshed.
    TokensRefreshed,
}

/// An async/await enabled Matrix client.
///
/// All of the state is held in an `Arc` so the `Client` can be cloned freely.
#[derive(Clone)]
pub struct Client {
    pub(crate) inner: Arc<ClientInner>,
}

pub(crate) struct ClientInner {
    /// All the data related to authentication and authorization.
    pub(crate) auth_ctx: Arc<AuthCtx>,

    /// The URL of the homeserver to connect to.
    homeserver: RwLock<Url>,
    /// The sliding sync proxy that is trusted by the homeserver.
    #[cfg(feature = "experimental-sliding-sync")]
    sliding_sync_proxy: StdRwLock<Option<Url>>,
    /// The underlying HTTP client.
    pub(crate) http_client: HttpClient,
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
    pub(crate) encryption_state_request_locks: Mutex<BTreeMap<OwnedRoomId, Arc<Mutex<()>>>>,
    pub(crate) typing_notice_times: DashMap<OwnedRoomId, Instant>,
    /// Event handlers. See `add_event_handler`.
    pub(crate) event_handlers: EventHandlerStore,
    /// Notification handlers. See `register_notification_handler`.
    notification_handlers: RwLock<Vec<NotificationHandlerFn>>,
    pub(crate) room_update_channels: StdMutex<BTreeMap<OwnedRoomId, broadcast::Sender<RoomUpdate>>>,
    pub(crate) sync_gap_broadcast_txs: StdMutex<BTreeMap<OwnedRoomId, Observable<()>>>,
    /// Whether the client should update its homeserver URL with the discovery
    /// information present in the login response.
    respect_login_well_known: bool,
    /// Lock making sure we're only doing one token refresh at a time.
    pub(crate) refresh_token_lock: Mutex<Result<(), RefreshTokenError>>,
    /// An event that can be listened on to wait for a successful sync. The
    /// event will only be fired if a sync loop is running. Can be used for
    /// synchronization, e.g. if we send out a request to create a room, we can
    /// wait for the sync to get the data to fetch a room object from the state
    /// store.
    pub(crate) sync_beat: event_listener::Event,
    /// Session change publisher. Allows the subscriber to handle changes to the
    /// session such as logging out when the access token is invalid or
    /// persisting updates to the access/refresh tokens.
    pub(crate) session_change_sender: broadcast::Sender<SessionChange>,
    /// Authentication data to keep in memory.
    pub(crate) auth_data: OnceCell<AuthData>,

    #[cfg(feature = "e2e-encryption")]
    pub(crate) cross_process_crypto_store_lock: OnceCell<CryptoStoreLock>,
    /// Latest "generation" of data known by the crypto store.
    ///
    /// This is a counter that only increments, set in the database (and can
    /// wrap). It's incremented whenever some process acquires a lock for the
    /// first time. *This assumes the crypto store lock is being held, to
    /// avoid data races on writing to this value in the store*.
    ///
    /// The current process will maintain this value in local memory and in the
    /// DB over time. Observing a different value than the one read in
    /// memory, when reading from the store indicates that somebody else has
    /// written into the database under our feet.
    ///
    /// TODO: this should live in the `OlmMachine`, since it's information
    /// related to the lock. As of today (2023-07-28), we blow up the entire
    /// olm machine when there's a generation mismatch. So storing the
    /// generation in the olm machine would make the client think there's
    /// *always* a mismatch, and that's why we need to store the generation
    /// outside the `OlmMachine`.
    #[cfg(feature = "e2e-encryption")]
    pub(crate) crypto_store_generation: Arc<Mutex<Option<u64>>>,
}

impl ClientInner {
    #[allow(clippy::too_many_arguments)]
    fn new(
        auth_ctx: Arc<AuthCtx>,
        homeserver: Url,
        #[cfg(feature = "experimental-sliding-sync")] sliding_sync_proxy: Option<Url>,
        http_client: HttpClient,
        base_client: BaseClient,
        server_versions: Option<Box<[MatrixVersion]>>,
        respect_login_well_known: bool,
    ) -> Self {
        let session_change_sender = broadcast::Sender::new(1);

        Self {
            homeserver: RwLock::new(homeserver),
            auth_ctx,
            #[cfg(feature = "experimental-sliding-sync")]
            sliding_sync_proxy: StdRwLock::new(sliding_sync_proxy),
            http_client,
            base_client,
            server_versions: OnceCell::new_with(server_versions),
            #[cfg(feature = "e2e-encryption")]
            group_session_locks: Default::default(),
            #[cfg(feature = "e2e-encryption")]
            key_claim_lock: Default::default(),
            members_request_locks: Default::default(),
            encryption_state_request_locks: Default::default(),
            typing_notice_times: Default::default(),
            event_handlers: Default::default(),
            notification_handlers: Default::default(),
            room_update_channels: Default::default(),
            sync_gap_broadcast_txs: Default::default(),
            respect_login_well_known,
            sync_beat: event_listener::Event::new(),
            refresh_token_lock: Mutex::new(Ok(())),
            session_change_sender,
            auth_data: Default::default(),
            #[cfg(feature = "e2e-encryption")]
            cross_process_crypto_store_lock: OnceCell::new(),
            #[cfg(feature = "e2e-encryption")]
            crypto_store_generation: Arc::new(Mutex::new(None)),
        }
    }
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

    /// Returns a subscriber that publishes an event every time the ignore user
    /// list changes.
    pub fn subscribe_to_ignore_user_list_changes(&self) -> Subscriber<()> {
        self.inner.base_client.subscribe_to_ignore_user_list_changes()
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
    /// # Examples
    ///
    /// ```no_run
    /// # use matrix_sdk::Client;
    /// # use url::Url;
    /// # async {
    /// # let homeserver = Url::parse("http://example.com")?;
    /// let client = Client::new(homeserver).await?;
    ///
    /// let capabilities = client.get_capabilities().await?;
    ///
    /// if capabilities.change_password.enabled {
    ///     // Change password
    /// }
    /// # anyhow::Ok(()) };
    /// ```
    pub async fn get_capabilities(&self) -> HttpResult<Capabilities> {
        let res = self.send(get_capabilities::v3::Request::new(), None).await?;
        Ok(res.capabilities)
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

    /// The sliding sync proxy that is trusted by the homeserver.
    #[cfg(feature = "experimental-sliding-sync")]
    pub fn sliding_sync_proxy(&self) -> Option<Url> {
        let server = self.inner.sliding_sync_proxy.read().unwrap();
        Some(server.as_ref()?.clone())
    }

    /// Force to set the sliding sync proxy URL.
    #[cfg(feature = "experimental-sliding-sync")]
    pub fn set_sliding_sync_proxy(&self, sliding_sync_proxy: Option<Url>) {
        let mut lock = self.inner.sliding_sync_proxy.write().unwrap();
        *lock = sliding_sync_proxy;
    }

    /// Get the Matrix user session meta information.
    ///
    /// If the client is currently logged in, this will return a
    /// [`SessionMeta`] object which contains the user ID and device ID.
    /// Otherwise it returns `None`.
    pub fn session_meta(&self) -> Option<&SessionMeta> {
        self.base_client().session_meta()
    }

    /// Performs a search for users.
    /// The search is performed case-insensitively on user IDs and display names
    ///
    /// # Arguments
    ///
    /// * `search_term` - The search term for the search
    /// * `limit` - The maximum number of results to return. Defaults to 10.
    ///
    /// [user directory]: https://spec.matrix.org/v1.6/client-server-api/#user-directory
    pub async fn search_users(
        &self,
        search_term: &str,
        limit: u64,
    ) -> HttpResult<search_users::v3::Response> {
        let mut request = search_users::v3::Request::new(search_term.to_owned());

        if let Some(limit) = UInt::new(limit) {
            request.limit = limit;
        }

        self.send(request, None).await
    }

    /// Get the user id of the current owner of the client.
    pub fn user_id(&self) -> Option<&UserId> {
        self.session_meta().map(|s| s.user_id.as_ref())
    }

    /// Get the device ID that identifies the current session.
    pub fn device_id(&self) -> Option<&DeviceId> {
        self.session_meta().map(|s| s.device_id.as_ref())
    }

    /// Get the current access token for this session, regardless of the
    /// authentication API used to log in.
    ///
    /// Will be `None` if the client has not been logged in.
    pub fn access_token(&self) -> Option<String> {
        self.inner.auth_data.get()?.access_token()
    }

    /// Access the authentication API used to log in this client.
    ///
    /// Will be `None` if the client has not been logged in.
    pub fn auth_api(&self) -> Option<AuthApi> {
        match self.inner.auth_data.get()? {
            AuthData::Matrix(_) => Some(AuthApi::Matrix(self.matrix_auth())),
            #[cfg(feature = "experimental-oidc")]
            AuthData::Oidc(_) => Some(AuthApi::Oidc(self.oidc())),
        }
    }

    /// Get the whole session info of this client.
    ///
    /// Will be `None` if the client has not been logged in.
    ///
    /// Can be used with [`Client::restore_session`] to restore a previously
    /// logged-in session.
    pub fn session(&self) -> Option<AuthSession> {
        match self.auth_api()? {
            AuthApi::Matrix(api) => api.session().map(Into::into),
            #[cfg(feature = "experimental-oidc")]
            AuthApi::Oidc(api) => api.full_session().map(Into::into),
        }
    }

    /// Get a reference to the state store.
    pub fn store(&self) -> &DynStateStore {
        self.base_client().store()
    }

    /// Access the native Matrix authentication API with this client.
    pub fn matrix_auth(&self) -> MatrixAuth {
        MatrixAuth::new(self.clone())
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

    /// Access the OpenID Connect API of the client.
    #[cfg(feature = "experimental-oidc")]
    pub fn oidc(&self) -> Oidc {
        Oidc::new(self.clone())
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
    /// * [`Room`] is only available for room-specific events, i.e. not for
    ///   events like global account data events or presence events.
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
    /// # use url::Url;
    /// # let homeserver = Url::parse("http://localhost:8080").unwrap();
    /// use matrix_sdk::{
    ///     deserialized_responses::EncryptionInfo,
    ///     event_handler::Ctx,
    ///     ruma::{
    ///         events::{
    ///             macros::EventContent,
    ///             push_rules::PushRulesEvent,
    ///             room::{message::SyncRoomMessageEvent, topic::SyncRoomTopicEvent},
    ///         },
    ///         push::Action,
    ///         Int, MilliSecondsSinceUnixEpoch,
    ///     },
    ///     Client, Room,
    /// };
    /// use serde::{Deserialize, Serialize};
    ///
    /// # futures_executor::block_on(async {
    /// # let client = matrix_sdk::Client::builder()
    /// #     .homeserver_url(homeserver)
    /// #     .server_versions([ruma::api::MatrixVersion::V1_0])
    /// #     .build()
    /// #     .await
    /// #     .unwrap();
    /// #
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
    /// client.add_event_handler(
    ///     |ev: SyncRoomMessageEvent, room: Room, push_actions: Vec<Action>| {
    ///         async move {
    ///             // A `Vec<Action>` parameter allows you to know which push actions
    ///             // are applicable for an event. For example, an event with
    ///             // `Action::SetTweak(Tweak::Highlight(true))` should be highlighted
    ///             // in the timeline.
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
    /// // Event handler closures can also capture local variables.
    /// // Make sure they are cheap to clone though, because they will be cloned
    /// // every time the closure is called.
    /// let data: std::sync::Arc<str> = "MyCustomIdentifier".into();
    ///
    /// client.add_event_handler(move |ev: SyncRoomMessageEvent | async move {
    ///     println!("Calling the handler with identifier {data}");
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
    /// # futures_executor::block_on(async {
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
    /// # Examples
    ///
    /// ```
    /// use matrix_sdk::{
    ///     event_handler::Ctx, ruma::events::room::message::SyncRoomMessageEvent,
    ///     Room,
    /// };
    /// # #[derive(Clone)]
    /// # struct SomeType;
    /// # fn obtain_gui_handle() -> SomeType { SomeType }
    /// # let homeserver = url::Url::parse("http://localhost:8080").unwrap();
    /// # futures_executor::block_on(async {
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
    /// or closures with exactly the three arguments [`Notification`], [`Room`],
    /// [`Client`] for now.
    pub async fn register_notification_handler<H, Fut>(&self, handler: H) -> &Self
    where
        H: Fn(Notification, Room, Client) -> Fut + SendOutsideWasm + SyncOutsideWasm + 'static,
        Fut: Future<Output = ()> + SendOutsideWasm + 'static,
    {
        self.inner.notification_handlers.write().await.push(Box::new(
            move |notification, room, client| Box::pin((handler)(notification, room, client)),
        ));

        self
    }

    /// Subscribe to all updates for the room with the given ID.
    ///
    /// The returned receiver will receive a new message for each sync response
    /// that contains updates for that room.
    pub fn subscribe_to_room_updates(&self, room_id: &RoomId) -> broadcast::Receiver<RoomUpdate> {
        match self.inner.room_update_channels.lock().unwrap().entry(room_id.to_owned()) {
            btree_map::Entry::Vacant(entry) => {
                let (tx, rx) = broadcast::channel(8);
                entry.insert(tx);
                rx
            }
            btree_map::Entry::Occupied(entry) => entry.get().subscribe(),
        }
    }

    pub(crate) async fn notification_handlers(
        &self,
    ) -> RwLockReadGuard<'_, Vec<NotificationHandlerFn>> {
        self.inner.notification_handlers.read().await
    }

    /// Get all the rooms the client knows about.
    ///
    /// This will return the list of joined, invited, and left rooms.
    pub fn rooms(&self) -> Vec<Room> {
        self.base_client()
            .get_rooms()
            .into_iter()
            .map(|room| Room::new(self.clone(), room))
            .collect()
    }

    /// Get all the rooms the client knows about, filtered by room state.
    pub fn rooms_filtered(&self, filter: RoomStateFilter) -> Vec<Room> {
        self.base_client()
            .get_rooms_filtered(filter)
            .into_iter()
            .map(|room| Room::new(self.clone(), room))
            .collect()
    }

    /// Returns the joined rooms this client knows about.
    pub fn joined_rooms(&self) -> Vec<Room> {
        self.base_client()
            .get_rooms_filtered(RoomStateFilter::JOINED)
            .into_iter()
            .map(|room| Room::new(self.clone(), room))
            .collect()
    }

    /// Returns the invited rooms this client knows about.
    pub fn invited_rooms(&self) -> Vec<Room> {
        self.base_client()
            .get_rooms_filtered(RoomStateFilter::INVITED)
            .into_iter()
            .map(|room| Room::new(self.clone(), room))
            .collect()
    }

    /// Returns the left rooms this client knows about.
    pub fn left_rooms(&self) -> Vec<Room> {
        self.base_client()
            .get_rooms_filtered(RoomStateFilter::LEFT)
            .into_iter()
            .map(|room| Room::new(self.clone(), room))
            .collect()
    }

    /// Get a room with the given room id.
    ///
    /// # Arguments
    ///
    /// `room_id` - The unique id of the room that should be fetched.
    pub fn get_room(&self, room_id: &RoomId) -> Option<Room> {
        self.base_client().get_room(room_id).map(|room| Room::new(self.clone(), room))
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

    /// Update the homeserver from the login response well-known if needed.
    ///
    /// # Arguments
    ///
    /// * `login_well_known` - The `well_known` field from a successful login
    ///   response.
    pub(crate) async fn maybe_update_login_well_known(
        &self,
        login_well_known: Option<&DiscoveryInfo>,
    ) {
        if self.inner.respect_login_well_known {
            if let Some(well_known) = login_well_known {
                if let Ok(homeserver) = Url::parse(&well_known.homeserver.base_url) {
                    self.set_homeserver(homeserver).await;
                }
            }
        }
    }

    /// Restore a session previously logged-in using one of the available
    /// authentication APIs.
    ///
    /// See the documentation of the corresponding authentication API's
    /// `restore_session` method for more information.
    ///
    /// # Panics
    ///
    /// Panics if a session was already restored or logged in.
    #[instrument(skip_all)]
    pub async fn restore_session(&self, session: impl Into<AuthSession>) -> Result<()> {
        let session = session.into();
        match session {
            AuthSession::Matrix(s) => self.matrix_auth().restore_session(s).await,
            #[cfg(feature = "experimental-oidc")]
            AuthSession::Oidc(s) => self.oidc().restore_session(s).await,
        }
    }

    /// Refresh the access token using the authentication API used to log into
    /// this session.
    ///
    /// See the documentation of the authentication API's `refresh_access_token`
    /// method for more information.
    pub async fn refresh_access_token(&self) -> Result<(), RefreshTokenError> {
        let Some(auth_api) = self.auth_api() else {
            return Err(RefreshTokenError::RefreshTokenRequired);
        };

        match auth_api {
            AuthApi::Matrix(a) => {
                trace!("Token refresh: Using the homeserver.");
                a.refresh_access_token().await?;
            }
            #[cfg(feature = "experimental-oidc")]
            AuthApi::Oidc(api) => {
                trace!("Token refresh: Using OIDC.");
                api.refresh_access_token().await?;
            }
        }

        Ok(())
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
    /// # use url::Url;
    /// # async {
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
    /// # };
    #[instrument(skip(self, definition))]
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
    pub async fn join_room_by_id(&self, room_id: &RoomId) -> Result<Room> {
        let request = join_room_by_id::v3::Request::new(room_id.to_owned());
        let response = self.send(request, None).await?;
        let base_room = self.base_client().room_joined(&response.room_id).await?;
        Ok(Room::new(self.clone(), base_room))
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
    ) -> Result<Room> {
        let request = assign!(join_room_by_id_or_alias::v3::Request::new(alias.to_owned()), {
            server_name: server_names.to_owned(),
        });
        let response = self.send(request, None).await?;
        let base_room = self.base_client().room_joined(&response.room_id).await?;
        Ok(Room::new(self.clone(), base_room))
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
    /// # async {
    /// let mut client = Client::new(homeserver).await.unwrap();
    ///
    /// client.public_rooms(limit, since, server).await;
    /// # };
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

    /// Create a room with the given parameters.
    ///
    /// Sends a request to `/_matrix/client/r0/createRoom` and returns the
    /// created room.
    ///
    /// If you want to create a direct message with one specific user, you can
    /// use [`create_dm`][Self::create_dm], which is more convenient than
    /// assembling the [`create_room::v3::Request`] yourself.
    ///
    /// If the `is_direct` field of the request is set to `true` and at least
    /// one user is invited, the room will be automatically added to the direct
    /// rooms in the account data.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use matrix_sdk::Client;
    ///
    /// # use matrix_sdk::ruma::api::client::room::{
    /// #     create_room::v3::Request as CreateRoomRequest,
    /// #     Visibility,
    /// # };
    /// # use url::Url;
    /// #
    /// # async {
    /// # let homeserver = Url::parse("http://example.com").unwrap();
    /// let request = CreateRoomRequest::new();
    /// let client = Client::new(homeserver).await.unwrap();
    /// assert!(client.create_room(request).await.is_ok());
    /// # };
    /// ```
    pub async fn create_room(&self, request: create_room::v3::Request) -> Result<Room> {
        let invite = request.invite.clone();
        let is_direct_room = request.is_direct;
        let response = self.send(request, None).await?;

        let base_room = self.base_client().get_or_create_room(&response.room_id, RoomState::Joined);

        let joined_room = Room::new(self.clone(), base_room);

        if is_direct_room && !invite.is_empty() {
            if let Err(error) =
                self.account().mark_as_dm(joined_room.room_id(), invite.as_slice()).await
            {
                // FIXME: Retry in the background
                error!("Failed to mark room as DM: {error}");
            }
        }

        Ok(joined_room)
    }

    /// Create a DM room.
    ///
    /// Convenience shorthand for [`create_room`][Self::create_room] with the
    /// given user being invited, the room marked `is_direct` and both the
    /// creator and invitee getting the default maximum power level.
    pub async fn create_dm(&self, user_id: &UserId) -> Result<Room> {
        self.create_room(assign!(create_room::v3::Request::new(), {
            invite: vec![user_id.to_owned()],
            is_direct: true,
            preset: Some(create_room::v3::RoomPreset::TrustedPrivateChat),
        }))
        .await
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
    /// # async {
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
    ///     println!("Found room {room:?}");
    /// }
    /// # anyhow::Ok(()) };
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
    /// # Examples
    ///
    /// ```no_run
    /// # use matrix_sdk::{Client, config::SyncSettings};
    /// # use url::Url;
    /// # async {
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
    /// # anyhow::Ok(()) };
    /// ```
    pub fn send<Request>(
        &self,
        request: Request,
        config: Option<RequestConfig>,
    ) -> SendRequest<Request>
    where
        Request: OutgoingRequest + Clone + Debug,
        HttpError: From<FromHttpResponseError<Request::EndpointError>>,
    {
        SendRequest { client: self.clone(), request, config, send_progress: Default::default() }
    }

    #[cfg(feature = "experimental-sliding-sync")]
    // FIXME: remove this as soon as Sliding-Sync isn't needing an external server
    // anymore
    pub(crate) async fn send_with_homeserver<Request>(
        &self,
        request: Request,
        config: Option<RequestConfig>,
        sliding_sync_proxy: Option<String>,
    ) -> HttpResult<Request::IncomingResponse>
    where
        Request: OutgoingRequest + Clone + Debug,
        HttpError: From<FromHttpResponseError<Request::EndpointError>>,
    {
        let res = Box::pin(self.send_inner(
            request.clone(),
            config,
            sliding_sync_proxy.clone(),
            Default::default(),
        ))
        .await;

        // An `M_UNKNOWN_TOKEN` error can potentially be fixed with a token refresh.
        if let Err(Some(ErrorKind::UnknownToken { soft_logout })) =
            res.as_ref().map_err(HttpError::client_api_error_kind)
        {
            trace!("Token refresh: Unknown token error received.");
            // If automatic token refresh isn't supported, there is nothing more to do.
            if !self.inner.auth_ctx.handle_refresh_tokens {
                trace!("Token refresh: Automatic refresh disabled.");
                self.broadcast_unknown_token(soft_logout);
                return res;
            }

            // Try to refresh the token and retry the request.
            if let Err(refresh_error) = self.refresh_access_token().await {
                match &refresh_error {
                    RefreshTokenError::RefreshTokenRequired => {
                        trace!("Token refresh: The session doesn't have a refresh token.");
                        // Refreshing access tokens is not supported by this `Session`, ignore.
                        self.broadcast_unknown_token(soft_logout);
                    }
                    #[cfg(feature = "experimental-oidc")]
                    RefreshTokenError::Oidc(oidc_error) => {
                        match **oidc_error {
                            OidcError::Oidc(OidcClientError::TokenRefresh(
                                TokenRefreshError::Token(TokenRequestError::Http(OidcHttpError {
                                    body:
                                        Some(OidcErrorBody {
                                            error: ClientErrorCode::InvalidGrant, ..
                                        }),
                                    ..
                                })),
                            )) => {
                                error!(
                                    "Token refresh: OIDC refresh_token rejected with invalid grant"
                                );
                                // The refresh was denied, signal to sign out the user.
                                self.broadcast_unknown_token(soft_logout);
                            }
                            _ => {
                                trace!("Token refresh: OIDC refresh encountered a problem.");
                                // The refresh failed for other reasons, no need
                                // to sign out.
                            }
                        };
                        return Err(refresh_error.into());
                    }
                    _ => {
                        trace!("Token refresh: Token refresh failed.");
                        // This isn't necessarily correct, but matches the behaviour when
                        // implementing OIDC.
                        self.broadcast_unknown_token(soft_logout);
                        return Err(refresh_error.into());
                    }
                }
            } else {
                trace!("Token refresh: Refresh succeeded, retrying request.");
                return Box::pin(self.send_inner(
                    request,
                    config,
                    sliding_sync_proxy,
                    Default::default(),
                ))
                .await;
            }
        }

        res
    }

    pub(crate) async fn send_inner<Request>(
        &self,
        request: Request,
        config: Option<RequestConfig>,
        homeserver: Option<String>,
        send_progress: SharedObservable<TransmissionProgress>,
    ) -> HttpResult<Request::IncomingResponse>
    where
        Request: OutgoingRequest + Debug,
        HttpError: From<FromHttpResponseError<Request::EndpointError>>,
    {
        let homeserver = match homeserver {
            Some(hs) => hs,
            None => self.homeserver().await.to_string(),
        };

        let access_token = self.access_token();
        let access_token = access_token.as_deref();
        {
            let hash = access_token.as_ref().map(|t| {
                let mut hasher = DefaultHasher::new();
                t.hash(&mut hasher);
                hasher.finish()
            });
            tracing::trace!("Attempting request with access_token {hash:?}");
        }

        self.inner
            .http_client
            .send(
                request,
                config,
                homeserver,
                access_token,
                self.server_versions().await?,
                send_progress,
            )
            .await
    }

    fn broadcast_unknown_token(&self, soft_logout: &bool) {
        info!("An unknown token error has been encountered.");
        _ = self
            .inner
            .session_change_sender
            .send(SessionChange::UnknownToken { soft_logout: *soft_logout });
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
                &[MatrixVersion::V1_0],
                Default::default(),
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
    /// # use url::Url;
    /// # async {
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
    /// # anyhow::Ok(()) };
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
    /// # use serde_json::json;
    /// # use url::Url;
    /// # use std::collections::BTreeMap;
    /// # async {
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
    /// # anyhow::Ok(()) };
    pub async fn delete_devices(
        &self,
        devices: &[OwnedDeviceId],
        auth_data: Option<uiaa::AuthData>,
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
    /// # async {
    /// # let homeserver = Url::parse("http://localhost:8080")?;
    /// # let username = "";
    /// # let password = "";
    /// use matrix_sdk::{
    ///     config::SyncSettings,
    ///     ruma::events::room::message::OriginalSyncRoomMessageEvent, Client,
    /// };
    ///
    /// let client = Client::new(homeserver).await?;
    /// client.matrix_auth().login_username(username, password).send().await?;
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
    /// # anyhow::Ok(()) };
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
            filter: sync_settings.filter.map(|f| *f),
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
    /// # async {
    /// # let homeserver = Url::parse("http://localhost:8080")?;
    /// # let username = "";
    /// # let password = "";
    /// use matrix_sdk::{
    ///     config::SyncSettings,
    ///     ruma::events::room::message::OriginalSyncRoomMessageEvent, Client,
    /// };
    ///
    /// let client = Client::new(homeserver).await?;
    /// client.matrix_auth().login_username(&username, &password).send().await?;
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
    /// # anyhow::Ok(()) };
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
    /// # async {
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
    /// };
    /// ```
    #[instrument(skip_all)]
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
    ///   `Result<LoopCtrl, Error>`, too. When returning
    ///   `Ok(LoopCtrl::Continue)` the sync will continue, if the callback
    ///   returns `Ok(LoopCtrl::Break)` the sync will be stopped and the
    ///   function returns `Ok(())`. In case the callback can't handle the
    ///   `Error` or has a different malfunction, it can return an `Err(Error)`,
    ///   which results in the sync ending and the `Err(Error)` being returned.
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
    /// # async {
    /// # let homeserver = Url::parse("http://localhost:8080").unwrap();
    /// # let mut client = Client::new(homeserver).await.unwrap();
    /// #
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
    /// };
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
    /// # async {
    /// # let homeserver = Url::parse("http://localhost:8080")?;
    /// # let username = "";
    /// # let password = "";
    /// use futures_util::StreamExt;
    /// use matrix_sdk::{config::SyncSettings, Client};
    ///
    /// let client = Client::new(homeserver).await?;
    /// client.matrix_auth().login_username(&username, &password).send().await?;
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
    /// # anyhow::Ok(()) };
    /// ```
    #[allow(unknown_lints, clippy::let_with_type_underscore)] // triggered by instrument macro
    #[instrument(skip(self))]
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

    /// Subscribes a new receiver to client SessionChange broadcasts.
    pub fn subscribe_to_session_changes(&self) -> broadcast::Receiver<SessionChange> {
        let broadcast = &self.inner.session_change_sender;
        broadcast.subscribe()
    }

    /// Sets a given pusher
    pub async fn set_pusher(&self, pusher: Pusher) -> HttpResult<set_pusher::v3::Response> {
        let request = set_pusher::v3::Request::post(pusher);
        self.send(request, None).await
    }

    /// Subscribe to sync gaps for the given room.
    ///
    /// This method is meant to be removed in favor of making event handlers
    /// more general in the future.
    pub fn subscribe_sync_gap(&self, room_id: &RoomId) -> Subscriber<()> {
        let mut lock = self.inner.sync_gap_broadcast_txs.lock().unwrap();
        let observable = lock.entry(room_id.to_owned()).or_default();
        Observable::subscribe(observable)
    }

    /// Get the profile for a given user id
    ///
    /// # Arguments
    ///
    /// * `user_id` the matrix id this function downloads the profile for
    pub async fn get_profile(&self, user_id: &UserId) -> Result<get_profile::v3::Response> {
        let request = get_profile::v3::Request::new(user_id.to_owned());
        Ok(self.send(request, Some(RequestConfig::short_retry())).await?)
    }

    /// Get the notification settings of the current owner of the client.
    pub async fn notification_settings(&self) -> NotificationSettings {
        let ruleset = self.account().push_rules().await.unwrap_or_else(|_| Ruleset::new());
        NotificationSettings::new(self.clone(), ruleset)
    }

    /// Create a new specialized `Client` that can process notifications.
    pub async fn notification_client(&self) -> Result<Client> {
        let client = Client {
            inner: Arc::new(ClientInner::new(
                self.inner.auth_ctx.clone(),
                self.inner.homeserver.read().await.clone(),
                #[cfg(feature = "experimental-sliding-sync")]
                self.inner.sliding_sync_proxy.read().unwrap().clone(),
                self.inner.http_client.clone(),
                self.inner.base_client.clone_with_in_memory_state_store(),
                self.inner.server_versions.get().cloned(),
                self.inner.respect_login_well_known,
            )),
        };

        // Copy the parent's session into the child.
        if let Some(session) = self.session() {
            client.restore_session(session).await?;
        }

        Ok(client)
    }
}

// The http mocking library is not supported for wasm32
#[cfg(all(test, not(target_arch = "wasm32")))]
pub(crate) mod tests {
    use std::time::Duration;

    use matrix_sdk_base::RoomState;
    use matrix_sdk_test::{
        async_test, test_json, JoinedRoomBuilder, StateTestEvent, SyncResponseBuilder,
    };
    #[cfg(target_arch = "wasm32")]
    wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_browser);

    use ruma::{events::ignored_user_list::IgnoredUserListEventContent, UserId};
    use url::Url;
    use wiremock::{
        matchers::{body_json, header, method, path},
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
        let client = Client::builder()
            .insecure_server_name_no_tls(alice.server_name())
            .build()
            .await
            .unwrap();

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
            Client::builder()
                .insecure_server_name_no_tls(alice.server_name())
                .build()
                .await
                .is_err(),
            "Creating a client from a user ID should fail when the .well-known request fails."
        );
    }

    #[async_test]
    async fn room_creation() {
        let server = MockServer::start().await;
        let client = logged_in_client(Some(server.uri())).await;

        let response = SyncResponseBuilder::default()
            .add_joined_room(
                JoinedRoomBuilder::default()
                    .add_state_event(StateTestEvent::Member)
                    .add_state_event(StateTestEvent::PowerLevels),
            )
            .build_sync_response();

        client.inner.base_client.receive_sync_response(response).await.unwrap();
        let room_id = &test_json::DEFAULT_SYNC_ROOM_ID;

        assert_eq!(client.homeserver().await, Url::parse(&server.uri()).unwrap());

        let room = client.get_room(room_id).unwrap();
        assert_eq!(room.state(), RoomState::Joined);
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

        client.matrix_auth().login_username("example", "wordpass").send().await.unwrap_err();
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

        client.matrix_auth().login_username("example", "wordpass").send().await.unwrap_err();
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

        client.matrix_auth().login_username("example", "wordpass").send().await.unwrap_err();
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

    #[async_test]
    async fn search_user_request() {
        let server = MockServer::start().await;
        let client = logged_in_client(Some(server.uri())).await;

        Mock::given(method("POST"))
            .and(path("_matrix/client/r0/user_directory/search"))
            .and(body_json(&*test_json::search_users::SEARCH_USERS_REQUEST))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(&*test_json::search_users::SEARCH_USERS_RESPONSE),
            )
            .mount(&server)
            .await;

        let response = client.search_users("test", 50).await.unwrap();
        let result = response.results.first().unwrap();
        assert_eq!(result.user_id.to_string(), "@test:example.me");
        assert_eq!(result.display_name.clone().unwrap(), "Test");
        assert_eq!(result.avatar_url.clone().unwrap().to_string(), "mxc://example.me/someid");
        assert_eq!(response.results.len(), 1);
        assert!(!response.limited);
    }
}
