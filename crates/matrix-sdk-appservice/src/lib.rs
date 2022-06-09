// Copyright 2021 Famedly GmbH
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

//! Matrix [Application Service] library
//!
//! The appservice crate aims to provide a batteries-included experience by
//! being a thin wrapper around the [`matrix_sdk`]. That means that we
//!
//! * ship with functionality to configure your webserver crate or simply run
//!   the webserver for you
//! * receive and validate requests from the homeserver correctly
//! * allow calling the homeserver with proper virtual user identity assertion
//! * have consistent room state by leveraging matrix-sdk's state store
//! * provide E2EE support by leveraging matrix-sdk's crypto store
//!
//! # Status
//!
//! The crate is in an experimental state. Follow
//! [matrix-org/matrix-rust-sdk#228] for progress.
//!
//! # Registration
//!
//! The crate relies on the registration being always in sync with the actual
//! registration used by the homeserver. That's because it's required for the
//! access tokens and because membership states for virtual users are determined
//! based on the registered namespace.
//!
//! **Note:** Non-exclusive registration namespaces are not yet supported and
//! hence might lead to undefined behavior.
//!
//! # Quickstart
//!
//! ```no_run
//! # async {
//! #
//! use matrix_sdk_appservice::{
//!     ruma::events::room::member::SyncRoomMemberEvent,
//!     AppService, AppServiceRegistration
//! };
//!
//! let homeserver_url = "http://127.0.0.1:8008";
//! let server_name = "localhost";
//! let registration = AppServiceRegistration::try_from_yaml_str(
//!     r"
//!         id: appservice
//!         url: http://127.0.0.1:9009
//!         as_token: as_token
//!         hs_token: hs_token
//!         sender_localpart: _appservice
//!         namespaces:
//!           users:
//!           - exclusive: true
//!             regex: '@_appservice_.*'
//!     ")?;
//!
//! let mut appservice = AppService::new(homeserver_url, server_name, registration).await?;
//! appservice.register_event_handler(|_ev: SyncRoomMemberEvent| async {
//!     // do stuff
//! });
//!
//! let (host, port) = appservice.registration().get_host_and_port()?;
//! appservice.run(host, port).await?;
//! #
//! # Ok::<(), Box<dyn std::error::Error + 'static>>(())
//! # };
//! ```
//!
//! Check the [examples directory] for fully working examples.
//!
//! [Application Service]: https://matrix.org/docs/spec/application_service/r0.1.2
//! [matrix-org/matrix-rust-sdk#228]: https://github.com/matrix-org/matrix-rust-sdk/issues/228
//! [examples directory]: https://github.com/matrix-org/matrix-rust-sdk/tree/main/crates/matrix-sdk-appservice/examples

use std::{
    convert::{TryFrom, TryInto},
    fs::File,
    future::Future,
    ops::Deref,
    path::PathBuf,
    sync::Arc,
};

use dashmap::DashMap;
pub use error::Error;
use event_handler::AppserviceFn;
use http::Uri;
pub use matrix_sdk;
#[doc(no_inline)]
pub use matrix_sdk::ruma;
use matrix_sdk::{
    bytes::Bytes,
    config::RequestConfig,
    event_handler::{EventHandler, EventHandlerResult, SyncEvent},
    reqwest::Url,
    Client, ClientBuildError, ClientBuilder, Session,
};
use regex::Regex;
use ruma::{
    api::{
        appservice::{
            event::push_events,
            query::{query_room_alias::v1 as query_room, query_user_id::v1 as query_user},
            Registration,
        },
        client::{account::register, session::login, sync::sync_events, uiaa::UserIdentifier},
    },
    assign,
    events::{room::member::MembershipState, AnyRoomEvent, AnyStateEvent},
    DeviceId, IdParseError, OwnedDeviceId, OwnedRoomId, OwnedServerName, UserId,
};
use serde::{de::DeserializeOwned, Deserialize};
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

mod error;
pub mod event_handler;
mod webserver;

pub type Result<T, E = Error> = std::result::Result<T, E>;
pub type Host = String;
pub type Port = u16;

const USER_KEY: &[u8] = b"appservice.users.";
pub const USER_MEMBER: &[u8] = b"appservice.users.membership.";

/// Builder for a virtual user
#[derive(Debug)]
pub struct VirtualUserBuilder<'a> {
    appservice: &'a AppService,
    localpart: &'a str,
    device_id: Option<OwnedDeviceId>,
    client_builder: ClientBuilder,
    log_in: bool,
    restored_session: Option<Session>,
}

impl<'a> VirtualUserBuilder<'a> {
    /// Create a new virtual user builder
    /// # Arguments
    ///
    /// * `localpart` - The localpart of the virtual user
    pub fn new(appservice: &'a AppService, localpart: &'a str) -> Self {
        Self {
            appservice,
            localpart,
            device_id: None,
            client_builder: Client::builder(),
            log_in: false,
            restored_session: None,
        }
    }

    /// Set the device id of the virtual user
    pub fn device_id(mut self, device_id: Option<OwnedDeviceId>) -> Self {
        self.device_id = device_id;
        self
    }

    /// Sets the client builder to use for the virtual user
    pub fn client_builder(mut self, client_builder: ClientBuilder) -> Self {
        self.client_builder = client_builder;
        self
    }

    /// Log in as the virtual user
    ///
    /// In some cases it is necessary to log in as the virtual user, such as to
    /// upload device keys
    pub fn login(mut self) -> Self {
        self.log_in = true;
        self
    }

    /// Restore a persisted session
    ///
    /// This is primarily useful if you enable
    /// [`VirtualUserBuilder::login()`] and want to restore a session
    /// from a previous run.
    pub fn restored_session(mut self, session: Session) -> Self {
        self.restored_session = Some(session);
        self
    }

    /// Build the virtual user
    ///
    /// # Errors
    /// This function returns an error if an invalid localpart is provided.
    pub async fn build(self) -> Result<Client> {
        if let Some(client) = self.appservice.clients.get(self.localpart) {
            return Ok(client.clone());
        }

        let user_id = UserId::parse_with_server_name(self.localpart, &self.appservice.server_name)?;
        if !(self.appservice.user_id_is_in_namespace(&user_id)?
            || self.localpart == self.appservice.registration.sender_localpart)
        {
            warn!("Virtual client id '{user_id}' is not in the namespace")
        }

        let mut builder = self.client_builder;

        if !self.log_in && self.localpart != self.appservice.registration.sender_localpart {
            builder = builder.assert_identity();
        }

        let client = builder
            .homeserver_url(self.appservice.homeserver_url.clone())
            .appservice_mode()
            .build()
            .await
            .map_err(ClientBuildError::assert_valid_builder_args)?;

        let session = if let Some(session) = self.restored_session {
            session
        } else if self.log_in && self.localpart != self.appservice.registration.sender_localpart {
            self.appservice
                .create_session(self.localpart, self.device_id.as_ref().map(|v| v.as_ref()), None)
                .await?
        } else {
            // Don’t log in
            Session {
                access_token: self.appservice.registration.as_token.clone(),
                user_id: user_id.clone(),
                device_id: self.device_id.unwrap_or_else(DeviceId::new),
            }
        };

        client.restore_login(session).await?;

        self.appservice.clients.insert(self.localpart.to_owned(), client.clone());

        Ok(client)
    }
}

/// AppService Registration
///
/// Wrapper around [`Registration`]
#[derive(Debug, Clone)]
pub struct AppServiceRegistration {
    inner: Registration,
}

impl AppServiceRegistration {
    /// Try to load registration from yaml string
    ///
    /// See the fields of [`Registration`] for the required format
    pub fn try_from_yaml_str(value: impl AsRef<str>) -> Result<Self> {
        Ok(Self { inner: serde_yaml::from_str(value.as_ref())? })
    }

    /// Try to load registration from yaml file
    ///
    /// See the fields of [`Registration`] for the required format
    pub fn try_from_yaml_file(path: impl Into<PathBuf>) -> Result<Self> {
        let file = File::open(path.into())?;

        Ok(Self { inner: serde_yaml::from_reader(file)? })
    }

    /// Get the host and port from the registration URL
    ///
    /// If no port is found it falls back to scheme defaults: 80 for http and
    /// 443 for https
    pub fn get_host_and_port(&self) -> Result<(Host, Port)> {
        let uri = Uri::try_from(&self.inner.url)?;

        let host = uri.host().ok_or(Error::MissingRegistrationHost)?.to_owned();
        let port = match uri.port() {
            Some(port) => Ok(port.as_u16()),
            None => match uri.scheme_str() {
                Some("http") => Ok(80),
                Some("https") => Ok(443),
                _ => Err(Error::MissingRegistrationPort),
            },
        }?;

        Ok((host, port))
    }
}

impl From<Registration> for AppServiceRegistration {
    fn from(value: Registration) -> Self {
        Self { inner: value }
    }
}

impl Deref for AppServiceRegistration {
    type Target = Registration;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

/// Cache data for the registration namespaces.
#[derive(Debug, Clone)]
pub struct NamespaceCache {
    /// List of user regexes in our namespace
    users: Vec<Regex>,
    /// List of alias regexes in our namespace
    #[allow(dead_code)]
    aliases: Vec<Regex>,
    /// List of room id regexes in our namespace
    #[allow(dead_code)]
    rooms: Vec<Regex>,
}

impl NamespaceCache {
    /// Creates a new registration cache from a [`Registration`] value
    pub fn from_registration(registration: &Registration) -> Result<Self> {
        let users = registration
            .namespaces
            .users
            .iter()
            .map(|user| Regex::new(&user.regex))
            .collect::<Result<Vec<_>, _>>()?;
        let aliases = registration
            .namespaces
            .aliases
            .iter()
            .map(|user| Regex::new(&user.regex))
            .collect::<Result<Vec<_>, _>>()?;
        let rooms = registration
            .namespaces
            .rooms
            .iter()
            .map(|user| Regex::new(&user.regex))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(NamespaceCache { users, aliases, rooms })
    }
}

type Localpart = String;

/// The `localpart` of the user associated with the application service via
/// `sender_localpart` in [`AppServiceRegistration`].
///
/// Dummy type for shared documentation
#[allow(dead_code)]
pub type MainUser = ();

/// The application service may specify the virtual user to act as through use
/// of a user_id query string parameter on the request. The user specified in
/// the query string must be covered by one of the [`AppServiceRegistration`]'s
/// `users` namespaces.
///
/// Dummy type for shared documentation
pub type VirtualUser = ();

/// AppService
#[derive(Debug, Clone)]
pub struct AppService {
    homeserver_url: Url,
    server_name: OwnedServerName,
    registration: Arc<AppServiceRegistration>,
    namespaces: Arc<NamespaceCache>,
    clients: Arc<DashMap<Localpart, Client>>,
    event_handler: event_handler::EventHandler,
}

impl AppService {
    /// Create new AppService
    ///
    /// Also creates and caches a [`Client`] for the [`MainUser`].
    /// A default [`ClientBuilder`] is used, if you want to customize it
    /// use [`with_client_builder()`][Self::with_client_builder] instead.
    ///
    /// # Arguments
    ///
    /// * `homeserver_url` - The homeserver that the client should connect to.
    /// * `server_name` - The server name to use when constructing user ids from
    ///   the localpart.
    /// * `registration` - The [AppService Registration] to use when interacting
    ///   with the homeserver.
    ///
    /// [AppService Registration]: https://matrix.org/docs/spec/application_service/r0.1.2#registration
    pub async fn new(
        homeserver_url: impl TryInto<Url, Error = url::ParseError>,
        server_name: impl TryInto<OwnedServerName, Error = IdParseError>,
        registration: AppServiceRegistration,
    ) -> Result<Self> {
        let appservice =
            Self::with_client_builder(homeserver_url, server_name, registration, Client::builder())
                .await?;

        Ok(appservice)
    }

    /// Same as [`new()`][Self::new] but lets you provide a [`ClientBuilder`]
    /// for the [`Client`]
    pub async fn with_client_builder(
        homeserver_url: impl TryInto<Url, Error = url::ParseError>,
        server_name: impl TryInto<OwnedServerName, Error = IdParseError>,
        registration: AppServiceRegistration,
        builder: ClientBuilder,
    ) -> Result<Self> {
        let homeserver_url = homeserver_url.try_into()?;
        let server_name = server_name.try_into()?;
        let registration = Arc::new(registration);
        let namespaces = Arc::new(NamespaceCache::from_registration(&registration)?);
        let clients = Arc::new(DashMap::new());
        let sender_localpart = registration.sender_localpart.clone();
        let event_handler = event_handler::EventHandler::default();

        let appservice = AppService {
            homeserver_url,
            server_name,
            registration,
            namespaces,
            clients,
            event_handler,
        };

        // we create and cache the [`MainUser`] by default
        appservice.virtual_user_builder(&sender_localpart).client_builder(builder).build().await?;

        Ok(appservice)
    }

    /// Create a [`Client`] for the given [`VirtualUser`]'s `localpart`
    ///
    /// Will create and return a [`Client`] that's configured to [assert the
    /// identity] on all outgoing homeserver requests if `localpart` is
    /// given.
    ///
    /// This method is a singleton that saves the client internally for re-use
    /// based on the `localpart`. The cached [`Client`] can be retrieved either
    /// by calling this method again or by calling
    /// [`get_cached_client()`][Self::get_cached_client] which is non-async
    /// convenience wrapper.
    ///
    /// Note that if you want to do actions like joining rooms with a virtual
    /// user it needs to be registered first. `Self::register_virtual_user()`
    /// can be used for that purpose.
    ///
    /// # Arguments
    ///
    /// * `localpart` - The localpart of the user we want assert our identity to
    ///
    /// [registration]: https://matrix.org/docs/spec/application_service/r0.1.2#registration
    /// [assert the identity]: https://matrix.org/docs/spec/application_service/r0.1.2#identity-assertion
    pub async fn virtual_user_client(&self, localpart: impl AsRef<str>) -> Result<Client> {
        self.virtual_user_builder(localpart.as_ref()).build().await
    }

    /// Same as [`virtual_user_client()`][Self::virtual_user_client] but with
    /// the ability to pass in a [`ClientBuilder`]
    ///
    /// Since this method is a singleton follow-up calls with different
    /// [`ClientBuilder`]s will be ignored.
    pub async fn virtual_user_client_with_client_builder(
        &self,
        localpart: impl AsRef<str>,
        builder: ClientBuilder,
    ) -> Result<Client> {
        self.virtual_user_builder(localpart.as_ref()).client_builder(builder).build().await
    }

    pub fn virtual_user_builder<'a>(&'a self, localpart: &'a str) -> VirtualUserBuilder<'a> {
        VirtualUserBuilder::new(self, localpart)
    }

    /// Create a session using appservice login for a virtual user.
    async fn create_session(
        &self,
        user: impl AsRef<str>,
        device_id: Option<&str>,
        initial_device_display_name: Option<&str>,
    ) -> Result<Session> {
        let homeserver = self.homeserver_url.clone();
        info!(homeserver = homeserver.as_str(), user = user.as_ref(), "Logging in as virtual user");

        let login_info = login::v3::LoginInfo::ApplicationService(
            login::v3::ApplicationService::new(UserIdentifier::UserIdOrLocalpart(user.as_ref())),
        );

        let request = assign!(login::v3::Request::new(login_info), {
            device_id: device_id.map(|d| d.into()),
            initial_device_display_name
        });

        let response = self
            .get_cached_client(None)?
            .send(request, Some(RequestConfig::short_retry().force_auth()))
            .await?;

        Ok(Session {
            access_token: response.access_token,
            user_id: response.user_id,
            device_id: response.device_id,
        })
    }

    /// Get cached [`Client`]
    ///
    /// Will return the client for the given `localpart` if previously
    /// constructed with [`virtual_user_client()`][Self::virtual_user_client] or
    /// [`virtual_user_client_with_config()`][Self::
    /// virtual_user_client_with_client_builder].
    ///
    /// If no `localpart` is given it assumes the [`MainUser`]'s `localpart`. If
    /// no client for `localpart` is found it will return an Error.
    pub fn get_cached_client(&self, localpart: Option<&str>) -> Result<Client> {
        let localpart = localpart.unwrap_or_else(|| self.registration.sender_localpart.as_ref());

        let entry = self.clients.get(localpart).ok_or(Error::NoClientForLocalpart)?;

        Ok(entry.value().clone())
    }

    /// Convenience wrapper around [`Client::register_event_handler()`] that
    /// attaches the event handler to the [`MainUser`]'s [`Client`]
    ///
    /// Note that the event handler in the [`AppService`] context only triggers
    /// [`join` room `timeline` events], so no state events or events from the
    /// `invite`, `knock` or `leave` scope. The rationale behind that is
    /// that incoming AppService transactions from the homeserver are not
    /// necessarily bound to a specific user but can cover a multitude of
    /// namespaces, and as such the AppService basically only "observes
    /// joined rooms". Also currently homeservers only push PDUs to appservices,
    /// no EDUs. There's the open [MSC2409] regarding supporting EDUs in the
    /// future, though it seems to be planned to put EDUs into a different
    /// JSON key than `events` to stay backwards compatible.
    ///
    /// [`join` room `timeline` events]: https://spec.matrix.org/unstable/client-server-api/#get_matrixclientr0sync
    /// [MSC2409]: https://github.com/matrix-org/matrix-doc/pull/2409
    pub async fn register_event_handler<Ev, Ctx, H>(&self, handler: H) -> Result<&Self>
    where
        Ev: SyncEvent + DeserializeOwned + Send + 'static,
        H: EventHandler<Ev, Ctx>,
        <H::Future as Future>::Output: EventHandlerResult,
    {
        let client = self.get_cached_client(None)?;
        client.register_event_handler(handler).await;

        Ok(self)
    }

    /// Convenience wrapper around [`Client::register_event_handler_context`]
    /// attaches the event handler context to the [`MainUser`]'s [`Client`].
    pub fn register_event_handler_context<T>(&self, ctx: T) -> Result<&Self>
    where
        T: Clone + Send + Sync + 'static,
    {
        let client = self.get_cached_client(None)?;
        client.register_event_handler_context(ctx);

        Ok(self)
    }

    /// Register a responder for queries about the existence of a user with a
    /// given mxid.
    ///
    /// See [GET /_matrix/app/v1/users/{userId}](https://matrix.org/docs/spec/application_service/r0.1.2#get-matrix-app-v1-users-userid).
    ///
    /// # Example
    /// ```no_run
    /// # use matrix_sdk_appservice::AppService;
    /// # fn run(appservice: AppService) {
    /// appservice.register_user_query(Box::new(|appservice, req| Box::pin(async move {
    ///     println!("Got request for {}", req.user_id);
    ///     true
    /// })));
    /// # }
    /// ```
    pub async fn register_user_query(
        &self,
        handler: AppserviceFn<query_user::IncomingRequest, bool>,
    ) {
        *self.event_handler.users.lock().await = Some(handler);
    }

    /// Register a responder for queries about the existence of a room with the
    /// given alias.
    ///
    /// See [GET /_matrix/app/v1/rooms/{roomAlias}](https://matrix.org/docs/spec/application_service/r0.1.2#get-matrix-app-v1-rooms-roomalias).
    ///
    /// # Example
    /// ```no_run
    /// # use matrix_sdk_appservice::AppService;
    /// # fn run(appservice: AppService) {
    /// appservice.register_room_query(Box::new(|appservice, req| Box::pin(async move {
    ///     println!("Got request for {}", req.room_alias);
    ///     true
    /// })));
    /// # }
    /// ```
    pub async fn register_room_query(
        &self,
        handler: AppserviceFn<query_room::IncomingRequest, bool>,
    ) {
        *self.event_handler.rooms.lock().await = Some(handler);
    }

    /// Register a virtual user by sending a [`register::v3::Request`] to the
    /// homeserver
    ///
    /// # Arguments
    ///
    /// * `localpart` - The localpart of the user to register. Must be covered
    ///   by the namespaces in the [`Registration`] in order to succeed.
    ///
    /// # Returns
    /// This function may return a UIAA response, which should be checked for
    /// with [`Error::uiaa_response()`].
    pub async fn register_virtual_user(&self, localpart: impl AsRef<str>) -> Result<()> {
        if self.is_user_registered(localpart.as_ref()).await? {
            return Ok(());
        }
        let request = assign!(register::v3::Request::new(), {
            username: Some(localpart.as_ref()),
            login_type: Some(&register::LoginType::ApplicationService),
        });

        let client = self.get_cached_client(None)?;
        client.register(request).await?;
        self.set_user_registered(localpart.as_ref()).await?;

        Ok(())
    }

    /// Add the given localpart to the database of registered localparts.
    async fn set_user_registered(&self, localpart: impl AsRef<str>) -> Result<()> {
        let client = self.get_cached_client(None)?;
        client
            .store()
            .set_custom_value(
                &[USER_KEY, localpart.as_ref().as_bytes()].concat(),
                vec![u8::from(true)],
            )
            .await?;
        Ok(())
    }

    /// Get whether a localpart is listed in the database as registered.
    async fn is_user_registered(&self, localpart: impl AsRef<str>) -> Result<bool> {
        let client = self.get_cached_client(None)?;
        let key = [USER_KEY, localpart.as_ref().as_bytes()].concat();
        let store = client.store().get_custom_value(&key).await?;
        let registered =
            store.and_then(|vec| vec.first().copied()).map_or(false, |b| b == u8::from(true));
        Ok(registered)
    }

    /// Get the AppService [registration]
    ///
    /// [registration]: https://matrix.org/docs/spec/application_service/r0.1.2#registration
    pub fn registration(&self) -> &AppServiceRegistration {
        &self.registration
    }

    /// Compare the given `hs_token` against `registration.hs_token`
    ///
    /// Returns `true` if the tokens match, `false` otherwise.
    pub fn compare_hs_token(&self, hs_token: impl AsRef<str>) -> bool {
        self.registration.hs_token == hs_token.as_ref()
    }

    /// Check if given `user_id` is in any of the [`AppServiceRegistration`]'s
    /// `users` namespaces
    pub fn user_id_is_in_namespace(&self, user_id: impl AsRef<str>) -> bool {
        for regex in &self.namespaces.users {
            if regex.is_match(user_id.as_ref()) {
                return true;
            }
        }

        false
    }

    /// Returns a [`warp::Filter`] to be used as [`warp::serve()`] route
    ///
    /// Note that if you handle any of the [application-service-specific
    /// routes], including the legacy routes, you will break the appservice
    /// functionality.
    ///
    /// [application-service-specific routes]: https://spec.matrix.org/unstable/application-service-api/#legacy-routes
    pub fn warp_filter(&self) -> warp::filters::BoxedFilter<(impl warp::Reply,)> {
        webserver::warp_filter(self.clone())
    }

    /// Receive an incoming [transaction], pushing the contained events to
    /// active virtual clients.
    ///
    /// [transaction]: https://spec.matrix.org/v1.2/application-service-api/#put_matrixappv1transactionstxnid
    pub async fn receive_transaction(
        &self,
        transaction: push_events::v1::IncomingRequest,
    ) -> Result<()> {
        let client = self.get_cached_client(None)?;

        // Find membership events affecting members in our namespace, and update
        // membership accordingly
        for event in transaction.events.iter() {
            let event = match event.deserialize() {
                Ok(AnyRoomEvent::State(AnyStateEvent::RoomMember(event))) => event,
                _ => continue,
            };
            if !self.user_id_is_in_namespace(event.state_key()) {
                continue;
            }
            let localpart = event.state_key().localpart();
            client
                .store()
                .set_custom_value(
                    &[USER_MEMBER, event.room_id().as_bytes(), b".", localpart.as_bytes()].concat(),
                    event.membership().to_string().into_bytes(),
                )
                .await?;
        }

        /// Helper type for extracting the room id for an event
        #[derive(Debug, Deserialize)]
        struct EventRoomId {
            room_id: Option<OwnedRoomId>,
        }

        // Spawn a task for each client that constructs and pushes a sync event
        let mut tasks: Vec<JoinHandle<_>> = Vec::new();
        let transaction = Arc::new(transaction);
        for virt_client in self.clients.iter() {
            let client = client.clone();
            let virt_client = virt_client.clone();
            let transaction = transaction.clone();
            let appserv_uid = self.registration.sender_localpart.clone();

            let task = tokio::spawn(async move {
                let user_id = match virt_client.user_id() {
                    Some(user_id) => user_id.localpart(),
                    // The client is not logged in, skipping
                    None => return Ok(()),
                };
                let mut response = sync_events::v3::Response::new(transaction.txn_id.to_string());

                // Clients expect events to be grouped per room, where the group also denotes
                // what the client's membership of the given room is. We take the
                // all the events in the transaction and sort them into appropriate
                // groups, falling back to a membership of "join" if it's unknown.
                for raw_event in &transaction.events {
                    let room_id = match raw_event.deserialize_as::<EventRoomId>()?.room_id {
                        Some(room_id) => room_id,
                        None => {
                            warn!("Transaction contained event with no ID");
                            continue;
                        }
                    };
                    let key = &[USER_MEMBER, room_id.as_bytes(), b".", user_id.as_bytes()].concat();
                    let membership = match client.store().get_custom_value(key).await? {
                        Some(value) => String::from_utf8(value).ok().map(MembershipState::from),
                        // Assume the appservice is in every known room
                        None if user_id == appserv_uid => Some(MembershipState::Join),
                        None => None,
                    };

                    match membership {
                        Some(MembershipState::Join) => {
                            let room = response.rooms.join.entry(room_id).or_default();
                            room.timeline.events.push(raw_event.clone().cast())
                        }
                        Some(MembershipState::Leave | MembershipState::Ban) => {
                            let room = response.rooms.leave.entry(room_id).or_default();
                            room.timeline.events.push(raw_event.clone().cast())
                        }
                        Some(MembershipState::Knock) => {
                            response.rooms.knock.entry(room_id).or_default();
                        }
                        Some(MembershipState::Invite) => {
                            response.rooms.invite.entry(room_id).or_default();
                        }
                        Some(unknown) => debug!("Unknown membership type: {unknown}"),
                        None => debug!("Assuming {user_id} is not in {room_id}"),
                    }
                }
                virt_client.receive_transaction(&transaction.txn_id, response).await?;
                Ok::<_, Error>(())
            });

            tasks.push(task);
        }
        for task in tasks {
            if let Err(e) = task.await {
                warn!("Joining sync task failed: {}", e);
            }
        }
        Ok(())
    }

    /// Convenience method that runs an http server depending on the selected
    /// server feature
    ///
    /// This is a blocking call that tries to listen on the provided host and
    /// port
    pub async fn run(&self, host: impl Into<String>, port: impl Into<u16>) -> Result<()> {
        let host = host.into();
        let port = port.into();
        info!("Starting AppService on {}:{}", &host, &port);

        webserver::run_server(self.clone(), host, port).await?;
        Ok(())
    }
}

/// Ruma always expects the path to start with `/_matrix`, so we transform
/// accordingly. Handles [legacy routes] and appservice being located on a sub
/// path.
///
/// [legacy routes]: https://matrix.org/docs/spec/application_service/r0.1.2#legacy-routes
// TODO: consider ruma PR
pub(crate) fn transform_request_path(
    mut request: http::Request<Bytes>,
) -> Result<http::Request<Bytes>> {
    let uri = request.uri();
    // remove trailing slash from path
    let path = uri.path().trim_end_matches('/').to_owned();

    if !path.starts_with("/_matrix/app/v1/") {
        let path = match path {
            // special-case paths without value at the end
            _ if path.ends_with("/_matrix/app/unstable/thirdparty/user") => {
                "/_matrix/app/v1/thirdparty/user".to_owned()
            }
            _ if path.ends_with("/_matrix/app/unstable/thirdparty/location") => {
                "/_matrix/app/v1/thirdparty/location".to_owned()
            }
            // regular paths with values at the end
            _ => {
                let mut path = path.split('/').into_iter().rev();
                let value = match path.next() {
                    Some(value) => value,
                    None => return Err(Error::UriEmptyPath),
                };

                let mut path = match path.next() {
                    Some(path_segment)
                        if ["transactions", "users", "rooms"].contains(&path_segment) =>
                    {
                        format!("/_matrix/app/v1/{}/{}", path_segment, value)
                    }
                    Some(path_segment) => match path.next() {
                        Some(path_segment2) if path_segment2 == "thirdparty" => {
                            format!("/_matrix/app/v1/thirdparty/{}/{}", path_segment, value)
                        }
                        _ => return Err(Error::UriPathUnknown),
                    },
                    None => return Err(Error::UriEmptyPath),
                };

                if let Some(query) = uri.query() {
                    path.push('?');
                    path.push_str(query);
                }

                path
            }
        };

        let mut parts = uri.clone().into_parts();
        parts.path_and_query = Some(path.parse()?);

        let uri = parts.try_into().map_err(http::Error::from)?;
        *request.uri_mut() = uri;
    }

    Ok(request)
}
