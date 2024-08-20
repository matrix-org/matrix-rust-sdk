// Copyright 2022 The Matrix.org Foundation C.I.C.
// Copyright 2022 Kévin Commaille
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

use std::{fmt, sync::Arc};

use matrix_sdk_base::{store::StoreConfig, BaseClient};
use ruma::{
    api::{
        client::discovery::{discover_homeserver, get_supported_versions},
        error::FromHttpResponseError,
        MatrixVersion,
    },
    OwnedServerName, ServerName,
};
use thiserror::Error;
use tokio::sync::{broadcast, Mutex, OnceCell};
use tracing::{debug, field::debug, instrument, Span};
use url::Url;

use super::{Client, ClientInner};
#[cfg(feature = "e2e-encryption")]
use crate::crypto::CollectStrategy;
#[cfg(feature = "e2e-encryption")]
use crate::encryption::EncryptionSettings;
#[cfg(not(target_arch = "wasm32"))]
use crate::http_client::HttpSettings;
#[cfg(feature = "experimental-oidc")]
use crate::oidc::OidcCtx;
use crate::{
    authentication::AuthCtx, client::ClientServerCapabilities, config::RequestConfig,
    error::RumaApiError, http_client::HttpClient, send_queue::SendQueueData, HttpError,
    IdParseError,
};

/// Builder that allows creating and configuring various parts of a [`Client`].
///
/// When setting the `StateStore` it is up to the user to open/connect
/// the storage backend before client creation.
///
/// # Examples
///
/// ```
/// use matrix_sdk::Client;
/// // To pass all the request through mitmproxy set the proxy and disable SSL
/// // verification
///
/// let client_builder = Client::builder()
///     .proxy("http://localhost:8080")
///     .disable_ssl_verification();
/// ```
///
/// # Example for using a custom http client
///
/// Note: setting a custom http client will ignore `user_agent`, `proxy`, and
/// `disable_ssl_verification` - you'd need to set these yourself if you want
/// them.
///
/// ```
/// use std::sync::Arc;
///
/// use matrix_sdk::Client;
///
/// // setting up a custom http client
/// let reqwest_builder = reqwest::ClientBuilder::new()
///     .https_only(true)
///     .no_proxy()
///     .user_agent("MyApp/v3.0");
///
/// let client_builder =
///     Client::builder().http_client(reqwest_builder.build()?);
/// # anyhow::Ok(())
/// ```
#[must_use]
#[derive(Clone, Debug)]
pub struct ClientBuilder {
    homeserver_cfg: Option<HomeserverConfig>,
    #[cfg(feature = "experimental-sliding-sync")]
    requires_sliding_sync: bool,
    #[cfg(feature = "experimental-sliding-sync")]
    sliding_sync_proxy: Option<String>,
    #[cfg(feature = "experimental-sliding-sync")]
    is_simplified_sliding_sync_enabled: bool,
    http_cfg: Option<HttpConfig>,
    store_config: BuilderStoreConfig,
    request_config: RequestConfig,
    respect_login_well_known: bool,
    server_versions: Option<Box<[MatrixVersion]>>,
    handle_refresh_tokens: bool,
    base_client: Option<BaseClient>,
    #[cfg(feature = "e2e-encryption")]
    encryption_settings: EncryptionSettings,
    #[cfg(feature = "e2e-encryption")]
    room_key_recipient_strategy: CollectStrategy,
}

impl ClientBuilder {
    pub(crate) fn new() -> Self {
        Self {
            homeserver_cfg: None,
            #[cfg(feature = "experimental-sliding-sync")]
            requires_sliding_sync: false,
            #[cfg(feature = "experimental-sliding-sync")]
            sliding_sync_proxy: None,
            // Simplified MSC3575 is turned on by default for the SDK.
            #[cfg(feature = "experimental-sliding-sync")]
            is_simplified_sliding_sync_enabled: true,
            http_cfg: None,
            store_config: BuilderStoreConfig::Custom(StoreConfig::default()),
            request_config: Default::default(),
            respect_login_well_known: true,
            server_versions: None,
            handle_refresh_tokens: false,
            base_client: None,
            #[cfg(feature = "e2e-encryption")]
            encryption_settings: Default::default(),
            #[cfg(feature = "e2e-encryption")]
            room_key_recipient_strategy: Default::default(),
        }
    }

    /// Set the homeserver URL to use.
    ///
    /// The following methods are mutually exclusive:
    /// [`homeserver_url()`][Self::homeserver_url]
    /// [`server_name()`][Self::server_name]
    /// [`insecure_server_name_no_tls()`][Self::insecure_server_name_no_tls]
    /// [`server_name_or_homeserver_url()`][Self::server_name_or_homeserver_url],
    /// If you set more than one, then whatever was set last will be used.
    pub fn homeserver_url(mut self, url: impl AsRef<str>) -> Self {
        self.homeserver_cfg = Some(HomeserverConfig::Url(url.as_ref().to_owned()));
        self
    }

    /// Ensures that the client is built with support for sliding-sync, either
    /// by discovering a proxy through the homeserver's well-known or by
    /// providing one through [`Self::sliding_sync_proxy`].
    ///
    /// In the future this may also perform a check for native support on the
    /// homeserver.
    #[cfg(feature = "experimental-sliding-sync")]
    pub fn requires_sliding_sync(mut self) -> Self {
        self.requires_sliding_sync = true;
        self
    }

    /// Set the sliding-sync proxy URL to use.
    ///
    /// This value is always used no matter if the homeserver URL was defined
    /// with [`Self::homeserver_url`] or auto-discovered via
    /// [`Self::server_name`], [`Self::insecure_server_name_no_tls`], or
    /// [`Self::server_name_or_homeserver_url`] - any proxy discovered via the
    /// well-known lookup will be ignored.
    #[cfg(feature = "experimental-sliding-sync")]
    pub fn sliding_sync_proxy(mut self, url: impl AsRef<str>) -> Self {
        self.sliding_sync_proxy = Some(url.as_ref().to_owned());
        self
    }

    /// Enable or disable Simplified MSC3575.
    #[cfg(feature = "experimental-sliding-sync")]
    pub fn simplified_sliding_sync(mut self, enable: bool) -> Self {
        self.is_simplified_sliding_sync_enabled = enable;
        self
    }

    /// Set the server name to discover the homeserver from.
    ///
    /// We assume we can connect in HTTPS to that server. If that's not the
    /// case, prefer using [`Self::insecure_server_name_no_tls`].
    ///
    /// The following methods are mutually exclusive:
    /// [`homeserver_url()`][Self::homeserver_url]
    /// [`server_name()`][Self::server_name]
    /// [`insecure_server_name_no_tls()`][Self::insecure_server_name_no_tls]
    /// [`server_name_or_homeserver_url()`][Self::server_name_or_homeserver_url],
    /// If you set more than one, then whatever was set last will be used.
    pub fn server_name(mut self, server_name: &ServerName) -> Self {
        self.homeserver_cfg = Some(HomeserverConfig::ServerName {
            server: server_name.to_owned(),
            // Assume HTTPS if not specified.
            protocol: UrlScheme::Https,
        });
        self
    }

    /// Set the server name to discover the homeserver from, assuming an HTTP
    /// (not secured) scheme. This also relaxes OIDC discovery checks to allow
    /// HTTP schemes.
    ///
    /// The following methods are mutually exclusive:
    /// [`homeserver_url()`][Self::homeserver_url]
    /// [`server_name()`][Self::server_name]
    /// [`insecure_server_name_no_tls()`][Self::insecure_server_name_no_tls]
    /// [`server_name_or_homeserver_url()`][Self::server_name_or_homeserver_url],
    /// If you set more than one, then whatever was set last will be used.
    pub fn insecure_server_name_no_tls(mut self, server_name: &ServerName) -> Self {
        self.homeserver_cfg = Some(HomeserverConfig::ServerName {
            server: server_name.to_owned(),
            protocol: UrlScheme::Http,
        });
        self
    }

    /// Set the server name to discover the homeserver from, falling back to
    /// using it as a homeserver URL if discovery fails. When falling back to a
    /// homeserver URL, a check is made to ensure that the server exists (unlike
    /// [`homeserver_url()`][Self::homeserver_url]), so you can guarantee that
    /// the client is ready to use.
    ///
    /// The following methods are mutually exclusive:
    /// [`homeserver_url()`][Self::homeserver_url]
    /// [`server_name()`][Self::server_name]
    /// [`insecure_server_name_no_tls()`][Self::insecure_server_name_no_tls]
    /// [`server_name_or_homeserver_url()`][Self::server_name_or_homeserver_url],
    /// If you set more than one, then whatever was set last will be used.
    pub fn server_name_or_homeserver_url(mut self, server_name_or_url: impl AsRef<str>) -> Self {
        self.homeserver_cfg =
            Some(HomeserverConfig::ServerNameOrUrl(server_name_or_url.as_ref().to_owned()));
        self
    }

    /// Set up the store configuration for a SQLite store.
    #[cfg(feature = "sqlite")]
    pub fn sqlite_store(
        mut self,
        path: impl AsRef<std::path::Path>,
        passphrase: Option<&str>,
    ) -> Self {
        self.store_config = BuilderStoreConfig::Sqlite {
            path: path.as_ref().to_owned(),
            passphrase: passphrase.map(ToOwned::to_owned),
        };
        self
    }

    /// Set up the store configuration for a IndexedDB store.
    #[cfg(feature = "indexeddb")]
    pub fn indexeddb_store(mut self, name: &str, passphrase: Option<&str>) -> Self {
        self.store_config = BuilderStoreConfig::IndexedDb {
            name: name.to_owned(),
            passphrase: passphrase.map(ToOwned::to_owned),
        };
        self
    }

    /// Set up the store configuration.
    ///
    /// The easiest way to get a [`StoreConfig`] is to use the
    /// `make_store_config` method from one of the store crates.
    ///
    /// # Arguments
    ///
    /// * `store_config` - The configuration of the store.
    ///
    /// # Examples
    ///
    /// ```
    /// # use matrix_sdk_base::store::MemoryStore;
    /// # let custom_state_store = MemoryStore::new();
    /// use matrix_sdk::{config::StoreConfig, Client};
    ///
    /// let store_config = StoreConfig::new().state_store(custom_state_store);
    /// let client_builder = Client::builder().store_config(store_config);
    /// ```
    pub fn store_config(mut self, store_config: StoreConfig) -> Self {
        self.store_config = BuilderStoreConfig::Custom(store_config);
        self
    }

    /// Update the client's homeserver URL with the discovery information
    /// present in the login response, if any.
    pub fn respect_login_well_known(mut self, value: bool) -> Self {
        self.respect_login_well_known = value;
        self
    }

    /// Set the default timeout, fail and retry behavior for all HTTP requests.
    pub fn request_config(mut self, request_config: RequestConfig) -> Self {
        self.request_config = request_config;
        self
    }

    /// Set the proxy through which all the HTTP requests should go.
    ///
    /// Note, only HTTP proxies are supported.
    ///
    /// # Arguments
    ///
    /// * `proxy` - The HTTP URL of the proxy.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use matrix_sdk::Client;
    ///
    /// let client_config = Client::builder().proxy("http://localhost:8080");
    /// ```
    #[cfg(not(target_arch = "wasm32"))]
    pub fn proxy(mut self, proxy: impl AsRef<str>) -> Self {
        self.http_settings().proxy = Some(proxy.as_ref().to_owned());
        self
    }

    /// Disable SSL verification for the HTTP requests.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn disable_ssl_verification(mut self) -> Self {
        self.http_settings().disable_ssl_verification = true;
        self
    }

    /// Set a custom HTTP user agent for the client.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn user_agent(mut self, user_agent: impl AsRef<str>) -> Self {
        self.http_settings().user_agent = Some(user_agent.as_ref().to_owned());
        self
    }

    /// Add the given list of certificates to the certificate store of the HTTP
    /// client.
    ///
    /// These additional certificates will be trusted and considered when
    /// establishing a HTTP request.
    ///
    /// Internally this will call the
    /// [`reqwest::ClientBuilder::add_root_certificate()`] method.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn add_root_certificates(mut self, certificates: Vec<reqwest::Certificate>) -> Self {
        self.http_settings().additional_root_certificates = certificates;
        self
    }

    /// Don't trust any system root certificates, only trust the certificates
    /// provided through
    /// [`add_root_certificates`][ClientBuilder::add_root_certificates].
    #[cfg(not(target_arch = "wasm32"))]
    pub fn disable_built_in_root_certificates(mut self) -> Self {
        self.http_settings().disable_built_in_root_certificates = true;
        self
    }

    /// Specify a [`reqwest::Client`] instance to handle sending requests and
    /// receiving responses.
    ///
    /// This method is mutually exclusive with
    /// [`proxy()`][ClientBuilder::proxy],
    /// [`disable_ssl_verification`][ClientBuilder::disable_ssl_verification],
    /// [`add_root_certificates`][ClientBuilder::add_root_certificates],
    /// [`disable_built_in_root_certificates`][ClientBuilder::disable_built_in_root_certificates],
    /// and [`user_agent()`][ClientBuilder::user_agent].
    pub fn http_client(mut self, client: reqwest::Client) -> Self {
        self.http_cfg = Some(HttpConfig::Custom(client));
        self
    }

    /// Specify the Matrix versions supported by the homeserver manually, rather
    /// than `build()` doing it using a `get_supported_versions` request.
    ///
    /// This is helpful for test code that doesn't care to mock that endpoint.
    pub fn server_versions(mut self, value: impl IntoIterator<Item = MatrixVersion>) -> Self {
        self.server_versions = Some(value.into_iter().collect());
        self
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn http_settings(&mut self) -> &mut HttpSettings {
        self.http_cfg.get_or_insert_with(Default::default).settings()
    }

    /// Handle [refreshing access tokens] automatically.
    ///
    /// By default, the `Client` forwards any error and doesn't handle errors
    /// with the access token, which means that
    /// [`Client::refresh_access_token()`] needs to be called manually to
    /// refresh access tokens.
    ///
    /// Enabling this setting means that the `Client` will try to refresh the
    /// token automatically, which means that:
    ///
    /// * If refreshing the token fails, the error is forwarded, so any endpoint
    ///   can return [`HttpError::RefreshToken`]. If an [`UnknownToken`] error
    ///   is encountered, it means that the user needs to be logged in again.
    ///
    /// * The access token and refresh token need to be watched for changes,
    ///   using the authentication API's `session_tokens_stream()` for example,
    ///   to be able to [restore the session] later.
    ///
    /// [refreshing access tokens]: https://spec.matrix.org/v1.3/client-server-api/#refreshing-access-tokens
    /// [`UnknownToken`]: ruma::api::client::error::ErrorKind::UnknownToken
    /// [restore the session]: Client::restore_session
    pub fn handle_refresh_tokens(mut self) -> Self {
        self.handle_refresh_tokens = true;
        self
    }

    /// Public for test only
    #[doc(hidden)]
    pub fn base_client(mut self, base_client: BaseClient) -> Self {
        self.base_client = Some(base_client);
        self
    }

    /// Enables specific encryption settings that will persist throughout the
    /// entire lifetime of the `Client`.
    #[cfg(feature = "e2e-encryption")]
    pub fn with_encryption_settings(mut self, settings: EncryptionSettings) -> Self {
        self.encryption_settings = settings;
        self
    }

    /// Set the strategy to be used for picking recipient devices, when sending
    /// an encrypted message.
    #[cfg(feature = "e2e-encryption")]
    pub fn with_room_key_recipient_strategy(mut self, strategy: CollectStrategy) -> Self {
        self.room_key_recipient_strategy = strategy;
        self
    }

    /// Create a [`Client`] with the options set on this builder.
    ///
    /// # Errors
    ///
    /// This method can fail for two general reasons:
    ///
    /// * Invalid input: a missing or invalid homeserver URL or invalid proxy
    ///   URL
    /// * HTTP error: If you supplied a user ID instead of a homeserver URL, a
    ///   server discovery request is made which can fail; if you didn't set
    ///   [`server_versions(false)`][Self::server_versions], that amounts to
    ///   another request that can fail
    #[instrument(skip_all, target = "matrix_sdk::client", fields(homeserver))]
    pub async fn build(self) -> Result<Client, ClientBuildError> {
        debug!("Starting to build the Client");

        let homeserver_cfg = self.homeserver_cfg.ok_or(ClientBuildError::MissingHomeserver)?;
        Span::current().record("homeserver", debug(&homeserver_cfg));

        #[cfg_attr(target_arch = "wasm32", allow(clippy::infallible_destructuring_match))]
        let inner_http_client = match self.http_cfg.unwrap_or_default() {
            #[cfg(not(target_arch = "wasm32"))]
            HttpConfig::Settings(mut settings) => {
                settings.timeout = self.request_config.timeout;
                settings.make_client()?
            }
            HttpConfig::Custom(c) => c,
        };

        let base_client = if let Some(base_client) = self.base_client {
            base_client
        } else {
            #[allow(unused_mut)]
            let mut client =
                BaseClient::with_store_config(build_store_config(self.store_config).await?);
            #[cfg(feature = "e2e-encryption")]
            (client.room_key_recipient_strategy = self.room_key_recipient_strategy.clone());
            client
        };

        let http_client = HttpClient::new(inner_http_client.clone(), self.request_config);

        #[allow(unused_variables)]
        let (homeserver, well_known) = match homeserver_cfg {
            HomeserverConfig::Url(url) => (url, None),

            HomeserverConfig::ServerName { server: server_name, protocol } => {
                let well_known = discover_homeserver(server_name, protocol, &http_client).await?;
                (well_known.homeserver.base_url.clone(), Some(well_known))
            }

            HomeserverConfig::ServerNameOrUrl(server_name_or_url) => {
                discover_homeserver_from_server_name_or_url(server_name_or_url, &http_client)
                    .await?
            }
        };

        #[cfg(feature = "experimental-oidc")]
        let allow_insecure_oidc = homeserver.starts_with("http://");

        #[cfg(feature = "experimental-sliding-sync")]
        let mut sliding_sync_proxy =
            self.sliding_sync_proxy.as_ref().map(|url| Url::parse(url)).transpose()?;

        #[cfg(feature = "experimental-sliding-sync")]
        if self.is_simplified_sliding_sync_enabled {
            // When using Simplified MSC3575, don't use a sliding sync proxy, allow the
            // requests to be sent directly to the homeserver.
            tracing::info!("Simplified MSC3575 is enabled, ignoring any sliding sync proxy.");
            sliding_sync_proxy = None;
        } else if let Some(well_known) = well_known {
            // Otherwise, if a proxy wasn't set, use the one discovered from the well-known.
            if sliding_sync_proxy.is_none() {
                sliding_sync_proxy =
                    well_known.sliding_sync_proxy.and_then(|p| Url::parse(&p.url).ok())
            }
        }

        #[cfg(feature = "experimental-sliding-sync")]
        if self.requires_sliding_sync && sliding_sync_proxy.is_none() {
            // In the future we will need to check for native support on the homeserver too.
            return Err(ClientBuildError::SlidingSyncNotAvailable);
        }

        let homeserver = Url::parse(&homeserver)?;

        let auth_ctx = Arc::new(AuthCtx {
            handle_refresh_tokens: self.handle_refresh_tokens,
            refresh_token_lock: Mutex::new(Ok(())),
            session_change_sender: broadcast::Sender::new(1),
            auth_data: OnceCell::default(),
            reload_session_callback: OnceCell::default(),
            save_session_callback: OnceCell::default(),
            #[cfg(feature = "experimental-oidc")]
            oidc: OidcCtx::new(allow_insecure_oidc),
        });

        // Enable the send queue by default.
        let send_queue = Arc::new(SendQueueData::new(true));

        let server_capabilities = ClientServerCapabilities {
            server_versions: self.server_versions,
            unstable_features: None,
        };

        let event_cache = OnceCell::new();
        let inner = ClientInner::new(
            auth_ctx,
            homeserver,
            #[cfg(feature = "experimental-sliding-sync")]
            sliding_sync_proxy,
            #[cfg(feature = "experimental-sliding-sync")]
            self.is_simplified_sliding_sync_enabled,
            http_client,
            base_client,
            server_capabilities,
            self.respect_login_well_known,
            event_cache,
            send_queue,
            #[cfg(feature = "e2e-encryption")]
            self.encryption_settings,
        )
        .await;

        debug!("Done building the Client");

        Ok(Client { inner })
    }
}

/// Discovers a homeserver from a server name or a URL.
///
/// Tries well-known discovery and checking if the URL points to a homeserver.
async fn discover_homeserver_from_server_name_or_url(
    mut server_name_or_url: String,
    http_client: &HttpClient,
) -> Result<(String, Option<discover_homeserver::Response>), ClientBuildError> {
    let mut discovery_error: Option<ClientBuildError> = None;

    // Attempt discovery as a server name first.
    let sanitize_result = sanitize_server_name(&server_name_or_url);

    if let Ok(server_name) = sanitize_result.as_ref() {
        let protocol = if server_name_or_url.starts_with("http://") {
            UrlScheme::Http
        } else {
            UrlScheme::Https
        };

        match discover_homeserver(server_name.clone(), protocol, http_client).await {
            Ok(well_known) => {
                return Ok((well_known.homeserver.base_url.clone(), Some(well_known)));
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
        if check_is_homeserver(&homeserver_url, http_client).await {
            return Ok((homeserver_url.to_string(), None));
        }
    }

    Err(discovery_error.unwrap_or(ClientBuildError::InvalidServerName))
}

/// Creates a server name from a user supplied string. The string is first
/// sanitized by removing whitespace, the http(s) scheme and any trailing
/// slashes before being parsed.
pub fn sanitize_server_name(s: &str) -> crate::Result<OwnedServerName, IdParseError> {
    ServerName::parse(
        s.trim().trim_start_matches("http://").trim_start_matches("https://").trim_end_matches('/'),
    )
}

/// Discovers a homeserver by looking up the well-known at the supplied server
/// name.
async fn discover_homeserver(
    server_name: OwnedServerName,
    protocol: UrlScheme,
    http_client: &HttpClient,
) -> Result<discover_homeserver::Response, ClientBuildError> {
    debug!("Trying to discover the homeserver");

    let homeserver = match protocol {
        UrlScheme::Http => format!("http://{server_name}"),
        UrlScheme::Https => format!("https://{server_name}"),
    };

    let well_known = http_client
        .send(
            discover_homeserver::Request::new(),
            Some(RequestConfig::short_retry()),
            homeserver,
            None,
            &[MatrixVersion::V1_0],
            Default::default(),
        )
        .await
        .map_err(|e| match e {
            HttpError::Api(err) => ClientBuildError::AutoDiscovery(err),
            err => ClientBuildError::Http(err),
        })?;

    debug!(homeserver_url = well_known.homeserver.base_url, "Discovered the homeserver");

    Ok(well_known)
}

/// Checks if the given URL represents a valid homeserver.
async fn check_is_homeserver(homeserver_url: &Url, http_client: &HttpClient) -> bool {
    match http_client
        .send(
            get_supported_versions::Request::new(),
            Some(RequestConfig::short_retry()),
            homeserver_url.to_string(),
            None,
            &[MatrixVersion::V1_0],
            Default::default(),
        )
        .await
    {
        Ok(_) => true,
        Err(e) => {
            debug!(error = %e, "Checking supported versions failed.");
            false
        }
    }
}

#[allow(clippy::unused_async)] // False positive when building with !sqlite & !indexeddb
async fn build_store_config(
    builder_config: BuilderStoreConfig,
) -> Result<StoreConfig, ClientBuildError> {
    #[allow(clippy::infallible_destructuring_match)]
    let store_config = match builder_config {
        #[cfg(feature = "sqlite")]
        BuilderStoreConfig::Sqlite { path, passphrase } => {
            let store_config = StoreConfig::new().state_store(
                matrix_sdk_sqlite::SqliteStateStore::open(&path, passphrase.as_deref()).await?,
            );

            #[cfg(feature = "e2e-encryption")]
            let store_config = store_config.crypto_store(
                matrix_sdk_sqlite::SqliteCryptoStore::open(&path, passphrase.as_deref()).await?,
            );

            store_config
        }

        #[cfg(feature = "indexeddb")]
        BuilderStoreConfig::IndexedDb { name, passphrase } => {
            build_indexeddb_store_config(&name, passphrase.as_deref()).await?
        }

        BuilderStoreConfig::Custom(config) => config,
    };
    Ok(store_config)
}

// The indexeddb stores only implement `IntoStateStore` and `IntoCryptoStore` on
// wasm32, so this only compiles there.
#[cfg(all(target_arch = "wasm32", feature = "indexeddb"))]
async fn build_indexeddb_store_config(
    name: &str,
    passphrase: Option<&str>,
) -> Result<StoreConfig, ClientBuildError> {
    #[cfg(feature = "e2e-encryption")]
    {
        let (state_store, crypto_store) =
            matrix_sdk_indexeddb::open_stores_with_name(name, passphrase).await?;
        Ok(StoreConfig::new().state_store(state_store).crypto_store(crypto_store))
    }

    #[cfg(not(feature = "e2e-encryption"))]
    {
        let state_store = matrix_sdk_indexeddb::open_state_store(name, passphrase).await?;
        Ok(StoreConfig::new().state_store(state_store))
    }
}

#[cfg(all(not(target_arch = "wasm32"), feature = "indexeddb"))]
async fn build_indexeddb_store_config(
    _name: &str,
    _passphrase: Option<&str>,
) -> Result<StoreConfig, ClientBuildError> {
    panic!("the IndexedDB is only available on the 'wasm32' arch")
}

#[derive(Clone, Copy, Debug)]
enum UrlScheme {
    Http,
    Https,
}

#[derive(Clone, Debug)]
enum HomeserverConfig {
    /// A precise URL, including the protocol.
    Url(String),
    /// A host/port pair representing a server URL.
    ServerName { server: OwnedServerName, protocol: UrlScheme },
    /// First attempts to build as a server name, then falls back to a URL,
    /// failing if no valid homeserver is found.
    ServerNameOrUrl(String),
}

#[derive(Clone, Debug)]
enum HttpConfig {
    #[cfg(not(target_arch = "wasm32"))]
    Settings(HttpSettings),
    Custom(reqwest::Client),
}

#[cfg(not(target_arch = "wasm32"))]
impl HttpConfig {
    fn settings(&mut self) -> &mut HttpSettings {
        match self {
            Self::Settings(s) => s,
            Self::Custom(_) => {
                *self = Self::default();
                match self {
                    Self::Settings(s) => s,
                    Self::Custom(_) => unreachable!(),
                }
            }
        }
    }
}

impl Default for HttpConfig {
    fn default() -> Self {
        #[cfg(not(target_arch = "wasm32"))]
        return Self::Settings(HttpSettings::default());

        #[cfg(target_arch = "wasm32")]
        return Self::Custom(reqwest::Client::new());
    }
}

#[derive(Clone)]
enum BuilderStoreConfig {
    #[cfg(feature = "sqlite")]
    Sqlite {
        path: std::path::PathBuf,
        passphrase: Option<String>,
    },
    #[cfg(feature = "indexeddb")]
    IndexedDb {
        name: String,
        passphrase: Option<String>,
    },
    Custom(StoreConfig),
}

#[cfg(not(tarpaulin_include))]
impl fmt::Debug for BuilderStoreConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        #[allow(clippy::infallible_destructuring_match)]
        match self {
            #[cfg(feature = "sqlite")]
            Self::Sqlite { path, .. } => {
                f.debug_struct("Sqlite").field("path", path).finish_non_exhaustive()
            }
            #[cfg(feature = "indexeddb")]
            Self::IndexedDb { name, .. } => {
                f.debug_struct("IndexedDb").field("name", name).finish_non_exhaustive()
            }
            Self::Custom(store_config) => f.debug_tuple("Custom").field(store_config).finish(),
        }
    }
}

/// Errors that can happen in [`ClientBuilder::build`].
#[derive(Debug, Error)]
pub enum ClientBuildError {
    /// No homeserver or user ID was configured
    #[error("no homeserver or user ID was configured")]
    MissingHomeserver,

    /// The supplied server name was invalid.
    #[error("The supplied server name is invalid")]
    InvalidServerName,

    /// Error looking up the .well-known endpoint on auto-discovery
    #[error("Error looking up the .well-known endpoint on auto-discovery")]
    AutoDiscovery(FromHttpResponseError<RumaApiError>),

    /// The builder requires support for sliding sync but it isn't available.
    #[error("The homeserver doesn't support sliding sync and a custom proxy wasn't configured.")]
    SlidingSyncNotAvailable,

    /// An error encountered when trying to parse the homeserver url.
    #[error(transparent)]
    Url(#[from] url::ParseError),

    /// Error doing an HTTP request.
    #[error(transparent)]
    Http(#[from] HttpError),

    /// Error opening the indexeddb store.
    #[cfg(feature = "indexeddb")]
    #[error(transparent)]
    IndexeddbStore(#[from] matrix_sdk_indexeddb::OpenStoreError),

    /// Error opening the sqlite store.
    #[cfg(feature = "sqlite")]
    #[error(transparent)]
    SqliteStore(#[from] matrix_sdk_sqlite::OpenStoreError),
}

impl ClientBuildError {
    /// Assert that a valid homeserver URL was given to the builder and no other
    /// invalid options were specified, which means the only possible error
    /// case is [`Self::Http`].
    #[doc(hidden)]
    pub fn assert_valid_builder_args(self) -> HttpError {
        match self {
            ClientBuildError::Http(e) => e,
            _ => unreachable!("homeserver URL was asserted to be valid"),
        }
    }
}

// The http mocking library is not supported for wasm32
#[cfg(all(test, not(target_arch = "wasm32")))]
pub(crate) mod tests {
    use assert_matches::assert_matches;
    use matrix_sdk_test::{async_test, test_json};
    use serde_json::{json_internal, Value as JsonValue};
    use wiremock::{
        matchers::{method, path},
        Mock, MockServer, ResponseTemplate,
    };

    use super::*;

    #[test]
    fn test_sanitize_server_name() {
        assert_eq!(sanitize_server_name("matrix.org").unwrap().as_str(), "matrix.org");
        assert_eq!(sanitize_server_name("https://matrix.org").unwrap().as_str(), "matrix.org");
        assert_eq!(sanitize_server_name("http://matrix.org").unwrap().as_str(), "matrix.org");
        assert_eq!(
            sanitize_server_name("https://matrix.server.org").unwrap().as_str(),
            "matrix.server.org"
        );
        assert_eq!(
            sanitize_server_name("https://matrix.server.org/").unwrap().as_str(),
            "matrix.server.org"
        );
        assert_eq!(
            sanitize_server_name("  https://matrix.server.org// ").unwrap().as_str(),
            "matrix.server.org"
        );
        assert_matches!(sanitize_server_name("https://matrix.server.org/something"), Err(_))
    }

    // Note: Due to a limitation of the http mocking library the following tests all
    // supply an http:// url, to `server_name_or_homeserver_url` rather than the plain server name,
    // otherwise  the builder will prepend https:// and the request will fail. In practice, this
    // isn't a problem as the builder first strips the scheme and then checks if the
    // name is a valid server name, so it is a close enough approximation.

    #[async_test]
    async fn test_discovery_invalid_server() {
        // Given a new client builder.
        let mut builder = make_non_sss_client_builder();

        // When building a client with an invalid server name.
        builder = builder.server_name_or_homeserver_url("⚠️ This won't work 🚫");
        let error = builder.build().await.unwrap_err();

        // Then the operation should fail due to the invalid server name.
        assert_matches!(error, ClientBuildError::InvalidServerName);
    }

    #[async_test]
    async fn test_discovery_no_server() {
        // Given a new client builder.
        let mut builder = make_non_sss_client_builder();

        // When building a client with a valid server name that doesn't exist.
        builder = builder.server_name_or_homeserver_url("localhost:3456");
        let error = builder.build().await.unwrap_err();

        // Then the operation should fail with an HTTP error.
        println!("{error}");
        assert_matches!(error, ClientBuildError::Http(_));
    }

    #[async_test]
    async fn test_discovery_web_server() {
        // Given a random web server that isn't a Matrix homeserver or hosting the
        // well-known file for one.
        let server = MockServer::start().await;
        let mut builder = make_non_sss_client_builder();

        // When building a client with the server's URL.
        builder = builder.server_name_or_homeserver_url(server.uri());
        let error = builder.build().await.unwrap_err();

        // Then the operation should fail with a server discovery error.
        assert_matches!(error, ClientBuildError::AutoDiscovery(FromHttpResponseError::Server(_)));
    }

    #[async_test]
    async fn test_discovery_direct_legacy() {
        // Given a homeserver without a well-known file.
        let homeserver = make_mock_homeserver().await;
        let mut builder = make_non_sss_client_builder();

        // When building a client with the server's URL.
        builder = builder.server_name_or_homeserver_url(homeserver.uri());
        let _client = builder.build().await.unwrap();

        // Then a client should be built without support for sliding sync or OIDC.
        #[cfg(feature = "experimental-sliding-sync")]
        assert!(_client.sliding_sync_proxy().is_none());
    }

    #[async_test]
    async fn test_discovery_direct_legacy_custom_proxy() {
        // Given a homeserver without a well-known file and with a custom sliding sync
        // proxy injected.
        let homeserver = make_mock_homeserver().await;
        let mut builder = make_non_sss_client_builder();
        #[cfg(feature = "experimental-sliding-sync")]
        {
            builder = builder.sliding_sync_proxy("https://localhost:1234");
        }

        // When building a client with the server's URL.
        builder = builder.server_name_or_homeserver_url(homeserver.uri());
        let _client = builder.build().await.unwrap();

        // Then a client should be built with support for sliding sync.
        #[cfg(feature = "experimental-sliding-sync")]
        assert_eq!(_client.sliding_sync_proxy(), Some("https://localhost:1234".parse().unwrap()));
    }

    #[async_test]
    async fn test_discovery_well_known_parse_error() {
        // Given a base server with a well-known file that has errors.
        let server = MockServer::start().await;
        let homeserver = make_mock_homeserver().await;
        let mut builder = make_non_sss_client_builder();

        let well_known = make_well_known_json(&homeserver.uri(), None);
        let bad_json = well_known.to_string().replace(',', "");
        Mock::given(method("GET"))
            .and(path("/.well-known/matrix/client"))
            .respond_with(ResponseTemplate::new(200).set_body_json(bad_json))
            .mount(&server)
            .await;

        // When building a client with the base server.
        builder = builder.server_name_or_homeserver_url(server.uri());
        let error = builder.build().await.unwrap_err();

        // Then the operation should fail due to the well-known file's contents.
        assert_matches!(
            error,
            ClientBuildError::AutoDiscovery(FromHttpResponseError::Deserialization(_))
        );
    }

    #[async_test]
    async fn test_discovery_well_known_legacy() {
        // Given a base server with a well-known file that points to a homeserver that
        // doesn't support sliding sync.
        let server = MockServer::start().await;
        let homeserver = make_mock_homeserver().await;
        let mut builder = make_non_sss_client_builder();

        Mock::given(method("GET"))
            .and(path("/.well-known/matrix/client"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(make_well_known_json(&homeserver.uri(), None)),
            )
            .mount(&server)
            .await;

        // When building a client with the base server.
        builder = builder.server_name_or_homeserver_url(server.uri());
        let _client = builder.build().await.unwrap();

        // Then a client should be built without support for sliding sync or OIDC.
        #[cfg(feature = "experimental-sliding-sync")]
        assert!(_client.sliding_sync_proxy().is_none());
    }

    #[async_test]
    async fn test_discovery_well_known_with_sliding_sync() {
        // Given a base server with a well-known file that points to a homeserver with a
        // sliding sync proxy.
        let server = MockServer::start().await;
        let homeserver = make_mock_homeserver().await;
        let mut builder = make_non_sss_client_builder();

        Mock::given(method("GET"))
            .and(path("/.well-known/matrix/client"))
            .respond_with(ResponseTemplate::new(200).set_body_json(make_well_known_json(
                &homeserver.uri(),
                Some("https://localhost:1234"),
            )))
            .mount(&server)
            .await;

        // When building a client with the base server.
        builder = builder.server_name_or_homeserver_url(server.uri());
        let _client = builder.build().await.unwrap();

        // Then a client should be built with support for sliding sync.
        #[cfg(feature = "experimental-sliding-sync")]
        assert_eq!(_client.sliding_sync_proxy(), Some("https://localhost:1234".parse().unwrap()));
    }

    #[async_test]
    #[cfg(feature = "experimental-sliding-sync")]
    async fn test_discovery_well_known_with_sliding_sync_override() {
        // Given a base server with a well-known file that points to a homeserver with a
        // sliding sync proxy.
        let server = MockServer::start().await;
        let homeserver = make_mock_homeserver().await;
        let mut builder = make_non_sss_client_builder();

        Mock::given(method("GET"))
            .and(path("/.well-known/matrix/client"))
            .respond_with(ResponseTemplate::new(200).set_body_json(make_well_known_json(
                &homeserver.uri(),
                Some("https://localhost:1234"),
            )))
            .mount(&server)
            .await;

        // When building a client with the base server and a custom sliding sync proxy
        // set.
        builder = builder.sliding_sync_proxy("https://localhost:9012");
        builder = builder.server_name_or_homeserver_url(server.uri());
        let client = builder.build().await.unwrap();

        // Then a client should be built and configured with the custom sliding sync
        // proxy.
        #[cfg(feature = "experimental-sliding-sync")]
        assert_eq!(client.sliding_sync_proxy(), Some("https://localhost:9012".parse().unwrap()));
    }

    #[async_test]
    #[cfg(feature = "experimental-sliding-sync")]
    async fn test_discovery_well_known_with_simplified_sliding_sync() {
        // Given a base server with a well-known file that points to a homeserver with a
        // sliding sync proxy.
        let server = MockServer::start().await;
        let homeserver = make_mock_homeserver().await;
        let mut builder = make_non_sss_client_builder();

        Mock::given(method("GET"))
            .and(path("/.well-known/matrix/client"))
            .respond_with(ResponseTemplate::new(200).set_body_json(make_well_known_json(
                &homeserver.uri(),
                Some("https://localhost:1234"),
            )))
            .mount(&server)
            .await;

        // When building a client for simplified sliding sync with the base server.
        builder = builder.simplified_sliding_sync(true);
        builder = builder.server_name_or_homeserver_url(server.uri());
        let client = builder.build().await.unwrap();

        // Then a client should not use the discovered sliding sync proxy.
        assert!(client.sliding_sync_proxy().is_none());
    }

    /* Requires sliding sync */

    #[async_test]
    #[cfg(feature = "experimental-sliding-sync")]
    async fn test_requires_sliding_sync_with_legacy_well_known() {
        // Given a base server with a well-known file that points to a homeserver that
        // doesn't support sliding sync.
        let server = MockServer::start().await;
        let homeserver = make_mock_homeserver().await;
        let mut builder = make_non_sss_client_builder();

        Mock::given(method("GET"))
            .and(path("/.well-known/matrix/client"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(make_well_known_json(&homeserver.uri(), None)),
            )
            .mount(&server)
            .await;

        // When building a client that requires sliding sync with the base server.
        builder = builder.requires_sliding_sync().server_name_or_homeserver_url(server.uri());
        let error = builder.build().await.unwrap_err();

        // Then the operation should fail due to the lack of sliding sync support.
        assert_matches!(error, ClientBuildError::SlidingSyncNotAvailable);
    }

    #[async_test]
    #[cfg(feature = "experimental-sliding-sync")]
    async fn test_requires_sliding_sync_with_well_known() {
        // Given a base server with a well-known file that points to a homeserver with a
        // sliding sync proxy.
        let server = MockServer::start().await;
        let homeserver = make_mock_homeserver().await;
        let mut builder = make_non_sss_client_builder();

        Mock::given(method("GET"))
            .and(path("/.well-known/matrix/client"))
            .respond_with(ResponseTemplate::new(200).set_body_json(make_well_known_json(
                &homeserver.uri(),
                Some("https://localhost:1234"),
            )))
            .mount(&server)
            .await;

        // When building a client that requires sliding sync with the base server.
        builder = builder.requires_sliding_sync().server_name_or_homeserver_url(server.uri());
        let _client = builder.build().await.unwrap();

        // Then a client should be built with support for sliding sync.
        assert_eq!(_client.sliding_sync_proxy(), Some("https://localhost:1234".parse().unwrap()));
    }

    #[async_test]
    #[cfg(feature = "experimental-sliding-sync")]
    async fn test_requires_sliding_sync_with_custom_proxy() {
        // Given a homeserver without a well-known file and with a custom sliding sync
        // proxy injected.
        let homeserver = make_mock_homeserver().await;
        let mut builder = make_non_sss_client_builder();
        builder = builder.sliding_sync_proxy("https://localhost:1234");

        // When building a client that requires sliding sync with the server's URL.
        builder = builder.requires_sliding_sync().server_name_or_homeserver_url(homeserver.uri());
        let _client = builder.build().await.unwrap();

        // Then a client should be built with support for sliding sync.
        assert_eq!(_client.sliding_sync_proxy(), Some("https://localhost:1234".parse().unwrap()));
    }

    /* Helper functions */

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

            object
        })
    }

    /// These tests were built with regular sliding sync in mind so until
    /// we remove it and update the tests, this makes a builder with SSS
    /// disabled.
    fn make_non_sss_client_builder() -> ClientBuilder {
        let mut builder = ClientBuilder::new();

        #[cfg(feature = "experimental-sliding-sync")]
        {
            builder = builder.simplified_sliding_sync(false);
        }

        builder
    }
}
