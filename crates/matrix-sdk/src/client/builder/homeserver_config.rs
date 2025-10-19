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

use ruma::{
    OwnedServerName, ServerName,
    api::client::discovery::{discover_homeserver, get_supported_versions},
};
use tracing::debug;
use url::Url;

use crate::{
    ClientBuildError, HttpError, config::RequestConfig, http_client::HttpClient,
    sanitize_server_name,
};

/// Configuration for the homeserver.
#[derive(Clone, Debug)]
pub(super) enum HomeserverConfig {
    /// A homeserver name URL, including the protocol.
    HomeserverUrl(String),

    /// A server name, with the protocol put apart.
    ServerName { server: OwnedServerName, protocol: UrlScheme },

    /// A server name with or without the protocol (it will fallback to `https`
    /// if absent), or a homeserver URL.
    ServerNameOrHomeserverUrl(String),
}

/// A simple helper to represent `http` or `https` in a URL.
#[derive(Clone, Copy, Debug)]
pub(super) enum UrlScheme {
    Http,
    Https,
}

/// The `Ok` result for `HomeserverConfig::discover`.
pub(super) struct HomeserverDiscoveryResult {
    pub server: Option<Url>,
    pub homeserver: Url,
    pub supported_versions: Option<get_supported_versions::Response>,
    pub well_known: Option<discover_homeserver::Response>,
}

impl HomeserverConfig {
    pub async fn discover(
        &self,
        http_client: &HttpClient,
    ) -> Result<HomeserverDiscoveryResult, ClientBuildError> {
        Ok(match self {
            Self::HomeserverUrl(url) => {
                let homeserver = Url::parse(url)?;

                HomeserverDiscoveryResult {
                    server: None, // We can't know the `server` if we only have a `homeserver`.
                    homeserver,
                    supported_versions: None,
                    well_known: None,
                }
            }

            Self::ServerName { server, protocol } => {
                let (server, well_known) =
                    discover_homeserver(server, protocol, http_client).await?;

                HomeserverDiscoveryResult {
                    server: Some(server),
                    homeserver: Url::parse(&well_known.homeserver.base_url)?,
                    supported_versions: None,
                    well_known: Some(well_known),
                }
            }

            Self::ServerNameOrHomeserverUrl(server_name_or_url) => {
                let (server, homeserver, supported_versions, well_known) =
                    discover_homeserver_from_server_name_or_url(
                        server_name_or_url.to_owned(),
                        http_client,
                    )
                    .await?;

                HomeserverDiscoveryResult { server, homeserver, supported_versions, well_known }
            }
        })
    }
}

/// Discovers a homeserver from a server name or a URL.
///
/// Tries well-known discovery and checking if the URL points to a homeserver.
async fn discover_homeserver_from_server_name_or_url(
    mut server_name_or_url: String,
    http_client: &HttpClient,
) -> Result<
    (
        Option<Url>,
        Url,
        Option<get_supported_versions::Response>,
        Option<discover_homeserver::Response>,
    ),
    ClientBuildError,
> {
    let mut discovery_error: Option<ClientBuildError> = None;

    // Attempt discovery as a server name first.
    let sanitize_result = sanitize_server_name(&server_name_or_url);

    if let Ok(server_name) = sanitize_result.as_ref() {
        let protocol = if server_name_or_url.starts_with("http://") {
            UrlScheme::Http
        } else {
            UrlScheme::Https
        };

        match discover_homeserver(server_name, &protocol, http_client).await {
            Ok((server, well_known)) => {
                return Ok((
                    Some(server),
                    Url::parse(&well_known.homeserver.base_url)?,
                    None,
                    Some(well_known),
                ));
            }
            Err(e) => {
                debug!(error = %e, "Well-known discovery failed.");
                discovery_error = Some(e);

                // Check if the server name points to a homeserver.
                server_name_or_url = match protocol {
                    UrlScheme::Http => format!("http://{server_name}"),
                    UrlScheme::Https => format!("https://{server_name}"),
                }
            }
        }
    }

    // When discovery fails, or the input isn't a valid server name, fallback to
    // trying a homeserver URL.
    if let Ok(homeserver_url) = Url::parse(&server_name_or_url) {
        // Make sure the URL is definitely for a homeserver.
        match get_supported_versions(&homeserver_url, http_client).await {
            Ok(response) => {
                return Ok((None, homeserver_url, Some(response), None));
            }
            Err(e) => {
                debug!(error = %e, "Checking supported versions failed.");
            }
        }
    }

    Err(discovery_error.unwrap_or(ClientBuildError::InvalidServerName))
}

/// Discovers a homeserver by looking up the well-known at the supplied server
/// name.
async fn discover_homeserver(
    server_name: &ServerName,
    protocol: &UrlScheme,
    http_client: &HttpClient,
) -> Result<(Url, discover_homeserver::Response), ClientBuildError> {
    debug!("Trying to discover the homeserver");

    let server = Url::parse(&match protocol {
        UrlScheme::Http => format!("http://{server_name}"),
        UrlScheme::Https => format!("https://{server_name}"),
    })?;

    let well_known = http_client
        .send(
            discover_homeserver::Request::new(),
            Some(RequestConfig::short_retry()),
            server.to_string(),
            None,
            (),
            Default::default(),
        )
        .await
        .map_err(|e| match e {
            HttpError::Api(err) => ClientBuildError::AutoDiscovery(*err),
            err => ClientBuildError::Http(err),
        })?;

    debug!(homeserver_url = well_known.homeserver.base_url, "Discovered the homeserver");

    Ok((server, well_known))
}

pub(super) async fn get_supported_versions(
    homeserver_url: &Url,
    http_client: &HttpClient,
) -> Result<get_supported_versions::Response, HttpError> {
    http_client
        .send(
            get_supported_versions::Request::new(),
            Some(RequestConfig::short_retry()),
            homeserver_url.to_string(),
            None,
            (),
            Default::default(),
        )
        .await
}

#[cfg(all(test, not(target_family = "wasm")))]
mod tests {
    use matrix_sdk_test::async_test;
    use ruma::OwnedServerName;
    use serde_json::json;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{method, path},
    };

    use super::*;
    use crate::http_client::HttpSettings;

    #[async_test]
    async fn test_url() {
        let http_client =
            HttpClient::new(HttpSettings::default().make_client().unwrap(), Default::default());

        let result = HomeserverConfig::HomeserverUrl("https://matrix-client.matrix.org".to_owned())
            .discover(&http_client)
            .await
            .unwrap();

        assert_eq!(result.server, None);
        assert_eq!(result.homeserver, Url::parse("https://matrix-client.matrix.org").unwrap());
        assert!(result.supported_versions.is_none());
    }

    #[async_test]
    async fn test_server_name() {
        let http_client =
            HttpClient::new(HttpSettings::default().make_client().unwrap(), Default::default());

        let server = MockServer::start().await;
        let homeserver = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/.well-known/matrix/client"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "m.homeserver": {
                    "base_url": homeserver.uri(),
                },
            })))
            .mount(&server)
            .await;

        let result = HomeserverConfig::ServerName {
            server: OwnedServerName::try_from(server.address().to_string()).unwrap(),
            protocol: UrlScheme::Http,
        }
        .discover(&http_client)
        .await
        .unwrap();

        assert_eq!(result.server, Some(Url::parse(&server.uri()).unwrap()));
        assert_eq!(result.homeserver, Url::parse(&homeserver.uri()).unwrap());
        assert!(result.supported_versions.is_none());
    }

    #[async_test]
    async fn test_server_name_or_url_with_name() {
        let http_client =
            HttpClient::new(HttpSettings::default().make_client().unwrap(), Default::default());

        let server = MockServer::start().await;
        let homeserver = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/.well-known/matrix/client"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "m.homeserver": {
                    "base_url": homeserver.uri(),
                },
            })))
            .mount(&server)
            .await;

        let result = HomeserverConfig::ServerNameOrHomeserverUrl(server.uri().to_string())
            .discover(&http_client)
            .await
            .unwrap();

        assert_eq!(result.server, Some(Url::parse(&server.uri()).unwrap()));
        assert_eq!(result.homeserver, Url::parse(&homeserver.uri()).unwrap());
        assert!(result.supported_versions.is_none());
    }

    #[async_test]
    async fn test_server_name_or_url_with_url() {
        let http_client =
            HttpClient::new(HttpSettings::default().make_client().unwrap(), Default::default());

        let homeserver = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/_matrix/client/versions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "versions": [],
            })))
            .mount(&homeserver)
            .await;

        let result = HomeserverConfig::ServerNameOrHomeserverUrl(homeserver.uri().to_string())
            .discover(&http_client)
            .await
            .unwrap();

        assert!(result.server.is_none());
        assert_eq!(result.homeserver, Url::parse(&homeserver.uri()).unwrap());
        assert!(result.supported_versions.is_some());
    }
}
