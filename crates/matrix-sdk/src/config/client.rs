// Copyright 2021 The Matrix.org Foundation C.I.C.
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

#[allow(unused_imports)]
use std::{
    fmt::{self, Debug},
    path::Path,
    sync::Arc,
};

use http::header::InvalidHeaderValue;
use matrix_sdk_base::StateStore;

use crate::{
    config::{RequestConfig, StoreConfig},
    HttpSend, Result,
};

/// Configuration for the creation of the `Client`.
///
/// When setting the `StateStore` it is up to the user to open/connect
/// the storage backend before client creation.
///
/// # Example
///
/// ```
/// use matrix_sdk::config::ClientConfig;
/// // To pass all the request through mitmproxy set the proxy and disable SSL
/// // verification
///
/// # futures::executor::block_on(async {
/// let client_config = ClientConfig::new()
///     .proxy("http://localhost:8080")?
///     .disable_ssl_verification();
/// # matrix_sdk::Result::<()>::Ok(())
/// # });
/// ```
///
/// # Example for using a custom client
/// Note: setting a custom client will ignore `user_agent`, `proxy`, and
/// `disable_ssl_verification` - you'd need to set these yourself if you
/// want them.
///
/// ```
/// use matrix_sdk::config::ClientConfig;
/// use reqwest::ClientBuilder;
/// use std::sync::Arc;
///
/// // setting up a custom builder
/// let builder = ClientBuilder::new()
///     .https_only(true)
///     .no_proxy()
///     .user_agent("MyApp/v3.0");
///
/// # futures::executor::block_on(async {
/// let client_config = ClientConfig::new()
///     .client(Arc::new(builder.build()?));
/// # matrix_sdk::Result::<()>::Ok(())
/// # });
/// ```
#[derive(Default)]
pub struct ClientConfig {
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) proxy: Option<reqwest::Proxy>,
    pub(crate) user_agent: Option<String>,
    pub(crate) disable_ssl_verification: bool,
    pub(crate) store_config: StoreConfig,
    pub(crate) request_config: RequestConfig,
    pub(crate) client: Option<Arc<dyn HttpSend>>,
    pub(crate) appservice_mode: bool,
    pub(crate) use_discovery_response: bool,
}

#[cfg(not(tarpaulin_include))]
impl Debug for ClientConfig {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut res = fmt.debug_struct("ClientConfig");

        #[cfg(not(target_arch = "wasm32"))]
        let res = res.field("proxy", &self.proxy);

        res.field("user_agent", &self.user_agent)
            .field("disable_ssl_verification", &self.disable_ssl_verification)
            .field("request_config", &self.request_config)
            .finish()
    }
}

impl ClientConfig {
    /// Create a new default `ClientConfig`.
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a new `ClientConfig` with the given [`StoreConfig`].
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
    /// # futures::executor::block_on(async {
    /// # use matrix_sdk_base::store::MemoryStore;
    /// # let custom_state_store = Box::new(MemoryStore::new());
    /// use matrix_sdk::{Client, config::{ClientConfig, StoreConfig}};
    ///
    /// let store_config = StoreConfig::new().state_store(custom_state_store);
    /// let client_config = ClientConfig::new()
    ///     .store_config(store_config)
    ///     .use_discovery_response();
    ///
    /// # Result::<_, matrix_sdk::Error>::Ok(())
    /// # });
    /// ```
    /// [`make_store_config`]: crate::store::make_store_config
    /// [`store`]: crate::store
    pub fn store_config(mut self, store_config: StoreConfig) -> Self {
        self.store_config = store_config;
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
    /// use matrix_sdk::{Client, config::ClientConfig};
    ///
    /// let client_config = ClientConfig::new()
    ///     .proxy("http://localhost:8080")?;
    ///
    /// # Result::<_, matrix_sdk::Error>::Ok(())
    /// # });
    /// ```
    #[cfg(not(target_arch = "wasm32"))]
    pub fn proxy(mut self, proxy: &str) -> Result<Self> {
        self.proxy = Some(reqwest::Proxy::all(proxy)?);
        Ok(self)
    }

    /// Disable SSL verification for the HTTP requests.
    #[must_use]
    pub fn disable_ssl_verification(mut self) -> Self {
        self.disable_ssl_verification = true;
        self
    }

    /// Set a custom HTTP user agent for the client.
    pub fn user_agent(mut self, user_agent: &str) -> Result<Self, InvalidHeaderValue> {
        self.user_agent = Some(user_agent.to_owned());
        Ok(self)
    }

    /// Set a custom implementation of a `StateStore`.
    ///
    /// The state store should be opened before being set.
    pub fn state_store(mut self, store: Box<dyn StateStore>) -> Self {
        self.store_config = self.store_config.state_store(store);
        self
    }

    /// Set the default timeout, fail and retry behavior for all HTTP requests.
    #[must_use]
    pub fn request_config(mut self, request_config: RequestConfig) -> Self {
        self.request_config = request_config;
        self
    }

    /// Get the [`RequestConfig`]
    pub fn get_request_config(&self) -> &RequestConfig {
        &self.request_config
    }

    /// Specify a client to handle sending requests and receiving responses.
    ///
    /// Any type that implements the `HttpSend` trait can be used to
    /// send/receive `http` types.
    #[must_use]
    pub fn client(mut self, client: Arc<dyn HttpSend>) -> Self {
        self.client = Some(client);
        self
    }

    /// Puts the client into application service mode
    ///
    /// This is low-level functionality. For an high-level API check the
    /// `matrix_sdk_appservice` crate.
    #[cfg(feature = "appservice")]
    #[must_use]
    pub fn appservice_mode(mut self) -> Self {
        self.appservice_mode = true;
        self
    }

    /// Set a custom implementation of a `CryptoStore`.
    ///
    /// The crypto store should be opened before being set.
    #[cfg(feature = "encryption")]
    #[must_use]
    pub fn crypto_store(
        mut self,
        store: Box<dyn matrix_sdk_base::crypto::store::CryptoStore>,
    ) -> Self {
        self.store_config = self.store_config.crypto_store(store);
        self
    }

    /// Update the client's homeserver URL with the discovery information
    /// present in the login response, if any.
    #[must_use]
    pub fn use_discovery_response(mut self) -> Self {
        self.use_discovery_response = true;
        self
    }
}
