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

mod homeserver_config;

use std::{fmt, sync::Arc};

use homeserver_config::*;
use matrix_sdk_base::{store::StoreConfig, BaseClient};
use ruma::{
    api::{error::FromHttpResponseError, MatrixVersion},
    OwnedServerName, ServerName,
};
use thiserror::Error;
use tokio::sync::{broadcast, Mutex, OnceCell};
use tracing::{debug, field::debug, instrument, Span};

use super::{Client, ClientInner};
#[cfg(feature = "e2e-encryption")]
use crate::crypto::{CollectStrategy, TrustRequirement};
#[cfg(feature = "e2e-encryption")]
use crate::encryption::EncryptionSettings;
#[cfg(not(target_arch = "wasm32"))]
use crate::http_client::HttpSettings;
#[cfg(feature = "experimental-oidc")]
use crate::oidc::OidcCtx;
#[cfg(feature = "experimental-sliding-sync")]
use crate::sliding_sync::VersionBuilder as SlidingSyncVersionBuilder;
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
    sliding_sync_version_builder: SlidingSyncVersionBuilder,
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
    #[cfg(feature = "e2e-encryption")]
    decryption_trust_requirement: TrustRequirement,
    cross_process_store_locks_holder_name: String,
}

impl ClientBuilder {
    const DEFAULT_CROSS_PROCESS_STORE_LOCKS_HOLDER_NAME: &str = "main";

    pub(crate) fn new() -> Self {
        Self {
            homeserver_cfg: None,
            #[cfg(feature = "experimental-sliding-sync")]
            sliding_sync_version_builder: SlidingSyncVersionBuilder::Native,
            http_cfg: None,
            store_config: BuilderStoreConfig::Custom(StoreConfig::new(
                Self::DEFAULT_CROSS_PROCESS_STORE_LOCKS_HOLDER_NAME.to_owned(),
            )),
            request_config: Default::default(),
            respect_login_well_known: true,
            server_versions: None,
            handle_refresh_tokens: false,
            base_client: None,
            #[cfg(feature = "e2e-encryption")]
            encryption_settings: Default::default(),
            #[cfg(feature = "e2e-encryption")]
            room_key_recipient_strategy: Default::default(),
            #[cfg(feature = "e2e-encryption")]
            decryption_trust_requirement: TrustRequirement::Untrusted,
            cross_process_store_locks_holder_name:
                Self::DEFAULT_CROSS_PROCESS_STORE_LOCKS_HOLDER_NAME.to_owned(),
        }
    }

    /// Set the homeserver URL to use.
    ///
    /// The following methods are mutually exclusive: [`Self::homeserver_url`],
    /// [`Self::server_name`] [`Self::insecure_server_name_no_tls`],
    /// [`Self::server_name_or_homeserver_url`].
    /// If you set more than one, then whatever was set last will be used.
    pub fn homeserver_url(mut self, url: impl AsRef<str>) -> Self {
        self.homeserver_cfg = Some(HomeserverConfig::HomeserverUrl(url.as_ref().to_owned()));
        self
    }

    /// Set the server name to discover the homeserver from.
    ///
    /// We assume we can connect in HTTPS to that server. If that's not the
    /// case, prefer using [`Self::insecure_server_name_no_tls`].
    ///
    /// The following methods are mutually exclusive: [`Self::homeserver_url`],
    /// [`Self::server_name`] [`Self::insecure_server_name_no_tls`],
    /// [`Self::server_name_or_homeserver_url`].
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
    /// The following methods are mutually exclusive: [`Self::homeserver_url`],
    /// [`Self::server_name`] [`Self::insecure_server_name_no_tls`],
    /// [`Self::server_name_or_homeserver_url`].
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
    /// [`Self::homeserver_url`], so you can guarantee that the client is ready
    /// to use.
    ///
    /// The following methods are mutually exclusive: [`Self::homeserver_url`],
    /// [`Self::server_name`] [`Self::insecure_server_name_no_tls`],
    /// [`Self::server_name_or_homeserver_url`].
    /// If you set more than one, then whatever was set last will be used.
    pub fn server_name_or_homeserver_url(mut self, server_name_or_url: impl AsRef<str>) -> Self {
        self.homeserver_cfg = Some(HomeserverConfig::ServerNameOrHomeserverUrl(
            server_name_or_url.as_ref().to_owned(),
        ));
        self
    }

    /// Set sliding sync to a specific version.
    #[cfg(feature = "experimental-sliding-sync")]
    pub fn sliding_sync_version_builder(
        mut self,
        version_builder: SlidingSyncVersionBuilder,
    ) -> Self {
        self.sliding_sync_version_builder = version_builder;
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
            cache_path: None,
            passphrase: passphrase.map(ToOwned::to_owned),
        };
        self
    }

    /// Set up the store configuration for a SQLite store with cached data
    /// separated out from state/crypto data.
    #[cfg(feature = "sqlite")]
    pub fn sqlite_store_with_cache_path(
        mut self,
        path: impl AsRef<std::path::Path>,
        cache_path: impl AsRef<std::path::Path>,
        passphrase: Option<&str>,
    ) -> Self {
        self.store_config = BuilderStoreConfig::Sqlite {
            path: path.as_ref().to_owned(),
            cache_path: Some(cache_path.as_ref().to_owned()),
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
    /// let store_config =
    ///     StoreConfig::new("cross-process-store-locks-holder-name".to_owned())
    ///         .state_store(custom_state_store);
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

    /// Set the trust requirement to be used when decrypting events.
    #[cfg(feature = "e2e-encryption")]
    pub fn with_decryption_trust_requirement(
        mut self,
        trust_requirement: TrustRequirement,
    ) -> Self {
        self.decryption_trust_requirement = trust_requirement;
        self
    }

    /// Set the cross-process store locks holder name.
    ///
    /// The SDK provides cross-process store locks (see
    /// [`matrix_sdk_common::store_locks::CrossProcessStoreLock`]). The
    /// `holder_name` will be the value used for all cross-process store locks
    /// used by the `Client` being built.
    ///
    /// If 2 concurrent `Client`s are running in 2 different process, this
    /// method must be called with different `hold_name` values.
    pub fn cross_process_store_locks_holder_name(mut self, holder_name: String) -> Self {
        self.cross_process_store_locks_holder_name = holder_name;
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
            let mut client = BaseClient::with_store_config(
                build_store_config(self.store_config, &self.cross_process_store_locks_holder_name)
                    .await?,
            );

            #[cfg(feature = "e2e-encryption")]
            {
                client.room_key_recipient_strategy = self.room_key_recipient_strategy;
                client.decryption_trust_requirement = self.decryption_trust_requirement;
            }

            client
        };

        let http_client = HttpClient::new(inner_http_client.clone(), self.request_config);

        #[allow(unused_variables)]
        let HomeserverDiscoveryResult { server, homeserver, well_known, supported_versions } =
            homeserver_cfg.discover(&http_client).await?;

        #[cfg(feature = "experimental-sliding-sync")]
        let sliding_sync_version = {
            let supported_versions = match supported_versions {
                Some(versions) => Some(versions),
                None if self.sliding_sync_version_builder.needs_get_supported_versions() => {
                    Some(get_supported_versions(&homeserver, &http_client).await?)
                }
                None => None,
            };

            let version = self
                .sliding_sync_version_builder
                .build(well_known.as_ref(), supported_versions.as_ref())?;

            tracing::info!(?version, "selected sliding sync version");

            version
        };

        #[cfg(feature = "experimental-oidc")]
        let allow_insecure_oidc = homeserver.scheme() == "http";

        let auth_ctx = Arc::new(AuthCtx {
            handle_refresh_tokens: self.handle_refresh_tokens,
            refresh_token_lock: Arc::new(Mutex::new(Ok(()))),
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
            server,
            homeserver,
            #[cfg(feature = "experimental-sliding-sync")]
            sliding_sync_version,
            http_client,
            base_client,
            server_capabilities,
            self.respect_login_well_known,
            event_cache,
            send_queue,
            #[cfg(feature = "e2e-encryption")]
            self.encryption_settings,
            self.cross_process_store_locks_holder_name,
        )
        .await;

        debug!("Done building the Client");

        Ok(Client { inner })
    }
}

/// Creates a server name from a user supplied string. The string is first
/// sanitized by removing whitespace, the http(s) scheme and any trailing
/// slashes before being parsed.
pub fn sanitize_server_name(s: &str) -> crate::Result<OwnedServerName, IdParseError> {
    ServerName::parse(
        s.trim().trim_start_matches("http://").trim_start_matches("https://").trim_end_matches('/'),
    )
}

#[allow(clippy::unused_async, unused)] // False positive when building with !sqlite & !indexeddb
async fn build_store_config(
    builder_config: BuilderStoreConfig,
    cross_process_store_locks_holder_name: &str,
) -> Result<StoreConfig, ClientBuildError> {
    #[allow(clippy::infallible_destructuring_match)]
    let store_config = match builder_config {
        #[cfg(feature = "sqlite")]
        BuilderStoreConfig::Sqlite { path, cache_path, passphrase } => {
            let store_config = StoreConfig::new(cross_process_store_locks_holder_name.to_owned())
                .state_store(
                    matrix_sdk_sqlite::SqliteStateStore::open(&path, passphrase.as_deref()).await?,
                )
                .event_cache_store(
                    matrix_sdk_sqlite::SqliteEventCacheStore::open(
                        cache_path.as_ref().unwrap_or(&path),
                        passphrase.as_deref(),
                    )
                    .await?,
                );

            #[cfg(feature = "e2e-encryption")]
            let store_config = store_config.crypto_store(
                matrix_sdk_sqlite::SqliteCryptoStore::open(&path, passphrase.as_deref()).await?,
            );

            store_config
        }

        #[cfg(feature = "indexeddb")]
        BuilderStoreConfig::IndexedDb { name, passphrase } => {
            build_indexeddb_store_config(
                &name,
                passphrase.as_deref(),
                cross_process_store_locks_holder_name,
            )
            .await?
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
    cross_process_store_locks_holder_name: &str,
) -> Result<StoreConfig, ClientBuildError> {
    let cross_process_store_locks_holder_name = cross_process_store_locks_holder_name.to_owned();

    #[cfg(feature = "e2e-encryption")]
    let store_config = {
        let (state_store, crypto_store) =
            matrix_sdk_indexeddb::open_stores_with_name(name, passphrase).await?;
        StoreConfig::new(cross_process_store_locks_holder_name)
            .state_store(state_store)
            .crypto_store(crypto_store)
    };

    #[cfg(not(feature = "e2e-encryption"))]
    let store_config = {
        let state_store = matrix_sdk_indexeddb::open_state_store(name, passphrase).await?;
        StoreConfig::new(cross_process_store_locks_holder_name).state_store(state_store)
    };

    let store_config = {
        tracing::warn!("The IndexedDB backend does not implement an event cache store, falling back to the in-memory event cache store…");
        store_config.event_cache_store(matrix_sdk_base::event_cache::store::MemoryStore::new())
    };

    Ok(store_config)
}

#[cfg(all(not(target_arch = "wasm32"), feature = "indexeddb"))]
#[allow(clippy::unused_async)]
async fn build_indexeddb_store_config(
    _name: &str,
    _passphrase: Option<&str>,
    _event_cache_store_lock_holder_name: &str,
) -> Result<StoreConfig, ClientBuildError> {
    panic!("the IndexedDB is only available on the 'wasm32' arch")
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
        cache_path: Option<std::path::PathBuf>,
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

    /// Error when building the sliding sync version.
    #[cfg(feature = "experimental-sliding-sync")]
    #[error(transparent)]
    SlidingSyncVersion(#[from] crate::sliding_sync::VersionBuilderError),

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

// The http mocking library is not supported for wasm32
#[cfg(all(test, not(target_arch = "wasm32")))]
pub(crate) mod tests {
    use assert_matches::assert_matches;
    use matrix_sdk_test::{async_test, test_json};
    use serde_json::{json_internal, Value as JsonValue};
    #[cfg(feature = "experimental-sliding-sync")]
    use url::Url;
    use wiremock::{
        matchers::{method, path},
        Mock, MockServer, ResponseTemplate,
    };

    use super::*;
    #[cfg(feature = "experimental-sliding-sync")]
    use crate::sliding_sync::Version as SlidingSyncVersion;

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
        let mut builder = ClientBuilder::new();

        // When building a client with an invalid server name.
        builder = builder.server_name_or_homeserver_url("⚠️ This won't work 🚫");
        let error = builder.build().await.unwrap_err();

        // Then the operation should fail due to the invalid server name.
        assert_matches!(error, ClientBuildError::InvalidServerName);
    }

    #[async_test]
    async fn test_discovery_no_server() {
        // Given a new client builder.
        let mut builder = ClientBuilder::new();

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
        let mut builder = ClientBuilder::new();

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
        let mut builder = ClientBuilder::new();

        // When building a client with the server's URL.
        builder = builder.server_name_or_homeserver_url(homeserver.uri());
        let _client = builder.build().await.unwrap();

        // Then a client should be built with native support for sliding sync.
        #[cfg(feature = "experimental-sliding-sync")]
        assert!(_client.sliding_sync_version().is_native());
    }

    #[async_test]
    async fn test_discovery_direct_legacy_custom_proxy() {
        // Given a homeserver without a well-known file and with a custom sliding sync
        // proxy injected.
        let homeserver = make_mock_homeserver().await;
        let mut builder = ClientBuilder::new();
        #[cfg(feature = "experimental-sliding-sync")]
        let url = {
            let url = Url::parse("https://localhost:1234").unwrap();
            builder = builder.sliding_sync_version_builder(SlidingSyncVersionBuilder::Proxy {
                url: url.clone(),
            });

            url
        };

        // When building a client with the server's URL.
        builder = builder.server_name_or_homeserver_url(homeserver.uri());
        let _client = builder.build().await.unwrap();

        // Then a client should be built with support for sliding sync.
        #[cfg(feature = "experimental-sliding-sync")]
        assert_matches!(
            _client.sliding_sync_version(),
            SlidingSyncVersion::Proxy { url: given_url } => {
                assert_eq!(given_url, url);
            }
        );
    }

    #[async_test]
    async fn test_discovery_well_known_parse_error() {
        // Given a base server with a well-known file that has errors.
        let server = MockServer::start().await;
        let homeserver = make_mock_homeserver().await;
        let mut builder = ClientBuilder::new();

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
        let mut builder = ClientBuilder::new();

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

        // Then a client should be built with native support for sliding sync.
        // It's native support because it's the default. Nothing is checked here.
        #[cfg(feature = "experimental-sliding-sync")]
        assert!(_client.sliding_sync_version().is_native());
    }

    #[async_test]
    #[cfg(feature = "experimental-sliding-sync")]
    async fn test_discovery_well_known_with_sliding_sync() {
        // Given a base server with a well-known file that points to a homeserver with a
        // sliding sync proxy.
        let server = MockServer::start().await;
        let homeserver = make_mock_homeserver().await;
        let mut builder = ClientBuilder::new();

        Mock::given(method("GET"))
            .and(path("/.well-known/matrix/client"))
            .respond_with(ResponseTemplate::new(200).set_body_json(make_well_known_json(
                &homeserver.uri(),
                Some("https://localhost:1234"),
            )))
            .mount(&server)
            .await;

        // When building a client with the base server, with sliding sync to
        // auto-discover the proxy.
        builder = builder
            .server_name_or_homeserver_url(server.uri())
            .sliding_sync_version_builder(SlidingSyncVersionBuilder::DiscoverProxy);
        let _client = builder.build().await.unwrap();

        // Then a client should be built with support for sliding sync.
        #[cfg(feature = "experimental-sliding-sync")]
        assert_matches!(
            _client.sliding_sync_version(),
            SlidingSyncVersion::Proxy { url } => {
                assert_eq!(url, Url::parse("https://localhost:1234").unwrap());
            }
        );
    }

    #[async_test]
    #[cfg(feature = "experimental-sliding-sync")]
    async fn test_discovery_well_known_with_sliding_sync_override() {
        // Given a base server with a well-known file that points to a homeserver with a
        // sliding sync proxy.
        let server = MockServer::start().await;
        let homeserver = make_mock_homeserver().await;
        let mut builder = ClientBuilder::new();

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
        let url = Url::parse("https://localhost:9012").unwrap();

        builder = builder
            .sliding_sync_version_builder(SlidingSyncVersionBuilder::Proxy { url: url.clone() })
            .server_name_or_homeserver_url(server.uri());

        let client = builder.build().await.unwrap();

        // Then a client should be built and configured with the custom sliding sync
        // proxy.
        assert_matches!(
            client.sliding_sync_version(),
            SlidingSyncVersion::Proxy { url: given_url } => {
                assert_eq!(url, given_url);
            }
        );
    }

    #[async_test]
    #[cfg(feature = "experimental-sliding-sync")]
    async fn test_sliding_sync_discover_proxy() {
        // Given a homeserver with a `.well-known` file.
        let homeserver = make_mock_homeserver().await;
        let mut builder = ClientBuilder::new();

        let expected_url = Url::parse("https://localhost:1234").unwrap();

        Mock::given(method("GET"))
            .and(path("/.well-known/matrix/client"))
            .respond_with(ResponseTemplate::new(200).set_body_json(make_well_known_json(
                &homeserver.uri(),
                Some(expected_url.as_str()),
            )))
            .mount(&homeserver)
            .await;

        // When building the client with sliding sync to auto-discover the
        // proxy version.
        builder = builder
            .server_name_or_homeserver_url(homeserver.uri())
            .sliding_sync_version_builder(SlidingSyncVersionBuilder::DiscoverProxy);

        let client = builder.build().await.unwrap();

        // Then, sliding sync has the correct proxy URL.
        assert_matches!(
            client.sliding_sync_version(),
            SlidingSyncVersion::Proxy { url } => {
                assert_eq!(url, expected_url);
            }
        );
    }

    #[async_test]
    #[cfg(feature = "experimental-sliding-sync")]
    async fn test_sliding_sync_discover_native() {
        // Given a homeserver with a `/versions` file.
        let homeserver = make_mock_homeserver().await;
        let mut builder = ClientBuilder::new();

        // When building the client with sliding sync to auto-discover the
        // native version.
        builder = builder
            .server_name_or_homeserver_url(homeserver.uri())
            .sliding_sync_version_builder(SlidingSyncVersionBuilder::DiscoverNative);

        let client = builder.build().await.unwrap();

        // Then, sliding sync has the correct native version.
        assert_matches!(client.sliding_sync_version(), SlidingSyncVersion::Native);
    }

    #[async_test]
    #[cfg(feature = "e2e-encryption")]
    async fn test_set_up_decryption_trust_requirement_cross_signed() {
        let homeserver = make_mock_homeserver().await;
        let builder = ClientBuilder::new()
            .server_name_or_homeserver_url(homeserver.uri())
            .with_decryption_trust_requirement(TrustRequirement::CrossSigned);

        let client = builder.build().await.unwrap();
        assert_matches!(
            client.base_client().decryption_trust_requirement,
            TrustRequirement::CrossSigned
        );
    }

    #[async_test]
    #[cfg(feature = "e2e-encryption")]
    async fn test_set_up_decryption_trust_requirement_untrusted() {
        let homeserver = make_mock_homeserver().await;

        let builder = ClientBuilder::new()
            .server_name_or_homeserver_url(homeserver.uri())
            .with_decryption_trust_requirement(TrustRequirement::Untrusted);

        let client = builder.build().await.unwrap();
        assert_matches!(
            client.base_client().decryption_trust_requirement,
            TrustRequirement::Untrusted
        );
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

    #[async_test]
    async fn test_cross_process_store_locks_holder_name() {
        {
            let homeserver = make_mock_homeserver().await;
            let client =
                ClientBuilder::new().homeserver_url(homeserver.uri()).build().await.unwrap();

            assert_eq!(client.cross_process_store_locks_holder_name(), "main");
        }

        {
            let homeserver = make_mock_homeserver().await;
            let client = ClientBuilder::new()
                .homeserver_url(homeserver.uri())
                .cross_process_store_locks_holder_name("foo".to_owned())
                .build()
                .await
                .unwrap();

            assert_eq!(client.cross_process_store_locks_holder_name(), "foo");
        }
    }
}
