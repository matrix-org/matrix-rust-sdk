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

#[cfg(not(any(feature = "warp")))]
compile_error!("one webserver feature must be enabled. available ones: `warp`");

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
    config::ClientConfig,
    event_handler::{EventHandler, EventHandlerResult, SyncEvent},
    reqwest::Url,
    Client, Session,
};
use regex::Regex;
use ruma::{
    api::{
        appservice::{
            query::{query_room_alias::v1 as query_room, query_user_id::v1 as query_user},
            Registration,
        },
        client::account::register,
    },
    assign, identifiers, DeviceId, ServerName, UserId,
};
use serde::de::DeserializeOwned;
use tracing::info;

mod error;
pub mod event_handler;
mod webserver;

pub type Result<T, E = Error> = std::result::Result<T, E>;
pub type Host = String;
pub type Port = u16;

const USER_KEY: &[u8] = b"appservice.users.";

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
    server_name: Box<ServerName>,
    registration: Arc<AppServiceRegistration>,
    clients: Arc<DashMap<Localpart, Client>>,
    event_handler: event_handler::EventHandler,
}

impl AppService {
    /// Create new AppService
    ///
    /// Also creates and caches a [`Client`] for the [`MainUser`].
    /// The default [`ClientConfig`] is used, if you want to customize it
    /// use [`Self::new_with_config()`] instead.
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
        server_name: impl TryInto<Box<ServerName>, Error = identifiers::Error>,
        registration: AppServiceRegistration,
    ) -> Result<Self> {
        let appservice = Self::new_with_config(
            homeserver_url,
            server_name,
            registration,
            ClientConfig::default(),
        )
        .await?;

        Ok(appservice)
    }

    /// Same as [`Self::new()`] but lets you provide a [`ClientConfig`] for the
    /// [`Client`]
    pub async fn new_with_config(
        homeserver_url: impl TryInto<Url, Error = url::ParseError>,
        server_name: impl TryInto<Box<ServerName>, Error = identifiers::Error>,
        registration: AppServiceRegistration,
        client_config: ClientConfig,
    ) -> Result<Self> {
        let homeserver_url = homeserver_url.try_into()?;
        let server_name = server_name.try_into()?;
        let registration = Arc::new(registration);
        let clients = Arc::new(DashMap::new());
        let sender_localpart = registration.sender_localpart.clone();
        let event_handler = event_handler::EventHandler::default();

        let appservice =
            AppService { homeserver_url, server_name, registration, clients, event_handler };

        // we create and cache the [`MainUser`] by default
        appservice.create_and_cache_client(&sender_localpart, client_config).await?;

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
    /// by calling this method again or by calling [`Self::get_cached_client()`]
    /// which is non-async convenience wrapper.
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
        let client =
            self.virtual_user_client_with_config(localpart, ClientConfig::default()).await?;

        Ok(client)
    }

    /// Same as [`Self::virtual_user_client()`] but with the ability to pass in
    /// a [`ClientConfig`]
    ///
    /// Since this method is a singleton follow-up calls with different
    /// [`ClientConfig`]s will be ignored.
    pub async fn virtual_user_client_with_config(
        &self,
        localpart: impl AsRef<str>,
        config: ClientConfig,
    ) -> Result<Client> {
        // TODO: check if localpart is covered by namespace?
        let localpart = localpart.as_ref();

        let client = if let Some(client) = self.clients.get(localpart) {
            client.clone()
        } else {
            self.create_and_cache_client(localpart, config).await?
        };

        Ok(client)
    }

    async fn create_and_cache_client(
        &self,
        localpart: &str,
        config: ClientConfig,
    ) -> Result<Client> {
        let user_id = UserId::parse_with_server_name(localpart, &self.server_name)?;

        // The `as_token` in the `Session` maps to the [`MainUser`]
        // (`sender_localpart`) by default, so we don't need to assert identity
        // in that case
        let config = if localpart != self.registration.sender_localpart {
            let request_config = config.get_request_config().assert_identity();
            config.request_config(request_config)
        } else {
            config
        };

        let client =
            Client::new_with_config(self.homeserver_url.clone(), config.appservice_mode()).await?;

        let session = Session {
            access_token: self.registration.as_token.clone(),
            user_id: user_id.clone(),
            // TODO: expose & proper E2EE
            device_id: DeviceId::new(),
        };

        client.restore_login(session).await?;
        self.clients.insert(localpart.to_owned(), client.clone());

        Ok(client)
    }

    /// Get cached [`Client`]
    ///
    /// Will return the client for the given `localpart` if previously
    /// constructed with [`Self::virtual_user_client()`] or
    /// [`Self::virtual_user_client_with_config()`].
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
            store.and_then(|vec| vec.get(0).copied()).map_or(false, |b| b == u8::from(true));
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
    pub fn user_id_is_in_namespace(&self, user_id: impl AsRef<str>) -> Result<bool> {
        for user in &self.registration.namespaces.users {
            // TODO: precompile on AppService construction
            let re = Regex::new(&user.regex)?;
            if re.is_match(user_id.as_ref()) {
                return Ok(true);
            }
        }

        Ok(false)
    }

    /// Returns a [`warp::Filter`] to be used as [`warp::serve()`] route
    ///
    /// Note that if you handle any of the [application-service-specific
    /// routes], including the legacy routes, you will break the appservice
    /// functionality.
    ///
    /// [application-service-specific routes]: https://spec.matrix.org/unstable/application-service-api/#legacy-routes
    #[cfg(feature = "warp")]
    #[cfg_attr(docs, doc(cfg(feature = "warp")))]
    pub fn warp_filter(&self) -> warp::filters::BoxedFilter<(impl warp::Reply,)> {
        webserver::warp::warp_filter(self.clone())
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

        #[cfg(feature = "warp")]
        {
            webserver::warp::run_server(self.clone(), host, port).await?;
            Ok(())
        }

        #[cfg(not(any(feature = "warp",)))]
        unreachable!()
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
    let path = uri.path().trim_end_matches('/').to_string();

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
                    path.push_str(&format!("?{}", query));
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
