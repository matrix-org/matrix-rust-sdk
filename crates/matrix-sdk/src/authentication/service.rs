// Copyright 2024 The Matrix.org Foundation C.I.C.
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

//! High-level authentication APIs.
#[cfg(feature = "experimental-oidc")]
use std::collections::HashMap;
use std::{fmt::Debug, path::PathBuf, sync::RwLock};

#[cfg(feature = "experimental-oidc")]
use mas_oidc_client::types::{
    client_credentials::ClientCredentials,
    errors::ClientErrorCode::AccessDenied,
    registration::{ClientMetadata, VerifiedClientMetadata},
    requests::Prompt,
};
#[cfg(feature = "experimental-oidc")]
use ruma::api::client::discovery::discover_homeserver::AuthenticationServerInfo;
use ruma::api::client::session::get_login_types;
use url::Url;

#[cfg(feature = "experimental-oidc")]
use crate::oidc::{
    registrations::{ClientId, OidcRegistrations, OidcRegistrationsError},
    AuthorizationResponse, Oidc, OidcAuthorizationData, OidcError,
};
use crate::{sanitize_server_name, Client, ClientBuilder, IdParseError};

/// A high-level service for authenticating a user with a homeserver.
/// TODO: Add an example code snippet.
pub struct AuthenticationService {
    /// The base path to use for storing data. This could be the same path used
    /// for the client, or the client might be placed within a subdirectory of
    /// the base path.
    pub base_path: PathBuf,
    /// The user agent sent when making requests to the homeserver.
    pub user_agent: Option<String>,
    /// The client used to make requests to the homeserver.
    client: RwLock<Option<Client>>,
    /// Details about the currently configured homeserver.
    homeserver_details: RwLock<Option<HomeserverLoginDetails>>,
    /// Metadata about the current client that will be used for dynamic OIDC
    /// client registration and likely shown to the user during login.
    #[cfg(feature = "experimental-oidc")]
    oidc_client_metadata: Option<ClientMetadata>,
    /// Any static client registrations that have been made for this client
    /// against particular authentication issuers.
    #[cfg(feature = "experimental-oidc")]
    oidc_static_registrations: HashMap<Url, ClientId>,
    /// A sliding sync proxy URL that will override any proxy discovered from
    /// the homeserver. Setting this value allows you to use a proxy against a
    /// homeserver that hasn't yet been configured with one.
    #[cfg(feature = "experimental-sliding-sync")]
    pub custom_sliding_sync_proxy: RwLock<Option<String>>,
}

impl Debug for AuthenticationService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthenticationService")
            .field("homeserver_details", &self.homeserver_details)
            .finish()
    }
}

/// Errors related to authentication through the `AuthenticationService`.
#[derive(Debug, thiserror::Error)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Error), uniffi(flat_error))]
pub enum AuthenticationError {
    /// Developer error, the call made on the service has been performed before
    /// the service has been configured with a homeserver.
    #[error("A successful call to configure_homeserver must be made first.")]
    ClientMissing,
    /// The supplied string couldn't be parsed as either a server name or a
    /// homeserver URL.
    #[error("Failed to construct a server name or homeserver URL: {message}")]
    InvalidServerName {
        /// The underlying error message.
        message: String,
    },
    /// Sliding sync is required, but isn't configured in the homeserver's
    /// well-known file.
    #[error("The homeserver doesn't provide a trusted sliding sync proxy in its well-known configuration.")]
    SlidingSyncNotAvailable,
    /// An error occurred whilst trying to use the supplied base path.
    #[error("Failed to use the supplied base path.")]
    InvalidBasePath,
    /// Developer error, attempting to use OIDC when the homeserver doesn't
    /// support it.
    #[error(
        "The homeserver doesn't provide an authentication issuer in its well-known configuration."
    )]
    OidcNotSupported,
    /// Developer error, attempting to use OIDC without supplying the relevant
    /// client metadata.
    #[error("Unable to use OIDC as no client metadata has been supplied.")]
    OidcMetadataMissing,
    /// Failed to validate the supplied OIDC metadata.
    #[error("Unable to use OIDC as the supplied client metadata is invalid.")]
    OidcMetadataInvalid,
    /// Failed to complete OIDC login due to an invalid callback URL.
    #[error("The supplied callback URL used to complete OIDC is invalid.")]
    OidcCallbackUrlInvalid,
    /// The OIDC login was cancelled by the user.
    #[error("The OIDC login was cancelled by the user.")]
    OidcCancelled,
    /// An unknown error occurred whilst trying to use OIDC.
    #[error("An error occurred with OIDC: {message}")]
    OidcError {
        /// The underlying error message.
        message: String,
    },
    /// An unknown error occurred.
    #[error("An error occurred: {message}")]
    Generic {
        /// The underlying error message.
        message: String,
    },
}

#[cfg(feature = "uniffi")]
impl From<anyhow::Error> for AuthenticationError {
    fn from(e: anyhow::Error) -> AuthenticationError {
        AuthenticationError::Generic { message: e.to_string() }
    }
}

impl From<IdParseError> for AuthenticationError {
    fn from(e: IdParseError) -> AuthenticationError {
        AuthenticationError::InvalidServerName { message: e.to_string() }
    }
}

#[cfg(feature = "experimental-oidc")]
impl From<OidcRegistrationsError> for AuthenticationError {
    fn from(e: OidcRegistrationsError) -> AuthenticationError {
        match e {
            OidcRegistrationsError::InvalidBasePath => AuthenticationError::InvalidBasePath,
            _ => AuthenticationError::OidcError { message: e.to_string() },
        }
    }
}

#[cfg(feature = "experimental-oidc")]
impl From<OidcError> for AuthenticationError {
    fn from(e: OidcError) -> AuthenticationError {
        AuthenticationError::OidcError { message: e.to_string() }
    }
}

/// Details about a homeserver's login capabilities.
#[derive(Clone, Debug)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Object))]
pub struct HomeserverLoginDetails {
    /// The URL of the homeserver.
    pub url: Url,
    /// Whether the homeserver supports login using OIDC as defined by MSC3861.
    #[cfg(feature = "experimental-oidc")]
    pub supports_oidc_login: bool,
    /// Whether the homeserver supports the password login flow.
    pub supports_password_login: bool,
}

#[cfg(feature = "uniffi")]
#[uniffi::export]
impl HomeserverLoginDetails {
    /// The URL of the homeserver.
    pub fn url(&self) -> String {
        self.url.to_string()
    }

    /// Whether the homeserver supports login using OIDC as defined by MSC3861.
    #[cfg(feature = "experimental-oidc")]
    pub fn supports_oidc_login(&self) -> bool {
        self.supports_oidc_login
    }

    /// Whether the homeserver supports the password login flow.
    pub fn supports_password_login(&self) -> bool {
        self.supports_password_login
    }
}

impl AuthenticationService {
    /// Creates a new service to authenticate a user with.
    pub fn new(
        base_path: PathBuf,
        user_agent: Option<String>,
        #[cfg(feature = "experimental-oidc")] oidc_client_metadata: Option<ClientMetadata>,
        #[cfg(feature = "experimental-oidc")] oidc_static_registrations: HashMap<Url, ClientId>,
        #[cfg(feature = "experimental-sliding-sync")] custom_sliding_sync_proxy: Option<String>,
    ) -> Self {
        AuthenticationService {
            base_path,
            user_agent,
            client: RwLock::new(None),
            homeserver_details: RwLock::new(None),
            #[cfg(feature = "experimental-oidc")]
            oidc_client_metadata,
            #[cfg(feature = "experimental-oidc")]
            oidc_static_registrations,
            #[cfg(feature = "experimental-sliding-sync")]
            custom_sliding_sync_proxy: RwLock::new(custom_sliding_sync_proxy),
        }
    }

    /// Returns the homeserver details for the currently configured homeserver,
    /// or `None` if a successful call to `configure_homeserver` is yet to be
    /// made.
    pub fn homeserver_details(&self) -> Option<HomeserverLoginDetails> {
        self.homeserver_details.read().unwrap().clone()
    }

    /// Updates the service to authenticate with the homeserver for the
    /// specified address.
    pub async fn configure_homeserver(
        &self,
        server_name_or_homeserver_url: String,
    ) -> Result<(), AuthenticationError> {
        let mut builder = self.new_client_builder();

        // Attempt discovery as a server name first.
        let result = sanitize_server_name(&server_name_or_homeserver_url);

        match result {
            Ok(server_name) => {
                if server_name_or_homeserver_url.starts_with("http://") {
                    builder = builder.insecure_server_name_no_tls(&server_name);
                } else {
                    builder = builder.server_name(&server_name);
                }
            }

            Err(e) => {
                // When the input isn't a valid server name check it is a URL.
                // If this is the case, build the client with a homeserver URL.
                if Url::parse(&server_name_or_homeserver_url).is_ok() {
                    builder = builder.homeserver_url(server_name_or_homeserver_url.clone());
                } else {
                    return Err(e.into());
                }
            }
        }

        let client = match builder.build().await {
            Ok(client) => Ok(client),
            Err(e) => {
                if !server_name_or_homeserver_url.starts_with("http://")
                    && !server_name_or_homeserver_url.starts_with("https://")
                {
                    Err(e)
                } else {
                    // When discovery fails, fallback to the homeserver URL if supplied.
                    let mut builder = self.new_client_builder();
                    builder = builder.homeserver_url(server_name_or_homeserver_url);
                    builder.build().await
                }
            }
        }
        .map_err(|e| AuthenticationError::Generic { message: e.to_string() })?;

        let details = self.details_from_client(&client).await?;

        // Now we've verified that it's a valid homeserver, make sure
        // there's a sliding sync proxy available one way or another.
        #[cfg(feature = "experimental-sliding-sync")]
        if self.custom_sliding_sync_proxy.read().unwrap().is_none()
            && client.sliding_sync_proxy().is_none()
        {
            return Err(AuthenticationError::SlidingSyncNotAvailable);
        }

        *self.client.write().unwrap() = Some(client);
        *self.homeserver_details.write().unwrap() = Some(details);

        Ok(())
    }

    /// Performs a password login using the current homeserver.
    pub async fn login(
        &self,
        username: String,
        password: String,
        initial_device_name: Option<String>,
        device_id: Option<String>,
    ) -> Result<Client, AuthenticationError> {
        let Some(client) = self.client.read().unwrap().clone() else {
            return Err(AuthenticationError::ClientMissing);
        };

        // Login and ask the server for the full user ID as this could be different from
        // the username that was entered.
        client
            .login(username, password, initial_device_name, device_id)
            .await
            .map_err(|e| AuthenticationError::Generic { message: e.to_string() })?;

        Ok(client)
    }

    /// Requests the URL needed for login in a web view using OIDC. Once the web
    /// view has succeeded, call `login_with_oidc_callback` with the callback it
    /// returns.
    #[cfg(feature = "experimental-oidc")]
    pub async fn url_for_oidc_login(&self) -> Result<OidcAuthorizationData, AuthenticationError> {
        let Some(client) = self.client.read().unwrap().clone() else {
            return Err(AuthenticationError::ClientMissing);
        };
        let oidc = client.oidc();

        let Some(authentication_server) = oidc.authentication_server_info() else {
            return Err(AuthenticationError::OidcNotSupported);
        };

        let Some(oidc_client_metadata) = &self.oidc_client_metadata else {
            return Err(AuthenticationError::OidcMetadataMissing);
        };

        let Some(redirect_uris) = &oidc_client_metadata.redirect_uris else {
            return Err(AuthenticationError::OidcMetadataInvalid);
        };
        let Some(redirect_url) = redirect_uris.first() else {
            return Err(AuthenticationError::OidcMetadataInvalid);
        };

        self.configure_oidc(&oidc, authentication_server, oidc_client_metadata.clone()).await?;

        let mut data_builder = oidc.login(redirect_url.clone(), None)?;
        // TODO: Add a check for the Consent prompt when MAS is updated.
        data_builder = data_builder.prompt(vec![Prompt::Consent]);
        let data = data_builder.build().await?;

        Ok(data)
    }

    /// Completes the OIDC login process.
    #[cfg(feature = "experimental-oidc")]
    pub async fn login_with_oidc_callback(
        &self,
        authentication_data: OidcAuthorizationData,
        callback_url: Url,
    ) -> Result<Client, AuthenticationError> {
        let Some(client) = self.client.read().unwrap().clone() else {
            return Err(AuthenticationError::ClientMissing);
        };

        let oidc = client.oidc();

        let response = AuthorizationResponse::parse_uri(&callback_url)
            .map_err(|_| AuthenticationError::OidcCallbackUrlInvalid)?;

        let code = match response {
            AuthorizationResponse::Success(code) => code,
            AuthorizationResponse::Error(err) => {
                if err.error.error == AccessDenied {
                    // The user cancelled the login in the web view.
                    return Err(AuthenticationError::OidcCancelled);
                }
                return Err(AuthenticationError::OidcError {
                    message: err.error.error.to_string(),
                });
            }
        };

        if code.state != authentication_data.state {
            return Err(AuthenticationError::OidcCallbackUrlInvalid);
        };

        oidc.finish_authorization(code).await?;

        oidc.finish_login()
            .await
            .map_err(|e| AuthenticationError::OidcError { message: e.to_string() })?;

        Ok(client)
    }
}

// TODO: Is it normal to split up the implementation into public and private
// like this?
impl AuthenticationService {
    /// A new client builder pre-configured with a user agent if specified
    fn new_client_builder(&self) -> ClientBuilder {
        let mut builder = ClientBuilder::new();

        if let Some(user_agent) = self.user_agent.clone() {
            builder = builder.user_agent(user_agent);
        }

        builder
    }

    /// Get the homeserver login details from a client.
    async fn details_from_client(
        &self,
        client: &Client,
    ) -> Result<HomeserverLoginDetails, AuthenticationError> {
        #[cfg(feature = "experimental-oidc")]
        let supports_oidc_login = client.oidc().authentication_server_info().is_some();
        let supports_password_login = client.supports_password_login().await.ok().unwrap_or(false);
        let url = client.homeserver();

        Ok(HomeserverLoginDetails {
            url,
            #[cfg(feature = "experimental-oidc")]
            supports_oidc_login,
            supports_password_login,
        })
    }

    /// Handle any necessary configuration in order for login via OIDC to
    /// succeed. This includes performing dynamic client registration against
    /// the homeserver's issuer or restoring a previous registration if one has
    /// been stored.
    #[cfg(feature = "experimental-oidc")]
    async fn configure_oidc(
        &self,
        oidc: &Oidc,
        authentication_server: &AuthenticationServerInfo,
        oidc_metadata: ClientMetadata,
    ) -> Result<(), AuthenticationError> {
        if oidc.client_credentials().is_some() {
            tracing::info!("OIDC is already configured.");
            return Ok(());
        };

        let oidc_metadata =
            oidc_metadata.validate().map_err(|_| AuthenticationError::OidcMetadataInvalid)?;

        if self.load_client_registration(oidc, authentication_server, oidc_metadata.clone()).await {
            tracing::info!("OIDC configuration loaded from disk.");
            return Ok(());
        }

        tracing::info!("Registering this client for OIDC.");
        let registration_response = oidc
            .register_client(&authentication_server.issuer, oidc_metadata.clone(), None)
            .await?;

        // The format of the credentials changes according to the client metadata that
        // was sent. Public clients only get a client ID.
        let credentials =
            ClientCredentials::None { client_id: registration_response.client_id.clone() };
        oidc.restore_registered_client(authentication_server.clone(), oidc_metadata, credentials);

        tracing::info!("Persisting OIDC registration data.");
        self.store_client_registration(oidc).await?;

        Ok(())
    }

    /// Stores the current OIDC dynamic client registration so it can be re-used
    /// if we ever log in via the same issuer again.
    #[cfg(feature = "experimental-oidc")]
    async fn store_client_registration(&self, oidc: &Oidc) -> Result<(), AuthenticationError> {
        let issuer = Url::parse(oidc.issuer().ok_or(AuthenticationError::OidcNotSupported)?)
            .map_err(|_| AuthenticationError::OidcError {
                message: String::from("Failed to parse issuer URL."),
            })?;
        let client_id = oidc
            .client_credentials()
            .ok_or(AuthenticationError::OidcError {
                message: String::from("Missing client registration."),
            })?
            .client_id()
            .to_owned();

        let metadata = oidc.client_metadata().ok_or(AuthenticationError::OidcError {
            message: String::from("Missing client metadata."),
        })?;

        let registrations = OidcRegistrations::new(
            &self.base_path,
            metadata.clone(),
            self.oidc_static_registrations.clone(),
        )?;
        registrations.set_and_write_client_id(ClientId(client_id), issuer)?;

        Ok(())
    }

    /// Attempts to load an existing OIDC dynamic client registration for the
    /// currently configured issuer.
    #[cfg(feature = "experimental-oidc")]
    async fn load_client_registration(
        &self,
        oidc: &Oidc,
        authentication_server: &AuthenticationServerInfo,
        oidc_metadata: VerifiedClientMetadata,
    ) -> bool {
        let Ok(issuer) = Url::parse(&authentication_server.issuer) else {
            tracing::error!("Failed to parse {:?}", authentication_server.issuer);
            return false;
        };
        let Some(registrations) = OidcRegistrations::new(
            &self.base_path,
            oidc_metadata.clone(),
            self.oidc_static_registrations.clone(),
        )
        .ok() else {
            return false;
        };
        let Some(client_id) = registrations.client_id(&issuer) else {
            return false;
        };

        oidc.restore_registered_client(
            authentication_server.clone(),
            oidc_metadata,
            ClientCredentials::None { client_id: client_id.0 },
        );

        true
    }
}

trait ClientExt {
    /// Whether or not the client's homeserver supports the password login flow.
    async fn supports_password_login(&self) -> crate::Result<bool>;
    /// Login using a username and password.
    async fn login(
        &self,
        username: String,
        password: String,
        initial_device_name: Option<String>,
        device_id: Option<String>,
    ) -> crate::Result<()>;
}

impl ClientExt for Client {
    async fn supports_password_login(&self) -> crate::Result<bool> {
        let login_types = self.matrix_auth().get_login_types().await?;
        let supports_password = login_types
            .flows
            .iter()
            .any(|login_type| matches!(login_type, get_login_types::v3::LoginType::Password(_)));
        Ok(supports_password)
    }

    async fn login(
        &self,
        username: String,
        password: String,
        initial_device_name: Option<String>,
        device_id: Option<String>,
    ) -> crate::Result<()> {
        let mut builder = self.matrix_auth().login_username(&username, &password);
        if let Some(initial_device_name) = initial_device_name.as_ref() {
            builder = builder.initial_device_display_name(initial_device_name);
        }
        if let Some(device_id) = device_id.as_ref() {
            builder = builder.device_id(device_id);
        }
        builder.send().await?;
        Ok(())
    }
}
