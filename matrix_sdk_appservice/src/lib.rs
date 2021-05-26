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
//! # struct AppserviceEventHandler;
//! #
//! # #[async_trait]
//! # impl EventHandler for AppserviceEventHandler {}
//! #
//! use matrix_sdk_appservice::{Appservice, AppserviceRegistration};
//!
//! let homeserver_url = "http://127.0.0.1:8008";
//! let server_name = "localhost";
//! let registration = AppserviceRegistration::try_from_yaml_str(
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
//! let mut appservice = Appservice::new(homeserver_url, server_name, registration).await?;
//! appservice.set_event_handler(Box::new(AppserviceEventHandler)).await?;
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

#[cfg(not(any(feature = "actix",)))]
compile_error!("one webserver feature must be enabled. available ones: `actix`");

use std::{
    convert::{TryFrom, TryInto},
    fs::File,
    ops::Deref,
    path::PathBuf,
    sync::Arc,
};

use dashmap::DashMap;
use http::Uri;
#[doc(inline)]
pub use matrix_sdk::api_appservice as api;
use matrix_sdk::{
    api::{
        error::ErrorKind,
        r0::{
            account::register::{LoginType, Request as RegistrationRequest},
            uiaa::UiaaResponse,
        },
    },
    api_appservice::Registration,
    assign,
    identifiers::{self, DeviceId, ServerNameBox, UserId},
    reqwest::Url,
    Client, ClientConfig, EventHandler, FromHttpResponseError, HttpError, ServerError, Session,
};
use regex::Regex;
use tracing::warn;

#[cfg(feature = "actix")]
mod actix;
mod error;

pub use error::Error;
pub type Result<T> = std::result::Result<T, Error>;
pub type Host = String;
pub type Port = u16;

/// Appservice Registration
///
/// Wrapper around [`Registration`]
#[derive(Debug, Clone)]
pub struct AppserviceRegistration {
    inner: Registration,
}

impl AppserviceRegistration {
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

impl From<Registration> for AppserviceRegistration {
    fn from(value: Registration) -> Self {
        Self { inner: value }
    }
}

impl Deref for AppserviceRegistration {
    type Target = Registration;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

type Localpart = String;

/// The main appservice user is the `sender_localpart` from the given
/// [`AppserviceRegistration`]
///
/// Dummy type for shared documentation
#[allow(dead_code)]
pub type MainAppserviceUser = ();

/// Appservice
#[derive(Debug, Clone)]
pub struct Appservice {
    homeserver_url: Url,
    server_name: ServerNameBox,
    registration: Arc<AppserviceRegistration>,
    clients: Arc<DashMap<Localpart, Client>>,
}

impl Appservice {
    /// Create new Appservice
    ///
    /// # Arguments
    ///
    /// * `homeserver_url` - The homeserver that the client should connect to.
    /// * `server_name` - The server name to use when constructing user ids from
    ///   the localpart.
    /// * `registration` - The [Appservice Registration] to use when interacting
    ///   with the homserver.
    ///
    /// [Appservice Registration]: https://matrix.org/docs/spec/application_service/r0.1.2#registration
    pub async fn new(
        homeserver_url: impl TryInto<Url, Error = url::ParseError>,
        server_name: impl TryInto<ServerNameBox, Error = identifiers::Error>,
        registration: AppserviceRegistration,
    ) -> Result<Self> {
        let homeserver_url = homeserver_url.try_into()?;
        let server_name = server_name.try_into()?;

        let client_sender_localpart = Client::new(homeserver_url.clone())?;

        client_session_with_login_restore(
            &client_sender_localpart,
            &registration,
            registration.sender_localpart.as_ref(),
            &server_name,
        )
        .await?;

        let registration = Arc::new(registration);

        Ok(Appservice { homeserver_url, server_name, registration, client_sender_localpart })
    }

    /// Create a [`Client`]
    ///
    /// Will create and return a [`Client`] that's configured to [assert the
    /// identity] on all outgoing homeserver requests if `localpart` is
    /// given. If not given the [`Client`] will use the [`MainAppserviceUser`].
    ///
    /// This method is a singleton that saves the client internally for re-use
    /// based on the `localpart`.
    ///
    /// # Arguments
    ///
    /// * `localpart` - The localpart of the user we want assert our identity to
    ///
    /// [registration]: https://matrix.org/docs/spec/application_service/r0.1.2#registration
    /// [assert the identity]: https://matrix.org/docs/spec/application_service/r0.1.2#identity-assertion
    pub async fn client(&mut self, localpart: Option<&str>) -> Result<Client> {
        let client = self.client_with_config(localpart, ClientConfig::default()).await?;

        Ok(client)
    }

    /// Same as [`Self::client`] but with the ability to pass in a
    /// [`ClientConfig`]
    ///
    /// Since this method is a singleton follow-up calls with different
    /// [`ClientConfig`]s will be ignored.
    pub async fn client_with_config(
        &mut self,
        localpart: Option<&str>,
        config: ClientConfig,
    ) -> Result<Client> {
        let localpart = localpart.unwrap_or_else(|| self.registration.sender_localpart.as_ref());

        let client = if let Some(client) = self.clients.get(localpart) {
            client.clone()
        } else {
            let user_id = UserId::parse_with_server_name(localpart, &self.server_name)?;

            // The `as_token` in the `Session` maps to the [`MainAppserviceUser`]
            // (`sender_localpart`) by default, so we don't need to assert identity
            // in that case
            if localpart != self.registration.sender_localpart {
                config.get_request_config().assert_identity();
            }

            let client = Client::new_with_config(self.homeserver_url.clone(), config)?;

            let session = Session {
                access_token: self.registration.as_token.clone(),
                user_id: user_id.clone(),
                // TODO: expose & proper E2EE
                device_id: DeviceId::new(),
            };

            client.restore_login(session).await?;
            self.clients.insert(localpart.to_owned(), client.clone());

            client
        };

        Ok(client)
    }

    /// Get cached [`Client`]
    ///
    /// Will return the client for the given `localpart` if previously
    /// constructed with [`Self::client()`] or [`Self::client_with_config()`].
    /// If no client for the `localpart` is found it will return an Error.
    pub fn get_cached_client(&self, localpart: Option<&str>) -> Result<Client> {
        let localpart = localpart.unwrap_or_else(|| self.registration.sender_localpart.as_ref());

        let entry = self.clients.get(localpart).ok_or(Error::NoClientForLocalpart)?;

        Ok(entry.value().clone())
    }

    /// Convenience wrapper around [`Client::set_event_handler()`]
    ///
    /// Attaches the event handler to [`Self::client()`] with `None` as
    /// `localpart`
    pub async fn set_event_handler(&mut self, handler: Box<dyn EventHandler>) -> Result<()> {
        let client = self.client(None).await?;

        client.set_event_handler(handler).await;

        Ok(())
    }

    /// Register a virtual user by sending a [`RegistrationRequest`] to the
    /// homeserver
    ///
    /// # Arguments
    ///
    /// * `localpart` - The localpart of the user to register. Must be covered
    ///   by the namespaces in the [`Registration`] in order to succeed.
    pub async fn register(&mut self, localpart: impl AsRef<str>) -> Result<()> {
        let request = assign!(RegistrationRequest::new(), {
            username: Some(localpart.as_ref()),
            login_type: Some(&LoginType::ApplicationService),
        });

        let client = self.client(None).await?;
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

    /// Get the Appservice [registration]
    ///
    /// [registration]: https://matrix.org/docs/spec/application_service/r0.1.2#registration
    pub fn registration(&self) -> &AppserviceRegistration {
        &self.registration
    }

    /// Compare the given `hs_token` against `registration.hs_token`
    ///
    /// Returns `true` if the tokens match, `false` otherwise.
    pub fn compare_hs_token(&self, hs_token: impl AsRef<str>) -> bool {
        self.registration.hs_token == hs_token.as_ref()
    }

    /// Check if given `user_id` is in any of the registration user namespaces
    pub fn user_id_is_in_namespace(&self, user_id: impl AsRef<str>) -> Result<bool> {
        for user in &self.registration.namespaces.users {
            // TODO: precompile on Appservice construction
            let re = Regex::new(&user.regex)?;
            if re.is_match(user_id.as_ref()) {
                return Ok(true);
            }
        }

        Ok(false)
    }

    /// Service to register on an Actix `App`
    #[cfg(feature = "actix")]
    #[cfg_attr(docs, doc(cfg(feature = "actix")))]
    pub fn actix_service(&self) -> actix::Scope {
        actix::get_scope().data(self.clone())
    }

    /// Convenience method that runs an http server depending on the selected
    /// server feature
    ///
    /// This is a blocking call that tries to listen on the provided host and
    /// port
    pub async fn run(&self, host: impl AsRef<str>, port: impl Into<u16>) -> Result<()> {
        #[cfg(feature = "actix")]
        {
            actix::run_server(self.clone(), host, port).await?;
            Ok(())
        }

        #[cfg(not(any(feature = "actix",)))]
        unreachable!()
    }
}
