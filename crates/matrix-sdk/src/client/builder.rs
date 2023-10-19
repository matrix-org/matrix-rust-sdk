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
    api::{client::discovery::discover_homeserver, error::FromHttpResponseError, MatrixVersion},
    OwnedServerName, ServerName,
};
use thiserror::Error;
use tokio::sync::{broadcast, Mutex, OnceCell};
use tracing::{debug, field::debug, instrument, Span};
use url::Url;

use super::{Client, ClientInner};
#[cfg(feature = "e2e-encryption")]
use crate::encryption::EncryptionSettings;
#[cfg(not(target_arch = "wasm32"))]
use crate::http_client::HttpSettings;
#[cfg(feature = "experimental-oidc")]
use crate::oidc::OidcCtx;
use crate::{
    authentication::AuthCtx, config::RequestConfig, error::RumaApiError, http_client::HttpClient,
    HttpError,
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
    sliding_sync_proxy: Option<String>,
    http_cfg: Option<HttpConfig>,
    store_config: BuilderStoreConfig,
    request_config: RequestConfig,
    respect_login_well_known: bool,
    server_versions: Option<Box<[MatrixVersion]>>,
    handle_refresh_tokens: bool,
    base_client: Option<BaseClient>,
    #[cfg(feature = "e2e-encryption")]
    encryption_settings: EncryptionSettings,
}

impl ClientBuilder {
    pub(crate) fn new() -> Self {
        Self {
            homeserver_cfg: None,
            #[cfg(feature = "experimental-sliding-sync")]
            sliding_sync_proxy: None,
            http_cfg: None,
            store_config: BuilderStoreConfig::Custom(StoreConfig::default()),
            request_config: Default::default(),
            respect_login_well_known: true,
            server_versions: None,
            handle_refresh_tokens: false,
            base_client: None,
            encryption_settings: Default::default(),
        }
    }

    /// Set the homeserver URL to use.
    ///
    /// This method is mutually exclusive with
    /// [`server_name()`][Self::server_name], if you set both whatever was set
    /// last will be used.
    pub fn homeserver_url(mut self, url: impl AsRef<str>) -> Self {
        self.homeserver_cfg = Some(HomeserverConfig::Url(url.as_ref().to_owned()));
        self
    }

    /// Set the sliding-sync proxy URL to use.
    ///
    /// This is used only if the homeserver URL was defined with
    /// [`Self::homeserver_url`]. If the homeserver address was defined with
    /// [`Self::server_name`], then auto-discovery via the `.well-known`
    /// endpoint will be performed.
    #[cfg(feature = "experimental-sliding-sync")]
    pub fn sliding_sync_proxy(mut self, url: impl AsRef<str>) -> Self {
        self.sliding_sync_proxy = Some(url.as_ref().to_owned());
        self
    }

    /// Set the server name to discover the homeserver from.
    ///
    /// We assume we can connect in HTTPS to that server. If that's not the
    /// case, prefer using [`Self::insecure_server_name_no_tls`].
    ///
    /// This method is mutually exclusive with
    /// [`homeserver_url()`][Self::homeserver_url], if you set both whatever was
    /// set last will be used.
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
    /// This method is mutually exclusive with
    /// [`homeserver_url()`][Self::homeserver_url], if you set both whatever was
    /// set last will be used.
    pub fn insecure_server_name_no_tls(mut self, server_name: &ServerName) -> Self {
        self.homeserver_cfg = Some(HomeserverConfig::ServerName {
            server: server_name.to_owned(),
            protocol: UrlScheme::Http,
        });
        self
    }

    /// Set up the store configuration for a SQLite store.
    ///
    /// This is the same as
    /// <code>.[store_config](Self::store_config)([matrix_sdk_sqlite]::[make_store_config](matrix_sdk_sqlite::make_store_config)(path, passphrase)?)</code>.
    /// except it delegates the actual store config creation to when
    /// `.build().await` is called.
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
    ///
    /// This is the same as
    /// <code>.[store_config](Self::store_config)([matrix_sdk_indexeddb]::[make_store_config](matrix_sdk_indexeddb::make_store_config)(path, passphrase).await?)</code>,
    /// except it delegates the actual store config creation to when
    /// `.build().await` is called.
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

    /// Specify a [`reqwest::Client`] instance to handle sending requests and
    /// receiving responses.
    ///
    /// This method is mutually exclusive with [`proxy()`][Self::proxy],
    /// [`disable_ssl_verification`][Self::disable_ssl_verification] and
    /// [`user_agent()`][Self::user_agent].
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
            #[allow(clippy::infallible_destructuring_match)]
            let store_config = match self.store_config {
                #[cfg(feature = "sqlite")]
                BuilderStoreConfig::Sqlite { path, passphrase } => {
                    matrix_sdk_sqlite::make_store_config(&path, passphrase.as_deref()).await?
                }
                #[cfg(feature = "indexeddb")]
                BuilderStoreConfig::IndexedDb { name, passphrase } => {
                    matrix_sdk_indexeddb::make_store_config(&name, passphrase.as_deref()).await?
                }
                BuilderStoreConfig::Custom(config) => config,
            };
            BaseClient::with_store_config(store_config)
        };

        let http_client = HttpClient::new(inner_http_client.clone(), self.request_config);

        #[cfg(feature = "experimental-oidc")]
        let mut authentication_server_info = None;
        #[cfg(feature = "experimental-oidc")]
        let allow_insecure_oidc;

        #[cfg(feature = "experimental-sliding-sync")]
        let mut sliding_sync_proxy: Option<Url> = None;

        let homeserver = match homeserver_cfg {
            HomeserverConfig::Url(url) => {
                #[cfg(feature = "experimental-sliding-sync")]
                {
                    sliding_sync_proxy =
                        self.sliding_sync_proxy.as_ref().map(|url| Url::parse(url)).transpose()?;
                }

                #[cfg(feature = "experimental-oidc")]
                {
                    allow_insecure_oidc = url.starts_with("http://");
                }

                url
            }
            HomeserverConfig::ServerName { server: server_name, protocol } => {
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

                #[cfg(feature = "experimental-oidc")]
                {
                    authentication_server_info = well_known.authentication;
                    allow_insecure_oidc = matches!(protocol, UrlScheme::Http);
                }

                #[cfg(feature = "experimental-sliding-sync")]
                if let Some(proxy) = well_known.sliding_sync_proxy.map(|p| p.url) {
                    sliding_sync_proxy = Url::parse(&proxy).ok();
                }

                debug!(
                    homeserver_url = well_known.homeserver.base_url,
                    "Discovered the homeserver"
                );

                well_known.homeserver.base_url
            }
        };

        let homeserver = Url::parse(&homeserver)?;

        let auth_ctx = Arc::new(AuthCtx {
            handle_refresh_tokens: self.handle_refresh_tokens,
            refresh_token_lock: Mutex::new(Ok(())),
            session_change_sender: broadcast::Sender::new(1),
            auth_data: OnceCell::default(),
            reload_session_callback: OnceCell::default(),
            save_session_callback: OnceCell::default(),
            #[cfg(feature = "experimental-oidc")]
            oidc: OidcCtx::new(authentication_server_info, allow_insecure_oidc),
        });

        let inner = Arc::new(ClientInner::new(
            auth_ctx,
            homeserver,
            #[cfg(feature = "experimental-sliding-sync")]
            sliding_sync_proxy,
            http_client,
            base_client,
            self.server_versions,
            self.respect_login_well_known,
            #[cfg(feature = "e2e-encryption")]
            self.encryption_settings,
        ));

        debug!("Done building the Client");

        Ok(Client { inner })
    }
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

    /// Error looking up the .well-known endpoint on auto-discovery
    #[error("Error looking up the .well-known endpoint on auto-discovery")]
    AutoDiscovery(FromHttpResponseError<RumaApiError>),

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
