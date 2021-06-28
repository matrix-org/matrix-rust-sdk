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
//! # use matrix_sdk::{async_trait, EventHandler};
//! #
//! # struct MyEventHandler;
//! #
//! # #[async_trait]
//! # impl EventHandler for MyEventHandler {}
//! #
//! use matrix_sdk_appservice::{AppService, AppServiceRegistration};
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
//! appservice.set_event_handler(Box::new(MyEventHandler)).await?;
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
//! [examples directory]: https://github.com/matrix-org/matrix-rust-sdk/tree/master/matrix_sdk_appservice/examples

#[cfg(not(any(feature = "actix", feature = "warp")))]
compile_error!("one webserver feature must be enabled. available ones: `actix`, `warp`");

use std::{
    convert::{TryFrom, TryInto},
    fs::File,
    ops::Deref,
    path::PathBuf,
    sync::Arc,
};

use dashmap::DashMap;
pub use error::Error;
use http::{uri::PathAndQuery, Uri};
pub use matrix_sdk;
use matrix_sdk::{
    bytes::Bytes, reqwest::Url, Client, ClientConfig, EventHandler, HttpError, Session,
};
use regex::Regex;
pub use ruma;
use ruma::{
    api::{
        appservice::Registration,
        client::{
            error::ErrorKind,
            r0::{
                account::register,
                membership::{self, join_room_by_id, join_room_by_id_or_alias},
                state::get_state_events,
                sync::sync_events,
                uiaa::UiaaResponse,
            },
        },
        error::{FromHttpResponseError, ServerError},
    },
    assign, identifiers,
    serde::Raw,
    DeviceId, RoomId, RoomIdOrAliasId, ServerName, ServerNameBox, UserId,
};
use serde::Deserialize;
use tracing::{error, info, trace, warn};

mod error;
mod webserver;

pub type Result<T> = std::result::Result<T, Error>;
pub type Host = String;
pub type Port = u16;

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
    server_name: ServerNameBox,
    registration: Arc<AppServiceRegistration>,
    clients: Arc<DashMap<Localpart, Client>>,
}

impl AppService {
    /// Create new AppService
    ///
    /// Also creates and caches a [`Client`] for the [`MainUser`].
    /// The default [`ClientConfig`] is used, if you want to customize it
    /// use [`Self::new_with_config()`] instead.
    ///
    /// Note that all regular state, also for [`VirtualUser`]s, is stored into
    /// the [`MainUser`]s state store, so if you want the state store
    /// persisted you need to configure it with [`Self::new_with_config()`].
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
        server_name: impl TryInto<ServerNameBox, Error = identifiers::Error>,
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
        server_name: impl TryInto<ServerNameBox, Error = identifiers::Error>,
        registration: AppServiceRegistration,
        client_config: ClientConfig,
    ) -> Result<Self> {
        let homeserver_url = homeserver_url.try_into()?;
        let server_name = server_name.try_into()?;
        let registration = Arc::new(registration);
        let clients = Arc::new(DashMap::new());
        let sender_localpart = registration.sender_localpart.clone();

        let appservice = AppService { homeserver_url, server_name, registration, clients };

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
    /// Note that if you want to do actions with a virtual user client that
    /// require the user to actually be registered with synapse you need to call
    /// [`Self::register_virtual_user()`] first.
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
            Client::new_with_config(self.homeserver_url.clone(), config.appservice_mode())?;

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
    /// If no `localpart` is given it assumes the [`MainUser`]'s `localpart`
    /// which is always available since it gets constructed as part of
    /// [`Self::new()`].
    ///
    /// Will return the client for the given `localpart` if previously
    /// constructed with [`Self::virtual_user_client()`] or
    /// [`Self::virtual_user_client_with_config()`]. If no client for
    /// `localpart` is found it will return an Error.
    pub fn get_cached_client(&self, localpart: Option<&str>) -> Result<Client> {
        let localpart = localpart.unwrap_or_else(|| self.registration.sender_localpart.as_ref());

        let entry = self.clients.get(localpart).ok_or(Error::NoClientForLocalpart)?;

        Ok(entry.value().clone())
    }

    /// Convenience wrapper around [`Client::set_event_handler()`] that attaches
    /// the event handler to the [`MainUser`]'s [`Client`]
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
    pub async fn set_event_handler(&mut self, handler: Box<dyn EventHandler>) -> Result<()> {
        let client = self.get_cached_client(None)?;

        client.set_event_handler(handler).await;

        Ok(())
    }

    /// Register a virtual user by sending a [`register::Request`] to the
    /// homeserver
    ///
    /// # Arguments
    ///
    /// * `localpart` - The localpart of the user to register. Must be covered
    ///   by the namespaces in the [`Registration`] in order to succeed.
    pub async fn register_virtual_user(&self, localpart: impl AsRef<str>) -> Result<()> {
        let request = assign!(register::Request::new(), {
            username: Some(localpart.as_ref()),
            login_type: Some(&register::LoginType::ApplicationService),
        });

        let client = self.get_cached_client(None)?;
        match client.register(request).await {
            Ok(_) => (),
            Err(error) => match error {
                matrix_sdk::Error::Http(HttpError::UiaaError(FromHttpResponseError::Http(
                    ServerError::Known(UiaaResponse::MatrixError(ref matrix_error)),
                ))) => {
                    match matrix_error.kind {
                        ErrorKind::UserInUse => {
                            // TODO: persist the fact that we registered that user
                            warn!("{}", matrix_error.message);
                        }
                        _ => return Err(error.into()),
                    }
                }
                _ => return Err(error.into()),
            },
        }

        Ok(())
    }

    /// Join a room by [`RoomId`].
    ///
    /// If no `localpart` is given it joins the [`MainUser`], otherwise the
    /// appropriate [`VirtualUser`].
    ///
    /// To be able to join a [`VirtualUser`] you need to create a client for it
    /// first by calling [`Self::virtual_user_client()`] or
    /// [`Self::virtual_user_client_with_config()`] since this method
    /// calls [`Self::get_cached_client()`] internally.
    ///
    /// This method will
    /// * If `localpart` is given
    ///   * Check if a [`Client`] for the given `localpart` exists and if it
    ///     doesn't create it by calling [`Self::virtual_user_client()`]. This
    ///     will use the default [`ClientConfig`], so if you want that
    ///     customized you must call [`Self::virtual_user_client_with_config()`]
    ///     once manually beforehand.
    ///   * Call [`Self::register_virtual_user()`] for the given `localpart` to
    ///     make sure we can actually join the room with the virtual user.
    /// * Sync initial state for the room before returning so that we have a
    ///   consistent room state. If syncing state fails we return an `Error` and
    ///   try to leave the room again.
    /// * Return a [`join_room_by_id::Response`] consisting of the joined rooms
    ///   `RoomId`.
    pub async fn join_room_by_id(
        &self,
        localpart: Option<&str>,
        room_id: &RoomId,
    ) -> Result<join_room_by_id::Response> {
        let client = self.join_client(localpart).await?;

        let join_response = client.join_room_by_id(room_id).await?;

        self.join_room_state(&client, &join_response.room_id).await.map(|_| join_response)
    }

    /// Join a room by [`RoomIdOrAliasId`].
    ///
    /// Same as [`Self::join_room_by_id`] but lets you provide
    /// [`RoomIdOrAliasId`] instead of just [`RoomId`].
    pub async fn join_room_by_id_or_alias(
        &self,
        localpart: Option<&str>,
        alias: &RoomIdOrAliasId,
        server_names: &[Box<ServerName>],
    ) -> Result<join_room_by_id_or_alias::Response> {
        let client = self.join_client(localpart).await?;

        let join_response = client.join_room_by_id_or_alias(alias, server_names).await?;

        self.join_room_state(&client, &join_response.room_id).await.map(|_| join_response)
    }

    async fn join_client(&self, localpart: Option<&str>) -> Result<Client> {
        let client = match localpart {
            Some(localpart) => {
                let client = match self.get_cached_client(Some(localpart)) {
                    Ok(client) => client,
                    Err(_) => self.virtual_user_client(localpart).await?,
                };
                self.register_virtual_user(localpart).await?;
                client
            }
            None => self.get_cached_client(None)?,
        };

        Ok(client)
    }

    async fn join_room_state(&self, client: &Client, room_id: &RoomId) -> Result<()> {
        if let Err(error) = self.update_state_for_room(&client, room_id).await {
            error!(
                "Syncing state for joined room {} failed, leaving room again: {}",
                room_id, error
            );

            let leave_request = membership::leave_room::Request::new(room_id);
            client.send(leave_request, None).await?;

            Err(error)
        } else {
            Ok(())
        }
    }

    async fn update_state_for_room(&self, client: &Client, room_id: &RoomId) -> Result<()> {
        let state_request = get_state_events::Request::new(room_id);
        let state_response = client.send(state_request, None).await?;

        let sync_response = try_state_into_sync_response(state_response)?;

        trace!("Updating state with sync_response: {:?}", &sync_response);

        client.receive_sync_response(sync_response).await?;

        Ok(())
    }

    /// Get the [`AppServiceRegistration`]
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

    /// Returns a closure to be used with [`actix_web::App::configure()`]
    ///
    /// Note that if you handle any of the [application-service-specific
    /// routes], including the legacy routes, you will break the appservice
    /// functionality.
    ///
    /// [application-service-specific routes]: https://spec.matrix.org/unstable/application-service-api/#legacy-routes
    #[cfg(feature = "actix")]
    #[cfg_attr(docs, doc(cfg(feature = "actix")))]
    pub fn actix_configure(&self) -> impl FnOnce(&mut actix_web::web::ServiceConfig) {
        let appservice = self.clone();

        move |config| {
            config.data(appservice);
            webserver::actix::configure(config);
        }
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
    /// This is a forever running call that tries to listen on the provided host
    /// and port
    pub async fn run(&self, host: impl Into<String>, port: impl Into<u16>) -> Result<()> {
        let host = host.into();
        let port = port.into();
        info!("Starting AppService on {}:{}", &host, &port);

        #[cfg(feature = "actix")]
        {
            webserver::actix::run_server(self.clone(), host, port).await?;
            Ok(())
        }

        #[cfg(feature = "warp")]
        {
            webserver::warp::run_server(self.clone(), host, port).await?;
            Ok(())
        }

        #[cfg(not(any(feature = "actix", feature = "warp",)))]
        unreachable!()
    }
}

/// Transforms [legacy routes] to the correct route so ruma can parse them
/// properly
///
/// [legacy routes]: https://matrix.org/docs/spec/application_service/r0.1.2#legacy-routes
pub(crate) fn transform_legacy_route(
    mut request: http::Request<Bytes>,
) -> Result<http::Request<Bytes>> {
    let uri = request.uri().to_owned();

    if !uri.path().starts_with("/_matrix/app/v1") {
        // rename legacy routes
        let mut parts = uri.into_parts();
        let path_and_query = match parts.path_and_query {
            Some(path_and_query) => format!("/_matrix/app/v1{}", path_and_query),
            None => "/_matrix/app/v1".to_owned(),
        };
        parts.path_and_query =
            Some(PathAndQuery::try_from(path_and_query).map_err(http::Error::from)?);
        let uri = parts.try_into().map_err(http::Error::from)?;

        *request.uri_mut() = uri;
    }

    Ok(request)
}

/// Convert state response into sync reponse
// TODO: Consider Ruma PR
fn try_state_into_sync_response(
    state_response: get_state_events::Response,
) -> serde_json::Result<sync_events::Response> {
    #[derive(Debug, Deserialize)]
    struct EventDeHelper {
        room_id: Option<RoomId>,
    }

    let mut response = sync_events::Response::new("batch".to_owned());

    for raw_event in state_response.room_state {
        let helper = raw_event.deserialize_as::<EventDeHelper>()?;
        let event_json = Raw::into_json(raw_event);

        if let Some(room_id) = helper.room_id {
            let join = response.rooms.join.entry(room_id).or_default();
            join.state.events.push(Raw::from_json(event_json));
        } else {
            warn!("Event without room_id: {}", event_json);
        }
    }

    Ok(response)
}
