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

use std::{error::Error, fmt::Debug, sync::RwLock};

use ruma::{
    api::{
        client::session::get_login_types,
        error::{DeserializationError, FromHttpResponseError},
    },
    IdParseError,
};
use url::Url;

use crate::{sanitize_server_name, Client, ClientBuildError, ClientBuilder, HttpError, ServerName};

/// A high-level service for authenticating a user with a homeserver.
#[derive(Debug)]
pub struct AuthenticationService {
    /// The user agent sent when making requests to the homeserver.
    pub user_agent: Option<String>,
    /// The client used to make requests to the homeserver.
    client: RwLock<Option<Client>>,
    /// Details about the currently configured homeserver.
    homeserver_details: RwLock<Option<HomeserverLoginDetails>>,
    /// A sliding sync proxy URL that will override any proxy discovered from
    /// the homeserver. Setting this value allows you to use a proxy against a
    /// homeserver that hasn't yet been configured with one.
    #[cfg(feature = "experimental-sliding-sync")]
    pub custom_sliding_sync_proxy: RwLock<Option<String>>,
}

/// Errors related to authentication through the `AuthenticationService`.
#[derive(Debug, thiserror::Error)]
pub enum AuthenticationError {
    /// Developer error, the call made on the service has been performed before
    /// the service has been configured with a homeserver.
    #[error("A successful call to configure_homeserver must be made first.")]
    ClientMissing,
    /// The supplied string couldn't be parsed as either a server name or a
    /// homeserver URL.
    #[error("Failed to construct a server name or homeserver URL: {0}")]
    InvalidServerName(IdParseError),
    /// There is no server responding at the supplied URL.
    #[error("Failed to discover a server")]
    ServerNotFound,
    /// There is a server at the supplied URL, but it is neither a homeserver
    /// nor is it hosting a well-known file for one.
    #[error("Failed to discover a homeserver")]
    HomeserverNotFound,
    /// The discovered well-known file is invalid.
    #[error("Failed to parse the well-known file: {0}")]
    InvalidWellKnownFile(DeserializationError),
    /// Sliding sync is required, but isn't configured in the homeserver's
    /// well-known file.
    #[error("The homeserver doesn't provide a trusted sliding sync proxy in its well-known configuration.")]
    SlidingSyncNotAvailable,
    /// An error occurred whilst trying to use the supplied base path.
    #[error("Failed to use the supplied base path.")]
    InvalidBasePath,
    /// An unknown error occurred.
    #[error("An error occurred: {0}")]
    Generic(Box<dyn Error>),
}

/// Details about a homeserver's login capabilities.
#[derive(Clone, Debug)]
pub struct HomeserverLoginDetails {
    /// The URL of the homeserver.
    pub url: Url,
    /// Whether the homeserver supports login using OIDC as defined by MSC3861.
    #[cfg(feature = "experimental-oidc")]
    pub supports_oidc_login: bool,
    /// Whether the homeserver supports the password login flow.
    pub supports_password_login: bool,
}

impl AuthenticationService {
    /// Creates a new service to authenticate a user with.
    pub fn new(
        user_agent: Option<String>,
        #[cfg(feature = "experimental-sliding-sync")] custom_sliding_sync_proxy: Option<String>,
    ) -> Self {
        AuthenticationService {
            user_agent,
            client: RwLock::new(None),
            homeserver_details: RwLock::new(None),
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
}

impl AuthenticationService {
    /// Builds a client for the given server name or homeserver URL.
    async fn build_client(
        &self,
        server_name_or_homeserver_url: String,
    ) -> Result<Client, AuthenticationError> {
        let mut build_error: AuthenticationError =
            AuthenticationError::Generic("Unknown error occurred.".into());

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
                        ) => AuthenticationError::InvalidWellKnownFile(e),
                        ClientBuildError::AutoDiscovery(_) => {
                            AuthenticationError::HomeserverNotFound
                        }
                        _ => AuthenticationError::Generic(Box::new(e)),
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
            Err(AuthenticationError::InvalidServerName(sanitize_result))
        } else {
            Err(build_error)
        }
    }

    /// A new client builder pre-configured with a user agent if specified.
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
        let supports_oidc_login = client.oidc().authentication_server_info().is_some();
        let login_types = client
            .matrix_auth()
            .get_login_types()
            .await
            .map_err(|e| AuthenticationError::Generic(Box::new(e)))?;
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
}

// The http mocking library is not supported for wasm32
#[cfg(all(test, not(target_arch = "wasm32")))]
pub(crate) mod tests {
    use std::sync::RwLock;

    use matrix_sdk_test::{async_test, test_json};
    use serde_json::{json_internal, Value as JsonValue};
    use wiremock::{
        matchers::{method, path},
        Mock, MockServer, ResponseTemplate,
    };

    use super::*;

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
        assert!(matches!(error, AuthenticationError::ServerNotFound));
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
        assert!(matches!(error, AuthenticationError::HomeserverNotFound));
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
        assert!(matches!(error, AuthenticationError::SlidingSyncNotAvailable));
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
        assert!(matches!(error, AuthenticationError::SlidingSyncNotAvailable));
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

    /* Helper functions */

    fn make_service(custom_sliding_sync_proxy: Option<String>) -> AuthenticationService {
        AuthenticationService {
            user_agent: None,
            client: Default::default(),
            homeserver_details: Default::default(),
            #[cfg(feature = "experimental-sliding-sync")]
            custom_sliding_sync_proxy: RwLock::new(custom_sliding_sync_proxy),
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
        homeserver
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
}
