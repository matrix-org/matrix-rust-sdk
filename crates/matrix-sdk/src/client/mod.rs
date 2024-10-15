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
    collections::{btree_map, BTreeMap},
    fmt::{self, Debug},
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex as StdMutex, RwLock as StdRwLock, Weak},
};

use eyeball::{SharedObservable, Subscriber};
use eyeball_im::VectorDiff;
use futures_util::{Stream, StreamExt};
use imbl::Vector;
#[cfg(feature = "e2e-encryption")]
use matrix_sdk_base::crypto::store::LockableCryptoStore;
use matrix_sdk_base::{
    event_cache_store::DynEventCacheStore,
    store::{DynStateStore, ServerCapabilities},
    sync::{Notification, RoomUpdates},
    BaseClient, RoomInfoNotableUpdate, RoomState, RoomStateFilter, SendOutsideWasm, SessionMeta,
    StateStoreDataKey, StateStoreDataValue,
};
#[cfg(feature = "e2e-encryption")]
use ruma::events::{room::encryption::RoomEncryptionEventContent, InitialStateEvent};
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
            knock::knock_room,
            membership::{join_room_by_id, join_room_by_id_or_alias},
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
    time::Instant,
    DeviceId, OwnedDeviceId, OwnedEventId, OwnedRoomId, OwnedRoomOrAliasId, OwnedServerName,
    RoomAliasId, RoomId, RoomOrAliasId, ServerName, UInt, UserId,
};
use serde::de::DeserializeOwned;
use tokio::sync::{broadcast, Mutex, OnceCell, RwLock, RwLockReadGuard};
use tracing::{debug, error, instrument, trace, warn, Instrument, Span};
use url::Url;

use self::futures::SendRequest;
#[cfg(feature = "experimental-oidc")]
use crate::oidc::Oidc;
#[cfg(feature = "experimental-sliding-sync")]
use crate::sliding_sync::Version as SlidingSyncVersion;
use crate::{
    authentication::{AuthCtx, AuthData, ReloadSessionCallback, SaveSessionCallback},
    config::RequestConfig,
    deduplicating_handler::DeduplicatingHandler,
    error::{HttpError, HttpResult},
    event_cache::EventCache,
    event_handler::{
        EventHandler, EventHandlerDropGuard, EventHandlerHandle, EventHandlerStore, SyncEvent,
    },
    http_client::HttpClient,
    matrix_auth::MatrixAuth,
    notification_settings::NotificationSettings,
    room_preview::RoomPreview,
    send_queue::SendQueueData,
    sync::{RoomUpdate, SyncResponse},
    Account, AuthApi, AuthSession, Error, Media, Pusher, RefreshTokenError, Result, Room,
    TransmissionProgress,
};
#[cfg(feature = "e2e-encryption")]
use crate::{
    encryption::{Encryption, EncryptionData, EncryptionSettings, VerificationState},
    store_locks::CrossProcessStoreLock,
};

mod builder;
pub(crate) mod futures;

pub use self::builder::{sanitize_server_name, ClientBuildError, ClientBuilder};

#[cfg(not(target_arch = "wasm32"))]
type NotificationHandlerFut = Pin<Box<dyn Future<Output = ()> + Send>>;
#[cfg(target_arch = "wasm32")]
type NotificationHandlerFut = Pin<Box<dyn Future<Output = ()>>>;

type NotificationHandlerFn =
    Box<dyn Fn(Notification, Room, Client) -> NotificationHandlerFut + Send + Sync>;

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
#[derive(Debug, Clone, PartialEq)]
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

#[derive(Default)]
pub(crate) struct ClientLocks {
    /// Lock ensuring that only a single room may be marked as a DM at once.
    /// Look at the [`Account::mark_as_dm()`] method for a more detailed
    /// explanation.
    pub(crate) mark_as_dm_lock: Mutex<()>,
    /// Lock ensuring that only a single secret store is getting opened at the
    /// same time.
    ///
    /// This is important so we don't accidentally create multiple different new
    /// default secret storage keys.
    #[cfg(feature = "e2e-encryption")]
    pub(crate) open_secret_store_lock: Mutex<()>,
    /// Lock ensuring that we're only storing a single secret at a time.
    ///
    /// Take a look at the [`SecretStore::put_secret`] method for a more
    /// detailed explanation.
    ///
    /// [`SecretStore::put_secret`]: crate::encryption::secret_storage::SecretStore::put_secret
    #[cfg(feature = "e2e-encryption")]
    pub(crate) store_secret_lock: Mutex<()>,
    /// Lock ensuring that only one method at a time might modify our backup.
    #[cfg(feature = "e2e-encryption")]
    pub(crate) backup_modify_lock: Mutex<()>,
    /// Lock ensuring that we're going to attempt to upload backups for a single
    /// requester.
    #[cfg(feature = "e2e-encryption")]
    pub(crate) backup_upload_lock: Mutex<()>,
    /// Handler making sure we only have one group session sharing request in
    /// flight per room.
    #[cfg(feature = "e2e-encryption")]
    pub(crate) group_session_deduplicated_handler: DeduplicatingHandler<OwnedRoomId>,
    /// Lock making sure we're only doing one key claim request at a time.
    #[cfg(feature = "e2e-encryption")]
    pub(crate) key_claim_lock: Mutex<()>,
    /// Handler to ensure that only one members request is running at a time,
    /// given a room.
    pub(crate) members_request_deduplicated_handler: DeduplicatingHandler<OwnedRoomId>,
    /// Handler to ensure that only one encryption state request is running at a
    /// time, given a room.
    pub(crate) encryption_state_deduplicated_handler: DeduplicatingHandler<OwnedRoomId>,

    /// Deduplicating handler for sending read receipts. The string is an
    /// internal implementation detail, see [`Self::send_single_receipt`].
    pub(crate) read_receipt_deduplicated_handler: DeduplicatingHandler<(String, OwnedEventId)>,

    #[cfg(feature = "e2e-encryption")]
    pub(crate) cross_process_crypto_store_lock:
        OnceCell<CrossProcessStoreLock<LockableCryptoStore>>,
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

pub(crate) struct ClientInner {
    /// All the data related to authentication and authorization.
    pub(crate) auth_ctx: Arc<AuthCtx>,

    /// The URL of the server.
    ///
    /// Not to be confused with the `Self::homeserver`. `server` is usually
    /// the server part in a user ID, e.g. with `@mnt_io:matrix.org`, here
    /// `matrix.org` is the server, whilst `matrix-client.matrix.org` is the
    /// homeserver (at the time of writing — 2024-08-28).
    ///
    /// This value is optional depending on how the `Client` has been built.
    /// If it's been built from a homeserver URL directly, we don't know the
    /// server. However, if the `Client` has been built from a server URL or
    /// name, then the homeserver has been discovered, and we know both.
    server: Option<Url>,

    /// The URL of the homeserver to connect to.
    ///
    /// This is the URL for the client-server Matrix API.
    homeserver: StdRwLock<Url>,

    #[cfg(feature = "experimental-sliding-sync")]
    sliding_sync_version: StdRwLock<SlidingSyncVersion>,

    /// The underlying HTTP client.
    pub(crate) http_client: HttpClient,

    /// User session data.
    base_client: BaseClient,

    /// Server capabilities, either prefilled during building or fetched from
    /// the server.
    server_capabilities: RwLock<ClientServerCapabilities>,

    /// Collection of locks individual client methods might want to use, either
    /// to ensure that only a single call to a method happens at once or to
    /// deduplicate multiple calls to a method.
    pub(crate) locks: ClientLocks,

    /// A mapping of the times at which the current user sent typing notices,
    /// keyed by room.
    pub(crate) typing_notice_times: StdRwLock<BTreeMap<OwnedRoomId, Instant>>,

    /// Event handlers. See `add_event_handler`.
    pub(crate) event_handlers: EventHandlerStore,

    /// Notification handlers. See `register_notification_handler`.
    notification_handlers: RwLock<Vec<NotificationHandlerFn>>,

    /// The sender-side of channels used to receive room updates.
    pub(crate) room_update_channels: StdMutex<BTreeMap<OwnedRoomId, broadcast::Sender<RoomUpdate>>>,

    /// The sender-side of a channel used to observe all the room updates of a
    /// sync response.
    pub(crate) room_updates_sender: broadcast::Sender<RoomUpdates>,

    /// Whether the client should update its homeserver URL with the discovery
    /// information present in the login response.
    respect_login_well_known: bool,

    /// An event that can be listened on to wait for a successful sync. The
    /// event will only be fired if a sync loop is running. Can be used for
    /// synchronization, e.g. if we send out a request to create a room, we can
    /// wait for the sync to get the data to fetch a room object from the state
    /// store.
    pub(crate) sync_beat: event_listener::Event,

    /// A central cache for events, inactive first.
    ///
    /// It becomes active when [`EventCache::subscribe`] is called.
    pub(crate) event_cache: OnceCell<EventCache>,

    /// End-to-end encryption related state.
    #[cfg(feature = "e2e-encryption")]
    pub(crate) e2ee: EncryptionData,

    /// The verification state of our own device.
    #[cfg(feature = "e2e-encryption")]
    pub(crate) verification_state: SharedObservable<VerificationState>,

    /// Data related to the [`SendQueue`].
    ///
    /// [`SendQueue`]: crate::send_queue::SendQueue
    pub(crate) send_queue_data: Arc<SendQueueData>,
}

impl ClientInner {
    /// Create a new `ClientInner`.
    ///
    /// All the fields passed as parameters here are those that must be cloned
    /// upon instantiation of a sub-client, e.g. a client specialized for
    /// notifications.
    #[allow(clippy::too_many_arguments)]
    async fn new(
        auth_ctx: Arc<AuthCtx>,
        server: Option<Url>,
        homeserver: Url,
        #[cfg(feature = "experimental-sliding-sync")] sliding_sync_version: SlidingSyncVersion,
        http_client: HttpClient,
        base_client: BaseClient,
        server_capabilities: ClientServerCapabilities,
        respect_login_well_known: bool,
        event_cache: OnceCell<EventCache>,
        send_queue: Arc<SendQueueData>,
        #[cfg(feature = "e2e-encryption")] encryption_settings: EncryptionSettings,
    ) -> Arc<Self> {
        let client = Self {
            server,
            homeserver: StdRwLock::new(homeserver),
            auth_ctx,
            #[cfg(feature = "experimental-sliding-sync")]
            sliding_sync_version: StdRwLock::new(sliding_sync_version),
            http_client,
            base_client,
            locks: Default::default(),
            server_capabilities: RwLock::new(server_capabilities),
            typing_notice_times: Default::default(),
            event_handlers: Default::default(),
            notification_handlers: Default::default(),
            room_update_channels: Default::default(),
            // A single `RoomUpdates` is sent once per sync, so we assume that 32 is sufficient
            // ballast for all observers to catch up.
            room_updates_sender: broadcast::Sender::new(32),
            respect_login_well_known,
            sync_beat: event_listener::Event::new(),
            event_cache,
            send_queue_data: send_queue,
            #[cfg(feature = "e2e-encryption")]
            e2ee: EncryptionData::new(encryption_settings),
            #[cfg(feature = "e2e-encryption")]
            verification_state: SharedObservable::new(VerificationState::Unknown),
        };

        #[allow(clippy::let_and_return)]
        let client = Arc::new(client);

        #[cfg(feature = "e2e-encryption")]
        client.e2ee.initialize_room_key_tasks(&client);

        let _ = client
            .event_cache
            .get_or_init(|| async { EventCache::new(WeakClient::from_inner(&client)) })
            .await;

        client
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
    pub async fn new(homeserver_url: Url) -> Result<Self, ClientBuildError> {
        Self::builder().homeserver_url(homeserver_url).build().await
    }

    /// Returns a subscriber that publishes an event every time the ignore user
    /// list changes.
    pub fn subscribe_to_ignore_user_list_changes(&self) -> Subscriber<Vec<String>> {
        self.inner.base_client.subscribe_to_ignore_user_list_changes()
    }

    /// Create a new [`ClientBuilder`].
    pub fn builder() -> ClientBuilder {
        ClientBuilder::new()
    }

    pub(crate) fn base_client(&self) -> &BaseClient {
        &self.inner.base_client
    }

    /// The underlying HTTP client.
    pub fn http_client(&self) -> &reqwest::Client {
        &self.inner.http_client.inner
    }

    pub(crate) fn locks(&self) -> &ClientLocks {
        &self.inner.locks
    }

    /// Change the homeserver URL used by this client.
    ///
    /// # Arguments
    ///
    /// * `homeserver_url` - The new URL to use.
    fn set_homeserver(&self, homeserver_url: Url) {
        *self.inner.homeserver.write().unwrap() = homeserver_url;
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

    /// The server used by the client.
    ///
    /// See `Self::server` to learn more.
    pub fn server(&self) -> Option<&Url> {
        self.inner.server.as_ref()
    }

    /// The homeserver of the client.
    pub fn homeserver(&self) -> Url {
        self.inner.homeserver.read().unwrap().clone()
    }

    /// Get the sliding sync version.
    #[cfg(feature = "experimental-sliding-sync")]
    pub fn sliding_sync_version(&self) -> SlidingSyncVersion {
        self.inner.sliding_sync_version.read().unwrap().clone()
    }

    /// Override the sliding sync version.
    #[cfg(feature = "experimental-sliding-sync")]
    pub fn set_sliding_sync_version(&self, version: SlidingSyncVersion) {
        let mut lock = self.inner.sliding_sync_version.write().unwrap();
        *lock = version;
    }

    /// Get the Matrix user session meta information.
    ///
    /// If the client is currently logged in, this will return a
    /// [`SessionMeta`] object which contains the user ID and device ID.
    /// Otherwise it returns `None`.
    pub fn session_meta(&self) -> Option<&SessionMeta> {
        self.base_client().session_meta()
    }

    /// Returns a receiver that gets events for each room info update. To watch
    /// for new events, use `receiver.resubscribe()`. Each event contains the
    /// room and a boolean whether this event should trigger a room list update.
    pub fn room_info_notable_update_receiver(&self) -> broadcast::Receiver<RoomInfoNotableUpdate> {
        self.base_client().room_info_notable_update_receiver()
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
        self.inner.auth_ctx.auth_data.get()?.access_token()
    }

    /// Access the authentication API used to log in this client.
    ///
    /// Will be `None` if the client has not been logged in.
    pub fn auth_api(&self) -> Option<AuthApi> {
        match self.inner.auth_ctx.auth_data.get()? {
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

    /// Get a reference to the event cache store.
    pub(crate) fn event_cache_store(&self) -> &DynEventCacheStore {
        self.base_client().event_cache_store()
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

    /// Get the pusher manager of the client.
    pub fn pusher(&self) -> Pusher {
        Pusher::new(self.clone())
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
    /// ```no_run
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
    /// ```no_run
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
    /// ```no_run
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
        H: Fn(Notification, Room, Client) -> Fut + Send + Sync + 'static,
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

    /// Subscribe to all updates to all rooms, whenever any has been received in
    /// a sync response.
    pub fn subscribe_to_all_room_updates(&self) -> broadcast::Receiver<RoomUpdates> {
        self.inner.room_updates_sender.subscribe()
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
        self.base_client().rooms().into_iter().map(|room| Room::new(self.clone(), room)).collect()
    }

    /// Get all the rooms the client knows about, filtered by room state.
    pub fn rooms_filtered(&self, filter: RoomStateFilter) -> Vec<Room> {
        self.base_client()
            .rooms_filtered(filter)
            .into_iter()
            .map(|room| Room::new(self.clone(), room))
            .collect()
    }

    /// Get a stream of all the rooms, in addition to the existing rooms.
    pub fn rooms_stream(&self) -> (Vector<Room>, impl Stream<Item = Vec<VectorDiff<Room>>> + '_) {
        let (rooms, stream) = self.base_client().rooms_stream();

        let map_room = |room| Room::new(self.clone(), room);

        (
            rooms.into_iter().map(map_room).collect(),
            stream.map(move |diffs| diffs.into_iter().map(|diff| diff.map(map_room)).collect()),
        )
    }

    /// Returns the joined rooms this client knows about.
    pub fn joined_rooms(&self) -> Vec<Room> {
        self.base_client()
            .rooms_filtered(RoomStateFilter::JOINED)
            .into_iter()
            .map(|room| Room::new(self.clone(), room))
            .collect()
    }

    /// Returns the invited rooms this client knows about.
    pub fn invited_rooms(&self) -> Vec<Room> {
        self.base_client()
            .rooms_filtered(RoomStateFilter::INVITED)
            .into_iter()
            .map(|room| Room::new(self.clone(), room))
            .collect()
    }

    /// Returns the left rooms this client knows about.
    pub fn left_rooms(&self) -> Vec<Room> {
        self.base_client()
            .rooms_filtered(RoomStateFilter::LEFT)
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

    /// Gets the preview of a room, whether the current user knows it (because
    /// they've joined/left/been invited to it) or not.
    pub async fn get_room_preview(
        &self,
        room_or_alias_id: &RoomOrAliasId,
        via: Vec<OwnedServerName>,
    ) -> Result<RoomPreview> {
        let room_id = match <&RoomId>::try_from(room_or_alias_id) {
            Ok(room_id) => room_id.to_owned(),
            Err(alias) => self.resolve_room_alias(alias).await?.room_id,
        };

        if let Some(room) = self.get_room(&room_id) {
            return Ok(RoomPreview::from_known(&room));
        }

        RoomPreview::from_unknown(self, room_id, room_or_alias_id, via).await
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
    pub(crate) fn maybe_update_login_well_known(&self, login_well_known: Option<&DiscoveryInfo>) {
        if self.inner.respect_login_well_known {
            if let Some(well_known) = login_well_known {
                if let Ok(homeserver) = Url::parse(&well_known.homeserver.base_url) {
                    self.set_homeserver(homeserver);
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
            AuthSession::Matrix(s) => Box::pin(self.matrix_auth().restore_session(s)).await,
            #[cfg(feature = "experimental-oidc")]
            AuthSession::Oidc(s) => Box::pin(self.oidc().restore_session(s)).await,
        }
    }

    pub(crate) async fn set_session_meta(
        &self,
        session_meta: SessionMeta,
        #[cfg(feature = "e2e-encryption")] custom_account: Option<vodozemac::olm::Account>,
    ) -> Result<()> {
        self.base_client()
            .set_session_meta(
                session_meta,
                #[cfg(feature = "e2e-encryption")]
                custom_account,
            )
            .await?;

        Ok(())
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
            AuthApi::Matrix(api) => {
                trace!("Token refresh: Using the homeserver.");
                Box::pin(api.refresh_access_token()).await?;
            }
            #[cfg(feature = "experimental-oidc")]
            AuthApi::Oidc(api) => {
                trace!("Token refresh: Using OIDC.");
                Box::pin(api.refresh_access_token()).await?;
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
    /// * `alias` - The `RoomId` or `RoomAliasId` of the room to be joined. An
    ///   alias looks like `#name:example.com`.
    pub async fn join_room_by_id_or_alias(
        &self,
        alias: &RoomOrAliasId,
        server_names: &[OwnedServerName],
    ) -> Result<Room> {
        let request = assign!(join_room_by_id_or_alias::v3::Request::new(alias.to_owned()), {
            via: server_names.to_owned(),
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
    /// use matrix_sdk::{
    ///     ruma::api::client::room::create_room::v3::Request as CreateRoomRequest,
    ///     Client,
    /// };
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
    ///
    /// If the `e2e-encryption` feature is enabled, the room will also be
    /// encrypted.
    ///
    /// # Arguments
    ///
    /// * `user_id` - The ID of the user to create a DM for.
    pub async fn create_dm(&self, user_id: &UserId) -> Result<Room> {
        #[cfg(feature = "e2e-encryption")]
        let initial_state =
            vec![InitialStateEvent::new(RoomEncryptionEventContent::with_recommended_defaults())
                .to_raw_any()];

        #[cfg(not(feature = "e2e-encryption"))]
        let initial_state = vec![];

        let request = assign!(create_room::v3::Request::new(), {
            invite: vec![user_id.to_owned()],
            is_direct: true,
            preset: Some(create_room::v3::RoomPreset::TrustedPrivateChat),
            initial_state,
        });

        self.create_room(request).await
    }

    /// Search the homeserver's directory for public rooms with a filter.
    ///
    /// # Arguments
    ///
    /// * `room_search` - The easiest way to create this request is using the
    ///   `get_public_rooms_filtered::Request` itself.
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
    ///   default request setting if one was set.
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
        SendRequest {
            client: self.clone(),
            request,
            config,
            send_progress: Default::default(),
            homeserver_override: None,
        }
    }

    pub(crate) async fn send_inner<Request>(
        &self,
        request: Request,
        config: Option<RequestConfig>,
        homeserver_override: Option<String>,
        send_progress: SharedObservable<TransmissionProgress>,
    ) -> HttpResult<Request::IncomingResponse>
    where
        Request: OutgoingRequest + Debug,
        HttpError: From<FromHttpResponseError<Request::EndpointError>>,
    {
        let homeserver = match homeserver_override {
            Some(hs) => hs,
            None => self.homeserver().to_string(),
        };

        let access_token = self.access_token();

        self.inner
            .http_client
            .send(
                request,
                config,
                homeserver,
                access_token.as_deref(),
                &self.server_versions().await?,
                send_progress,
            )
            .await
    }

    fn broadcast_unknown_token(&self, soft_logout: &bool) {
        _ = self
            .inner
            .auth_ctx
            .session_change_sender
            .send(SessionChange::UnknownToken { soft_logout: *soft_logout });
    }

    /// Fetches server capabilities from network; no caching.
    async fn fetch_server_capabilities(
        &self,
    ) -> HttpResult<(Box<[MatrixVersion]>, BTreeMap<String, bool>)> {
        let resp = self
            .inner
            .http_client
            .send(
                get_supported_versions::Request::new(),
                None,
                self.homeserver().to_string(),
                None,
                &[MatrixVersion::V1_0],
                Default::default(),
            )
            .await?;

        // Fill both unstable features and server versions at once.
        let mut versions = resp.known_versions().collect::<Vec<_>>();
        if versions.is_empty() {
            versions.push(MatrixVersion::V1_0);
        }

        Ok((versions.into(), resp.unstable_features))
    }

    /// Load server capabilities from storage, or fetch them from network and
    /// cache them.
    async fn load_or_fetch_server_capabilities(
        &self,
    ) -> HttpResult<(Box<[MatrixVersion]>, BTreeMap<String, bool>)> {
        match self.store().get_kv_data(StateStoreDataKey::ServerCapabilities).await {
            Ok(Some(stored)) => {
                if let Some((versions, unstable_features)) =
                    stored.into_server_capabilities().and_then(|cap| cap.maybe_decode())
                {
                    return Ok((versions.into(), unstable_features));
                }
            }
            Ok(None) => {
                // fallthrough: cache is empty
            }
            Err(err) => {
                warn!("error when loading cached server capabilities: {err}");
                // fallthrough to network.
            }
        }

        let (versions, unstable_features) = self.fetch_server_capabilities().await?;

        // Attempt to cache the result in storage.
        {
            let encoded = ServerCapabilities::new(&versions, unstable_features.clone());
            if let Err(err) = self
                .store()
                .set_kv_data(
                    StateStoreDataKey::ServerCapabilities,
                    StateStoreDataValue::ServerCapabilities(encoded),
                )
                .await
            {
                warn!("error when caching server capabilities: {err}");
            }
        }

        Ok((versions, unstable_features))
    }

    async fn get_or_load_and_cache_server_capabilities<
        T,
        F: Fn(&ClientServerCapabilities) -> Option<T>,
    >(
        &self,
        f: F,
    ) -> HttpResult<T> {
        let caps = &self.inner.server_capabilities;
        if let Some(val) = f(&*caps.read().await) {
            return Ok(val);
        }

        let mut guard = caps.write().await;
        if let Some(val) = f(&guard) {
            return Ok(val);
        }

        let (versions, unstable_features) = self.load_or_fetch_server_capabilities().await?;

        guard.server_versions = Some(versions);
        guard.unstable_features = Some(unstable_features);

        // SAFETY: both fields were set above, so the function will always return some.
        Ok(f(&guard).unwrap())
    }

    pub(crate) async fn server_versions(&self) -> HttpResult<Box<[MatrixVersion]>> {
        self.get_or_load_and_cache_server_capabilities(|caps| caps.server_versions.clone()).await
    }

    /// Get unstable features from by fetching from the server or the cache.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use matrix_sdk::{Client, config::SyncSettings};
    /// # use url::Url;
    /// # async {
    /// # let homeserver = Url::parse("http://localhost:8080")?;
    /// # let mut client = Client::new(homeserver).await?;
    /// let unstable_features = client.unstable_features().await?;
    /// let msc_x = unstable_features.get("msc_x").unwrap_or(&false);
    /// # anyhow::Ok(()) };
    /// ```
    pub async fn unstable_features(&self) -> HttpResult<BTreeMap<String, bool>> {
        self.get_or_load_and_cache_server_capabilities(|caps| caps.unstable_features.clone()).await
    }

    /// Empty the server version and unstable features cache.
    ///
    /// Since the SDK caches server capabilities (versions and unstable
    /// features), it's possible to have a stale entry in the cache. This
    /// functions makes it possible to force reset it.
    pub async fn reset_server_capabilities(&self) -> Result<()> {
        // Empty the in-memory caches.
        let mut guard = self.inner.server_capabilities.write().await;
        guard.server_versions = None;
        guard.unstable_features = None;

        // Empty the store cache.
        Ok(self.store().remove_kv_data(StateStoreDataKey::ServerCapabilities).await?)
    }

    /// Check whether MSC 4028 is enabled on the homeserver.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use matrix_sdk::{Client, config::SyncSettings};
    /// # use url::Url;
    /// # async {
    /// # let homeserver = Url::parse("http://localhost:8080")?;
    /// # let mut client = Client::new(homeserver).await?;
    /// let msc4028_enabled =
    ///     client.can_homeserver_push_encrypted_event_to_device().await?;
    /// # anyhow::Ok(()) };
    /// ```
    pub async fn can_homeserver_push_encrypted_event_to_device(&self) -> HttpResult<bool> {
        Ok(self.unstable_features().await?.get("org.matrix.msc4028").copied().unwrap_or(false))
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
    ///   server.
    ///
    /// * `auth_data` - This request requires user interactive auth, the first
    ///   request needs to set this to `None` and will always fail with an
    ///   `UiaaResponse`. The response will contain information for the
    ///   interactive auth and the same request needs to be made but this time
    ///   with some `auth_data` provided.
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
    ///             if let Ok(event) = e.raw().deserialize() {
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
        let broadcast = &self.inner.auth_ctx.session_change_sender;
        broadcast.subscribe()
    }

    /// Sets the save/restore session callbacks.
    ///
    /// This is another mechanism to get synchronous updates to session tokens,
    /// while [`Self::subscribe_to_session_changes`] provides an async update.
    pub fn set_session_callbacks(
        &self,
        reload_session_callback: Box<ReloadSessionCallback>,
        save_session_callback: Box<SaveSessionCallback>,
    ) -> Result<()> {
        self.inner
            .auth_ctx
            .reload_session_callback
            .set(reload_session_callback)
            .map_err(|_| Error::MultipleSessionCallbacks)?;

        self.inner
            .auth_ctx
            .save_session_callback
            .set(save_session_callback)
            .map_err(|_| Error::MultipleSessionCallbacks)?;

        Ok(())
    }

    /// Get the notification settings of the current owner of the client.
    pub async fn notification_settings(&self) -> NotificationSettings {
        let ruleset = self.account().push_rules().await.unwrap_or_else(|_| Ruleset::new());
        NotificationSettings::new(self.clone(), ruleset)
    }

    /// Create a new specialized `Client` that can process notifications.
    pub async fn notification_client(&self) -> Result<Client> {
        let client = Client {
            inner: ClientInner::new(
                self.inner.auth_ctx.clone(),
                self.server().cloned(),
                self.homeserver(),
                #[cfg(feature = "experimental-sliding-sync")]
                self.sliding_sync_version(),
                self.inner.http_client.clone(),
                self.inner.base_client.clone_with_in_memory_state_store().await?,
                self.inner.server_capabilities.read().await.clone(),
                self.inner.respect_login_well_known,
                self.inner.event_cache.clone(),
                self.inner.send_queue_data.clone(),
                #[cfg(feature = "e2e-encryption")]
                self.inner.e2ee.encryption_settings,
            )
            .await,
        };

        Ok(client)
    }

    /// The [`EventCache`] instance for this [`Client`].
    pub fn event_cache(&self) -> &EventCache {
        // SAFETY: always initialized in the `Client` ctor.
        self.inner.event_cache.get().unwrap()
    }

    /// Waits until an at least partially synced room is received, and returns
    /// it.
    ///
    /// **Note: this function will loop endlessly until either it finds the room
    /// or an externally set timeout happens.**
    pub async fn await_room_remote_echo(&self, room_id: &RoomId) -> Room {
        loop {
            if let Some(room) = self.get_room(room_id) {
                if room.is_state_partially_or_fully_synced() {
                    debug!("Found just created room!");
                    return room;
                }
                debug!("Room wasn't partially synced, waiting for sync beat to try again");
            } else {
                debug!("Room wasn't found, waiting for sync beat to try again");
            }
            self.inner.sync_beat.listen().await;
        }
    }

    /// Knock on a room given its `room_id_or_alias` to ask for permission to
    /// join it.
    pub async fn knock(&self, room_id_or_alias: OwnedRoomOrAliasId) -> Result<Room> {
        let request = knock_room::v3::Request::new(room_id_or_alias);
        let response = self.send(request, None).await?;
        let base_room = self.inner.base_client.room_knocked(&response.room_id).await?;
        Ok(Room::new(self.clone(), base_room))
    }
}

/// A weak reference to the inner client, useful when trying to get a handle
/// on the owning client.
#[derive(Clone)]
pub(crate) struct WeakClient {
    client: Weak<ClientInner>,
}

impl WeakClient {
    /// Construct a [`WeakClient`] from a `Arc<ClientInner>`.
    pub fn from_inner(client: &Arc<ClientInner>) -> Self {
        Self { client: Arc::downgrade(client) }
    }

    /// Construct a [`WeakClient`] from a [`Client`].
    pub fn from_client(client: &Client) -> Self {
        Self::from_inner(&client.inner)
    }

    /// Attempts to get a [`Client`] from this [`WeakClient`].
    pub fn get(&self) -> Option<Client> {
        self.client.upgrade().map(|inner| Client { inner })
    }

    /// Gets the number of strong (`Arc`) pointers still pointing to this
    /// client.
    #[allow(dead_code)]
    pub fn strong_count(&self) -> usize {
        self.client.strong_count()
    }
}

#[derive(Clone)]
struct ClientServerCapabilities {
    /// The Matrix versions the server supports (well-known ones only).
    server_versions: Option<Box<[MatrixVersion]>>,

    /// The unstable features and their on/off state on the server.
    unstable_features: Option<BTreeMap<String, bool>>,
}

// The http mocking library is not supported for wasm32
#[cfg(all(test, not(target_arch = "wasm32")))]
pub(crate) mod tests {
    use std::{sync::Arc, time::Duration};

    use assert_matches::assert_matches;
    use futures_util::FutureExt;
    use matrix_sdk_base::{
        store::{MemoryStore, StoreConfig},
        RoomState,
    };
    use matrix_sdk_test::{
        async_test, test_json, JoinedRoomBuilder, StateTestEvent, SyncResponseBuilder,
        DEFAULT_TEST_ROOM_ID,
    };
    #[cfg(target_arch = "wasm32")]
    wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_browser);

    use ruma::{
        api::{client::room::create_room::v3::Request as CreateRoomRequest, MatrixVersion},
        assign,
        events::ignored_user_list::IgnoredUserListEventContent,
        owned_room_id, room_id, RoomId, ServerName, UserId,
    };
    use serde_json::json;
    use tokio::{
        spawn,
        time::{sleep, timeout},
    };
    use url::Url;
    use wiremock::{
        matchers::{body_json, header, method, path, query_param_is_missing},
        Mock, MockServer, ResponseTemplate,
    };

    use super::Client;
    use crate::{
        client::WeakClient,
        config::{RequestConfig, SyncSettings},
        test_utils::{
            logged_in_client, no_retry_test_client, set_client_session, test_client_builder,
            test_client_builder_with_server,
        },
        Error,
    };

    #[async_test]
    async fn test_account_data() {
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
    async fn test_successful_discovery() {
        // Imagine this is `matrix.org`.
        let server = MockServer::start().await;

        // Imagine this is `matrix-client.matrix.org`.
        let homeserver = MockServer::start().await;

        // Imagine Alice has the user ID `@alice:matrix.org`.
        let server_url = server.uri();
        let domain = server_url.strip_prefix("http://").unwrap();
        let alice = UserId::parse("@alice:".to_owned() + domain).unwrap();

        // The `.well-known` is on the server (e.g. `matrix.org`).
        Mock::given(method("GET"))
            .and(path("/.well-known/matrix/client"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(
                test_json::WELL_KNOWN.to_string().replace("HOMESERVER_URL", &homeserver.uri()),
                "application/json",
            ))
            .mount(&server)
            .await;

        // The `/versions` is on the homeserver (e.g. `matrix-client.matrix.org`).
        Mock::given(method("GET"))
            .and(path("/_matrix/client/versions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(&*test_json::VERSIONS))
            .mount(&homeserver)
            .await;

        let client = Client::builder()
            .insecure_server_name_no_tls(alice.server_name())
            .build()
            .await
            .unwrap();

        assert_eq!(client.server().unwrap(), &Url::parse(&server.uri()).unwrap());
        assert_eq!(client.homeserver(), Url::parse(&homeserver.uri()).unwrap());
    }

    #[async_test]
    async fn test_discovery_broken_server() {
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
    async fn test_room_creation() {
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

        assert_eq!(client.homeserver(), Url::parse(&server.uri()).unwrap());

        let room = client.get_room(&DEFAULT_TEST_ROOM_ID).unwrap();
        assert_eq!(room.state(), RoomState::Joined);
    }

    #[async_test]
    async fn test_retry_limit_http_requests() {
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
    async fn test_retry_timeout_http_requests() {
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
    async fn test_short_retry_initial_http_requests() {
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
    async fn test_no_retry_http_requests() {
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
    async fn test_set_homeserver() {
        let client = no_retry_test_client(Some("http://localhost".to_owned())).await;
        assert_eq!(client.homeserver().as_ref(), "http://localhost/");

        let homeserver = Url::parse("http://example.com/").unwrap();
        client.set_homeserver(homeserver.clone());
        assert_eq!(client.homeserver(), homeserver);
    }

    #[async_test]
    async fn test_search_user_request() {
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
        assert_eq!(response.results.len(), 1);
        let result = &response.results[0];
        assert_eq!(result.user_id.to_string(), "@test:example.me");
        assert_eq!(result.display_name.clone().unwrap(), "Test");
        assert_eq!(result.avatar_url.clone().unwrap().to_string(), "mxc://example.me/someid");
        assert!(!response.limited);
    }

    #[async_test]
    async fn test_request_unstable_features() {
        let server = MockServer::start().await;
        let client = logged_in_client(Some(server.uri())).await;

        Mock::given(method("GET"))
            .and(path("_matrix/client/versions"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(&*test_json::api_responses::VERSIONS),
            )
            .mount(&server)
            .await;

        let unstable_features = client.unstable_features().await.unwrap();
        assert_eq!(unstable_features.get("org.matrix.e2e_cross_signing"), Some(&true));
        assert_eq!(unstable_features.get("you.shall.pass"), None);
    }

    #[async_test]
    async fn test_can_homeserver_push_encrypted_event_to_device() {
        let server = MockServer::start().await;
        let client = logged_in_client(Some(server.uri())).await;

        Mock::given(method("GET"))
            .and(path("_matrix/client/versions"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(&*test_json::api_responses::VERSIONS),
            )
            .mount(&server)
            .await;

        let msc4028_enabled = client.can_homeserver_push_encrypted_event_to_device().await.unwrap();
        assert!(msc4028_enabled);
    }

    #[async_test]
    async fn test_recently_visited_rooms() {
        // Tracking recently visited rooms requires authentication
        let client = no_retry_test_client(Some("http://localhost".to_owned())).await;
        assert_matches!(
            client.account().track_recently_visited_room(owned_room_id!("!alpha:localhost")).await,
            Err(Error::AuthenticationRequired)
        );

        let client = logged_in_client(None).await;
        let account = client.account();

        // We should start off with an empty list
        assert_eq!(account.get_recently_visited_rooms().await.unwrap().len(), 0);

        // Tracking a valid room id should add it to the list
        account.track_recently_visited_room(owned_room_id!("!alpha:localhost")).await.unwrap();
        assert_eq!(account.get_recently_visited_rooms().await.unwrap().len(), 1);
        assert_eq!(account.get_recently_visited_rooms().await.unwrap(), ["!alpha:localhost"]);

        // And the existing list shouldn't be changed
        assert_eq!(account.get_recently_visited_rooms().await.unwrap().len(), 1);
        assert_eq!(account.get_recently_visited_rooms().await.unwrap(), ["!alpha:localhost"]);

        // Tracking the same room again shouldn't change the list
        account.track_recently_visited_room(owned_room_id!("!alpha:localhost")).await.unwrap();
        assert_eq!(account.get_recently_visited_rooms().await.unwrap().len(), 1);
        assert_eq!(account.get_recently_visited_rooms().await.unwrap(), ["!alpha:localhost"]);

        // Tracking a second room should add it to the front of the list
        account.track_recently_visited_room(owned_room_id!("!beta:localhost")).await.unwrap();
        assert_eq!(account.get_recently_visited_rooms().await.unwrap().len(), 2);
        assert_eq!(
            account.get_recently_visited_rooms().await.unwrap(),
            [room_id!("!beta:localhost"), room_id!("!alpha:localhost")]
        );

        // Tracking the first room yet again should move it to the front of the list
        account.track_recently_visited_room(owned_room_id!("!alpha:localhost")).await.unwrap();
        assert_eq!(account.get_recently_visited_rooms().await.unwrap().len(), 2);
        assert_eq!(
            account.get_recently_visited_rooms().await.unwrap(),
            [room_id!("!alpha:localhost"), room_id!("!beta:localhost")]
        );

        // Tracking should be capped at 20
        for n in 0..20 {
            account
                .track_recently_visited_room(RoomId::parse(format!("!{n}:localhost")).unwrap())
                .await
                .unwrap();
        }

        assert_eq!(account.get_recently_visited_rooms().await.unwrap().len(), 20);

        // And the initial rooms should've been pushed out
        let rooms = account.get_recently_visited_rooms().await.unwrap();
        assert!(!rooms.contains(&owned_room_id!("!alpha:localhost")));
        assert!(!rooms.contains(&owned_room_id!("!beta:localhost")));

        // And the last tracked room should be the first
        assert_eq!(rooms.first().unwrap(), room_id!("!19:localhost"));
    }

    #[async_test]
    async fn test_client_no_cycle_with_event_cache() {
        let client = logged_in_client(None).await;

        // Wait for the init tasks to die.
        sleep(Duration::from_secs(1)).await;

        let weak_client = WeakClient::from_client(&client);
        assert_eq!(weak_client.strong_count(), 1);

        {
            let room_id = room_id!("!room:example.org");

            // Have the client know the room.
            let response = SyncResponseBuilder::default()
                .add_joined_room(JoinedRoomBuilder::new(room_id))
                .build_sync_response();
            client.inner.base_client.receive_sync_response(response).await.unwrap();

            client.event_cache().subscribe().unwrap();

            let (_room_event_cache, _drop_handles) =
                client.get_room(room_id).unwrap().event_cache().await.unwrap();
        }

        drop(client);

        // Give a bit of time for background tasks to die.
        sleep(Duration::from_secs(1)).await;

        // The weak client must be the last reference to the client now.
        assert_eq!(weak_client.strong_count(), 0);
        let client = weak_client.get();
        assert!(
            client.is_none(),
            "too many strong references to the client: {}",
            Arc::strong_count(&client.unwrap().inner)
        );
    }

    #[async_test]
    async fn test_server_capabilities_caching() {
        let server = MockServer::start().await;
        let server_url = server.uri();
        let domain = server_url.strip_prefix("http://").unwrap();
        let server_name = <&ServerName>::try_from(domain).unwrap();

        Mock::given(method("GET"))
            .and(path("/.well-known/matrix/client"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(
                test_json::WELL_KNOWN.to_string().replace("HOMESERVER_URL", server_url.as_ref()),
                "application/json",
            ))
            .named("well known mock")
            .expect(2)
            .mount(&server)
            .await;

        let versions_mock = Mock::given(method("GET"))
            .and(path("/_matrix/client/versions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(&*test_json::VERSIONS))
            .named("first versions mock")
            .expect(1)
            .mount_as_scoped(&server)
            .await;

        let memory_store = Arc::new(MemoryStore::new());
        let client = Client::builder()
            .insecure_server_name_no_tls(server_name)
            .store_config(StoreConfig::new().state_store(memory_store.clone()))
            .build()
            .await
            .unwrap();

        assert_eq!(client.server_versions().await.unwrap().len(), 1);

        // This second call hits the in-memory cache.
        assert!(client
            .server_versions()
            .await
            .unwrap()
            .iter()
            .any(|version| *version == MatrixVersion::V1_0));

        drop(client);

        let client = Client::builder()
            .insecure_server_name_no_tls(server_name)
            .store_config(StoreConfig::new().state_store(memory_store.clone()))
            .build()
            .await
            .unwrap();

        // This third call hits the on-disk cache.
        assert_eq!(
            client.unstable_features().await.unwrap().get("org.matrix.e2e_cross_signing"),
            Some(&true)
        );

        drop(versions_mock);
        server.verify().await;

        // Now, reset the cache, and observe the endpoint being called again once.
        client.reset_server_capabilities().await.unwrap();

        Mock::given(method("GET"))
            .and(path("/_matrix/client/versions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(&*test_json::VERSIONS))
            .expect(1)
            .named("second versions mock")
            .mount(&server)
            .await;

        // Hits network again.
        assert_eq!(client.server_versions().await.unwrap().len(), 1);
        // Hits in-memory cache again.
        assert!(client
            .server_versions()
            .await
            .unwrap()
            .iter()
            .any(|version| *version == MatrixVersion::V1_0));
    }

    #[async_test]
    async fn test_no_network_doesnt_cause_infinite_retries() {
        // Note: not `no_retry_test_client` or `logged_in_client` which uses the former,
        // since we want infinite retries for transient errors.
        let client =
            test_client_builder(None).request_config(RequestConfig::new()).build().await.unwrap();
        set_client_session(&client).await;

        // We don't define a mock server on purpose here, so that the error is really a
        // network error.
        client.whoami().await.unwrap_err();
    }

    #[async_test]
    async fn test_await_room_remote_echo_returns_the_room_if_it_was_already_synced() {
        let (client_builder, server) = test_client_builder_with_server().await;
        let client = client_builder.request_config(RequestConfig::new()).build().await.unwrap();
        set_client_session(&client).await;

        let builder = Mock::given(method("GET"))
            .and(path("/_matrix/client/r0/sync"))
            .and(header("authorization", "Bearer 1234"))
            .and(query_param_is_missing("since"));

        let room_id = room_id!("!room:example.org");
        let joined_room_builder = JoinedRoomBuilder::new(room_id);
        let mut sync_response_builder = SyncResponseBuilder::new();
        sync_response_builder.add_joined_room(joined_room_builder);
        let response_body = sync_response_builder.build_json_sync_response();

        builder
            .respond_with(ResponseTemplate::new(200).set_body_json(response_body))
            .mount(&server)
            .await;

        client.sync_once(SyncSettings::default()).await.unwrap();

        let room = client.await_room_remote_echo(room_id).now_or_never().unwrap();
        assert_eq!(room.room_id(), room_id);
    }

    #[async_test]
    async fn test_await_room_remote_echo_returns_the_room_when_it_is_ready() {
        let (client_builder, server) = test_client_builder_with_server().await;
        let client = client_builder.request_config(RequestConfig::new()).build().await.unwrap();
        set_client_session(&client).await;

        let builder = Mock::given(method("GET"))
            .and(path("/_matrix/client/r0/sync"))
            .and(header("authorization", "Bearer 1234"))
            .and(query_param_is_missing("since"));

        let room_id = room_id!("!room:example.org");
        let joined_room_builder = JoinedRoomBuilder::new(room_id);
        let mut sync_response_builder = SyncResponseBuilder::new();
        sync_response_builder.add_joined_room(joined_room_builder);
        let response_body = sync_response_builder.build_json_sync_response();

        builder
            .respond_with(ResponseTemplate::new(200).set_body_json(response_body))
            .mount(&server)
            .await;

        let client = Arc::new(client);

        // Perform the /sync request with a delay so it starts after the
        // `await_room_remote_echo` call has happened
        spawn({
            let client = client.clone();
            async move {
                sleep(Duration::from_millis(100)).await;
                client.sync_once(SyncSettings::default()).await.unwrap();
            }
        });

        let room =
            timeout(Duration::from_secs(10), client.await_room_remote_echo(room_id)).await.unwrap();
        assert_eq!(room.room_id(), room_id);
    }

    #[async_test]
    async fn test_await_room_remote_echo_will_timeout_if_no_room_is_found() {
        let (client_builder, _) = test_client_builder_with_server().await;
        let client = client_builder.request_config(RequestConfig::new()).build().await.unwrap();
        set_client_session(&client).await;

        let room_id = room_id!("!room:example.org");
        // Room is not present so the client won't be able to find it. The call will
        // timeout.
        timeout(Duration::from_secs(1), client.await_room_remote_echo(room_id)).await.unwrap_err();
    }

    #[async_test]
    async fn test_await_room_remote_echo_will_timeout_if_room_is_found_but_not_synced() {
        let (client_builder, server) = test_client_builder_with_server().await;
        let client = client_builder.request_config(RequestConfig::new()).build().await.unwrap();
        set_client_session(&client).await;

        Mock::given(method("POST"))
            .and(path("_matrix/client/r0/createRoom"))
            .and(header("authorization", "Bearer 1234"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({ "room_id": "!room:example.org"})),
            )
            .mount(&server)
            .await;

        // Create a room in the internal store
        let room = client
            .create_room(assign!(CreateRoomRequest::new(), {
                invite: vec![],
                is_direct: false,
            }))
            .await
            .unwrap();

        // Room is locally present, but not synced, the call will timeout
        timeout(Duration::from_secs(1), client.await_room_remote_echo(room.room_id()))
            .await
            .unwrap_err();
    }
}
