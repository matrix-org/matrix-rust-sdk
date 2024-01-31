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
use std::{
    fmt::Debug,
    path::PathBuf,
    sync::{Arc, RwLock},
};

#[cfg(feature = "experimental-oidc")]
use mas_oidc_client::types::{
    client_credentials::ClientCredentials,
    errors::ClientErrorCode::AccessDenied,
    registration::{ClientMetadata, VerifiedClientMetadata},
    requests::Prompt,
};
#[cfg(feature = "experimental-oidc")]
use ruma::api::client::discovery::discover_homeserver::AuthenticationServerInfo;
use ruma::api::{client::session::get_login_types, error::FromHttpResponseError};
use url::Url;

#[cfg(test)]
#[cfg(feature = "experimental-oidc")]
use crate::oidc::backend::mock::MockImpl as OidcBackend;
#[cfg(feature = "experimental-oidc")]
use crate::oidc::{
    registrations::{ClientId, OidcRegistrations, OidcRegistrationsError},
    AuthorizationResponse, Oidc, OidcAuthorizationData, OidcError,
};
use crate::{
    sanitize_server_name, Client, ClientBuildError, ClientBuilder, HttpError, IdParseError,
    ServerName,
};

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
    /// An injected OIDC backend for testing purposes.
    #[cfg(test)]
    #[cfg(feature = "experimental-oidc")]
    oidc_backend: Option<Arc<OidcBackend>>,
}

impl Debug for AuthenticationService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthenticationService")
            .field("homeserver_details", &self.homeserver_details)
            .finish()
    }
}

/// Errors related to authentication through the `AuthenticationService`.
#[derive(Debug, PartialEq, thiserror::Error)]
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
    /// There is no server responding at the supplied URL.
    #[error("Failed to discover a server")]
    ServerNotFound,
    /// There is a server at the supplied URL, but it is neither a homeserver
    /// nor is it hosting a well-known file for one.
    #[error("Failed to discover a homeserver")]
    HomeserverNotFound,
    /// The discovered well-known file is invalid.
    #[error("Failed to parse the well-known file: {message}")]
    InvalidWellKnownFile {
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
            #[cfg(test)]
            #[cfg(feature = "experimental-oidc")]
            oidc_backend: None,
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
        let client = self.build_client(server_name_or_homeserver_url).await?;
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

        let mut builder = client.matrix_auth().login_username(&username, &password);
        if let Some(initial_device_name) = initial_device_name.as_ref() {
            builder = builder.initial_device_display_name(initial_device_name);
        }
        if let Some(device_id) = device_id.as_ref() {
            builder = builder.device_id(device_id);
        }
        builder
            .send()
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
        let oidc = self.oidc_from_client(&client);

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

        let oidc = self.oidc_from_client(&client);

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
    /// Builds a client for the given server name or homeserver URL.
    async fn build_client(
        &self,
        server_name_or_homeserver_url: String,
    ) -> Result<Client, AuthenticationError> {
        let mut build_error: AuthenticationError =
            AuthenticationError::Generic { message: "Unknown error occurred.".to_owned() };

        // Attempt discovery as a server name first.
        let sanitize_result = sanitize_server_name(&server_name_or_homeserver_url);
        if let Ok(server_name) = sanitize_result.as_ref() {
            let insecure = server_name_or_homeserver_url.starts_with("http://");
            match self.build_client_for_server_name(server_name, insecure).await {
                Ok(client) => return Ok(client),
                Err(e) => {
                    build_error = match e {
                        ClientBuildError::Http(HttpError::Reqwest(_)) => {
                            AuthenticationError::ServerNotFound
                        }
                        ClientBuildError::AutoDiscovery(
                            FromHttpResponseError::Deserialization(e),
                        ) => AuthenticationError::InvalidWellKnownFile { message: e.to_string() },
                        ClientBuildError::AutoDiscovery(_) => {
                            AuthenticationError::HomeserverNotFound
                        }
                        _ => AuthenticationError::Generic { message: e.to_string() },
                    }
                }
            };
        }

        // When discovery fails, or the input isn't a valid server name, fallback to
        // trying a homeserver URL if supplied.
        if let Ok(homeserver_url) = Url::parse(&server_name_or_homeserver_url) {
            if let Some(client) = self.build_client_for_homeserver_url(homeserver_url).await {
                return Ok(client);
            }
            // No need to worry about the error branch here as the server name
            // is preferred (to get a well-known file), so we'll return the
            // error from above instead.
        };

        if let Err(sanitize_result) = sanitize_result {
            return Err(sanitize_result.into());
        } else {
            return Err(build_error);
        }
    }

    /// A new client builder pre-configured with a user agent if specified
    fn new_client_builder(&self) -> ClientBuilder {
        let mut builder = ClientBuilder::new();

        if let Some(user_agent) = self.user_agent.clone() {
            builder = builder.user_agent(user_agent);
        }

        builder
    }

    /// Builds a client for the given server name.
    async fn build_client_for_server_name(
        &self,
        server_name: &ServerName,
        insecure: bool,
    ) -> Result<Client, ClientBuildError> {
        let mut builder = self.new_client_builder();

        if insecure {
            builder = builder.insecure_server_name_no_tls(server_name);
        } else {
            builder = builder.server_name(server_name);
        }

        builder.build().await
    }

    /// Builds a client for the given homeserver URL, validating that it is
    /// actually a homeserver. Returns an `Option` as building with a server
    /// name is preferred, so we'll return the error from building with that
    /// if this fails.
    async fn build_client_for_homeserver_url(&self, homeserver_url: Url) -> Option<Client> {
        let mut builder = self.new_client_builder();
        builder = builder.homeserver_url(homeserver_url);

        let client = builder.build().await.ok()?;

        // Building should always succeed, so we need to check that a homeserver
        // actually exists at the supplied URL.
        match client.server_versions().await {
            Ok(_) => Some(client),
            Err(_) => None,
        }
    }

    /// Get the homeserver login details from a client.
    async fn details_from_client(
        &self,
        client: &Client,
    ) -> Result<HomeserverLoginDetails, AuthenticationError> {
        #[cfg(feature = "experimental-oidc")]
        let supports_oidc_login =
            self.oidc_from_client(client).authentication_server_info().is_some();
        let login_types = client
            .matrix_auth()
            .get_login_types()
            .await
            .map_err(|e| AuthenticationError::Generic { message: e.to_string() })?;
        let supports_password_login = login_types
            .flows
            .iter()
            .any(|login_type| matches!(login_type, get_login_types::v3::LoginType::Password(_)));
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

    /// A helper method for tests that uses the injected OIDC client if present,
    /// otherwise simply calls `oidc` on the client.
    #[cfg(feature = "experimental-oidc")]
    fn oidc_from_client(&self, client: &Client) -> Oidc {
        #[cfg(test)]
        if let Some(backend) = self.oidc_backend.as_ref().map(Arc::clone) {
            return Oidc { client: client.clone(), backend };
        }

        client.oidc()
    }
}

// The http mocking library is not supported for wasm32
#[cfg(all(test, not(target_arch = "wasm32")))]
pub(crate) mod tests {
    use std::{
        path::Path,
        sync::{Arc, RwLock},
    };

    use matrix_sdk_test::{async_test, test_json};
    use serde_json::{json, json_internal, Value as JsonValue};
    use wiremock::{
        matchers::{body_string_contains, header, method, path},
        Mock, MockServer, ResponseTemplate,
    };

    use super::*;
    use crate::oidc::OidcSessionTokens;

    // Note: Due to a limitation of the http mocking library these tests all supply an http:// url,
    // rather than the plain server name, otherwise the service will prepend https:// to the name
    // and the request will fail. In practice, this isn't a problem as the service
    // first strips the scheme and then checks if the name is a valid server
    // name, so it is a close enough approximation.

    #[async_test]
    async fn test_configure_invalid_server() {
        // Given a new service.
        let service = make_service(None);

        // When configuring the authentication service with an invalid server name.
        let error =
            service.configure_homeserver("⚠️ This won't work 🚫".to_owned()).await.unwrap_err();

        // Then the configuration should fail due to the invalid server name.
        assert!(matches!(error, AuthenticationError::InvalidServerName { .. }));
        assert!(service.homeserver_details().is_none());
    }

    #[async_test]
    async fn test_configure_no_server() {
        // Given a new service.
        let service = make_service(None);

        // When configuring the authentication service with a valid server name that
        // doesn't exist.
        let error = service.configure_homeserver("localhost:3456".to_owned()).await.unwrap_err();

        // Then the configuration should fail with no server response.
        assert_eq!(error, AuthenticationError::ServerNotFound);
        assert!(service.homeserver_details().is_none());
    }

    #[async_test]
    async fn test_configure_web_server() {
        // Given a random web server that isn't a Matrix homeserver or hosting the
        // well-known file for one.
        let server = MockServer::start().await;
        let service = make_service(None);

        // When configuring the authentication service with the server's URL.
        let error = service.configure_homeserver(server.uri()).await.unwrap_err();

        // Then the configuration should fail because a homeserver couldn't be found.
        assert_eq!(error, AuthenticationError::HomeserverNotFound);
        assert!(service.homeserver_details().is_none());
    }

    #[async_test]
    async fn test_configure_direct_legacy() {
        // Given a homeserver without a well-known file.
        let homeserver = make_mock_homeserver().await;
        let service = make_service(None);

        // When configuring the authentication service with the server's URL.
        let error = service.configure_homeserver(homeserver.uri()).await.unwrap_err();

        // Then the configuration should fail due to sliding sync not being discovered.
        assert_eq!(error, AuthenticationError::SlidingSyncNotAvailable);
        assert!(service.homeserver_details().is_none());
    }

    #[async_test]
    async fn test_configure_direct_legacy_custom_proxy() {
        // Given a homeserver without a well-known file and a service with a custom
        // sliding sync proxy injected.
        let homeserver = make_mock_homeserver().await;
        let service = make_service(Some("https://localhost:1234".to_owned()));

        // When configuring the authentication service with the server's URL.
        service.configure_homeserver(homeserver.uri()).await.unwrap();

        // Then the configuration should succeed and password authentication should be
        // available.
        let details = service.homeserver_details().unwrap();
        assert_eq!(details.url, homeserver.uri().parse().unwrap());
        assert_eq!(details.supports_password_login, true);
        #[cfg(feature = "experimental-oidc")]
        assert_eq!(details.supports_oidc_login, false);
    }

    #[async_test]
    async fn test_configure_well_known_parse_error() {
        // Given a base server with a well-known file that has errors.
        let server = MockServer::start().await;
        let homeserver = make_mock_homeserver().await;
        let service = make_service(None);

        let well_known = make_well_known_json(&homeserver.uri(), None, None);
        let bad_json = well_known.to_string().replace(",", "");
        Mock::given(method("GET"))
            .and(path("/.well-known/matrix/client"))
            .respond_with(ResponseTemplate::new(200).set_body_json(bad_json))
            .mount(&server)
            .await;

        // When configuring the authentication service with the base server.
        let error = service.configure_homeserver(server.uri()).await.unwrap_err();

        // Then the configuration should fail due to an invalid well-known file.
        assert!(matches!(error, AuthenticationError::InvalidWellKnownFile { .. }));
        assert!(service.homeserver_details().is_none());
    }

    #[async_test]
    async fn test_configure_well_known_legacy() {
        // Given a base server with a well-known file that points to a homeserver that
        // doesn't support sliding sync.
        let server = MockServer::start().await;
        let homeserver = make_mock_homeserver().await;
        let service = make_service(None);

        Mock::given(method("GET"))
            .and(path("/.well-known/matrix/client"))
            .respond_with(ResponseTemplate::new(200).set_body_json(make_well_known_json(
                &homeserver.uri(),
                None,
                None,
            )))
            .mount(&server)
            .await;

        // When configuring the authentication service with the base server.
        let error = service.configure_homeserver(server.uri()).await.unwrap_err();

        // Then the configuration should fail due to sliding sync not being configured.
        assert_eq!(error, AuthenticationError::SlidingSyncNotAvailable);
        assert!(service.homeserver_details().is_none());
    }

    #[async_test]
    async fn test_configure_well_known_with_sliding_sync() {
        // Given a base server with a well-known file that points to a homeserver with a
        // sliding sync proxy.
        let server = MockServer::start().await;
        let homeserver = make_mock_homeserver().await;
        let service = make_service(None);

        Mock::given(method("GET"))
            .and(path("/.well-known/matrix/client"))
            .respond_with(ResponseTemplate::new(200).set_body_json(make_well_known_json(
                &homeserver.uri(),
                Some("https://localhost:1234"),
                None,
            )))
            .mount(&server)
            .await;

        // When configuring the authentication service with the base server.
        service.configure_homeserver(server.uri()).await.unwrap();

        // Then the configuration should succeed and password authentication should be
        // available.
        let details = service.homeserver_details().unwrap();
        assert_eq!(details.url, homeserver.uri().parse().unwrap());
        assert_eq!(details.supports_password_login, true);
        #[cfg(feature = "experimental-oidc")]
        assert_eq!(details.supports_oidc_login, false);
    }

    #[async_test]
    async fn test_configure_well_known_matrix2point0() {
        // Given a base server with a well-known file that points to a homeserver that
        // is Matrix 2.0 ready (OIDC & Sliding Sync).
        let server = MockServer::start().await;
        let homeserver = make_mock_homeserver().await;
        let service = make_service(None);

        Mock::given(method("GET"))
            .and(path("/.well-known/matrix/client"))
            .respond_with(ResponseTemplate::new(200).set_body_json(make_well_known_json(
                &homeserver.uri(),
                Some("https://localhost:1234"),
                Some("https://localhost:5678"),
            )))
            .mount(&server)
            .await;

        // When configuring the authentication service with the base server.
        service.configure_homeserver(server.uri()).await.unwrap();

        // Then the configuration should succeed and both password and OIDC
        // authentication should be available.
        let details = service.homeserver_details().unwrap();
        assert_eq!(details.url, homeserver.uri().parse().unwrap());
        assert_eq!(details.supports_password_login, true);
        #[cfg(feature = "experimental-oidc")]
        assert_eq!(details.supports_oidc_login, true);
    }

    #[async_test]
    async fn test_login() {
        // Given a service configured with a homeserver that supports password login.
        let homeserver = make_mock_homeserver().await;
        let service = make_service(Some("https://localhost:1234".to_owned()));
        service.configure_homeserver(homeserver.uri()).await.unwrap();
        assert_eq!(service.homeserver_details().unwrap().supports_password_login, true);

        // When logging in with a username and password.
        let client =
            service.login("example".to_owned(), "wordpass".to_owned(), None, None).await.unwrap();

        // Then a client should be created that is logged in and ready to use.
        assert!(client.logged_in());
    }

    #[async_test]
    async fn test_login_wrong_password() {
        // Given a service configured with a homeserver that supports password login.
        let homeserver = make_mock_homeserver().await;
        let service = make_service(Some("https://localhost:1234".to_owned()));
        service.configure_homeserver(homeserver.uri()).await.unwrap();
        assert_eq!(service.homeserver_details().unwrap().supports_password_login, true);

        // When logging in with the wrong password.
        let error = service
            .login("example".to_owned(), "badpass".to_owned(), None, None)
            .await
            .unwrap_err();

        // Then the login should fail and not return a client.
        assert!(matches!(error, AuthenticationError::Generic { .. }));
    }

    #[async_test]
    #[cfg(feature = "experimental-oidc")]
    async fn test_oidc_login() {
        // Given a service configured with a homeserver that's ready for Matrix 2.0.
        let server = MockServer::start().await;
        let homeserver = make_mock_homeserver().await;
        let oidc_backend = make_oidc_backend();
        let service = make_service_oidc(None, Some(Arc::new(oidc_backend)));
        mock_client_registrations(service.oidc_client_metadata.clone().unwrap());

        Mock::given(method("GET"))
            .and(path("/.well-known/matrix/client"))
            .respond_with(ResponseTemplate::new(200).set_body_json(make_well_known_json(
                &homeserver.uri(),
                Some("https://localhost:1234"),
                Some(crate::oidc::backend::mock::ISSUER_URL),
            )))
            .mount(&server)
            .await;

        service.configure_homeserver(server.uri()).await.unwrap();
        assert_eq!(service.homeserver_details().unwrap().supports_oidc_login, true);

        // When logging in with OIDC.
        let data = service.url_for_oidc_login().await.unwrap();
        let callback = format!("https://example.com/login?state={}&code=1337", data.state);
        let client =
            service.login_with_oidc_callback(data, Url::parse(&callback).unwrap()).await.unwrap();

        // Then a client should be created that is logged in and ready to use.
        assert!(client.logged_in());
    }

    #[async_test]
    #[cfg(feature = "experimental-oidc")]
    async fn test_oidc_login_cancellation() {
        // Given a service configured with a homeserver that's ready for Matrix 2.0.
        let server = MockServer::start().await;
        let homeserver = make_mock_homeserver().await;
        let oidc_backend = make_oidc_backend();
        let service = make_service_oidc(None, Some(Arc::new(oidc_backend)));
        mock_client_registrations(service.oidc_client_metadata.clone().unwrap());

        Mock::given(method("GET"))
            .and(path("/.well-known/matrix/client"))
            .respond_with(ResponseTemplate::new(200).set_body_json(make_well_known_json(
                &homeserver.uri(),
                Some("https://localhost:1234"),
                Some(crate::oidc::backend::mock::ISSUER_URL),
            )))
            .mount(&server)
            .await;

        service.configure_homeserver(server.uri()).await.unwrap();
        assert_eq!(service.homeserver_details().unwrap().supports_oidc_login, true);

        // When cancelling a login request from the login page.
        let data = service.url_for_oidc_login().await.unwrap();
        let callback =
            format!("https://example.com/login?state={}&error=access_denied", data.state);
        let error = service
            .login_with_oidc_callback(data, Url::parse(&callback).unwrap())
            .await
            .unwrap_err();

        // Then the login should fail with a cancellation error.
        assert_eq!(error, AuthenticationError::OidcCancelled);
    }

    #[async_test]
    #[cfg(feature = "experimental-oidc")]
    async fn test_oidc_login_old_data() {
        // Given a service configured with a homeserver that's ready for Matrix 2.0 and
        // has provided some authorization data.
        let server = MockServer::start().await;
        let homeserver = make_mock_homeserver().await;
        let oidc_backend = make_oidc_backend();
        let service = make_service_oidc(None, Some(Arc::new(oidc_backend)));
        mock_client_registrations(service.oidc_client_metadata.clone().unwrap());

        Mock::given(method("GET"))
            .and(path("/.well-known/matrix/client"))
            .respond_with(ResponseTemplate::new(200).set_body_json(make_well_known_json(
                &homeserver.uri(),
                Some("https://localhost:1234"),
                Some(crate::oidc::backend::mock::ISSUER_URL),
            )))
            .mount(&server)
            .await;

        service.configure_homeserver(server.uri()).await.unwrap();
        assert_eq!(service.homeserver_details().unwrap().supports_oidc_login, true);

        let data = service.url_for_oidc_login().await.unwrap();

        // When attempting to log in using the URL from old OIDC authorization data.
        let old_data =
            OidcAuthorizationData { state: "old_state".to_string(), url: data.url.clone() };
        let callback = format!("https://example.com/login?state={}&code=1337", old_data.state);
        let error = service
            .login_with_oidc_callback(data, Url::parse(&callback).unwrap())
            .await
            .unwrap_err();

        // Then the login should fail with an invalid callback.
        assert_eq!(error, AuthenticationError::OidcCallbackUrlInvalid);
    }

    /* Helper functions */

    fn make_service(custom_sliding_sync_proxy: Option<String>) -> AuthenticationService {
        #[cfg(not(feature = "experimental-oidc"))]
        return make_service_oidc(custom_sliding_sync_proxy);
        #[cfg(feature = "experimental-oidc")]
        make_service_oidc(custom_sliding_sync_proxy, None)
    }

    fn make_service_oidc(
        custom_sliding_sync_proxy: Option<String>,
        #[cfg(feature = "experimental-oidc")] oidc_backend: Option<Arc<OidcBackend>>,
    ) -> AuthenticationService {
        #[cfg(feature = "experimental-oidc")]
        let oidc_client_metadata = match &oidc_backend {
            Some(_) => Some(ClientMetadata {
                redirect_uris: Some(vec!["https://example.com/login".parse().unwrap()]),
                ..Default::default()
            }),
            None => None,
        };
        AuthenticationService {
            base_path: "/tmp/matrix".into(),
            user_agent: None,
            client: Default::default(),
            homeserver_details: Default::default(),
            #[cfg(feature = "experimental-oidc")]
            oidc_client_metadata,
            #[cfg(feature = "experimental-oidc")]
            oidc_static_registrations: Default::default(),
            #[cfg(feature = "experimental-sliding-sync")]
            custom_sliding_sync_proxy: RwLock::new(custom_sliding_sync_proxy),
            #[cfg(feature = "experimental-oidc")]
            oidc_backend,
        }
    }

    async fn make_mock_homeserver() -> MockServer {
        let homeserver = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/_matrix/client/versions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(&*test_json::VERSIONS))
            .mount(&homeserver)
            .await;
        Mock::given(method("GET"))
            .and(path("/_matrix/client/r0/login"))
            .respond_with(ResponseTemplate::new(200).set_body_json(&*test_json::LOGIN_TYPES))
            .mount(&homeserver)
            .await;
        Mock::given(method("POST"))
            .and(path("/_matrix/client/r0/login"))
            .and(body_string_contains("password\":\"wordpass\""))
            .respond_with(ResponseTemplate::new(200).set_body_json(&*test_json::LOGIN))
            .mount(&homeserver)
            .await;
        Mock::given(method("POST"))
            .and(path("/_matrix/client/r0/login"))
            .and(body_string_contains("password\":\"badpass\""))
            .respond_with(ResponseTemplate::new(403).set_body_json(&*test_json::LOGIN_RESPONSE_ERR))
            .mount(&homeserver)
            .await;
        Mock::given(method("GET"))
            .and(path("/_matrix/client/r0/account/whoami"))
            .and(header("authorization", "Bearer 4cc3ss"))
            .respond_with(ResponseTemplate::new(200).set_body_json(make_whoami_json()))
            .mount(&homeserver)
            .await;
        homeserver
    }

    fn make_oidc_backend() -> OidcBackend {
        let backend = OidcBackend::new();
        let next_tokens = OidcSessionTokens {
            access_token: "4cc3ss".to_owned(),
            refresh_token: Some("r3fr3sh".to_owned()),
            latest_id_token: None,
        };
        backend.next_session_tokens(next_tokens)
    }

    /// Pre-fills the OIDC registrations file as the mock OIDC backend doesn't
    /// support dynamic client registration.
    fn mock_client_registrations(metadata: ClientMetadata) {
        let client_id = "BestMatrixClient".to_owned();
        let registrations = OidcRegistrations::new(
            Path::new("/tmp/matrix"),
            metadata.validate().unwrap(),
            Default::default(),
        )
        .unwrap();

        registrations
            .set_and_write_client_id(
                ClientId(client_id),
                Url::parse(crate::oidc::backend::mock::ISSUER_URL).unwrap(),
            )
            .unwrap();
    }

    fn make_well_known_json(
        homeserver_url: &str,
        sliding_sync_proxy_url: Option<&str>,
        authentication_issuer: Option<&str>,
    ) -> JsonValue {
        ::serde_json::Value::Object({
            let mut object = ::serde_json::Map::new();
            let _ = object.insert(
                "m.homeserver".into(),
                json_internal!({
                    "base_url": homeserver_url
                }),
            );

            if let Some(sliding_sync_proxy_url) = sliding_sync_proxy_url {
                let _ = object.insert(
                    "org.matrix.msc3575.proxy".into(),
                    json_internal!({
                        "url": sliding_sync_proxy_url
                    }),
                );
            }

            if let Some(authentication_issuer) = authentication_issuer {
                let _ = object.insert(
                    "org.matrix.msc2965.authentication".into(),
                    json_internal!({
                        "issuer": authentication_issuer,
                        "account": authentication_issuer.to_owned() + "/account"
                    }),
                );
            }

            object
        })
    }

    /// A custom whoami response as the one provided by `test_json` doesn't
    /// include a device ID.
    fn make_whoami_json() -> JsonValue {
        json!({
            "user_id": "@user:example.com",
            "device_id": "D3V1C31D"
        })
    }

    trait ServerExt {
        /// Convenience method to convert a string to a URL and returns it as a
        /// Localized URL. No localization is actually performed.
        fn server_name(&self) -> String;
    }

    impl ServerExt for MockServer {
        fn server_name(&self) -> String {
            self.uri().replace("http://", "")
        }
    }
}
