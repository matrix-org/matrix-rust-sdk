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

use std::{
    collections::BTreeMap,
    fmt::{self, Debug},
    future::Future,
    io::Read,
    pin::Pin,
    sync::{Arc, RwLock as StdRwLock},
};

use anymap2::any::CloneAnySendSync;
#[cfg(target_arch = "wasm32")]
use async_once_cell::OnceCell;
use dashmap::DashMap;
use futures_core::stream::Stream;
use matrix_sdk_base::{
    deserialized_responses::SyncResponse,
    media::{MediaEventContent, MediaFormat, MediaRequest, MediaThumbnailSize},
    BaseClient, Session, StateStore,
};
use matrix_sdk_common::{
    instant::{Duration, Instant},
    locks::{Mutex, RwLock, RwLockReadGuard},
};
use mime::{self, Mime};
#[cfg(feature = "appservice")]
use ruma::TransactionId;
use ruma::{
    api::{
        client::{
            account::{register, whoami},
            alias::get_alias,
            device::{delete_devices, get_devices},
            directory::{get_public_rooms, get_public_rooms_filtered},
            discovery::{
                get_capabilities::{self, Capabilities},
                get_supported_versions,
            },
            filter::{create_filter::v3::Request as FilterUploadRequest, FilterDefinition},
            media::{create_content, get_content, get_content_thumbnail},
            membership::{join_room_by_id, join_room_by_id_or_alias},
            push::get_notifications::v3::Notification,
            room::create_room,
            session::{get_login_types, login, sso_login, sso_login_with_provider},
            sync::sync_events,
            uiaa::{AuthData, UserIdentifier},
        },
        error::FromHttpResponseError,
        MatrixVersion, OutgoingRequest, SendAccessToken,
    },
    assign,
    events::room::MediaSource,
    presence::PresenceState,
    DeviceId, MxcUri, OwnedDeviceId, OwnedRoomId, OwnedServerName, RoomAliasId, RoomId,
    RoomOrAliasId, ServerName, UInt, UserId,
};
use serde::de::DeserializeOwned;
#[cfg(not(target_arch = "wasm32"))]
use tokio::sync::OnceCell;
use tracing::{debug, error, info, instrument, warn};
use url::Url;

#[cfg(feature = "e2e-encryption")]
use crate::encryption::Encryption;
use crate::{
    attachment::{AttachmentInfo, Thumbnail},
    config::RequestConfig,
    error::{HttpError, HttpResult},
    event_handler::{EventHandler, EventHandlerData, EventHandlerResult, EventKind, SyncEvent},
    http_client::HttpClient,
    room, Account, Error, Result,
};

mod builder;
mod login_builder;

#[cfg(all(feature = "sso-login", not(target_arch = "wasm32")))]
pub use self::login_builder::SsoLoginBuilder;
pub use self::{
    builder::{ClientBuildError, ClientBuilder},
    login_builder::LoginBuilder,
};

/// A conservative upload speed of 1Mbps
const DEFAULT_UPLOAD_SPEED: u64 = 125_000;
/// 5 min minimal upload request timeout, used to clamp the request timeout.
const MIN_UPLOAD_REQUEST_TIMEOUT: Duration = Duration::from_secs(60 * 5);

type EventHandlerFut = Pin<Box<dyn Future<Output = ()> + Send>>;
type EventHandlerFn = Box<dyn Fn(EventHandlerData<'_>) -> EventHandlerFut + Send + Sync>;
type EventHandlerMap = BTreeMap<(EventKind, &'static str), Vec<EventHandlerFn>>;

type NotificationHandlerFut = EventHandlerFut;
type NotificationHandlerFn =
    Box<dyn Fn(Notification, room::Room, Client) -> NotificationHandlerFut + Send + Sync>;

type AnyMap = anymap2::Map<dyn CloneAnySendSync + Send + Sync>;

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
    /// The underlying HTTP client.
    http_client: HttpClient,
    /// User session data.
    pub(crate) base_client: BaseClient,
    /// The Matrix versions the server supports (well-known ones only)
    server_versions: OnceCell<Box<[MatrixVersion]>>,
    /// Locks making sure we only have one group session sharing request in
    /// flight per room.
    #[cfg(feature = "e2e-encryption")]
    pub(crate) group_session_locks: DashMap<OwnedRoomId, Arc<Mutex<()>>>,
    #[cfg(feature = "e2e-encryption")]
    /// Lock making sure we're only doing one key claim request at a time.
    pub(crate) key_claim_lock: Mutex<()>,
    pub(crate) members_request_locks: DashMap<OwnedRoomId, Arc<Mutex<()>>>,
    pub(crate) typing_notice_times: DashMap<OwnedRoomId, Instant>,
    /// Event handlers. See `register_event_handler`.
    event_handlers: RwLock<EventHandlerMap>,
    /// Custom event handler context. See `register_event_handler_context`.
    event_handler_data: StdRwLock<AnyMap>,
    /// Notification handlers. See `register_notification_handler`.
    notification_handlers: RwLock<Vec<NotificationHandlerFn>>,
    /// Whether the client should operate in application service style mode.
    /// This is low-level functionality. For an high-level API check the
    /// `matrix_sdk_appservice` crate.
    appservice_mode: bool,
    /// Whether the client should update its homeserver URL with the discovery
    /// information present in the login response.
    respect_login_well_known: bool,
    /// An event that can be listened on to wait for a successful sync. The
    /// event will only be fired if a sync loop is running. Can be used for
    /// synchronization, e.g. if we send out a request to create a room, we can
    /// wait for the sync to get the data to fetch a room object from the state
    /// store.
    pub(crate) sync_beat: event_listener::Event,
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
    pub async fn set_homeserver(&self, homeserver_url: Url) {
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
    /// * `incoming_transaction` - The sync response converted from a
    ///   transaction received from the homeserver.
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
        if let Some(server) = &self.inner.authentication_issuer {
            Some(server.read().await.clone())
        } else {
            None
        }
    }

    /// Get the user id of the current owner of the client.
    pub fn user_id(&self) -> Option<&UserId> {
        self.session().map(|s| s.user_id.as_ref())
    }

    /// Get the device ID that identifies the current session.
    pub fn device_id(&self) -> Option<&DeviceId> {
        self.session().map(|s| s.device_id.as_ref())
    }

    /// Get the whole session info of this client.
    ///
    /// Will be `None` if the client has not been logged in.
    ///
    /// Can be used with [`Client::restore_login`] to restore a previously
    /// logged-in session.
    pub fn session(&self) -> Option<&Session> {
        self.base_client().session()
    }

    /// Get a reference to the state store.
    pub fn store(&self) -> &dyn StateStore {
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

    /// Register a handler for a specific event type.
    ///
    /// The handler is a function or closure with one or more arguments. The
    /// first argument is the event itself. All additional arguments are
    /// "context" arguments: They have to implement [`EventHandlerContext`].
    /// This trait is named that way because most of the types implementing it
    /// give additional context about an event: The room it was in, its raw form
    /// and other similar things. As an exception to this,
    /// [`Client`] also implements the `EventHandlerContext` trait
    /// so you don't have to clone your client into the event handler manually.
    ///
    /// Some context arguments are not universally applicable. A context
    /// argument that isn't available for the given event type will result in
    /// the event handler being skipped and an error being logged. The following
    /// context argument types are only available for a subset of event types:
    ///
    /// * [`Room`][room::Room] is only available for room-specific events, i.e.
    ///   not for events like global account data events or presence events
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
    /// client
    ///     .register_event_handler(
    ///         |ev: SyncRoomMessageEvent, room: Room, client: Client| async move {
    ///             // Common usage: Room event plus room and client.
    ///         },
    ///     )
    ///     .await
    ///     .register_event_handler(
    ///         |ev: SyncRoomMessageEvent, room: Room, encryption_info: Option<EncryptionInfo>| {
    ///             async move {
    ///                 // An `Option<EncryptionInfo>` parameter lets you distinguish between
    ///                 // unencrypted events and events that were decrypted by the SDK.
    ///             }
    ///         },
    ///     )
    ///     .await
    ///     .register_event_handler(|ev: SyncRoomTopicEvent| async move {
    ///         // You can omit any or all arguments after the first.
    ///     })
    ///     .await;
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
    /// client.register_event_handler(|ev: SyncTokenEvent, room: Room| async move {
    ///     todo!("Display the token");
    /// }).await;
    ///
    /// // Adding your custom data to the handler can be done as well
    /// let data = "MyCustomIdentifier".to_owned();
    ///
    /// client.register_event_handler({
    ///     let data = data.clone();
    ///     move |ev: SyncRoomMessageEvent | {
    ///         let data = data.clone();
    ///         async move {
    ///             println!("Calling the handler with identifier {}", data);
    ///         }
    ///     }
    /// }).await;
    /// # });
    /// ```
    pub async fn register_event_handler<Ev, Ctx, H>(&self, handler: H) -> &Self
    where
        Ev: SyncEvent + DeserializeOwned + Send + 'static,
        H: EventHandler<Ev, Ctx>,
        <H::Future as Future>::Output: EventHandlerResult,
    {
        let event_type = H::ID.1;
        self.inner.event_handlers.write().await.entry(H::ID).or_default().push(Box::new(
            move |data| {
                let maybe_fut = serde_json::from_str(data.raw.get())
                    .map(|ev| handler.clone().handle_event(ev, data));

                Box::pin(async move {
                    match maybe_fut {
                        Ok(Some(fut)) => {
                            fut.await.print_error(event_type);
                        }
                        Ok(None) => {
                            error!(
                                "Event handler for {} has an invalid context argument",
                                event_type
                            );
                        }
                        Err(e) => {
                            warn!(
                                "Failed to deserialize `{}` event, skipping event handler.\n\
                                 Deserialization error: {}",
                                event_type, e,
                            );
                        }
                    }
                })
            },
        ));

        self
    }

    pub(crate) async fn event_handlers(&self) -> RwLockReadGuard<'_, EventHandlerMap> {
        self.inner.event_handlers.read().await
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
    ///     event_handler::Ctx,
    ///     room::Room,
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
    /// client
    ///     .register_event_handler_context(my_gui_handle.clone())
    ///     .register_event_handler(
    ///         |ev: SyncRoomMessageEvent, room: Room, gui_handle: Ctx<SomeType>| async move {
    ///             // gui_handle.send(DisplayMessage { message: ev });
    ///         },
    ///     )
    ///     .await;
    /// # });
    /// ```
    pub fn register_event_handler_context<T>(&self, ctx: T) -> &Self
    where
        T: Clone + Send + Sync + 'static,
    {
        self.inner.event_handler_data.write().unwrap().insert(ctx);
        self
    }

    pub(crate) fn event_handler_context<T>(&self) -> Option<T>
    where
        T: Clone + Send + Sync + 'static,
    {
        let map = self.inner.event_handler_data.read().unwrap();
        map.get::<T>().cloned()
    }

    /// Register a handler for a notification.
    ///
    /// Similar to [`Client::register_event_handler`], but only allows functions
    /// or closures with exactly the three arguments [`Notification`],
    /// [`room::Room`], [`Client`] for now.
    pub async fn register_notification_handler<H, Fut>(&self, handler: H) -> &Self
    where
        H: Fn(Notification, room::Room, Client) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ()> + Send + 'static,
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
            .filter_map(|room| room::Joined::new(self.clone(), room))
            .collect()
    }

    /// Returns the invited rooms this client knows about.
    pub fn invited_rooms(&self) -> Vec<room::Invited> {
        self.base_client()
            .get_stripped_rooms()
            .into_iter()
            .filter_map(|room| room::Invited::new(self.clone(), room))
            .collect()
    }

    /// Returns the left rooms this client knows about.
    pub fn left_rooms(&self) -> Vec<room::Left> {
        self.base_client()
            .get_rooms()
            .into_iter()
            .filter_map(|room| room::Left::new(self.clone(), room))
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
        self.base_client().get_room(room_id).and_then(|room| room::Joined::new(self.clone(), room))
    }

    /// Get an invited room with the given room id.
    ///
    /// # Arguments
    ///
    /// `room_id` - The unique id of the room that should be fetched.
    pub fn get_invited_room(&self, room_id: &RoomId) -> Option<room::Invited> {
        self.base_client().get_room(room_id).and_then(|room| room::Invited::new(self.clone(), room))
    }

    /// Get a left room with the given room id.
    ///
    /// # Arguments
    ///
    /// `room_id` - The unique id of the room that should be fetched.
    pub fn get_left_room(&self, room_id: &RoomId) -> Option<room::Left> {
        self.base_client().get_room(room_id).and_then(|room| room::Left::new(self.clone(), room))
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
        let request = get_alias::v3::Request::new(room_alias);
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
            sso_login_with_provider::v3::Request::new(id, redirect_url)
                .try_into_http_request::<Vec<u8>>(
                    homeserver.as_str(),
                    SendAccessToken::None,
                    server_versions,
                )
        } else {
            sso_login::v3::Request::new(redirect_url).try_into_http_request::<Vec<u8>>(
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
    /// Alternatively the [`restore_login`] method can be used to restore a
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
    /// # use std::convert::TryFrom;
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
    ///     .send()
    ///     .await?;
    ///
    /// println!(
    ///     "Logged in as {}, got device_id {} and access_token {}",
    ///     user, response.device_id, response.access_token,
    /// );
    /// # anyhow::Ok(()) });
    /// ```
    ///
    /// [`restore_login`]: #method.restore_login
    pub fn login_username<'a>(
        &self,
        id: &'a (impl AsRef<str> + ?Sized),
        password: &'a str,
    ) -> LoginBuilder<'a> {
        self.login_identifier(UserIdentifier::UserIdOrLocalpart(id.as_ref()), password)
    }

    /// Login to the server with a user identifier and password.
    ///
    /// This is more general form of [`login_username`][Self::login_username]
    /// that also accepts third-party identifiers instead of just the user ID or
    /// its localpart.
    pub fn login_identifier<'a>(
        &self,
        id: UserIdentifier<'a>,
        password: &'a str,
    ) -> LoginBuilder<'a> {
        LoginBuilder::new_password(self.clone(), id, password)
    }

    /// Login to the server with a token.
    ///
    /// This token is usually received in the SSO flow after following the URL
    /// provided by [`get_sso_login_url`], note that this is not the access
    /// token of a session.
    ///
    /// This should only be used for the first login.
    ///
    /// The [`restore_login`] method should be used to restore a logged-in
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
    /// # use std::convert::TryFrom;
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
    ///     .send()
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
    /// [`restore_login`]: #method.restore_login
    pub fn login_token<'a>(&self, token: &'a str) -> LoginBuilder<'a> {
        LoginBuilder::new_token(self.clone(), token)
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
    /// The [`restore_login`] method should be used to restore a logged-in
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
    ///     .send()
    ///     .await
    ///     .unwrap();
    ///
    /// println!("Logged in as {}, got device_id {} and access_token {}",
    ///          response.user_id, response.device_id, response.access_token);
    /// # })
    /// ```
    ///
    /// [`get_sso_login_url`]: #method.get_sso_login_url
    /// [`login_token`]: #method.login_token
    /// [`restore_login`]: #method.restore_login
    #[cfg(all(feature = "sso-login", not(target_arch = "wasm32")))]
    pub fn login_sso<'a, F, Fut>(&self, use_sso_login_url: F) -> SsoLoginBuilder<'a, F>
    where
        F: FnOnce(String) -> Fut + Send,
        Fut: Future<Output = Result<()>> + Send,
    {
        SsoLoginBuilder::new(self.clone(), use_sso_login_url)
    }

    /// Login to the server with a username and password.
    #[deprecated = "Replaced by [`Client::login_username`](#method.login_username)"]
    #[instrument(skip(self, user, password))]
    pub async fn login(
        &self,
        user: impl AsRef<str>,
        password: &str,
        device_id: Option<&str>,
        initial_device_display_name: Option<&str>,
    ) -> Result<login::v3::Response> {
        let mut builder = self.login_username(&user, password);
        if let Some(value) = device_id {
            builder = builder.device_id(value);
        }
        if let Some(value) = initial_device_display_name {
            builder = builder.initial_device_display_name(value);
        }

        builder.send().await
    }

    /// Login to the server via Single Sign-On.
    #[deprecated = "Replaced by [`Client::login_sso`](#method.login_sso)"]
    #[cfg(all(feature = "sso-login", not(target_arch = "wasm32")))]
    #[deny(clippy::future_not_send)]
    pub async fn login_with_sso<C>(
        &self,
        use_sso_login_url: impl FnOnce(String) -> C + Send,
        server_url: Option<&str>,
        server_response: Option<&str>,
        device_id: Option<&str>,
        initial_device_display_name: Option<&str>,
        idp_id: Option<&str>,
    ) -> Result<login::v3::Response>
    where
        C: Future<Output = Result<()>> + Send,
    {
        let mut builder = self.login_sso(use_sso_login_url);
        if let Some(value) = server_url {
            builder = builder.server_url(value);
        }
        if let Some(value) = server_response {
            builder = builder.server_response(value);
        }
        if let Some(value) = device_id {
            builder = builder.device_id(value);
        }
        if let Some(value) = initial_device_display_name {
            builder = builder.initial_device_display_name(value);
        }
        if let Some(value) = idp_id {
            builder = builder.identity_provider_id(value);
        }

        builder.send().await
    }

    /// Login to the server with a token.
    #[deprecated = "Replaced by [`Client::login_token`](#method.login_token)"]
    #[instrument(skip(self, token))]
    #[cfg_attr(not(target_arch = "wasm32"), deny(clippy::future_not_send))]
    pub async fn login_with_token(
        &self,
        token: &str,
        device_id: Option<&str>,
        initial_device_display_name: Option<&str>,
    ) -> Result<login::v3::Response> {
        let mut builder = self.login_token(token);
        if let Some(value) = device_id {
            builder = builder.device_id(value);
        }
        if let Some(value) = initial_device_display_name {
            builder = builder.initial_device_display_name(value);
        }

        builder.send().await
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
    /// use matrix_sdk::{Client, Session, ruma::{device_id, user_id}};
    /// # use url::Url;
    /// # use futures::executor::block_on;
    /// # block_on(async {
    ///
    /// let homeserver = Url::parse("http://example.com")?;
    /// let client = Client::new(homeserver).await?;
    ///
    /// let session = Session {
    ///     access_token: "My-Token".to_owned(),
    ///     user_id: user_id!("@example:localhost").to_owned(),
    ///     device_id: device_id!("MYDEVICEID").to_owned(),
    /// };
    ///
    /// client.restore_login(session).await?;
    /// # anyhow::Ok(()) });
    /// ```
    ///
    /// The `Session` object can also be created from the response the
    /// [`Client::login()`] method returns:
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
    /// let session: Session = client
    ///     .login("example", "my-password", None, None)
    ///     .await?
    ///     .into();
    ///
    /// // Persist the `Session` so it can later be used to restore the login.
    /// client.restore_login(session).await?;
    /// # anyhow::Ok(()) });
    /// ```
    ///
    /// [`login`]: #method.login
    pub async fn restore_login(&self, session: Session) -> Result<()> {
        Ok(self.inner.base_client.restore_login(session).await?)
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
    /// # use std::convert::TryFrom;
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
    /// request.username = Some("user");
    /// request.password = Some("password");
    /// request.auth = Some(uiaa::AuthData::FallbackAcknowledgement(
    ///     uiaa::FallbackAcknowledgement::new("foobar"),
    /// ));
    ///
    /// let client = Client::new(homeserver).await.unwrap();
    /// client.register(request).await;
    /// # })
    /// ```
    #[instrument(skip_all)]
    pub async fn register(
        &self,
        registration: impl Into<register::v3::Request<'_>>,
    ) -> HttpResult<register::v3::Response> {
        let homeserver = self.homeserver().await;
        info!("Registering to {}", homeserver);

        let config = if self.inner.appservice_mode {
            Some(RequestConfig::short_retry().force_auth())
        } else {
            None
        };

        let request = registration.into();
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
    ///     .filter(Filter::FilterId(&filter_id));
    ///
    /// let response = client.sync_once(sync_settings).await.unwrap();
    /// # });
    #[instrument(skip(self, definition))]
    pub async fn get_or_upload_filter(
        &self,
        filter_name: &str,
        definition: FilterDefinition<'_>,
    ) -> Result<String> {
        if let Some(filter) = self.inner.base_client.get_filter(filter_name).await? {
            debug!("Found filter locally");
            Ok(filter)
        } else {
            debug!("Didn't find filter locally");
            let user_id = self.user_id().ok_or(Error::AuthenticationRequired)?;
            let request = FilterUploadRequest::new(user_id, definition);
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
    pub async fn join_room_by_id(
        &self,
        room_id: &RoomId,
    ) -> HttpResult<join_room_by_id::v3::Response> {
        let request = join_room_by_id::v3::Request::new(room_id);
        self.send(request, None).await
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
    ) -> HttpResult<join_room_by_id_or_alias::v3::Response> {
        let request = assign!(join_room_by_id_or_alias::v3::Request::new(alias), {
            server_name: server_names,
        });
        self.send(request, None).await
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
    /// # use std::convert::TryInto;
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
            since,
            server,
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
    pub async fn create_room(
        &self,
        room: impl Into<create_room::v3::Request<'_>>,
    ) -> HttpResult<create_room::v3::Response> {
        let request = room.into();
        self.send(request, None).await
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
    /// # use std::convert::TryFrom;
    /// # use url::Url;
    /// # use matrix_sdk::Client;
    /// # use futures::executor::block_on;
    /// # block_on(async {
    /// # let homeserver = Url::parse("http://example.com")?;
    /// use matrix_sdk::ruma::{
    ///     api::client::directory::get_public_rooms_filtered,
    ///     directory::Filter,
    /// };
    /// # let mut client = Client::new(homeserver).await?;
    ///
    /// let mut filter = Filter::new();
    /// filter.generic_search_term = Some("rust");
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
        room_search: impl Into<get_public_rooms_filtered::v3::Request<'_>>,
    ) -> HttpResult<get_public_rooms_filtered::v3::Response> {
        let request = room_search.into();
        self.send(request, None).await
    }

    /// Upload some media to the server.
    ///
    /// # Arguments
    ///
    /// * `content_type` - The type of the media, this will be used as the
    /// content-type header.
    ///
    /// * `reader` - A `Reader` that will be used to fetch the raw bytes of the
    /// media.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use std::{path::PathBuf, fs::File, io::Read};
    /// # use matrix_sdk::{Client, ruma::room_id};
    /// # use url::Url;
    /// # use futures::executor::block_on;
    /// # use mime;
    /// # block_on(async {
    /// # let homeserver = Url::parse("http://localhost:8080")?;
    /// # let mut client = Client::new(homeserver).await?;
    /// let path = PathBuf::from("/home/example/my-cat.jpg");
    /// let mut image = File::open(path)?;
    ///
    /// let response = client
    ///     .upload(&mime::IMAGE_JPEG, &mut image)
    ///     .await?;
    ///
    /// println!("Cat URI: {}", response.content_uri);
    /// # anyhow::Ok(()) });
    /// ```
    pub async fn upload(
        &self,
        content_type: &Mime,
        reader: &mut (impl Read + ?Sized),
    ) -> Result<create_content::v3::Response> {
        let mut data = Vec::new();
        reader.read_to_end(&mut data)?;

        let timeout = std::cmp::max(
            Duration::from_secs(data.len() as u64 / DEFAULT_UPLOAD_SPEED),
            MIN_UPLOAD_REQUEST_TIMEOUT,
        );

        let request = assign!(create_content::v3::Request::new(&data), {
            content_type: Some(content_type.essence_str()),
        });

        let request_config = self.inner.http_client.request_config.timeout(timeout);
        Ok(self
            .inner
            .http_client
            .send(
                request,
                Some(request_config),
                self.homeserver().await.to_string(),
                self.session(),
                self.server_versions().await?,
            )
            .await?)
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
    /// # use std::convert::TryFrom;
    /// # block_on(async {
    /// # let homeserver = Url::parse("http://localhost:8080")?;
    /// # let mut client = Client::new(homeserver).await?;
    /// use matrix_sdk::ruma::{api::client::profile, user_id};
    ///
    /// // First construct the request you want to make
    /// // See https://docs.rs/ruma-client-api/latest/ruma_client_api/index.html
    /// // for all available Endpoints
    /// let user_id = user_id!("@example:localhost");
    /// let request = profile::get_profile::v3::Request::new(&user_id);
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
        Request: OutgoingRequest + Debug,
        HttpError: From<FromHttpResponseError<Request::EndpointError>>,
    {
        self.inner
            .http_client
            .send(
                request,
                config,
                self.homeserver().await.to_string(),
                self.session(),
                self.server_versions().await?,
            )
            .await
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
            )
            .await?
            .known_versions()
            .into_iter()
            .collect();

        if server_versions.is_empty() {
            Ok(vec![MatrixVersion::V1_0].into())
        } else {
            Ok(server_versions)
        }
    }

    async fn server_versions(&self) -> HttpResult<&[MatrixVersion]> {
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
    /// # use std::convert::TryFrom;
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
    /// #    ruma::{
    /// #        api::{
    /// #            client::uiaa,
    /// #            error::{FromHttpResponseError, ServerError},
    /// #        },
    /// #        device_id,
    /// #    },
    /// #    Client, Error, config::SyncSettings,
    /// # };
    /// # use futures::executor::block_on;
    /// # use serde_json::json;
    /// # use url::Url;
    /// # use std::{collections::BTreeMap, convert::TryFrom};
    /// # block_on(async {
    /// # let homeserver = Url::parse("http://localhost:8080")?;
    /// # let mut client = Client::new(homeserver).await?;
    /// let devices = &[device_id!("DEVICEID").to_owned()];
    ///
    /// if let Err(e) = client.delete_devices(devices, None).await {
    ///     if let Some(info) = e.uiaa_response() {
    ///         let mut password = uiaa::Password::new(
    ///             uiaa::UserIdentifier::UserIdOrLocalpart("example"),
    ///             "wordpass",
    ///         );
    ///         password.session = info.session.as_deref();
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
        auth_data: Option<AuthData<'_>>,
    ) -> HttpResult<delete_devices::v3::Response> {
        let mut request = delete_devices::v3::Request::new(devices);
        request.auth = auth_data;

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
    ///     Client, config::SyncSettings,
    ///     ruma::events::room::message::OriginalSyncRoomMessageEvent,
    /// };
    ///
    /// let client = Client::new(homeserver).await?;
    /// client.login(&username, &password, None, None).await?;
    ///
    /// // Sync once so we receive the client state and old messages.
    /// client.sync_once(SyncSettings::default()).await?;
    ///
    /// // Register our handler so we start responding once we receive a new
    /// // event.
    /// client.register_event_handler(|ev: OriginalSyncRoomMessageEvent| async move {
    ///     println!("Received event {}: {:?}", ev.sender, ev.content);
    /// }).await;
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
    /// [`filter`]: crate::config::SyncSettings#method.filter
    /// [`Filter`]: ruma::api::client::sync::sync_events::v3::Filter
    /// [`next_batch`]: SyncResponse#structfield.next_batch
    /// [`get_or_upload_filter()`]: #method.get_or_upload_filter
    /// [long polling]: #long-polling
    /// [filtered]: #filtering-events
    #[instrument(skip(self))]
    pub async fn sync_once(
        &self,
        sync_settings: crate::config::SyncSettings<'_>,
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
            filter: sync_settings.filter.as_ref(),
            since: sync_settings.token.as_deref(),
            full_state: sync_settings.full_state,
            set_presence: &PresenceState::Online,
            timeout: sync_settings.timeout,
        });

        let request_config = self.inner.http_client.request_config.timeout(
            sync_settings.timeout.unwrap_or_else(|| Duration::from_secs(0))
                + self.inner.http_client.request_config.timeout,
        );

        let response = self.send(request, Some(request_config)).await?;
        let response = self.process_sync(response).await?;

        #[cfg(feature = "e2e-encryption")]
        if let Err(e) = self.send_outgoing_requests().await {
            error!(error = ?e, "Error while sending outgoing E2EE requests");
        }

        self.inner.sync_beat.notify(usize::MAX);

        Ok(response)
    }

    /// Repeatedly synchronize the client state with the server.
    ///
    /// This method will never return, if cancellation is needed the method
    /// should be wrapped in a cancelable task or the
    /// [`Client::sync_with_callback`] method can be used.
    ///
    /// This method will internally call [`Client::sync_once`] in a loop.
    ///
    /// This method can be used with the [`Client::register_event_handler`]
    /// method to react to individual events. If you instead wish to handle
    /// events in a bulk manner the [`Client::sync_with_callback`] and
    /// [`Client::sync_stream`] methods can be used instead. Those two methods
    /// repeatedly return the whole sync response.
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
    /// use matrix_sdk::{
    ///     Client, config::SyncSettings,
    ///     ruma::events::room::message::OriginalSyncRoomMessageEvent,
    /// };
    ///
    /// let client = Client::new(homeserver).await?;
    /// client.login(&username, &password, None, None).await?;
    ///
    /// // Register our handler so we start responding once we receive a new
    /// // event.
    /// client.register_event_handler(|ev: OriginalSyncRoomMessageEvent| async move {
    ///     println!("Received event {}: {:?}", ev.sender, ev.content);
    /// }).await;
    ///
    /// // Now keep on syncing forever. `sync()` will use the latest sync token
    /// // automatically.
    /// client.sync(SyncSettings::default()).await;
    /// # anyhow::Ok(()) });
    /// ```
    ///
    /// [argument docs]: #method.sync_once
    /// [`sync_with_callback`]: #method.sync_with_callback
    pub async fn sync(&self, sync_settings: crate::config::SyncSettings<'_>) {
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
    ///
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
    #[instrument(skip(self, callback))]
    pub async fn sync_with_callback<C>(
        &self,
        mut sync_settings: crate::config::SyncSettings<'_>,
        callback: impl Fn(SyncResponse) -> C,
    ) where
        C: Future<Output = LoopCtrl>,
    {
        let mut last_sync_time: Option<Instant> = None;

        if sync_settings.token.is_none() {
            sync_settings.token = self.sync_token().await;
        }

        loop {
            // TODO we should abort the sync loop if the error is a storage error or
            // the access token got invalid.
            if let Ok(r) = self.sync_loop_helper(&mut sync_settings).await {
                if callback(r).await == LoopCtrl::Break {
                    return;
                }
            }

            Client::delay_sync(&mut last_sync_time).await
        }
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
    /// use matrix_sdk::{Client, config::SyncSettings};
    ///
    /// let client = Client::new(homeserver).await?;
    /// client.login(&username, &password, None, None).await?;
    ///
    /// let mut sync_stream = Box::pin(client.sync_stream(SyncSettings::default()).await);
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
    #[instrument(skip(self))]
    pub async fn sync_stream<'a>(
        &'a self,
        mut sync_settings: crate::config::SyncSettings<'a>,
    ) -> impl Stream<Item = Result<SyncResponse>> + 'a {
        let mut last_sync_time: Option<Instant> = None;

        if sync_settings.token.is_none() {
            sync_settings.token = self.sync_token().await;
        }

        async_stream::stream! {
            loop {
                yield self.sync_loop_helper(&mut sync_settings).await;

                Client::delay_sync(&mut last_sync_time).await
            }
        }
    }

    /// Get the current, if any, sync token of the client.
    /// This will be None if the client didn't sync at least once.
    pub async fn sync_token(&self) -> Option<String> {
        self.inner.base_client.sync_token().await
    }

    /// Get a media file's content.
    ///
    /// If the content is encrypted and encryption is enabled, the content will
    /// be decrypted.
    ///
    /// # Arguments
    ///
    /// * `request` - The `MediaRequest` of the content.
    ///
    /// * `use_cache` - If we should use the media cache for this request.
    pub async fn get_media_content(
        &self,
        request: &MediaRequest,
        use_cache: bool,
    ) -> Result<Vec<u8>> {
        let content = if use_cache {
            self.inner.base_client.store().get_media_content(request).await?
        } else {
            None
        };

        if let Some(content) = content {
            Ok(content)
        } else {
            let content: Vec<u8> = match &request.source {
                MediaSource::Encrypted(file) => {
                    let content: Vec<u8> =
                        self.send(get_content::v3::Request::from_url(&file.url)?, None).await?.file;

                    #[cfg(feature = "e2e-encryption")]
                    let content = {
                        let mut cursor = std::io::Cursor::new(content);
                        let mut reader = matrix_sdk_base::crypto::AttachmentDecryptor::new(
                            &mut cursor,
                            file.as_ref().clone().into(),
                        )?;

                        let mut decrypted = Vec::new();
                        reader.read_to_end(&mut decrypted)?;

                        decrypted
                    };

                    content
                }
                MediaSource::Plain(uri) => {
                    if let MediaFormat::Thumbnail(size) = &request.format {
                        self.send(
                            get_content_thumbnail::v3::Request::from_url(
                                uri,
                                size.width,
                                size.height,
                            )?,
                            None,
                        )
                        .await?
                        .file
                    } else {
                        self.send(get_content::v3::Request::from_url(uri)?, None).await?.file
                    }
                }
            };

            if use_cache {
                self.inner.base_client.store().add_media_content(request, content.clone()).await?;
            }

            Ok(content)
        }
    }

    /// Remove a media file's content from the store.
    ///
    /// # Arguments
    ///
    /// * `request` - The `MediaRequest` of the content.
    pub async fn remove_media_content(&self, request: &MediaRequest) -> Result<()> {
        Ok(self.inner.base_client.store().remove_media_content(request).await?)
    }

    /// Delete all the media content corresponding to the given
    /// uri from the store.
    ///
    /// # Arguments
    ///
    /// * `uri` - The `MxcUri` of the files.
    pub async fn remove_media_content_for_uri(&self, uri: &MxcUri) -> Result<()> {
        Ok(self.inner.base_client.store().remove_media_content_for_uri(uri).await?)
    }

    /// Get the file of the given media event content.
    ///
    /// If the content is encrypted and encryption is enabled, the content will
    /// be decrypted.
    ///
    /// Returns `Ok(None)` if the event content has no file.
    ///
    /// This is a convenience method that calls the
    /// [`get_media_content`](#method.get_media_content) method.
    ///
    /// # Arguments
    ///
    /// * `event_content` - The media event content.
    ///
    /// * `use_cache` - If we should use the media cache for this file.
    pub async fn get_file(
        &self,
        event_content: impl MediaEventContent,
        use_cache: bool,
    ) -> Result<Option<Vec<u8>>> {
        if let Some(source) = event_content.source() {
            Ok(Some(
                self.get_media_content(
                    &MediaRequest { source, format: MediaFormat::File },
                    use_cache,
                )
                .await?,
            ))
        } else {
            Ok(None)
        }
    }

    /// Remove the file of the given media event content from the cache.
    ///
    /// This is a convenience method that calls the
    /// [`remove_media_content`](#method.remove_media_content) method.
    ///
    /// # Arguments
    ///
    /// * `event_content` - The media event content.
    pub async fn remove_file(&self, event_content: impl MediaEventContent) -> Result<()> {
        if let Some(source) = event_content.source() {
            self.remove_media_content(&MediaRequest { source, format: MediaFormat::File }).await?
        }

        Ok(())
    }

    /// Get a thumbnail of the given media event content.
    ///
    /// If the content is encrypted and encryption is enabled, the content will
    /// be decrypted.
    ///
    /// Returns `Ok(None)` if the event content has no thumbnail.
    ///
    /// This is a convenience method that calls the
    /// [`get_media_content`](#method.get_media_content) method.
    ///
    /// # Arguments
    ///
    /// * `event_content` - The media event content.
    ///
    /// * `size` - The _desired_ size of the thumbnail. The actual thumbnail may
    ///   not match the size specified.
    ///
    /// * `use_cache` - If we should use the media cache for this thumbnail.
    pub async fn get_thumbnail(
        &self,
        event_content: impl MediaEventContent,
        size: MediaThumbnailSize,
        use_cache: bool,
    ) -> Result<Option<Vec<u8>>> {
        if let Some(source) = event_content.thumbnail_source() {
            Ok(Some(
                self.get_media_content(
                    &MediaRequest { source, format: MediaFormat::Thumbnail(size) },
                    use_cache,
                )
                .await?,
            ))
        } else {
            Ok(None)
        }
    }

    /// Remove the thumbnail of the given media event content from the cache.
    ///
    /// This is a convenience method that calls the
    /// [`remove_media_content`](#method.remove_media_content) method.
    ///
    /// # Arguments
    ///
    /// * `event_content` - The media event content.
    ///
    /// * `size` - The _desired_ size of the thumbnail. Must match the size
    ///   requested with [`get_thumbnail`](#method.get_thumbnail).
    pub async fn remove_thumbnail(
        &self,
        event_content: impl MediaEventContent,
        size: MediaThumbnailSize,
    ) -> Result<()> {
        if let Some(source) = event_content.source() {
            self.remove_media_content(&MediaRequest {
                source,
                format: MediaFormat::Thumbnail(size),
            })
            .await?
        }

        Ok(())
    }

    /// Gets information about the owner of a given access token.
    pub async fn whoami(&self) -> HttpResult<whoami::v3::Response> {
        let request = whoami::v3::Request::new();
        self.send(request, None).await
    }

    /// Upload the file to be read from `reader` and construct an attachment
    /// message with `body`, `content_type`, `info` and `thumbnail`.
    pub(crate) async fn prepare_attachment_message<R: Read, T: Read>(
        &self,
        body: &str,
        content_type: &Mime,
        reader: &mut R,
        info: Option<AttachmentInfo>,
        thumbnail: Option<Thumbnail<'_, T>>,
    ) -> Result<ruma::events::room::message::MessageType> {
        let (thumbnail_source, thumbnail_info) = if let Some(thumbnail) = thumbnail {
            let response = self.upload(thumbnail.content_type, thumbnail.reader).await?;
            let url = response.content_uri;

            use ruma::events::room::ThumbnailInfo;
            let thumbnail_info = assign!(
                thumbnail.info.as_ref().map(|info| ThumbnailInfo::from(info.clone())).unwrap_or_default(),
                { mimetype: Some(thumbnail.content_type.as_ref().to_owned()) }
            );

            (Some(MediaSource::Plain(url)), Some(Box::new(thumbnail_info)))
        } else {
            (None, None)
        };

        let response = self.upload(content_type, reader).await?;

        let url = response.content_uri;

        use ruma::events::room::{self, message};
        Ok(match content_type.type_() {
            mime::IMAGE => {
                let info = assign!(info.map(room::ImageInfo::from).unwrap_or_default(), {
                    mimetype: Some(content_type.as_ref().to_owned()),
                    thumbnail_source,
                    thumbnail_info,
                });
                message::MessageType::Image(message::ImageMessageEventContent::plain(
                    body.to_owned(),
                    url,
                    Some(Box::new(info)),
                ))
            }
            mime::AUDIO => {
                let info = assign!(info.map(message::AudioInfo::from).unwrap_or_default(), {
                    mimetype: Some(content_type.as_ref().to_owned()),
                });
                message::MessageType::Audio(message::AudioMessageEventContent::plain(
                    body.to_owned(),
                    url,
                    Some(Box::new(info)),
                ))
            }
            mime::VIDEO => {
                let info = assign!(info.map(message::VideoInfo::from).unwrap_or_default(), {
                    mimetype: Some(content_type.as_ref().to_owned()),
                    thumbnail_source,
                    thumbnail_info
                });
                message::MessageType::Video(message::VideoMessageEventContent::plain(
                    body.to_owned(),
                    url,
                    Some(Box::new(info)),
                ))
            }
            _ => {
                let info = assign!(info.map(message::FileInfo::from).unwrap_or_default(), {
                    mimetype: Some(content_type.as_ref().to_owned()),
                    thumbnail_source,
                    thumbnail_info
                });
                message::MessageType::File(message::FileMessageEventContent::plain(
                    body.to_owned(),
                    url,
                    Some(Box::new(info)),
                ))
            }
        })
    }
}

// The http mocking library is not supported for wasm32
#[cfg(all(test, not(target_arch = "wasm32")))]
pub(crate) mod tests {
    use std::time::Duration;

    use matrix_sdk_test::{async_test, test_json, EventBuilder, JoinedRoomBuilder, StateTestEvent};
    #[cfg(target_arch = "wasm32")]
    wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_browser);

    use ruma::{api::MatrixVersion, device_id, user_id, UserId};
    use url::Url;
    use wiremock::{
        matchers::{header, method, path},
        Mock, MockServer, ResponseTemplate,
    };

    use super::{Client, ClientBuilder, Session};
    use crate::config::{RequestConfig, SyncSettings};

    fn test_client_builder(homeserver_url: Option<String>) -> ClientBuilder {
        let homeserver = homeserver_url.as_deref().unwrap_or("http://localhost:1234");
        Client::builder().homeserver_url(homeserver).server_versions([MatrixVersion::V1_0])
    }

    async fn no_retry_test_client(homeserver_url: Option<String>) -> Client {
        test_client_builder(homeserver_url)
            .request_config(RequestConfig::new().disable_retry())
            .build()
            .await
            .unwrap()
    }

    pub(crate) async fn logged_in_client(homeserver_url: Option<String>) -> Client {
        let session = Session {
            access_token: "1234".to_owned(),
            user_id: user_id!("@example:localhost").to_owned(),
            device_id: device_id!("DEVICEID").to_owned(),
        };
        let client = no_retry_test_client(homeserver_url).await;
        client.restore_login(session).await.unwrap();

        client
    }

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

        // let bc = &client.base_client;
        // let ignored_users = bc.ignored_users.read().await;
        // assert_eq!(1, ignored_users.len())
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
        let client = Client::builder().user_id(&alice).build().await.unwrap();

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
            Client::builder().user_id(&alice).build().await.is_err(),
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

        assert!(client.inner.http_client.request_config.retry_limit.unwrap() == 3);

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

        assert!(client.inner.http_client.request_config.retry_timeout.unwrap() == retry_timeout);

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
}
