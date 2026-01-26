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
    collections::{BTreeMap, BTreeSet, btree_map},
    fmt::{self, Debug},
    future::{Future, ready},
    pin::Pin,
    sync::{Arc, Mutex as StdMutex, RwLock as StdRwLock, Weak},
    time::Duration,
};

use eyeball::{SharedObservable, Subscriber};
use eyeball_im::{Vector, VectorDiff};
use futures_core::Stream;
use futures_util::StreamExt;
#[cfg(feature = "e2e-encryption")]
use matrix_sdk_base::crypto::{DecryptionSettings, store::LockableCryptoStore};
use matrix_sdk_base::{
    BaseClient, RoomInfoNotableUpdate, RoomState, RoomStateFilter, SendOutsideWasm, SessionMeta,
    StateStoreDataKey, StateStoreDataValue, StoreError, SyncOutsideWasm, ThreadingSupport,
    event_cache::store::EventCacheStoreLock,
    media::store::MediaStoreLock,
    store::{
        DynStateStore, RoomLoadSettings, SupportedVersionsResponse, TtlStoreValue,
        WellKnownResponse,
    },
    sync::{Notification, RoomUpdates},
};
use matrix_sdk_common::ttl_cache::TtlCache;
#[cfg(feature = "e2e-encryption")]
use ruma::events::{InitialStateEvent, room::encryption::RoomEncryptionEventContent};
use ruma::{
    DeviceId, OwnedDeviceId, OwnedEventId, OwnedRoomId, OwnedRoomOrAliasId, OwnedServerName,
    RoomAliasId, RoomId, RoomOrAliasId, ServerName, UInt, UserId,
    api::{
        FeatureFlag, MatrixVersion, Metadata, OutgoingRequest, SupportedVersions,
        auth_scheme::{AuthScheme, SendAccessToken},
        client::{
            account::whoami,
            alias::{create_alias, delete_alias, get_alias},
            authenticated_media,
            device::{self, delete_devices, get_devices, update_device},
            directory::{get_public_rooms, get_public_rooms_filtered},
            discovery::{
                discover_homeserver::{self, RtcFocusInfo},
                get_capabilities::{self, v3::Capabilities},
                get_supported_versions,
            },
            error::ErrorKind,
            filter::{FilterDefinition, create_filter::v3::Request as FilterUploadRequest},
            knock::knock_room,
            media,
            membership::{join_room_by_id, join_room_by_id_or_alias},
            room::create_room,
            session::login::v3::DiscoveryInfo,
            sync::sync_events,
            threads::get_thread_subscriptions_changes,
            uiaa,
            user_directory::search_users,
        },
        error::FromHttpResponseError,
        path_builder::PathBuilder,
    },
    assign,
    events::direct::DirectUserIdentifier,
    push::Ruleset,
    time::Instant,
};
use serde::de::DeserializeOwned;
use tokio::sync::{Mutex, OnceCell, RwLock, RwLockReadGuard, broadcast};
use tracing::{Instrument, Span, debug, error, info, instrument, trace, warn};
use url::Url;

use self::{
    caches::{CachedValue, ClientCaches},
    futures::SendRequest,
};
use crate::{
    Account, AuthApi, AuthSession, Error, HttpError, Media, Pusher, RefreshTokenError, Result,
    Room, SessionTokens, TransmissionProgress,
    authentication::{
        AuthCtx, AuthData, ReloadSessionCallback, SaveSessionCallback, matrix::MatrixAuth,
        oauth::OAuth,
    },
    client::thread_subscriptions::ThreadSubscriptionCatchup,
    config::{RequestConfig, SyncToken},
    deduplicating_handler::DeduplicatingHandler,
    error::HttpResult,
    event_cache::EventCache,
    event_handler::{
        EventHandler, EventHandlerContext, EventHandlerDropGuard, EventHandlerHandle,
        EventHandlerStore, ObservableEventHandler, SyncEvent,
    },
    http_client::{HttpClient, SupportedPathBuilder},
    latest_events::LatestEvents,
    media::MediaError,
    notification_settings::NotificationSettings,
    room::RoomMember,
    room_preview::RoomPreview,
    send_queue::{SendQueue, SendQueueData},
    sliding_sync::Version as SlidingSyncVersion,
    sync::{RoomUpdate, SyncResponse},
};
#[cfg(feature = "e2e-encryption")]
use crate::{
    cross_process_lock::CrossProcessLock,
    encryption::{Encryption, EncryptionData, EncryptionSettings, VerificationState},
};

mod builder;
pub(crate) mod caches;
pub(crate) mod futures;
pub(crate) mod thread_subscriptions;

pub use self::builder::{ClientBuildError, ClientBuilder, sanitize_server_name};
#[cfg(feature = "experimental-search")]
use crate::search_index::SearchIndex;

#[cfg(not(target_family = "wasm"))]
type NotificationHandlerFut = Pin<Box<dyn Future<Output = ()> + Send>>;
#[cfg(target_family = "wasm")]
type NotificationHandlerFut = Pin<Box<dyn Future<Output = ()>>>;

#[cfg(not(target_family = "wasm"))]
type NotificationHandlerFn =
    Box<dyn Fn(Notification, Room, Client) -> NotificationHandlerFut + Send + Sync>;
#[cfg(target_family = "wasm")]
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

/// Information about the server vendor obtained from the federation API.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct ServerVendorInfo {
    /// The server name.
    pub server_name: String,
    /// The server version.
    pub version: String,
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
    pub(crate) cross_process_crypto_store_lock: OnceCell<CrossProcessLock<LockableCryptoStore>>,

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

    /// The sliding sync version.
    sliding_sync_version: StdRwLock<SlidingSyncVersion>,

    /// The underlying HTTP client.
    pub(crate) http_client: HttpClient,

    /// User session data.
    pub(super) base_client: BaseClient,

    /// Collection of in-memory caches for the [`Client`].
    pub(crate) caches: ClientCaches,

    /// Collection of locks individual client methods might want to use, either
    /// to ensure that only a single call to a method happens at once or to
    /// deduplicate multiple calls to a method.
    pub(crate) locks: ClientLocks,

    /// The cross-process store locks holder name.
    ///
    /// The SDK provides cross-process store locks (see
    /// [`matrix_sdk_common::cross_process_lock::CrossProcessLock`]). The
    /// `holder_name` is the value used for all cross-process store locks
    /// used by this `Client`.
    ///
    /// If multiple `Client`s are running in different processes, this
    /// value MUST be different for each `Client`.
    cross_process_store_locks_holder_name: String,

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

    /// Whether to enable the experimental support for sending and receiving
    /// encrypted room history on invite, per [MSC4268].
    ///
    /// [MSC4268]: https://github.com/matrix-org/matrix-spec-proposals/pull/4268
    #[cfg(feature = "e2e-encryption")]
    pub(crate) enable_share_history_on_invite: bool,

    /// Data related to the [`SendQueue`].
    ///
    /// [`SendQueue`]: crate::send_queue::SendQueue
    pub(crate) send_queue_data: Arc<SendQueueData>,

    /// The `max_upload_size` value of the homeserver, it contains the max
    /// request size you can send.
    pub(crate) server_max_upload_size: Mutex<OnceCell<UInt>>,

    /// The entry point to get the [`LatestEvent`] of rooms and threads.
    ///
    /// [`LatestEvent`]: crate::latest_event::LatestEvent
    latest_events: OnceCell<LatestEvents>,

    /// Service handling the catching up of thread subscriptions in the
    /// background.
    thread_subscription_catchup: OnceCell<Arc<ThreadSubscriptionCatchup>>,

    #[cfg(feature = "experimental-search")]
    /// Handler for [`RoomIndex`]'s of each room
    search_index: SearchIndex,
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
        sliding_sync_version: SlidingSyncVersion,
        http_client: HttpClient,
        base_client: BaseClient,
        supported_versions: CachedValue<SupportedVersions>,
        well_known: CachedValue<Option<WellKnownResponse>>,
        respect_login_well_known: bool,
        event_cache: OnceCell<EventCache>,
        send_queue: Arc<SendQueueData>,
        latest_events: OnceCell<LatestEvents>,
        #[cfg(feature = "e2e-encryption")] encryption_settings: EncryptionSettings,
        #[cfg(feature = "e2e-encryption")] enable_share_history_on_invite: bool,
        cross_process_store_locks_holder_name: String,
        #[cfg(feature = "experimental-search")] search_index_handler: SearchIndex,
        thread_subscription_catchup: OnceCell<Arc<ThreadSubscriptionCatchup>>,
    ) -> Arc<Self> {
        let caches = ClientCaches {
            supported_versions: supported_versions.into(),
            well_known: well_known.into(),
            server_metadata: Mutex::new(TtlCache::new()),
        };

        let client = Self {
            server,
            homeserver: StdRwLock::new(homeserver),
            auth_ctx,
            sliding_sync_version: StdRwLock::new(sliding_sync_version),
            http_client,
            base_client,
            caches,
            locks: Default::default(),
            cross_process_store_locks_holder_name,
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
            latest_events,
            #[cfg(feature = "e2e-encryption")]
            e2ee: EncryptionData::new(encryption_settings),
            #[cfg(feature = "e2e-encryption")]
            verification_state: SharedObservable::new(VerificationState::Unknown),
            #[cfg(feature = "e2e-encryption")]
            enable_share_history_on_invite,
            server_max_upload_size: Mutex::new(OnceCell::new()),
            #[cfg(feature = "experimental-search")]
            search_index: search_index_handler,
            thread_subscription_catchup,
        };

        #[allow(clippy::let_and_return)]
        let client = Arc::new(client);

        #[cfg(feature = "e2e-encryption")]
        client.e2ee.initialize_tasks(&client);

        let _ = client
            .event_cache
            .get_or_init(|| async {
                EventCache::new(
                    WeakClient::from_inner(&client),
                    client.base_client.event_cache_store().clone(),
                )
            })
            .await;

        let _ = client
            .thread_subscription_catchup
            .get_or_init(|| async {
                ThreadSubscriptionCatchup::new(Client { inner: client.clone() })
            })
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

    pub(crate) fn auth_ctx(&self) -> &AuthCtx {
        &self.inner.auth_ctx
    }

    /// The cross-process store locks holder name.
    ///
    /// The SDK provides cross-process store locks (see
    /// [`matrix_sdk_common::cross_process_lock::CrossProcessLock`]). The
    /// `holder_name` is the value used for all cross-process store locks
    /// used by this `Client`.
    pub fn cross_process_store_locks_holder_name(&self) -> &str {
        &self.inner.cross_process_store_locks_holder_name
    }

    /// Change the homeserver URL used by this client.
    ///
    /// # Arguments
    ///
    /// * `homeserver_url` - The new URL to use.
    fn set_homeserver(&self, homeserver_url: Url) {
        *self.inner.homeserver.write().unwrap() = homeserver_url;
    }

    /// Change to a different homeserver and re-resolve well-known.
    #[cfg(feature = "e2e-encryption")]
    pub(crate) async fn switch_homeserver_and_re_resolve_well_known(
        &self,
        homeserver_url: Url,
    ) -> Result<()> {
        self.set_homeserver(homeserver_url);
        self.reset_well_known().await?;
        if let Some(well_known) = self.load_or_fetch_well_known().await? {
            self.set_homeserver(Url::parse(&well_known.homeserver.base_url)?);
        }
        Ok(())
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
        let res = self.send(get_capabilities::v3::Request::new()).await?;
        Ok(res.capabilities)
    }

    /// Get the server vendor information from the federation API.
    ///
    /// This method calls the `/_matrix/federation/v1/version` endpoint to get
    /// both the server's software name and version.
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
    /// let server_info = client.server_vendor_info(None).await?;
    /// println!(
    ///     "Server: {}, Version: {}",
    ///     server_info.server_name, server_info.version
    /// );
    /// # anyhow::Ok(()) };
    /// ```
    #[cfg(feature = "federation-api")]
    pub async fn server_vendor_info(
        &self,
        request_config: Option<RequestConfig>,
    ) -> HttpResult<ServerVendorInfo> {
        use ruma::api::federation::discovery::get_server_version;

        let res = self
            .send_inner(get_server_version::v1::Request::new(), request_config, Default::default())
            .await?;

        // Extract server info, using defaults if fields are missing.
        let server = res.server.unwrap_or_default();
        let server_name_str = server.name.unwrap_or_else(|| "unknown".to_owned());
        let version = server.version.unwrap_or_else(|| "unknown".to_owned());

        Ok(ServerVendorInfo { server_name: server_name_str, version })
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

    /// Check whether the client has been activated.
    ///
    /// A client is considered active when:
    ///
    /// 1. It has a `SessionMeta` (user ID, device ID and access token), i.e. it
    ///    is logged in,
    /// 2. Has loaded cached data from storage,
    /// 3. If encryption is enabled, it also initialized or restored its
    ///    `OlmMachine`.
    pub fn is_active(&self) -> bool {
        self.inner.base_client.is_active()
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
    pub fn sliding_sync_version(&self) -> SlidingSyncVersion {
        self.inner.sliding_sync_version.read().unwrap().clone()
    }

    /// Override the sliding sync version.
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
    /// for new events, use `receiver.resubscribe()`.
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

        self.send(request).await
    }

    /// Get the user id of the current owner of the client.
    pub fn user_id(&self) -> Option<&UserId> {
        self.session_meta().map(|s| s.user_id.as_ref())
    }

    /// Get the device ID that identifies the current session.
    pub fn device_id(&self) -> Option<&DeviceId> {
        self.session_meta().map(|s| s.device_id.as_ref())
    }

    /// Get the current access token for this session.
    ///
    /// Will be `None` if the client has not been logged in.
    pub fn access_token(&self) -> Option<String> {
        self.auth_ctx().access_token()
    }

    /// Get the current tokens for this session.
    ///
    /// To be notified of changes in the session tokens, use
    /// [`Client::subscribe_to_session_changes()`] or
    /// [`Client::set_session_callbacks()`].
    ///
    /// Returns `None` if the client has not been logged in.
    pub fn session_tokens(&self) -> Option<SessionTokens> {
        self.auth_ctx().session_tokens()
    }

    /// Access the authentication API used to log in this client.
    ///
    /// Will be `None` if the client has not been logged in.
    pub fn auth_api(&self) -> Option<AuthApi> {
        match self.auth_ctx().auth_data.get()? {
            AuthData::Matrix => Some(AuthApi::Matrix(self.matrix_auth())),
            AuthData::OAuth(_) => Some(AuthApi::OAuth(self.oauth())),
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
            AuthApi::OAuth(api) => api.full_session().map(Into::into),
        }
    }

    /// Get a reference to the state store.
    pub fn state_store(&self) -> &DynStateStore {
        self.base_client().state_store()
    }

    /// Get a reference to the event cache store.
    pub fn event_cache_store(&self) -> &EventCacheStoreLock {
        self.base_client().event_cache_store()
    }

    /// Get a reference to the media store.
    pub fn media_store(&self) -> &MediaStoreLock {
        self.base_client().media_store()
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

    /// Access the OAuth 2.0 API of the client.
    pub fn oauth(&self) -> OAuth {
        OAuth::new(self.clone())
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
    /// use matrix_sdk::{
    ///     deserialized_responses::EncryptionInfo,
    ///     event_handler::Ctx,
    ///     ruma::{
    ///         events::{
    ///             macros::EventContent,
    ///             push_rules::PushRulesEvent,
    ///             room::{
    ///                 message::SyncRoomMessageEvent,
    ///                 topic::SyncRoomTopicEvent,
    ///                 member::{StrippedRoomMemberEvent, SyncRoomMemberEvent},
    ///             },
    ///         },
    ///         push::Action,
    ///         Int, MilliSecondsSinceUnixEpoch,
    ///     },
    ///     Client, Room,
    /// };
    /// use serde::{Deserialize, Serialize};
    ///
    /// # async fn example(client: Client) {
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
    /// // This will handle membership events in joined rooms. Invites are special, see below.
    /// client.add_event_handler(
    ///     |ev: SyncRoomMemberEvent| async move {},
    /// );
    ///
    /// // To handle state events in invited rooms (including invite membership events),
    /// // `StrippedRoomMemberEvent` should be used.
    /// // https://spec.matrix.org/v1.16/client-server-api/#stripped-state
    /// client.add_event_handler(
    ///     |ev: StrippedRoomMemberEvent| async move {},
    /// );
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
    /// client.add_event_handler(async |ev: SyncTokenEvent, room: Room| -> () {
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
    /// # }
    /// ```
    pub fn add_event_handler<Ev, Ctx, H>(&self, handler: H) -> EventHandlerHandle
    where
        Ev: SyncEvent + DeserializeOwned + SendOutsideWasm + 'static,
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
        Ev: SyncEvent + DeserializeOwned + SendOutsideWasm + 'static,
        H: EventHandler<Ev, Ctx>,
    {
        self.add_event_handler_impl(handler, Some(room_id.to_owned()))
    }

    /// Observe a specific event type.
    ///
    /// `Ev` represents the kind of event that will be observed. `Ctx`
    /// represents the context that will come with the event. It relies on the
    /// same mechanism as [`Client::add_event_handler`]. The main difference is
    /// that it returns an [`ObservableEventHandler`] and doesn't require a
    /// user-defined closure. It is possible to subscribe to the
    /// [`ObservableEventHandler`] to get an [`EventHandlerSubscriber`], which
    /// implements a [`Stream`]. The `Stream::Item` will be of type `(Ev,
    /// Ctx)`.
    ///
    /// Be careful that only the most recent value can be observed. Subscribers
    /// are notified when a new value is sent, but there is no guarantee
    /// that they will see all values.
    ///
    /// # Example
    ///
    /// Let's see a classical usage:
    ///
    /// ```
    /// use futures_util::StreamExt as _;
    /// use matrix_sdk::{
    ///     Client, Room,
    ///     ruma::{events::room::message::SyncRoomMessageEvent, push::Action},
    /// };
    ///
    /// # async fn example(client: Client) -> Option<()> {
    /// let observer =
    ///     client.observe_events::<SyncRoomMessageEvent, (Room, Vec<Action>)>();
    ///
    /// let mut subscriber = observer.subscribe();
    ///
    /// let (event, (room, push_actions)) = subscriber.next().await?;
    /// # Some(())
    /// # }
    /// ```
    ///
    /// Now let's see how to get several contexts that can be useful for you:
    ///
    /// ```
    /// use matrix_sdk::{
    ///     Client, Room,
    ///     deserialized_responses::EncryptionInfo,
    ///     ruma::{
    ///         events::room::{
    ///             message::SyncRoomMessageEvent, topic::SyncRoomTopicEvent,
    ///         },
    ///         push::Action,
    ///     },
    /// };
    ///
    /// # async fn example(client: Client) {
    /// // Observe `SyncRoomMessageEvent` and fetch `Room` + `Client`.
    /// let _ = client.observe_events::<SyncRoomMessageEvent, (Room, Client)>();
    ///
    /// // Observe `SyncRoomMessageEvent` and fetch `Room` + `EncryptionInfo`
    /// // to distinguish between unencrypted events and events that were decrypted
    /// // by the SDK.
    /// let _ = client
    ///     .observe_events::<SyncRoomMessageEvent, (Room, Option<EncryptionInfo>)>(
    ///     );
    ///
    /// // Observe `SyncRoomMessageEvent` and fetch `Room` + push actions.
    /// // For example, an event with `Action::SetTweak(Tweak::Highlight(true))`
    /// // should be highlighted in the timeline.
    /// let _ =
    ///     client.observe_events::<SyncRoomMessageEvent, (Room, Vec<Action>)>();
    ///
    /// // Observe `SyncRoomTopicEvent` and fetch nothing else.
    /// let _ = client.observe_events::<SyncRoomTopicEvent, ()>();
    /// # }
    /// ```
    ///
    /// [`EventHandlerSubscriber`]: crate::event_handler::EventHandlerSubscriber
    pub fn observe_events<Ev, Ctx>(&self) -> ObservableEventHandler<(Ev, Ctx)>
    where
        Ev: SyncEvent + DeserializeOwned + SendOutsideWasm + SyncOutsideWasm + 'static,
        Ctx: EventHandlerContext + SendOutsideWasm + SyncOutsideWasm + 'static,
    {
        self.observe_room_events_impl(None)
    }

    /// Observe a specific room, and event type.
    ///
    /// This method works the same way as [`Client::observe_events`], except
    /// that the observability will only be applied for events in the room with
    /// the specified ID. See that method for more details.
    ///
    /// Be careful that only the most recent value can be observed. Subscribers
    /// are notified when a new value is sent, but there is no guarantee
    /// that they will see all values.
    pub fn observe_room_events<Ev, Ctx>(
        &self,
        room_id: &RoomId,
    ) -> ObservableEventHandler<(Ev, Ctx)>
    where
        Ev: SyncEvent + DeserializeOwned + SendOutsideWasm + SyncOutsideWasm + 'static,
        Ctx: EventHandlerContext + SendOutsideWasm + SyncOutsideWasm + 'static,
    {
        self.observe_room_events_impl(Some(room_id.to_owned()))
    }

    /// Shared implementation for `Client::observe_events` and
    /// `Client::observe_room_events`.
    fn observe_room_events_impl<Ev, Ctx>(
        &self,
        room_id: Option<OwnedRoomId>,
    ) -> ObservableEventHandler<(Ev, Ctx)>
    where
        Ev: SyncEvent + DeserializeOwned + SendOutsideWasm + SyncOutsideWasm + 'static,
        Ctx: EventHandlerContext + SendOutsideWasm + SyncOutsideWasm + 'static,
    {
        // The default value is `None`. It becomes `Some((Ev, Ctx))` once it has a
        // new value.
        let shared_observable = SharedObservable::new(None);

        ObservableEventHandler::new(
            shared_observable.clone(),
            self.event_handler_drop_guard(self.add_event_handler_impl(
                move |event: Ev, context: Ctx| {
                    shared_observable.set(Some((event, context)));

                    ready(())
                },
                room_id,
            )),
        )
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
    ///     Client, event_handler::EventHandlerHandle,
    ///     ruma::events::room::member::SyncRoomMemberEvent,
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
    ///     Room, event_handler::Ctx,
    ///     ruma::events::room::message::SyncRoomMessageEvent,
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
        self.rooms_filtered(RoomStateFilter::JOINED)
    }

    /// Returns the invited rooms this client knows about.
    pub fn invited_rooms(&self) -> Vec<Room> {
        self.rooms_filtered(RoomStateFilter::INVITED)
    }

    /// Returns the left rooms this client knows about.
    pub fn left_rooms(&self) -> Vec<Room> {
        self.rooms_filtered(RoomStateFilter::LEFT)
    }

    /// Returns the joined space rooms this client knows about.
    pub fn joined_space_rooms(&self) -> Vec<Room> {
        self.base_client()
            .rooms_filtered(RoomStateFilter::JOINED)
            .into_iter()
            .flat_map(|room| room.is_space().then_some(Room::new(self.clone(), room)))
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

    /// Gets the preview of a room, whether the current user has joined it or
    /// not.
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
            // The cached data can only be trusted if the room state is joined or
            // banned: for invite and knock rooms, no updates will be received
            // for the rooms after the invite/knock action took place so we may
            // have very out to date data for important fields such as
            // `join_rule`. For left rooms, the homeserver should return the latest info.
            match room.state() {
                RoomState::Joined | RoomState::Banned => {
                    return Ok(RoomPreview::from_known_room(&room).await);
                }
                RoomState::Left | RoomState::Invited | RoomState::Knocked => {}
            }
        }

        RoomPreview::from_remote_room(self, room_id, room_or_alias_id, via).await
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
        self.send(request).await
    }

    /// Checks if a room alias is not in use yet.
    ///
    /// Returns:
    /// - `Ok(true)` if the room alias is available.
    /// - `Ok(false)` if it's not (the resolve alias request returned a `404`
    ///   status code).
    /// - An `Err` otherwise.
    pub async fn is_room_alias_available(&self, alias: &RoomAliasId) -> HttpResult<bool> {
        match self.resolve_room_alias(alias).await {
            // The room alias was resolved, so it's already in use.
            Ok(_) => Ok(false),
            Err(error) => {
                match error.client_api_error_kind() {
                    // The room alias wasn't found, so it's available.
                    Some(ErrorKind::NotFound) => Ok(true),
                    _ => Err(error),
                }
            }
        }
    }

    /// Adds a new room alias associated with a room to the room directory.
    pub async fn create_room_alias(&self, alias: &RoomAliasId, room_id: &RoomId) -> HttpResult<()> {
        let request = create_alias::v3::Request::new(alias.to_owned(), room_id.to_owned());
        self.send(request).await?;
        Ok(())
    }

    /// Removes a room alias from the room directory.
    pub async fn remove_room_alias(&self, alias: &RoomAliasId) -> HttpResult<()> {
        let request = delete_alias::v3::Request::new(alias.to_owned());
        self.send(request).await?;
        Ok(())
    }

    /// Update the homeserver from the login response well-known if needed.
    ///
    /// # Arguments
    ///
    /// * `login_well_known` - The `well_known` field from a successful login
    ///   response.
    pub(crate) fn maybe_update_login_well_known(&self, login_well_known: Option<&DiscoveryInfo>) {
        if self.inner.respect_login_well_known
            && let Some(well_known) = login_well_known
            && let Ok(homeserver) = Url::parse(&well_known.homeserver.base_url)
        {
            self.set_homeserver(homeserver);
        }
    }

    /// Similar to [`Client::restore_session_with`], with
    /// [`RoomLoadSettings::default()`].
    ///
    /// # Panics
    ///
    /// Panics if a session was already restored or logged in.
    #[instrument(skip_all)]
    pub async fn restore_session(&self, session: impl Into<AuthSession>) -> Result<()> {
        self.restore_session_with(session, RoomLoadSettings::default()).await
    }

    /// Restore a session previously logged-in using one of the available
    /// authentication APIs. The number of rooms to restore is controlled by
    /// [`RoomLoadSettings`].
    ///
    /// See the documentation of the corresponding authentication API's
    /// `restore_session` method for more information.
    ///
    /// # Panics
    ///
    /// Panics if a session was already restored or logged in.
    #[instrument(skip_all)]
    pub async fn restore_session_with(
        &self,
        session: impl Into<AuthSession>,
        room_load_settings: RoomLoadSettings,
    ) -> Result<()> {
        let session = session.into();
        match session {
            AuthSession::Matrix(session) => {
                Box::pin(self.matrix_auth().restore_session(session, room_load_settings)).await
            }
            AuthSession::OAuth(session) => {
                Box::pin(self.oauth().restore_session(*session, room_load_settings)).await
            }
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
            AuthApi::Matrix(api) => {
                trace!("Token refresh: Using the homeserver.");
                Box::pin(api.refresh_access_token()).await?;
            }
            AuthApi::OAuth(api) => {
                trace!("Token refresh: Using OAuth 2.0.");
                Box::pin(api.refresh_access_token()).await?;
            }
        }

        Ok(())
    }

    /// Log out the current session using the proper authentication API.
    ///
    /// # Errors
    ///
    /// Returns an error if the session is not authenticated or if an error
    /// occurred while making the request to the server.
    pub async fn logout(&self) -> Result<(), Error> {
        let auth_api = self.auth_api().ok_or(Error::AuthenticationRequired)?;
        match auth_api {
            AuthApi::Matrix(matrix_auth) => {
                matrix_auth.logout().await?;
                Ok(())
            }
            AuthApi::OAuth(oauth) => Ok(oauth.logout().await?),
        }
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
            let response = self.send(request).await?;

            self.inner.base_client.receive_filter_upload(filter_name, &response).await?;

            Ok(response.filter_id)
        }
    }

    /// Prepare to join a room by ID, by getting the current details about it
    async fn prepare_join_room_by_id(&self, room_id: &RoomId) -> Option<PreJoinRoomInfo> {
        let room = self.get_room(room_id)?;

        let inviter = match room.invite_details().await {
            Ok(details) => details.inviter,
            Err(Error::WrongRoomState(_)) => None,
            Err(e) => {
                warn!("Error fetching invite details for room: {e:?}");
                None
            }
        };

        Some(PreJoinRoomInfo { inviter })
    }

    /// Finish joining a room.
    ///
    /// If the room was an invite that should be marked as a DM, will include it
    /// in the DM event after creating the joined room.
    ///
    /// If encrypted history sharing is enabled, will check to see if we have a
    /// key bundle, and import it if so.
    ///
    /// # Arguments
    ///
    /// * `room_id` - The `RoomId` of the room that was joined.
    /// * `pre_join_room_info` - Information about the room before we joined.
    async fn finish_join_room(
        &self,
        room_id: &RoomId,
        pre_join_room_info: Option<PreJoinRoomInfo>,
    ) -> Result<Room> {
        info!(?room_id, ?pre_join_room_info, "Completing room join");
        let mark_as_dm = if let Some(room) = self.get_room(room_id) {
            room.state() == RoomState::Invited
                && room.is_direct().await.unwrap_or_else(|e| {
                    warn!(%room_id, "is_direct() failed: {e}");
                    false
                })
        } else {
            false
        };

        let base_room = self
            .base_client()
            .room_joined(
                room_id,
                pre_join_room_info
                    .as_ref()
                    .and_then(|info| info.inviter.as_ref())
                    .map(|i| i.user_id().to_owned()),
            )
            .await?;
        let room = Room::new(self.clone(), base_room);

        if mark_as_dm {
            room.set_is_direct(true).await?;
        }

        #[cfg(feature = "e2e-encryption")]
        if self.inner.enable_share_history_on_invite
            && let Some(inviter) =
                pre_join_room_info.as_ref().and_then(|info| info.inviter.as_ref())
        {
            crate::room::shared_room_history::maybe_accept_key_bundle(&room, inviter.user_id())
                .await?;
        }

        // Suppress "unused variable" and "unused field" lints
        #[cfg(not(feature = "e2e-encryption"))]
        let _ = pre_join_room_info.map(|i| i.inviter);

        Ok(room)
    }

    /// Join a room by `RoomId`.
    ///
    /// Returns the `Room` in the joined state.
    ///
    /// # Arguments
    ///
    /// * `room_id` - The `RoomId` of the room to be joined.
    #[instrument(skip(self))]
    pub async fn join_room_by_id(&self, room_id: &RoomId) -> Result<Room> {
        // See who invited us to this room, if anyone. Note we have to do this before
        // making the `/join` request, otherwise we could race against the sync.
        let pre_join_info = self.prepare_join_room_by_id(room_id).await;

        let request = join_room_by_id::v3::Request::new(room_id.to_owned());
        let response = self.send(request).await?;
        self.finish_join_room(&response.room_id, pre_join_info).await
    }

    /// Join a room by `RoomOrAliasId`.
    ///
    /// Returns the `Room` in the joined state.
    ///
    /// # Arguments
    ///
    /// * `alias` - The `RoomId` or `RoomAliasId` of the room to be joined. An
    ///   alias looks like `#name:example.com`.
    /// * `server_names` - The server names to be used for resolving the alias,
    ///   if needs be.
    #[instrument(skip(self))]
    pub async fn join_room_by_id_or_alias(
        &self,
        alias: &RoomOrAliasId,
        server_names: &[OwnedServerName],
    ) -> Result<Room> {
        let pre_join_info = {
            match alias.try_into() {
                Ok(room_id) => self.prepare_join_room_by_id(room_id).await,
                Err(_) => {
                    // The id is a room alias. We assume (possibly incorrectly?) that we are not
                    // responding to an invitation to the room, and therefore don't need to handle
                    // things that happen as a result of invites.
                    None
                }
            }
        };
        let request = assign!(join_room_by_id_or_alias::v3::Request::new(alias.to_owned()), {
            via: server_names.to_owned(),
        });
        let response = self.send(request).await?;
        self.finish_join_room(&response.room_id, pre_join_info).await
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
    #[cfg_attr(not(target_family = "wasm"), deny(clippy::future_not_send))]
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
        self.send(request).await
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
    ///     Client,
    ///     ruma::api::client::room::create_room::v3::Request as CreateRoomRequest,
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
        let response = self.send(request).await?;

        let base_room = self.base_client().get_or_create_room(&response.room_id, RoomState::Joined);

        let joined_room = Room::new(self.clone(), base_room);

        if is_direct_room
            && !invite.is_empty()
            && let Err(error) =
                self.account().mark_as_dm(joined_room.room_id(), invite.as_slice()).await
        {
            // FIXME: Retry in the background
            error!("Failed to mark room as DM: {error}");
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
        let initial_state = vec![
            InitialStateEvent::with_empty_state_key(
                RoomEncryptionEventContent::with_recommended_defaults(),
            )
            .to_raw_any(),
        ];

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

    /// Get the existing DM room with the given user, if any.
    pub fn get_dm_room(&self, user_id: &UserId) -> Option<Room> {
        let rooms = self.joined_rooms();

        // Find the room we share with the `user_id` and only with `user_id`
        let room = rooms.into_iter().find(|r| {
            let targets = r.direct_targets();
            targets.len() == 1 && targets.contains(<&DirectUserIdentifier>::from(user_id))
        });

        trace!(?user_id, ?room, "Found DM room with user");
        room
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
        self.send(request).await
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
    /// let response = client.send(request).await?;
    ///
    /// // Check the corresponding Response struct to find out what types are
    /// // returned
    /// # anyhow::Ok(()) };
    /// ```
    pub fn send<Request>(&self, request: Request) -> SendRequest<Request>
    where
        Request: OutgoingRequest + Clone + Debug,
        for<'a> Request::Authentication: AuthScheme<Input<'a> = SendAccessToken<'a>>,
        Request::PathBuilder: SupportedPathBuilder,
        for<'a> <Request::PathBuilder as PathBuilder>::Input<'a>: SendOutsideWasm + SyncOutsideWasm,
        HttpError: From<FromHttpResponseError<Request::EndpointError>>,
    {
        SendRequest {
            client: self.clone(),
            request,
            config: None,
            send_progress: Default::default(),
        }
    }

    pub(crate) async fn send_inner<Request>(
        &self,
        request: Request,
        config: Option<RequestConfig>,
        send_progress: SharedObservable<TransmissionProgress>,
    ) -> HttpResult<Request::IncomingResponse>
    where
        Request: OutgoingRequest + Debug,
        for<'a> Request::Authentication: AuthScheme<Input<'a> = SendAccessToken<'a>>,
        Request::PathBuilder: SupportedPathBuilder,
        for<'a> <Request::PathBuilder as PathBuilder>::Input<'a>: SendOutsideWasm + SyncOutsideWasm,
        HttpError: From<FromHttpResponseError<Request::EndpointError>>,
    {
        let homeserver = self.homeserver().to_string();
        let access_token = self.access_token();
        let skip_auth = config.map(|c| c.skip_auth).unwrap_or(self.request_config().skip_auth);

        let path_builder_input =
            Request::PathBuilder::get_path_builder_input(self, skip_auth).await?;

        let result = self
            .inner
            .http_client
            .send(
                request,
                config,
                homeserver,
                access_token.as_deref(),
                path_builder_input,
                send_progress,
            )
            .await;

        if let Err(Some(ErrorKind::UnknownToken { .. })) =
            result.as_ref().map_err(HttpError::client_api_error_kind)
            && let Some(access_token) = &access_token
        {
            // Mark the access token as expired.
            self.auth_ctx().set_access_token_expired(access_token);
        }

        result
    }

    fn broadcast_unknown_token(&self, soft_logout: &bool) {
        _ = self
            .inner
            .auth_ctx
            .session_change_sender
            .send(SessionChange::UnknownToken { soft_logout: *soft_logout });
    }

    /// Fetches server versions from network; no caching.
    pub async fn fetch_server_versions(
        &self,
        request_config: Option<RequestConfig>,
    ) -> HttpResult<get_supported_versions::Response> {
        // Since this was called by the user, try to refresh the access token if
        // necessary.
        self.fetch_server_versions_inner(false, request_config).await
    }

    /// Fetches server versions from network; no caching.
    ///
    /// If the access token is expired and `failsafe` is `false`, this will
    /// attempt to refresh the access token, otherwise this will try to make an
    /// unauthenticated request instead.
    pub(crate) async fn fetch_server_versions_inner(
        &self,
        failsafe: bool,
        request_config: Option<RequestConfig>,
    ) -> HttpResult<get_supported_versions::Response> {
        if !failsafe {
            // `Client::send()` handles refreshing access tokens.
            return self
                .send(get_supported_versions::Request::new())
                .with_request_config(request_config)
                .await;
        }

        let homeserver = self.homeserver().to_string();

        // If we have a fresh access token, try with it first.
        if !request_config.as_ref().is_some_and(|config| config.skip_auth && !config.force_auth)
            && self.auth_ctx().has_valid_access_token()
            && let Some(access_token) = self.access_token()
        {
            let result = self
                .inner
                .http_client
                .send(
                    get_supported_versions::Request::new(),
                    request_config,
                    homeserver.clone(),
                    Some(&access_token),
                    (),
                    Default::default(),
                )
                .await;

            if let Err(Some(ErrorKind::UnknownToken { .. })) =
                result.as_ref().map_err(HttpError::client_api_error_kind)
            {
                // If the access token is actually expired, mark it as expired and fallback to
                // the unauthenticated request below.
                self.auth_ctx().set_access_token_expired(&access_token);
            } else {
                // If the request succeeded or it's an other error, just stop now.
                return result;
            }
        }

        // Try without authentication.
        self.inner
            .http_client
            .send(
                get_supported_versions::Request::new(),
                request_config,
                homeserver.clone(),
                None,
                (),
                Default::default(),
            )
            .await
    }

    /// Fetches client well_known from network; no caching.
    ///
    /// 1. If the [`Client::server`] value is available, we use it to fetch the
    ///    well-known contents.
    /// 2. If it's not, we try extracting the server name from the
    ///    [`Client::user_id`] and building the server URL from it.
    /// 3. If we couldn't get the well-known contents with either the explicit
    ///    server name or the implicit extracted one, we try the homeserver URL
    ///    as a last resort.
    pub async fn fetch_client_well_known(&self) -> Option<discover_homeserver::Response> {
        let homeserver = self.homeserver();
        let scheme = homeserver.scheme();

        // Use the server name, either an explicit one or an implicit one taken from
        // the user id: sometimes we'll have only the homeserver url available and no
        // server name, but the server name can be extracted from the current user id.
        let server_url = self
            .server()
            .map(|server| server.to_string())
            // If the server name wasn't available, extract it from the user id and build a URL:
            // Reuse the same scheme as the homeserver url does, assuming if it's `http` there it
            // will be the same for the public server url, lacking a better candidate.
            .or_else(|| self.user_id().map(|id| format!("{}://{}", scheme, id.server_name())));

        // If the server name is available, first try using it
        let response = if let Some(server_url) = server_url {
            // First try using the server name
            self.fetch_client_well_known_with_url(server_url).await
        } else {
            None
        };

        // If we didn't get a well-known value yet, try with the homeserver url instead:
        if response.is_none() {
            // Sometimes people configure their well-known directly on the homeserver so use
            // this as a fallback when the server name is unknown.
            warn!(
                "Fetching the well-known from the server name didn't work, using the homeserver url instead"
            );
            self.fetch_client_well_known_with_url(homeserver.to_string()).await
        } else {
            response
        }
    }

    async fn fetch_client_well_known_with_url(
        &self,
        url: String,
    ) -> Option<discover_homeserver::Response> {
        let well_known = self
            .inner
            .http_client
            .send(
                discover_homeserver::Request::new(),
                Some(RequestConfig::short_retry()),
                url,
                None,
                (),
                Default::default(),
            )
            .await;

        match well_known {
            Ok(well_known) => Some(well_known),
            Err(http_error) => {
                // It is perfectly valid to not have a well-known file.
                // Maybe we should check for a specific error code to be sure?
                warn!("Failed to fetch client well-known: {http_error}");
                None
            }
        }
    }

    /// Load supported versions from storage, or fetch them from network and
    /// cache them.
    ///
    /// If `failsafe` is true, this will try to minimize side effects to avoid
    /// possible deadlocks.
    async fn load_or_fetch_supported_versions(
        &self,
        failsafe: bool,
    ) -> HttpResult<SupportedVersionsResponse> {
        match self.state_store().get_kv_data(StateStoreDataKey::SupportedVersions).await {
            Ok(Some(stored)) => {
                if let Some(supported_versions) =
                    stored.into_supported_versions().and_then(|value| value.into_data())
                {
                    return Ok(supported_versions);
                }
            }
            Ok(None) => {
                // fallthrough: cache is empty
            }
            Err(err) => {
                warn!("error when loading cached supported versions: {err}");
                // fallthrough to network.
            }
        }

        let server_versions = self.fetch_server_versions_inner(failsafe, None).await?;
        let supported_versions = SupportedVersionsResponse {
            versions: server_versions.versions,
            unstable_features: server_versions.unstable_features,
        };

        // Only attempt to cache the result in storage if the request was authenticated.
        if self.auth_ctx().has_valid_access_token()
            && let Err(err) = self
                .state_store()
                .set_kv_data(
                    StateStoreDataKey::SupportedVersions,
                    StateStoreDataValue::SupportedVersions(TtlStoreValue::new(
                        supported_versions.clone(),
                    )),
                )
                .await
        {
            warn!("error when caching supported versions: {err}");
        }

        Ok(supported_versions)
    }

    pub(crate) async fn get_cached_supported_versions(&self) -> Option<SupportedVersions> {
        let supported_versions = &self.inner.caches.supported_versions;

        if let CachedValue::Cached(val) = &*supported_versions.read().await {
            Some(val.clone())
        } else {
            None
        }
    }

    /// Get the Matrix versions and features supported by the homeserver by
    /// fetching them from the server or the cache.
    ///
    /// This is equivalent to calling both [`Client::server_versions()`] and
    /// [`Client::unstable_features()`]. To always fetch the result from the
    /// homeserver, you can call [`Client::fetch_server_versions()`] instead,
    /// and then `.as_supported_versions()` on the response.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use ruma::api::{FeatureFlag, MatrixVersion};
    /// # use matrix_sdk::{Client, config::SyncSettings};
    /// # use url::Url;
    /// # async {
    /// # let homeserver = Url::parse("http://localhost:8080")?;
    /// # let mut client = Client::new(homeserver).await?;
    ///
    /// let supported = client.supported_versions().await?;
    /// let supports_1_1 = supported.versions.contains(&MatrixVersion::V1_1);
    /// println!("The homeserver supports Matrix 1.1: {supports_1_1:?}");
    ///
    /// let msc_x_feature = FeatureFlag::from("msc_x");
    /// let supports_msc_x = supported.features.contains(&msc_x_feature);
    /// println!("The homeserver supports msc X: {supports_msc_x:?}");
    /// # anyhow::Ok(()) };
    /// ```
    pub async fn supported_versions(&self) -> HttpResult<SupportedVersions> {
        self.supported_versions_inner(false).await
    }

    /// Get the Matrix versions and features supported by the homeserver by
    /// fetching them from the server or the cache.
    ///
    /// If `failsafe` is true, this will try to minimize side effects to avoid
    /// possible deadlocks.
    pub(crate) async fn supported_versions_inner(
        &self,
        failsafe: bool,
    ) -> HttpResult<SupportedVersions> {
        let cached_supported_versions = &self.inner.caches.supported_versions;
        if let CachedValue::Cached(val) = &*cached_supported_versions.read().await {
            return Ok(val.clone());
        }

        let mut guarded_supported_versions = cached_supported_versions.write().await;
        if let CachedValue::Cached(val) = &*guarded_supported_versions {
            return Ok(val.clone());
        }

        let supported = self.load_or_fetch_supported_versions(failsafe).await?;

        // Fill both unstable features and server versions at once.
        let mut supported_versions = supported.supported_versions();
        if supported_versions.versions.is_empty() {
            supported_versions.versions = [MatrixVersion::V1_0].into();
        }

        // Only cache the result if the request was authenticated.
        if self.auth_ctx().has_valid_access_token() {
            *guarded_supported_versions = CachedValue::Cached(supported_versions.clone());
        }

        Ok(supported_versions)
    }

    /// Get the Matrix versions and features supported by the homeserver by
    /// fetching them from the cache.
    ///
    /// For a version of this function that fetches the supported versions and
    /// features from the homeserver if the [`SupportedVersions`] aren't
    /// found in the cache, take a look at the [`Client::server_versions()`]
    /// method.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use ruma::api::{FeatureFlag, MatrixVersion};
    /// # use matrix_sdk::{Client, config::SyncSettings};
    /// # use url::Url;
    /// # async {
    /// # let homeserver = Url::parse("http://localhost:8080")?;
    /// # let mut client = Client::new(homeserver).await?;
    ///
    /// let supported =
    ///     if let Some(supported) = client.supported_versions_cached().await? {
    ///         supported
    ///     } else {
    ///         client.fetch_server_versions(None).await?.as_supported_versions()
    ///     };
    ///
    /// let supports_1_1 = supported.versions.contains(&MatrixVersion::V1_1);
    /// println!("The homeserver supports Matrix 1.1: {supports_1_1:?}");
    ///
    /// let msc_x_feature = FeatureFlag::from("msc_x");
    /// let supports_msc_x = supported.features.contains(&msc_x_feature);
    /// println!("The homeserver supports msc X: {supports_msc_x:?}");
    /// # anyhow::Ok(()) };
    /// ```
    pub async fn supported_versions_cached(&self) -> Result<Option<SupportedVersions>, StoreError> {
        if let Some(cached) = self.get_cached_supported_versions().await {
            Ok(Some(cached))
        } else if let Some(stored) =
            self.state_store().get_kv_data(StateStoreDataKey::SupportedVersions).await?
        {
            if let Some(supported) =
                stored.into_supported_versions().and_then(|value| value.into_data())
            {
                Ok(Some(supported.supported_versions()))
            } else {
                Ok(None)
            }
        } else {
            Ok(None)
        }
    }

    /// Get the Matrix versions supported by the homeserver by fetching them
    /// from the server or the cache.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use ruma::api::MatrixVersion;
    /// # use matrix_sdk::{Client, config::SyncSettings};
    /// # use url::Url;
    /// # async {
    /// # let homeserver = Url::parse("http://localhost:8080")?;
    /// # let mut client = Client::new(homeserver).await?;
    ///
    /// let server_versions = client.server_versions().await?;
    /// let supports_1_1 = server_versions.contains(&MatrixVersion::V1_1);
    /// println!("The homeserver supports Matrix 1.1: {supports_1_1:?}");
    /// # anyhow::Ok(()) };
    /// ```
    pub async fn server_versions(&self) -> HttpResult<BTreeSet<MatrixVersion>> {
        Ok(self.supported_versions().await?.versions)
    }

    /// Get the unstable features supported by the homeserver by fetching them
    /// from the server or the cache.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use matrix_sdk::ruma::api::FeatureFlag;
    /// # use matrix_sdk::{Client, config::SyncSettings};
    /// # use url::Url;
    /// # async {
    /// # let homeserver = Url::parse("http://localhost:8080")?;
    /// # let mut client = Client::new(homeserver).await?;
    ///
    /// let msc_x_feature = FeatureFlag::from("msc_x");
    /// let unstable_features = client.unstable_features().await?;
    /// let supports_msc_x = unstable_features.contains(&msc_x_feature);
    /// println!("The homeserver supports msc X: {supports_msc_x:?}");
    /// # anyhow::Ok(()) };
    /// ```
    pub async fn unstable_features(&self) -> HttpResult<BTreeSet<FeatureFlag>> {
        Ok(self.supported_versions().await?.features)
    }

    /// Empty the supported versions and unstable features cache.
    ///
    /// Since the SDK caches the supported versions, it's possible to have a
    /// stale entry in the cache. This functions makes it possible to force
    /// reset it.
    pub async fn reset_supported_versions(&self) -> Result<()> {
        // Empty the in-memory caches.
        self.inner.caches.supported_versions.write().await.take();

        // Empty the store cache.
        Ok(self.state_store().remove_kv_data(StateStoreDataKey::SupportedVersions).await?)
    }

    /// Load well-known from storage, or fetch it from network and cache it.
    async fn load_or_fetch_well_known(&self) -> HttpResult<Option<WellKnownResponse>> {
        match self.state_store().get_kv_data(StateStoreDataKey::WellKnown).await {
            Ok(Some(stored)) => {
                if let Some(well_known) =
                    stored.into_well_known().and_then(|value| value.into_data())
                {
                    return Ok(well_known);
                }
            }
            Ok(None) => {
                // fallthrough: cache is empty
            }
            Err(err) => {
                warn!("error when loading cached well-known: {err}");
                // fallthrough to network.
            }
        }

        let well_known = self.fetch_client_well_known().await.map(Into::into);

        // Attempt to cache the result in storage.
        if let Err(err) = self
            .state_store()
            .set_kv_data(
                StateStoreDataKey::WellKnown,
                StateStoreDataValue::WellKnown(TtlStoreValue::new(well_known.clone())),
            )
            .await
        {
            warn!("error when caching well-known: {err}");
        }

        Ok(well_known)
    }

    async fn get_or_load_and_cache_well_known(&self) -> HttpResult<Option<WellKnownResponse>> {
        let well_known = &self.inner.caches.well_known;
        if let CachedValue::Cached(val) = &*well_known.read().await {
            return Ok(val.clone());
        }

        let mut guarded_well_known = well_known.write().await;
        if let CachedValue::Cached(val) = &*guarded_well_known {
            return Ok(val.clone());
        }

        let well_known = self.load_or_fetch_well_known().await?;

        *guarded_well_known = CachedValue::Cached(well_known.clone());

        Ok(well_known)
    }

    /// Get information about the homeserver's advertised RTC foci by fetching
    /// the well-known file from the server or the cache.
    ///
    /// # Examples
    /// ```no_run
    /// # use matrix_sdk::{Client, config::SyncSettings, ruma::api::client::discovery::discover_homeserver::RtcFocusInfo};
    /// # use url::Url;
    /// # async {
    /// # let homeserver = Url::parse("http://localhost:8080")?;
    /// # let mut client = Client::new(homeserver).await?;
    /// let rtc_foci = client.rtc_foci().await?;
    /// let default_livekit_focus_info = rtc_foci.iter().find_map(|focus| match focus {
    ///     RtcFocusInfo::LiveKit(info) => Some(info),
    ///     _ => None,
    /// });
    /// if let Some(info) = default_livekit_focus_info {
    ///     println!("Default LiveKit service URL: {}", info.service_url);
    /// }
    /// # anyhow::Ok(()) };
    /// ```
    pub async fn rtc_foci(&self) -> HttpResult<Vec<RtcFocusInfo>> {
        let well_known = self.get_or_load_and_cache_well_known().await?;

        Ok(well_known.map(|well_known| well_known.rtc_foci).unwrap_or_default())
    }

    /// Empty the well-known cache.
    ///
    /// Since the SDK caches the well-known, it's possible to have a stale entry
    /// in the cache. This functions makes it possible to force reset it.
    pub async fn reset_well_known(&self) -> Result<()> {
        // Empty the in-memory caches.
        self.inner.caches.well_known.write().await.take();

        // Empty the store cache.
        Ok(self.state_store().remove_kv_data(StateStoreDataKey::WellKnown).await?)
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
        Ok(self.unstable_features().await?.contains(&FeatureFlag::from("org.matrix.msc4028")))
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

        self.send(request).await
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

        self.send(request).await
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

        self.send(request).await
    }

    /// Check whether a device with a specific ID exists on the server.
    ///
    /// Returns Ok(true) if the device exists, Ok(false) if the server responded
    /// with 404 and the underlying error otherwise.
    ///
    /// # Arguments
    ///
    /// * `device_id` - The ID of the device to query.
    pub async fn device_exists(&self, device_id: OwnedDeviceId) -> Result<bool> {
        let request = device::get_device::v3::Request::new(device_id);
        match self.send(request).await {
            Ok(_) => Ok(true),
            Err(err) => {
                if let Some(error) = err.as_client_api_error()
                    && error.status_code == 404
                {
                    Ok(false)
                } else {
                    Err(err.into())
                }
            }
        }
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
    ///     Client, config::SyncSettings,
    ///     ruma::events::room::message::OriginalSyncRoomMessageEvent,
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

        let token = match sync_settings.token {
            SyncToken::Specific(token) => Some(token),
            SyncToken::NoToken => None,
            SyncToken::ReusePrevious => self.sync_token().await,
        };

        let request = assign!(sync_events::v3::Request::new(), {
            filter: sync_settings.filter.map(|f| *f),
            since: token,
            full_state: sync_settings.full_state,
            set_presence: sync_settings.set_presence,
            timeout: sync_settings.timeout,
            use_state_after: true,
        });
        let mut request_config = self.request_config();
        if let Some(timeout) = sync_settings.timeout {
            let base_timeout = request_config.timeout.unwrap_or(Duration::from_secs(30));
            request_config.timeout = Some(base_timeout + timeout);
        }

        let response = self.send(request).with_request_config(request_config).await?;
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
    ///     Client, config::SyncSettings,
    ///     ruma::events::room::message::OriginalSyncRoomMessageEvent,
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
    ///         for (room_id, room) in response.rooms.joined {
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
    ///         for (room_id, room) in sync_response.rooms.joined {
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
        sync_settings: crate::config::SyncSettings,
        callback: impl Fn(Result<SyncResponse, Error>) -> C,
    ) -> Result<(), Error>
    where
        C: Future<Output = Result<LoopCtrl, Error>>,
    {
        let mut sync_stream = Box::pin(self.sync_stream(sync_settings).await);

        while let Some(result) = sync_stream.next().await {
            trace!("Running callback");
            if callback(result).await? == LoopCtrl::Break {
                trace!("Callback told us to stop");
                break;
            }
            trace!("Done running callback");
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
    /// use matrix_sdk::{Client, config::SyncSettings};
    ///
    /// let client = Client::new(homeserver).await?;
    /// client.matrix_auth().login_username(&username, &password).send().await?;
    ///
    /// let mut sync_stream =
    ///     Box::pin(client.sync_stream(SyncSettings::default()).await);
    ///
    /// while let Some(Ok(response)) = sync_stream.next().await {
    ///     for room in response.rooms.joined.values() {
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
        let mut is_first_sync = true;
        let mut timeout = None;
        let mut last_sync_time: Option<Instant> = None;

        let parent_span = Span::current();

        async_stream::stream!({
            loop {
                trace!("Syncing");

                if sync_settings.ignore_timeout_on_first_sync {
                    if is_first_sync {
                        timeout = sync_settings.timeout.take();
                    } else if sync_settings.timeout.is_none() && timeout.is_some() {
                        sync_settings.timeout = timeout.take();
                    }

                    is_first_sync = false;
                }

                yield self
                    .sync_loop_helper(&mut sync_settings)
                    .instrument(parent_span.clone())
                    .await;

                Client::delay_sync(&mut last_sync_time).await
            }
        })
    }

    /// Get the current, if any, sync token of the client.
    /// This will be None if the client didn't sync at least once.
    pub(crate) async fn sync_token(&self) -> Option<String> {
        self.inner.base_client.sync_token().await
    }

    /// Gets information about the owner of a given access token.
    pub async fn whoami(&self) -> HttpResult<whoami::v3::Response> {
        let request = whoami::v3::Request::new();
        self.send(request).await
    }

    /// Subscribes a new receiver to client SessionChange broadcasts.
    pub fn subscribe_to_session_changes(&self) -> broadcast::Receiver<SessionChange> {
        let broadcast = &self.auth_ctx().session_change_sender;
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
    ///
    /// See [`CrossProcessLock::new`] to learn more about
    /// `cross_process_store_locks_holder_name`.
    ///
    /// [`CrossProcessLock::new`]: matrix_sdk_common::cross_process_lock::CrossProcessLock::new
    pub async fn notification_client(
        &self,
        cross_process_store_locks_holder_name: String,
    ) -> Result<Client> {
        let client = Client {
            inner: ClientInner::new(
                self.inner.auth_ctx.clone(),
                self.server().cloned(),
                self.homeserver(),
                self.sliding_sync_version(),
                self.inner.http_client.clone(),
                self.inner
                    .base_client
                    .clone_with_in_memory_state_store(&cross_process_store_locks_holder_name, false)
                    .await?,
                self.inner.caches.supported_versions.read().await.clone(),
                self.inner.caches.well_known.read().await.clone(),
                self.inner.respect_login_well_known,
                self.inner.event_cache.clone(),
                self.inner.send_queue_data.clone(),
                self.inner.latest_events.clone(),
                #[cfg(feature = "e2e-encryption")]
                self.inner.e2ee.encryption_settings,
                #[cfg(feature = "e2e-encryption")]
                self.inner.enable_share_history_on_invite,
                cross_process_store_locks_holder_name,
                #[cfg(feature = "experimental-search")]
                self.inner.search_index.clone(),
                self.inner.thread_subscription_catchup.clone(),
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

    /// The [`LatestEvents`] instance for this [`Client`].
    pub async fn latest_events(&self) -> &LatestEvents {
        self.inner
            .latest_events
            .get_or_init(|| async {
                LatestEvents::new(
                    WeakClient::from_client(self),
                    self.event_cache().clone(),
                    SendQueue::new(self.clone()),
                )
            })
            .await
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
    pub async fn knock(
        &self,
        room_id_or_alias: OwnedRoomOrAliasId,
        reason: Option<String>,
        server_names: Vec<OwnedServerName>,
    ) -> Result<Room> {
        let request =
            assign!(knock_room::v3::Request::new(room_id_or_alias), { reason, via: server_names });
        let response = self.send(request).await?;
        let base_room = self.inner.base_client.room_knocked(&response.room_id).await?;
        Ok(Room::new(self.clone(), base_room))
    }

    /// Checks whether the provided `user_id` belongs to an ignored user.
    pub async fn is_user_ignored(&self, user_id: &UserId) -> bool {
        self.base_client().is_user_ignored(user_id).await
    }

    /// Gets the `max_upload_size` value from the homeserver, getting either a
    /// cached value or with a `/_matrix/client/v1/media/config` request if it's
    /// missing.
    ///
    /// Check the spec for more info:
    /// <https://spec.matrix.org/v1.14/client-server-api/#get_matrixclientv1mediaconfig>
    pub async fn load_or_fetch_max_upload_size(&self) -> Result<UInt> {
        let max_upload_size_lock = self.inner.server_max_upload_size.lock().await;
        if let Some(data) = max_upload_size_lock.get() {
            return Ok(data.to_owned());
        }

        // Use the authenticated endpoint when the server supports it.
        let supported_versions = self.supported_versions().await?;
        let use_auth = authenticated_media::get_media_config::v1::Request::PATH_BUILDER
            .is_supported(&supported_versions);

        let upload_size = if use_auth {
            self.send(authenticated_media::get_media_config::v1::Request::default())
                .await?
                .upload_size
        } else {
            #[allow(deprecated)]
            self.send(media::get_media_config::v3::Request::default()).await?.upload_size
        };

        match max_upload_size_lock.set(upload_size) {
            Ok(_) => Ok(upload_size),
            Err(error) => {
                Err(Error::Media(MediaError::FetchMaxUploadSizeFailed(error.to_string())))
            }
        }
    }

    /// The settings to use for decrypting events.
    #[cfg(feature = "e2e-encryption")]
    pub fn decryption_settings(&self) -> &DecryptionSettings {
        &self.base_client().decryption_settings
    }

    /// Returns the [`SearchIndex`] for this [`Client`].
    #[cfg(feature = "experimental-search")]
    pub fn search_index(&self) -> &SearchIndex {
        &self.inner.search_index
    }

    /// Whether the client is configured to take thread subscriptions (MSC4306
    /// and MSC4308) into account.
    ///
    /// This may cause filtering out of thread subscriptions, and loading the
    /// thread subscriptions via the sliding sync extension, when the room
    /// list service is being used.
    pub fn enabled_thread_subscriptions(&self) -> bool {
        match self.base_client().threading_support {
            ThreadingSupport::Enabled { with_subscriptions } => with_subscriptions,
            ThreadingSupport::Disabled => false,
        }
    }

    /// Fetch thread subscriptions changes between `from` and up to `to`.
    ///
    /// The `limit` optional parameter can be used to limit the number of
    /// entries in a response. It can also be overridden by the server, if
    /// it's deemed too large.
    pub async fn fetch_thread_subscriptions(
        &self,
        from: Option<String>,
        to: Option<String>,
        limit: Option<UInt>,
    ) -> Result<get_thread_subscriptions_changes::unstable::Response> {
        let request = assign!(get_thread_subscriptions_changes::unstable::Request::new(), {
            from,
            to,
            limit,
        });
        Ok(self.send(request).await?)
    }

    pub(crate) fn thread_subscription_catchup(&self) -> &ThreadSubscriptionCatchup {
        self.inner.thread_subscription_catchup.get().unwrap()
    }

    /// Perform database optimizations if any are available, i.e. vacuuming in
    /// SQLite.
    ///
    /// **Warning:** this was added to check if SQLite fragmentation was the
    /// source of performance issues, **DO NOT use in production**.
    #[doc(hidden)]
    pub async fn optimize_stores(&self) -> Result<()> {
        trace!("Optimizing state store...");
        self.state_store().optimize().await?;

        trace!("Optimizing event cache store...");
        if let Some(clean_lock) = self.event_cache_store().lock().await?.as_clean() {
            clean_lock.optimize().await?;
        }

        trace!("Optimizing media store...");
        self.media_store().lock().await?.optimize().await?;

        Ok(())
    }

    /// Returns the sizes of the existing stores, if known.
    pub async fn get_store_sizes(&self) -> Result<StoreSizes> {
        #[cfg(feature = "e2e-encryption")]
        let crypto_store_size = if let Some(olm_machine) = self.olm_machine().await.as_ref()
            && let Ok(Some(store_size)) = olm_machine.store().get_size().await
        {
            Some(store_size)
        } else {
            None
        };
        #[cfg(not(feature = "e2e-encryption"))]
        let crypto_store_size = None;

        let state_store_size = self.state_store().get_size().await.ok().flatten();

        let event_cache_store_size = if let Some(clean_lock) =
            self.event_cache_store().lock().await?.as_clean()
            && let Ok(Some(store_size)) = clean_lock.get_size().await
        {
            Some(store_size)
        } else {
            None
        };

        let media_store_size = self.media_store().lock().await?.get_size().await.ok().flatten();

        Ok(StoreSizes {
            crypto_store: crypto_store_size,
            state_store: state_store_size,
            event_cache_store: event_cache_store_size,
            media_store: media_store_size,
        })
    }
}

/// Contains the disk size of the different stores, if known. It won't be
/// available for in-memory stores.
#[derive(Debug, Clone)]
pub struct StoreSizes {
    /// The size of the CryptoStore.
    pub crypto_store: Option<usize>,
    /// The size of the StateStore.
    pub state_store: Option<usize>,
    /// The size of the EventCacheStore.
    pub event_cache_store: Option<usize>,
    /// The size of the MediaStore.
    pub media_store: Option<usize>,
}

#[cfg(any(feature = "testing", test))]
impl Client {
    /// Test helper to mark users as tracked by the crypto layer.
    #[cfg(feature = "e2e-encryption")]
    pub async fn update_tracked_users_for_testing(
        &self,
        user_ids: impl IntoIterator<Item = &UserId>,
    ) {
        let olm = self.olm_machine().await;
        let olm = olm.as_ref().unwrap();
        olm.update_tracked_users(user_ids).await.unwrap();
    }
}

/// A weak reference to the inner client, useful when trying to get a handle
/// on the owning client.
#[derive(Clone, Debug)]
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

/// Information about the state of a room before we joined it.
#[derive(Debug, Clone, Default)]
struct PreJoinRoomInfo {
    /// The user who invited us to the room, if any.
    pub inviter: Option<RoomMember>,
}

// The http mocking library is not supported for wasm32
#[cfg(all(test, not(target_family = "wasm")))]
pub(crate) mod tests {
    use std::{sync::Arc, time::Duration};

    use assert_matches::assert_matches;
    use assert_matches2::assert_let;
    use eyeball::SharedObservable;
    use futures_util::{FutureExt, StreamExt, pin_mut};
    use js_int::{UInt, uint};
    use matrix_sdk_base::{
        RoomState,
        store::{MemoryStore, StoreConfig},
    };
    use matrix_sdk_test::{
        DEFAULT_TEST_ROOM_ID, JoinedRoomBuilder, StateTestEvent, SyncResponseBuilder, async_test,
        event_factory::EventFactory,
    };
    #[cfg(target_family = "wasm")]
    wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_browser);

    use ruma::{
        RoomId, ServerName, UserId,
        api::{
            FeatureFlag, MatrixVersion,
            client::{
                discovery::discover_homeserver::RtcFocusInfo,
                room::create_room::v3::Request as CreateRoomRequest,
            },
        },
        assign,
        events::{
            ignored_user_list::IgnoredUserListEventContent,
            media_preview_config::{InviteAvatars, MediaPreviewConfigEventContent, MediaPreviews},
        },
        owned_device_id, owned_room_id, owned_user_id, room_alias_id, room_id, user_id,
    };
    use serde_json::json;
    use stream_assert::{assert_next_matches, assert_pending};
    use tokio::{
        spawn,
        time::{sleep, timeout},
    };
    use url::Url;

    use super::Client;
    use crate::{
        Error, TransmissionProgress,
        client::{WeakClient, futures::SendMediaUploadRequest},
        config::{RequestConfig, SyncSettings},
        futures::SendRequest,
        media::MediaError,
        test_utils::{client::MockClientBuilder, mocks::MatrixMockServer},
    };

    #[async_test]
    async fn test_account_data() {
        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;

        let f = EventFactory::new();
        server
            .mock_sync()
            .ok_and_run(&client, |builder| {
                builder.add_global_account_data(
                    f.ignored_user_list([owned_user_id!("@someone:example.org")]),
                );
            })
            .await;

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
        let server = MatrixMockServer::new().await;
        let server_url = server.uri();

        // Imagine this is `matrix-client.matrix.org`.
        let homeserver = MatrixMockServer::new().await;
        let homeserver_url = homeserver.uri();

        // Imagine Alice has the user ID `@alice:matrix.org`.
        let domain = server_url.strip_prefix("http://").unwrap();
        let alice = UserId::parse("@alice:".to_owned() + domain).unwrap();

        // The `.well-known` is on the server (e.g. `matrix.org`).
        server
            .mock_well_known()
            .ok_with_homeserver_url(&homeserver_url)
            .mock_once()
            .named("well-known")
            .mount()
            .await;

        // The `/versions` is on the homeserver (e.g. `matrix-client.matrix.org`).
        homeserver.mock_versions().ok().mock_once().named("versions").mount().await;

        let client = Client::builder()
            .insecure_server_name_no_tls(alice.server_name())
            .build()
            .await
            .unwrap();

        assert_eq!(client.server().unwrap(), &Url::parse(&server_url).unwrap());
        assert_eq!(client.homeserver(), Url::parse(&homeserver_url).unwrap());
        client.server_versions().await.unwrap();
    }

    #[async_test]
    async fn test_discovery_broken_server() {
        let server = MatrixMockServer::new().await;
        let server_url = server.uri();
        let domain = server_url.strip_prefix("http://").unwrap();
        let alice = UserId::parse("@alice:".to_owned() + domain).unwrap();

        server.mock_well_known().error404().mock_once().named("well-known").mount().await;

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
        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;

        server
            .mock_sync()
            .ok_and_run(&client, |builder| {
                builder.add_joined_room(
                    JoinedRoomBuilder::default()
                        .add_state_event(StateTestEvent::Member)
                        .add_state_event(StateTestEvent::PowerLevels),
                );
            })
            .await;

        let room = client.get_room(&DEFAULT_TEST_ROOM_ID).unwrap();
        assert_eq!(room.state(), RoomState::Joined);
    }

    #[async_test]
    async fn test_retry_limit_http_requests() {
        let server = MatrixMockServer::new().await;
        let client = server
            .client_builder()
            .on_builder(|builder| builder.request_config(RequestConfig::new().retry_limit(4)))
            .build()
            .await;

        assert!(client.request_config().retry_limit.unwrap() == 4);

        server.mock_who_am_i().error500().expect(4).mount().await;

        client.whoami().await.unwrap_err();
    }

    #[async_test]
    async fn test_retry_timeout_http_requests() {
        // Keep this timeout small so that the test doesn't take long
        let retry_timeout = Duration::from_secs(5);
        let server = MatrixMockServer::new().await;
        let client = server
            .client_builder()
            .on_builder(|builder| {
                builder.request_config(RequestConfig::new().max_retry_time(retry_timeout))
            })
            .build()
            .await;

        assert!(client.request_config().max_retry_time.unwrap() == retry_timeout);

        server.mock_login().error500().expect(2..).mount().await;

        client.matrix_auth().login_username("example", "wordpass").send().await.unwrap_err();
    }

    #[async_test]
    async fn test_short_retry_initial_http_requests() {
        let server = MatrixMockServer::new().await;
        let client = server
            .client_builder()
            .on_builder(|builder| builder.request_config(RequestConfig::short_retry()))
            .build()
            .await;

        server.mock_login().error500().expect(3..).mount().await;

        client.matrix_auth().login_username("example", "wordpass").send().await.unwrap_err();
    }

    #[async_test]
    async fn test_no_retry_http_requests() {
        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;

        server.mock_devices().error500().mock_once().mount().await;

        client.devices().await.unwrap_err();
    }

    #[async_test]
    async fn test_set_homeserver() {
        let client = MockClientBuilder::new(None).build().await;
        assert_eq!(client.homeserver().as_ref(), "http://localhost/");

        let homeserver = Url::parse("http://example.com/").unwrap();
        client.set_homeserver(homeserver.clone());
        assert_eq!(client.homeserver(), homeserver);
    }

    #[async_test]
    async fn test_search_user_request() {
        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;

        server.mock_user_directory().ok().mock_once().mount().await;

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
        let server = MatrixMockServer::new().await;
        let client = server.client_builder().no_server_versions().build().await;

        server.mock_versions().ok_with_unstable_features().mock_once().mount().await;

        let unstable_features = client.unstable_features().await.unwrap();
        assert!(unstable_features.contains(&FeatureFlag::from("org.matrix.e2e_cross_signing")));
        assert!(!unstable_features.contains(&FeatureFlag::from("you.shall.pass")));
    }

    #[async_test]
    async fn test_can_homeserver_push_encrypted_event_to_device() {
        let server = MatrixMockServer::new().await;
        let client = server.client_builder().no_server_versions().build().await;

        server.mock_versions().ok_with_unstable_features().mock_once().mount().await;

        let msc4028_enabled = client.can_homeserver_push_encrypted_event_to_device().await.unwrap();
        assert!(msc4028_enabled);
    }

    #[async_test]
    async fn test_recently_visited_rooms() {
        // Tracking recently visited rooms requires authentication
        let client = MockClientBuilder::new(None).unlogged().build().await;
        assert_matches!(
            client.account().track_recently_visited_room(owned_room_id!("!alpha:localhost")).await,
            Err(Error::AuthenticationRequired)
        );

        let client = MockClientBuilder::new(None).build().await;
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
        let client = MockClientBuilder::new(None).build().await;

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
    async fn test_supported_versions_caching() {
        let server = MatrixMockServer::new().await;

        let versions_mock = server
            .mock_versions()
            .expect_default_access_token()
            .ok_with_unstable_features()
            .named("first versions mock")
            .expect(1)
            .mount_as_scoped()
            .await;

        let memory_store = Arc::new(MemoryStore::new());
        let client = server
            .client_builder()
            .no_server_versions()
            .on_builder(|builder| {
                builder.store_config(
                    StoreConfig::new("cross-process-store-locks-holder-name".to_owned())
                        .state_store(memory_store.clone()),
                )
            })
            .build()
            .await;

        assert!(client.server_versions().await.unwrap().contains(&MatrixVersion::V1_0));

        // The result was cached.
        assert_matches!(client.supported_versions_cached().await, Ok(Some(_)));
        // This subsequent call hits the in-memory cache.
        assert!(client.server_versions().await.unwrap().contains(&MatrixVersion::V1_0));

        drop(client);

        let client = server
            .client_builder()
            .no_server_versions()
            .on_builder(|builder| {
                builder.store_config(
                    StoreConfig::new("cross-process-store-locks-holder-name".to_owned())
                        .state_store(memory_store.clone()),
                )
            })
            .build()
            .await;

        // These calls to the new client hit the on-disk cache.
        assert!(
            client
                .unstable_features()
                .await
                .unwrap()
                .contains(&FeatureFlag::from("org.matrix.e2e_cross_signing"))
        );
        let supported = client.supported_versions().await.unwrap();
        assert!(supported.versions.contains(&MatrixVersion::V1_0));
        assert!(supported.features.contains(&FeatureFlag::from("org.matrix.e2e_cross_signing")));

        // Then this call hits the in-memory cache.
        let supported = client.supported_versions().await.unwrap();
        assert!(supported.versions.contains(&MatrixVersion::V1_0));
        assert!(supported.features.contains(&FeatureFlag::from("org.matrix.e2e_cross_signing")));

        drop(versions_mock);

        // Now, reset the cache, and observe the endpoints being called again once.
        client.reset_supported_versions().await.unwrap();

        server.mock_versions().ok().expect(1).named("second versions mock").mount().await;

        // Hits network again.
        assert!(client.server_versions().await.unwrap().contains(&MatrixVersion::V1_0));
        // Hits in-memory cache again.
        assert!(client.server_versions().await.unwrap().contains(&MatrixVersion::V1_0));
    }

    #[async_test]
    async fn test_well_known_caching() {
        let server = MatrixMockServer::new().await;
        let server_url = server.uri();
        let domain = server_url.strip_prefix("http://").unwrap();
        let server_name = <&ServerName>::try_from(domain).unwrap();
        let rtc_foci = vec![RtcFocusInfo::livekit("https://livekit.example.com".to_owned())];

        let well_known_mock = server
            .mock_well_known()
            .ok()
            .named("well known mock")
            .expect(2) // One for ClientBuilder discovery, one for the ServerInfo cache.
            .mount_as_scoped()
            .await;

        let memory_store = Arc::new(MemoryStore::new());
        let client = Client::builder()
            .insecure_server_name_no_tls(server_name)
            .store_config(
                StoreConfig::new("cross-process-store-locks-holder-name".to_owned())
                    .state_store(memory_store.clone()),
            )
            .build()
            .await
            .unwrap();

        assert_eq!(client.rtc_foci().await.unwrap(), rtc_foci);

        // This subsequent call hits the in-memory cache.
        assert_eq!(client.rtc_foci().await.unwrap(), rtc_foci);

        drop(client);

        let client = server
            .client_builder()
            .no_server_versions()
            .on_builder(|builder| {
                builder.store_config(
                    StoreConfig::new("cross-process-store-locks-holder-name".to_owned())
                        .state_store(memory_store.clone()),
                )
            })
            .build()
            .await;

        // This call to the new client hits the on-disk cache.
        assert_eq!(client.rtc_foci().await.unwrap(), rtc_foci);

        // Then this call hits the in-memory cache.
        assert_eq!(client.rtc_foci().await.unwrap(), rtc_foci);

        drop(well_known_mock);

        // Now, reset the cache, and observe the endpoints being called again once.
        client.reset_well_known().await.unwrap();

        server.mock_well_known().ok().named("second well known mock").expect(1).mount().await;

        // Hits network again.
        assert_eq!(client.rtc_foci().await.unwrap(), rtc_foci);
        // Hits in-memory cache again.
        assert_eq!(client.rtc_foci().await.unwrap(), rtc_foci);
    }

    #[async_test]
    async fn test_missing_well_known_caching() {
        let server = MatrixMockServer::new().await;
        let rtc_foci: Vec<RtcFocusInfo> = vec![];

        let well_known_mock = server
            .mock_well_known()
            .error_unrecognized()
            .named("first well-known mock")
            .expect(1)
            .mount_as_scoped()
            .await;

        let memory_store = Arc::new(MemoryStore::new());
        let client = server
            .client_builder()
            .on_builder(|builder| {
                builder.store_config(
                    StoreConfig::new("cross-process-store-locks-holder-name".to_owned())
                        .state_store(memory_store.clone()),
                )
            })
            .build()
            .await;

        assert_eq!(client.rtc_foci().await.unwrap(), rtc_foci);

        // This subsequent call hits the in-memory cache.
        assert_eq!(client.rtc_foci().await.unwrap(), rtc_foci);

        drop(client);

        let client = server
            .client_builder()
            .on_builder(|builder| {
                builder.store_config(
                    StoreConfig::new("cross-process-store-locks-holder-name".to_owned())
                        .state_store(memory_store.clone()),
                )
            })
            .build()
            .await;

        // This call to the new client hits the on-disk cache.
        assert_eq!(client.rtc_foci().await.unwrap(), rtc_foci);

        // Then this call hits the in-memory cache.
        assert_eq!(client.rtc_foci().await.unwrap(), rtc_foci);

        drop(well_known_mock);

        // Now, reset the cache, and observe the endpoints being called again once.
        client.reset_well_known().await.unwrap();

        server
            .mock_well_known()
            .error_unrecognized()
            .expect(1)
            .named("second well-known mock")
            .mount()
            .await;

        // Hits network again.
        assert_eq!(client.rtc_foci().await.unwrap(), rtc_foci);
        // Hits in-memory cache again.
        assert_eq!(client.rtc_foci().await.unwrap(), rtc_foci);
    }

    #[async_test]
    async fn test_no_network_doesnt_cause_infinite_retries() {
        // We want infinite retries for transient errors.
        let client = MockClientBuilder::new(None)
            .on_builder(|builder| builder.request_config(RequestConfig::new()))
            .build()
            .await;

        // We don't define a mock server on purpose here, so that the error is really a
        // network error.
        client.whoami().await.unwrap_err();
    }

    #[async_test]
    async fn test_await_room_remote_echo_returns_the_room_if_it_was_already_synced() {
        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;

        let room_id = room_id!("!room:example.org");

        server
            .mock_sync()
            .ok_and_run(&client, |builder| {
                builder.add_joined_room(JoinedRoomBuilder::new(room_id));
            })
            .await;

        let room = client.await_room_remote_echo(room_id).now_or_never().unwrap();
        assert_eq!(room.room_id(), room_id);
    }

    #[async_test]
    async fn test_await_room_remote_echo_returns_the_room_when_it_is_ready() {
        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;

        let room_id = room_id!("!room:example.org");

        let client = Arc::new(client);

        // Perform the /sync request with a delay so it starts after the
        // `await_room_remote_echo` call has happened
        spawn({
            let client = client.clone();
            async move {
                sleep(Duration::from_millis(100)).await;

                server
                    .mock_sync()
                    .ok_and_run(&client, |builder| {
                        builder.add_joined_room(JoinedRoomBuilder::new(room_id));
                    })
                    .await;
            }
        });

        let room =
            timeout(Duration::from_secs(10), client.await_room_remote_echo(room_id)).await.unwrap();
        assert_eq!(room.room_id(), room_id);
    }

    #[async_test]
    async fn test_await_room_remote_echo_will_timeout_if_no_room_is_found() {
        let client = MockClientBuilder::new(None).build().await;

        let room_id = room_id!("!room:example.org");
        // Room is not present so the client won't be able to find it. The call will
        // timeout.
        timeout(Duration::from_secs(1), client.await_room_remote_echo(room_id)).await.unwrap_err();
    }

    #[async_test]
    async fn test_await_room_remote_echo_will_timeout_if_room_is_found_but_not_synced() {
        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;

        server.mock_create_room().ok().mount().await;

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

    #[async_test]
    async fn test_is_room_alias_available_if_alias_is_not_resolved() {
        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;

        server.mock_room_directory_resolve_alias().not_found().expect(1).mount().await;

        let ret = client.is_room_alias_available(room_alias_id!("#some_alias:matrix.org")).await;
        assert_matches!(ret, Ok(true));
    }

    #[async_test]
    async fn test_is_room_alias_available_if_alias_is_resolved() {
        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;

        server
            .mock_room_directory_resolve_alias()
            .ok("!some_room_id:matrix.org", Vec::new())
            .expect(1)
            .mount()
            .await;

        let ret = client.is_room_alias_available(room_alias_id!("#some_alias:matrix.org")).await;
        assert_matches!(ret, Ok(false));
    }

    #[async_test]
    async fn test_is_room_alias_available_if_error_found() {
        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;

        server.mock_room_directory_resolve_alias().error500().expect(1).mount().await;

        let ret = client.is_room_alias_available(room_alias_id!("#some_alias:matrix.org")).await;
        assert_matches!(ret, Err(_));
    }

    #[async_test]
    async fn test_create_room_alias() {
        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;

        server.mock_room_directory_create_room_alias().ok().expect(1).mount().await;

        let ret = client
            .create_room_alias(
                room_alias_id!("#some_alias:matrix.org"),
                room_id!("!some_room:matrix.org"),
            )
            .await;
        assert_matches!(ret, Ok(()));
    }

    #[async_test]
    async fn test_room_preview_for_invited_room_hits_summary_endpoint() {
        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;

        let room_id = room_id!("!a-room:matrix.org");

        // Make sure the summary endpoint is called once
        server.mock_room_summary().ok(room_id).mock_once().mount().await;

        // We create a locally cached invited room
        let invited_room = client.inner.base_client.get_or_create_room(room_id, RoomState::Invited);

        // And we get a preview, the server endpoint was reached
        let preview = client
            .get_room_preview(room_id.into(), Vec::new())
            .await
            .expect("Room preview should be retrieved");

        assert_eq!(invited_room.room_id().to_owned(), preview.room_id);
    }

    #[async_test]
    async fn test_room_preview_for_left_room_hits_summary_endpoint() {
        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;

        let room_id = room_id!("!a-room:matrix.org");

        // Make sure the summary endpoint is called once
        server.mock_room_summary().ok(room_id).mock_once().mount().await;

        // We create a locally cached left room
        let left_room = client.inner.base_client.get_or_create_room(room_id, RoomState::Left);

        // And we get a preview, the server endpoint was reached
        let preview = client
            .get_room_preview(room_id.into(), Vec::new())
            .await
            .expect("Room preview should be retrieved");

        assert_eq!(left_room.room_id().to_owned(), preview.room_id);
    }

    #[async_test]
    async fn test_room_preview_for_knocked_room_hits_summary_endpoint() {
        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;

        let room_id = room_id!("!a-room:matrix.org");

        // Make sure the summary endpoint is called once
        server.mock_room_summary().ok(room_id).mock_once().mount().await;

        // We create a locally cached knocked room
        let knocked_room = client.inner.base_client.get_or_create_room(room_id, RoomState::Knocked);

        // And we get a preview, the server endpoint was reached
        let preview = client
            .get_room_preview(room_id.into(), Vec::new())
            .await
            .expect("Room preview should be retrieved");

        assert_eq!(knocked_room.room_id().to_owned(), preview.room_id);
    }

    #[async_test]
    async fn test_room_preview_for_joined_room_retrieves_local_room_info() {
        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;

        let room_id = room_id!("!a-room:matrix.org");

        // Make sure the summary endpoint is not called
        server.mock_room_summary().ok(room_id).never().mount().await;

        // We create a locally cached joined room
        let joined_room = client.inner.base_client.get_or_create_room(room_id, RoomState::Joined);

        // And we get a preview, no server endpoint was reached
        let preview = client
            .get_room_preview(room_id.into(), Vec::new())
            .await
            .expect("Room preview should be retrieved");

        assert_eq!(joined_room.room_id().to_owned(), preview.room_id);
    }

    #[async_test]
    async fn test_media_preview_config() {
        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;

        server
            .mock_sync()
            .ok_and_run(&client, |builder| {
                builder.add_custom_global_account_data(json!({
                    "content": {
                        "media_previews": "private",
                        "invite_avatars": "off"
                    },
                    "type": "m.media_preview_config"
                }));
            })
            .await;

        let (initial_value, stream) =
            client.account().observe_media_preview_config().await.unwrap();

        let initial_value: MediaPreviewConfigEventContent = initial_value.unwrap();
        assert_eq!(initial_value.invite_avatars, Some(InviteAvatars::Off));
        assert_eq!(initial_value.media_previews, Some(MediaPreviews::Private));
        pin_mut!(stream);
        assert_pending!(stream);

        server
            .mock_sync()
            .ok_and_run(&client, |builder| {
                builder.add_custom_global_account_data(json!({
                    "content": {
                        "media_previews": "off",
                        "invite_avatars": "on"
                    },
                    "type": "m.media_preview_config"
                }));
            })
            .await;

        assert_next_matches!(
            stream,
            MediaPreviewConfigEventContent {
                media_previews: Some(MediaPreviews::Off),
                invite_avatars: Some(InviteAvatars::On),
                ..
            }
        );
        assert_pending!(stream);
    }

    #[async_test]
    async fn test_unstable_media_preview_config() {
        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;

        server
            .mock_sync()
            .ok_and_run(&client, |builder| {
                builder.add_custom_global_account_data(json!({
                    "content": {
                        "media_previews": "private",
                        "invite_avatars": "off"
                    },
                    "type": "io.element.msc4278.media_preview_config"
                }));
            })
            .await;

        let (initial_value, stream) =
            client.account().observe_media_preview_config().await.unwrap();

        let initial_value: MediaPreviewConfigEventContent = initial_value.unwrap();
        assert_eq!(initial_value.invite_avatars, Some(InviteAvatars::Off));
        assert_eq!(initial_value.media_previews, Some(MediaPreviews::Private));
        pin_mut!(stream);
        assert_pending!(stream);

        server
            .mock_sync()
            .ok_and_run(&client, |builder| {
                builder.add_custom_global_account_data(json!({
                    "content": {
                        "media_previews": "off",
                        "invite_avatars": "on"
                    },
                    "type": "io.element.msc4278.media_preview_config"
                }));
            })
            .await;

        assert_next_matches!(
            stream,
            MediaPreviewConfigEventContent {
                media_previews: Some(MediaPreviews::Off),
                invite_avatars: Some(InviteAvatars::On),
                ..
            }
        );
        assert_pending!(stream);
    }

    #[async_test]
    async fn test_media_preview_config_not_found() {
        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;

        let (initial_value, _) = client.account().observe_media_preview_config().await.unwrap();

        assert!(initial_value.is_none());
    }

    #[async_test]
    async fn test_load_or_fetch_max_upload_size_with_auth_matrix_version() {
        // The default Matrix version we use is 1.11 or higher, so authenticated media
        // is supported.
        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;

        assert!(!client.inner.server_max_upload_size.lock().await.initialized());

        server.mock_authenticated_media_config().ok(uint!(2)).mock_once().mount().await;
        client.load_or_fetch_max_upload_size().await.unwrap();

        assert_eq!(*client.inner.server_max_upload_size.lock().await.get().unwrap(), uint!(2));
    }

    #[async_test]
    async fn test_load_or_fetch_max_upload_size_with_auth_stable_feature() {
        // The server must advertise support for the stable feature for authenticated
        // media support, so we mock the `GET /versions` response.
        let server = MatrixMockServer::new().await;
        let client = server.client_builder().no_server_versions().build().await;

        server
            .mock_versions()
            .ok_custom(
                &["v1.7", "v1.8", "v1.9", "v1.10"],
                &[("org.matrix.msc3916.stable", true)].into(),
            )
            .named("versions")
            .expect(1)
            .mount()
            .await;

        assert!(!client.inner.server_max_upload_size.lock().await.initialized());

        server.mock_authenticated_media_config().ok(uint!(2)).mock_once().mount().await;
        client.load_or_fetch_max_upload_size().await.unwrap();

        assert_eq!(*client.inner.server_max_upload_size.lock().await.get().unwrap(), uint!(2));
    }

    #[async_test]
    async fn test_load_or_fetch_max_upload_size_no_auth() {
        // The server must not support Matrix 1.11 or higher for unauthenticated
        // media requests, so we mock the `GET /versions` response.
        let server = MatrixMockServer::new().await;
        let client = server.client_builder().no_server_versions().build().await;

        server
            .mock_versions()
            .ok_custom(&["v1.1"], &Default::default())
            .named("versions")
            .expect(1)
            .mount()
            .await;

        assert!(!client.inner.server_max_upload_size.lock().await.initialized());

        server.mock_media_config().ok(uint!(2)).mock_once().mount().await;
        client.load_or_fetch_max_upload_size().await.unwrap();

        assert_eq!(*client.inner.server_max_upload_size.lock().await.get().unwrap(), uint!(2));
    }

    #[async_test]
    async fn test_uploading_a_too_large_media_file() {
        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;

        server.mock_authenticated_media_config().ok(uint!(1)).mock_once().mount().await;
        client.load_or_fetch_max_upload_size().await.unwrap();
        assert_eq!(*client.inner.server_max_upload_size.lock().await.get().unwrap(), uint!(1));

        let data = vec![1, 2];
        let upload_request =
            ruma::api::client::media::create_content::v3::Request::new(data.clone());
        let request = SendRequest {
            client: client.clone(),
            request: upload_request,
            config: None,
            send_progress: SharedObservable::new(TransmissionProgress::default()),
        };
        let media_request = SendMediaUploadRequest::new(request);

        let error = media_request.await.err();
        assert_let!(Some(Error::Media(MediaError::MediaTooLargeToUpload { max, current })) = error);
        assert_eq!(max, uint!(1));
        assert_eq!(current, UInt::new_wrapping(data.len() as u64));
    }

    #[async_test]
    async fn test_dont_ignore_timeout_on_first_sync() {
        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;

        server
            .mock_sync()
            .timeout(Some(Duration::from_secs(30)))
            .ok(|_| {})
            .mock_once()
            .named("sync_with_timeout")
            .mount()
            .await;

        // Call the endpoint once to check the timeout.
        let mut stream = Box::pin(client.sync_stream(SyncSettings::new()).await);

        timeout(Duration::from_secs(1), async {
            stream.next().await.unwrap().unwrap();
        })
        .await
        .unwrap();
    }

    #[async_test]
    async fn test_ignore_timeout_on_first_sync() {
        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;

        server
            .mock_sync()
            .timeout(None)
            .ok(|_| {})
            .mock_once()
            .named("sync_no_timeout")
            .mount()
            .await;
        server
            .mock_sync()
            .timeout(Some(Duration::from_secs(30)))
            .ok(|_| {})
            .mock_once()
            .named("sync_with_timeout")
            .mount()
            .await;

        // Call each version of the endpoint once to check the timeouts.
        let mut stream = Box::pin(
            client.sync_stream(SyncSettings::new().ignore_timeout_on_first_sync(true)).await,
        );

        timeout(Duration::from_secs(1), async {
            stream.next().await.unwrap().unwrap();
            stream.next().await.unwrap().unwrap();
        })
        .await
        .unwrap();
    }

    #[async_test]
    async fn test_get_dm_room_returns_the_room_we_have_with_this_user() {
        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;
        // This is the user ID that is inside MemberAdditional.
        // Note the confusing username, so we can share
        // GlobalAccountDataTestEvent::Direct with the invited test.
        let user_id = user_id!("@invited:localhost");

        // When we receive a sync response saying "invited" is invited to a DM
        let f = EventFactory::new();
        let response = SyncResponseBuilder::default()
            .add_joined_room(
                JoinedRoomBuilder::default().add_state_event(StateTestEvent::MemberAdditional),
            )
            .add_global_account_data(
                f.direct().add_user(user_id.to_owned().into(), *DEFAULT_TEST_ROOM_ID),
            )
            .build_sync_response();
        client.base_client().receive_sync_response(response).await.unwrap();

        // Then get_dm_room finds this room
        let found_room = client.get_dm_room(user_id).expect("DM not found!");
        assert!(found_room.get_member_no_sync(user_id).await.unwrap().is_some());
    }

    #[async_test]
    async fn test_get_dm_room_still_finds_room_where_participant_is_only_invited() {
        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;
        // This is the user ID that is inside MemberInvite
        let user_id = user_id!("@invited:localhost");

        // When we receive a sync response saying "invited" is invited to a DM
        let f = EventFactory::new();
        let response = SyncResponseBuilder::default()
            .add_joined_room(
                JoinedRoomBuilder::default().add_state_event(StateTestEvent::MemberInvite),
            )
            .add_global_account_data(
                f.direct().add_user(user_id.to_owned().into(), *DEFAULT_TEST_ROOM_ID),
            )
            .build_sync_response();
        client.base_client().receive_sync_response(response).await.unwrap();

        // Then get_dm_room finds this room
        let found_room = client.get_dm_room(user_id).expect("DM not found!");
        assert!(found_room.get_member_no_sync(user_id).await.unwrap().is_some());
    }

    #[async_test]
    async fn test_get_dm_room_still_finds_left_room() {
        // See the discussion in https://github.com/matrix-org/matrix-rust-sdk/issues/2017
        // and the high-level issue at https://github.com/vector-im/element-x-ios/issues/1077

        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;
        // This is the user ID that is inside MemberAdditional.
        // Note the confusing username, so we can share
        // GlobalAccountDataTestEvent::Direct with the invited test.
        let user_id = user_id!("@invited:localhost");

        // When we receive a sync response saying "invited" is invited to a DM
        let f = EventFactory::new();
        let response = SyncResponseBuilder::default()
            .add_joined_room(
                JoinedRoomBuilder::default().add_state_event(StateTestEvent::MemberLeave),
            )
            .add_global_account_data(
                f.direct().add_user(user_id.to_owned().into(), *DEFAULT_TEST_ROOM_ID),
            )
            .build_sync_response();
        client.base_client().receive_sync_response(response).await.unwrap();

        // Then get_dm_room finds this room
        let found_room = client.get_dm_room(user_id).expect("DM not found!");
        assert!(found_room.get_member_no_sync(user_id).await.unwrap().is_some());
    }

    #[async_test]
    async fn test_device_exists() {
        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;

        server.mock_get_device().ok().expect(1).mount().await;

        assert_matches!(client.device_exists(owned_device_id!("ABCDEF")).await, Ok(true));
    }

    #[async_test]
    async fn test_device_exists_404() {
        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;

        assert_matches!(client.device_exists(owned_device_id!("ABCDEF")).await, Ok(false));
    }

    #[async_test]
    async fn test_device_exists_500() {
        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;

        server.mock_get_device().error500().expect(1).mount().await;

        assert_matches!(client.device_exists(owned_device_id!("ABCDEF")).await, Err(_));
    }

    #[async_test]
    async fn test_fetching_well_known_with_homeserver_url() {
        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;
        server.mock_well_known().ok().mount().await;

        assert_matches!(client.fetch_client_well_known().await, Some(_));
    }

    #[async_test]
    async fn test_fetching_well_known_with_server_name() {
        let server = MatrixMockServer::new().await;
        let server_name = ServerName::parse(server.server().address().to_string()).unwrap();

        server.mock_well_known().ok().mount().await;

        let client = MockClientBuilder::new(None)
            .on_builder(|builder| builder.insecure_server_name_no_tls(&server_name))
            .build()
            .await;

        assert_matches!(client.fetch_client_well_known().await, Some(_));
    }

    #[async_test]
    async fn test_fetching_well_known_with_domain_part_of_user_id() {
        let server = MatrixMockServer::new().await;
        server.mock_well_known().ok().mount().await;

        let user_id =
            UserId::parse(format!("@user:{}", server.server().address())).expect("Invalid user id");
        let client = MockClientBuilder::new(None)
            .logged_in_with_token("A_TOKEN".to_owned(), user_id, owned_device_id!("ABCDEF"))
            .build()
            .await;

        assert_matches!(client.fetch_client_well_known().await, Some(_));
    }
}
