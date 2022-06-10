use std::sync::Arc;

#[cfg(target_arch = "wasm32")]
use async_once_cell::OnceCell;
use matrix_sdk_base::{locks::RwLock, store::StoreConfig, BaseClient, StateStore};
use ruma::{
    api::{client::discovery::discover_homeserver, error::FromHttpResponseError, MatrixVersion},
    OwnedServerName, ServerName, UserId,
};
use thiserror::Error;
#[cfg(not(target_arch = "wasm32"))]
use tokio::sync::OnceCell;
use url::Url;

use super::{Client, ClientInner};
use crate::{
    config::RequestConfig,
    error::RumaApiError,
    http_client::{HttpClient, HttpSend, HttpSettings},
    HttpError,
};

/// Builder that allows creating and configuring various parts of a [`Client`].
///
/// When setting the `StateStore` it is up to the user to open/connect
/// the storage backend before client creation.
///
/// # Example
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
/// use matrix_sdk::Client;
/// use std::sync::Arc;
///
/// // setting up a custom http client
/// let reqwest_builder = reqwest::ClientBuilder::new()
///     .https_only(true)
///     .no_proxy()
///     .user_agent("MyApp/v3.0");
///
/// let client_builder = Client::builder()
///     .http_client(Arc::new(reqwest_builder.build()?));
/// # anyhow::Ok(())
/// ```
#[must_use]
#[derive(Debug)]
pub struct ClientBuilder {
    homeserver_cfg: Option<HomeserverConfig>,
    http_cfg: Option<HttpConfig>,
    store_config: StoreConfig,
    request_config: RequestConfig,
    respect_login_well_known: bool,
    appservice_mode: bool,
    server_versions: Option<Arc<[MatrixVersion]>>,
}

impl ClientBuilder {
    pub(crate) fn new() -> Self {
        Self {
            homeserver_cfg: None,
            http_cfg: None,
            store_config: Default::default(),
            request_config: Default::default(),
            respect_login_well_known: true,
            appservice_mode: false,
            server_versions: None,
        }
    }

    /// Set the homeserver URL to use.
    ///
    /// This method is mutually exclusive with [`user_id()`][Self::user_id], if
    /// you set both whatever was set last will be used.
    pub fn homeserver_url(mut self, url: impl AsRef<str>) -> Self {
        self.homeserver_cfg = Some(HomeserverConfig::Url(url.as_ref().to_owned()));
        self
    }

    /// Set the user ID to discover the homeserver from.
    ///
    /// `builder.user_id(id)` is a shortcut for
    /// `builder.server_name(id.server_name())`.
    ///
    /// This method is mutually exclusive with
    /// [`homeserver_url()`][Self::homeserver_url], if you set both whatever was
    /// set last will be used.
    pub fn user_id(self, user_id: &UserId) -> Self {
        self.server_name(user_id.server_name())
    }

    /// Set the server name to discover the homeserver from.
    ///
    /// This method is mutually exclusive with
    /// [`homeserver_url()`][Self::homeserver_url], if you set both whatever was
    /// set last will be used.
    pub fn server_name(mut self, server_name: &ServerName) -> Self {
        self.homeserver_cfg = Some(HomeserverConfig::ServerName(server_name.to_owned()));
        self
    }

    /// Create a new `ClientBuilder` with the given [`StoreConfig`].
    ///
    /// The easiest way to get a [`StoreConfig`] is to use the
    /// [`make_store_config`] method from the [`store`] module or directly from
    /// one of the store crates.
    ///
    /// # Arguments
    ///
    /// * `store_config` - The configuration of the store.
    ///
    /// # Example
    ///
    /// ```
    /// # use matrix_sdk_base::store::MemoryStore;
    /// # let custom_state_store = Box::new(MemoryStore::new());
    /// use matrix_sdk::{Client, config::StoreConfig};
    ///
    /// let store_config = StoreConfig::new().state_store(custom_state_store);
    /// let client_builder = Client::builder().store_config(store_config);
    /// ```
    /// [`make_store_config`]: crate::store::make_store_config
    /// [`store`]: crate::store
    pub fn store_config(mut self, store_config: StoreConfig) -> Self {
        self.store_config = store_config;
        self
    }

    /// Set a custom implementation of a `StateStore`.
    ///
    /// The state store should be opened before being set.
    pub fn state_store(mut self, store: Box<dyn StateStore>) -> Self {
        self.store_config = self.store_config.state_store(store);
        self
    }

    /// Set a custom implementation of a `CryptoStore`.
    ///
    /// The crypto store should be opened before being set.
    #[cfg(feature = "e2e-encryption")]
    pub fn crypto_store(
        mut self,
        store: Box<dyn matrix_sdk_base::crypto::store::CryptoStore>,
    ) -> Self {
        self.store_config = self.store_config.crypto_store(store);
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
    /// # Example
    ///
    /// ```
    /// # futures::executor::block_on(async {
    /// use matrix_sdk::Client;
    ///
    /// let client_config = Client::builder()
    ///     .proxy("http://localhost:8080");
    ///
    /// # anyhow::Ok(())
    /// # });
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

    /// Specify an HTTP client to handle sending requests and receiving
    /// responses.
    ///
    /// Any type that implements the `HttpSend` trait can be used to send /
    /// receive `http` types.
    ///
    /// This method is mutually exclusive with
    /// [`user_agent()`][Self::user_agent],
    pub fn http_client(mut self, client: Arc<dyn HttpSend>) -> Self {
        self.http_cfg = Some(HttpConfig::Custom(client));
        self
    }

    /// Puts the client into application service mode
    ///
    /// This is low-level functionality. For an high-level API check the
    /// `matrix_sdk_appservice` crate.
    #[doc(hidden)]
    #[cfg(feature = "appservice")]
    pub fn appservice_mode(mut self) -> Self {
        self.appservice_mode = true;
        self
    }

    /// All outgoing http requests will have a GET query key-value appended with
    /// `user_id` being the key and the `user_id` from the `Session` being
    /// the value. Will error if there's no `Session`. This is called
    /// [identity assertion] in the Matrix Application Service Spec
    ///
    /// [identity assertion]: https://spec.matrix.org/unstable/application-service-api/#identity-assertion
    #[doc(hidden)]
    #[cfg(feature = "appservice")]
    pub fn assert_identity(mut self) -> Self {
        self.request_config.assert_identity = true;
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
    pub async fn build(self) -> Result<Client, ClientBuildError> {
        let homeserver_cfg = self.homeserver_cfg.ok_or(ClientBuildError::MissingHomeserver)?;

        let inner_http_client = match self.http_cfg.unwrap_or_default() {
            #[allow(unused_mut)]
            HttpConfig::Settings(mut settings) => {
                #[cfg(not(target_arch = "wasm32"))]
                {
                    settings.timeout = self.request_config.timeout;
                }

                Arc::new(settings.make_client()?)
            }
            HttpConfig::Custom(c) => c,
        };

        let base_client = BaseClient::with_store_config(self.store_config);
        let http_client = HttpClient::new(inner_http_client.clone(), self.request_config);

        let homeserver = match homeserver_cfg {
            HomeserverConfig::Url(url) => url,
            HomeserverConfig::ServerName(server_name) => {
                let homeserver = homeserver_from_name(&server_name);
                let well_known = http_client
                    .send(
                        discover_homeserver::Request::new(),
                        None,
                        homeserver,
                        None,
                        [MatrixVersion::V1_0].into_iter().collect(),
                    )
                    .await
                    .map_err(|e| match e {
                        HttpError::Api(err) => ClientBuildError::AutoDiscovery(err),
                        err => ClientBuildError::Http(err),
                    })?;

                well_known.homeserver.base_url
            }
        };

        let homeserver = RwLock::new(Url::parse(&homeserver)?);

        let inner = Arc::new(ClientInner {
            homeserver,
            http_client,
            base_client,
            server_versions: OnceCell::new_with(self.server_versions),
            #[cfg(feature = "e2e-encryption")]
            group_session_locks: Default::default(),
            #[cfg(feature = "e2e-encryption")]
            key_claim_lock: Default::default(),
            members_request_locks: Default::default(),
            typing_notice_times: Default::default(),
            event_handlers: Default::default(),
            event_handler_data: Default::default(),
            notification_handlers: Default::default(),
            appservice_mode: self.appservice_mode,
            respect_login_well_known: self.respect_login_well_known,
            sync_beat: event_listener::Event::new(),
        });

        Ok(Client { inner })
    }
}

fn homeserver_from_name(server_name: &ServerName) -> String {
    #[cfg(not(test))]
    return format!("https://{}", server_name);

    // Mockito only knows how to test http endpoints:
    // https://github.com/lipanski/mockito/issues/127
    #[cfg(test)]
    return format!("http://{}", server_name);
}

#[derive(Debug)]
enum HomeserverConfig {
    Url(String),
    ServerName(OwnedServerName),
}

#[derive(Debug)]
enum HttpConfig {
    Settings(HttpSettings),
    Custom(Arc<dyn HttpSend>),
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
        Self::Settings(HttpSettings::default())
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

    /// Error opening the sled store.
    #[cfg(feature = "sled")]
    #[error(transparent)]
    SledStore(#[from] matrix_sdk_sled::OpenStoreError),
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
