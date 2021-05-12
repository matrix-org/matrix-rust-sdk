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
//! The appservice crate aims to provide a batteries-included experience. That means that we
//! * ship with functionality to configure your webserver crate or simply run the webserver for you
//! * receive and validate requests from the homeserver correctly
//! * allow calling the homeserver with proper virtual user identity assertion
//! * have the goal to have a consistent room state available by leveraging the stores that the
//!   matrix-sdk provides
//!
//! # Quickstart
//!
//! ```no_run
//! # async {
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
//!     ")
//!     .unwrap();
//!
//! let appservice = Appservice::new(homeserver_url, server_name, registration).await.unwrap();
//! // set event handler with `appservice.client().set_event_handler()` here
//! let (host, port) = appservice.get_host_and_port_from_registration().unwrap();
//! appservice.run(host, port).await.unwrap();
//! # };
//! ```
//!
//! [Application Service]: https://matrix.org/docs/spec/application_service/r0.1.2

#[cfg(not(any(feature = "actix",)))]
compile_error!("one webserver feature must be enabled. available ones: `actix`");

use std::{
    convert::{TryFrom, TryInto},
    fs::File,
    ops::Deref,
    path::PathBuf,
};

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
    Client, ClientConfig, FromHttpResponseError, HttpError, RequestConfig, ServerError, Session,
};
use regex::Regex;
#[cfg(not(feature = "actix"))]
use tracing::error;
use tracing::warn;

#[cfg(feature = "actix")]
mod actix;
mod error;

pub use error::Error;
pub type Result<T> = std::result::Result<T, Error>;
pub type Host = String;
pub type Port = u16;

/// Appservice Registration
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

async fn create_client(
    homeserver_url: &Url,
    server_name: &ServerNameBox,
    registration: &AppserviceRegistration,
    localpart: Option<&str>,
) -> Result<Client> {
    let client = if localpart.is_some() {
        let request_config = RequestConfig::default().assert_identity();
        let config = ClientConfig::default().request_config(request_config);
        Client::new_with_config(homeserver_url.clone(), config)?
    } else {
        Client::new(homeserver_url.clone())?
    };

    let session = Session {
        access_token: registration.as_token.clone(),
        user_id: UserId::parse_with_server_name(
            localpart.unwrap_or(&registration.sender_localpart),
            &server_name,
        )?,
        device_id: DeviceId::new(),
    };
    client.restore_login(session).await?;

    Ok(client)
}

/// Appservice
#[derive(Debug, Clone)]
pub struct Appservice {
    homeserver_url: Url,
    server_name: ServerNameBox,
    registration: AppserviceRegistration,
    client_sender_localpart: Client,
}

impl Appservice {
    /// Create new Appservice
    ///
    /// # Arguments
    ///
    /// * `homeserver_url` - The homeserver that the client should connect to.
    /// * `server_name` - The server name to use when constructing user ids from the localpart.
    /// * `registration` - The [Appservice Registration] to use when interacting with the homserver.
    ///
    /// [Appservice Registration]: https://matrix.org/docs/spec/application_service/r0.1.2#registration
    pub async fn new(
        homeserver_url: impl TryInto<Url, Error = url::ParseError>,
        server_name: impl TryInto<ServerNameBox, Error = identifiers::Error>,
        registration: AppserviceRegistration,
    ) -> Result<Self> {
        let homeserver_url = homeserver_url.try_into()?;
        let server_name = server_name.try_into()?;

        let client = create_client(&homeserver_url, &server_name, &registration, None).await?;

        Ok(Appservice {
            homeserver_url,
            server_name,
            registration,
            client_sender_localpart: client,
        })
    }

    /// Get `Client` for the user associated with the application service
    /// (`sender_localpart` of the [registration])
    ///
    /// [registration]: https://matrix.org/docs/spec/application_service/r0.1.2#registration
    pub fn client(&self) -> Client {
        self.client_sender_localpart.clone()
    }

    /// Get `Client` for the given `localpart`
    ///
    /// If the `localpart` is covered by the `namespaces` in the [registration] all requests to the
    /// homeserver will [assert the identity] to the according virtual user.
    ///
    /// [registration]: https://matrix.org/docs/spec/application_service/r0.1.2#registration
    /// [assert the identity]:
    /// https://matrix.org/docs/spec/application_service/r0.1.2#identity-assertion
    pub async fn client_with_localpart(
        &self,
        localpart: impl AsRef<str> + Into<Box<str>>,
    ) -> Result<Client> {
        let user_id = UserId::parse_with_server_name(localpart, &self.server_name)?;
        let localpart = user_id.localpart().to_owned();

        let client = create_client(
            &self.homeserver_url,
            &self.server_name,
            &self.registration,
            Some(&localpart),
        )
        .await?;

        self.ensure_registered(localpart).await?;

        Ok(client)
    }

    async fn ensure_registered(&self, localpart: impl AsRef<str>) -> Result<()> {
        let request = assign!(RegistrationRequest::new(), {
            username: Some(localpart.as_ref()),
            login_type: Some(&LoginType::ApplicationService),
        });

        match self.client().register(request).await {
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
    pub fn registration(&self) -> &Registration {
        &self.registration
    }

    /// Compare the given `hs_token` against `registration.hs_token`
    ///
    /// Returns `true` if the tokens match, `false` otherwise.
    pub fn hs_token_matches(&self, hs_token: impl AsRef<str>) -> bool {
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

    /// Get the host and port from the registration URL
    ///
    /// If no port is found it falls back to scheme defaults: 80 for http and 443 for https
    pub fn get_host_and_port_from_registration(&self) -> Result<(Host, Port)> {
        let uri = Uri::try_from(&self.registration.url)?;

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

    /// Service to register on an Actix `App`
    #[cfg(feature = "actix")]
    #[cfg_attr(docs, doc(cfg(feature = "actix")))]
    pub fn actix_service(&self) -> actix::Scope {
        actix::get_scope().data(self.clone())
    }

    /// Convenience method that runs an http server depending on the selected server feature
    ///
    /// This is a blocking call that tries to listen on the provided host and port
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
