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

//! High-level OAuth 2.0 API.
//!
//! The OAuth 2.0 interactions with the Matrix API are currently a
//! work-in-progress and are defined by [MSC3861] and its sub-proposals. And
//! more documentation is available at [areweoidcyet.com].
//!
//! This authentication API is available with [`Client::oauth()`].
//!
//! # Homeserver support
//!
//! After building the client, you can check that the homeserver supports
//! logging in via OAuth 2.0 when [`OAuth::server_metadata()`] succeeds.
//!
//! # Registration
//!
//! Registration is only required the first time a client encounters a
//! homeserver.
//!
//! Note that only public clients are supported by this API, i.e. clients
//! without credentials.
//!
//! If the server supports dynamic registration, it can be done by using
//! [`OAuth::register_client()`]. After registration, the client ID should be
//! persisted and reused for every session that interacts with that same server.
//!
//! If dynamic registration is not available, the homeserver should document how
//! to obtain a client ID.
//!
//! To provide the client ID and metadata if dynamic registration is not
//! available, or if the client is already registered with the issuer, call
//! [`OAuth::restore_registered_client()`].
//!
//! # Login
//!
//! Before logging in, make sure to register the client or to restore its
//! registration, as it is the first step to know how to interact with the
//! issuer.
//!
//! With OAuth 2.0, logging into a Matrix account is simply logging in with a
//! predefined scope, part of it declaring the device ID of the session.
//!
//! [`OAuth::login()`] constructs an [`OAuthAuthCodeUrlBuilder`] that can be
//! configured, and then calling [`OAuthAuthCodeUrlBuilder::build()`] will
//! provide the URL to present to the user in a web browser. After
//! authenticating with the server, the user will be redirected to the provided
//! redirect URI, with a code in the query that will allow to finish the
//! login process by calling [`OAuth::finish_login()`].
//!
//! # Persisting/restoring a session
//!
//! A full OAuth 2.0 session requires two parts:
//!
//! - The client ID obtained after client registration,
//! - The user session obtained after login.
//!
//! Both parts are usually stored separately because the client ID can be reused
//! for any session with the same server, while the user session is unique.
//!
//! _Note_ that the type returned by [`OAuth::full_session()`] is not
//! (de)serializable. This is done on purpose because the client ID and metadata
//! should be stored separately than the user session, as they should be reused
//! for the same homeserver across different user sessions.
//!
//! To restore a previous session, use [`OAuth::restore_session()`].
//!
//! # Refresh tokens
//!
//! The use of refresh tokens with OAuth 2.0 servers is more common than in the
//! Matrix specification. For this reason, it is recommended to configure the
//! client with [`ClientBuilder::handle_refresh_tokens()`], to handle refreshing
//! tokens automatically.
//!
//! Applications should then listen to session tokens changes after logging in
//! with [`Client::subscribe_to_session_changes()`] to be able to restore the
//! session at a later time, otherwise the end-user will need to login again.
//!
//! # Unknown token error
//!
//! A request to the Matrix API can return an [`Error`] with an
//! [`ErrorKind::UnknownToken`].
//!
//! The first step is to try to refresh the token with
//! [`OAuth::refresh_access_token()`]. This step is done automatically if the
//! client was built with [`ClientBuilder::handle_refresh_tokens()`].
//!
//! If refreshing the access token fails, the next step is to try to request a
//! new login authorization with [`OAuth::login()`], using the device ID from
//! the session.
//!
//! If this fails again, the client should assume to be logged out, and all
//! local data should be erased.
//!
//! # Account management.
//!
//! The server might advertise a URL that allows the user to manage their
//! account, it can be obtained with [`OAuth::account_management_url()`].
//!
//! # Logout
//!
//! To log the [`Client`] out of the session, simply call [`OAuth::logout()`].
//!
//! # Examples
//!
//! Most methods have examples, there is also an example CLI application that
//! supports all the actions described here, in [`examples/oauth_cli`].
//!
//! [MSC3861]: https://github.com/matrix-org/matrix-spec-proposals/pull/3861
//! [areweoidcyet.com]: https://areweoidcyet.com/
//! [`ClientBuilder::handle_refresh_tokens()`]: crate::ClientBuilder::handle_refresh_tokens()
//! [`Error`]: ruma::api::client::error::Error
//! [`ErrorKind::UnknownToken`]: ruma::api::client::error::ErrorKind::UnknownToken
//! [`examples/oauth_cli`]: https://github.com/matrix-org/matrix-rust-sdk/tree/main/examples/oauth_cli

use std::{
    borrow::Cow,
    collections::{BTreeSet, HashMap},
    fmt,
    sync::Arc,
};

use as_variant::as_variant;
#[cfg(feature = "e2e-encryption")]
use error::CrossProcessRefreshLockError;
use error::{
    OAuthAuthorizationCodeError, OAuthClientRegistrationError, OAuthDiscoveryError,
    OAuthTokenRevocationError, RedirectUriQueryParseError,
};
#[cfg(all(feature = "e2e-encryption", not(target_arch = "wasm32")))]
use matrix_sdk_base::crypto::types::qr_login::QrCodeData;
#[cfg(feature = "e2e-encryption")]
use matrix_sdk_base::once_cell::sync::OnceCell;
use matrix_sdk_base::{store::RoomLoadSettings, SessionMeta};
use oauth2::{
    basic::BasicClient as OAuthClient, AccessToken, PkceCodeVerifier, RedirectUrl, RefreshToken,
    RevocationUrl, Scope, StandardErrorResponse, StandardRevocableToken, TokenResponse, TokenUrl,
};
pub use oauth2::{ClientId, CsrfToken};
use ruma::{
    api::client::discovery::{
        get_authentication_issuer,
        get_authorization_server_metadata::{
            self,
            msc2965::{AccountManagementAction, AuthorizationServerMetadata},
        },
    },
    serde::Raw,
    DeviceId, OwnedDeviceId,
};
use serde::{Deserialize, Serialize};
use sha2::Digest as _;
use tokio::sync::Mutex;
use tracing::{debug, error, instrument, trace, warn};
use url::Url;

mod account_management_url;
mod auth_code_builder;
#[cfg(feature = "e2e-encryption")]
mod cross_process;
pub mod error;
mod http_client;
mod oidc_discovery;
#[cfg(all(feature = "e2e-encryption", not(target_arch = "wasm32")))]
pub mod qrcode;
pub mod registration;
#[cfg(not(target_arch = "wasm32"))]
mod registration_store;
#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests;

#[cfg(feature = "e2e-encryption")]
use self::cross_process::{CrossProcessRefreshLockGuard, CrossProcessRefreshManager};
#[cfg(all(feature = "e2e-encryption", not(target_arch = "wasm32")))]
use self::qrcode::LoginWithQrCode;
#[cfg(not(target_arch = "wasm32"))]
pub use self::registration_store::OAuthRegistrationStore;
pub use self::{
    account_management_url::{AccountManagementActionFull, AccountManagementUrlBuilder},
    auth_code_builder::{OAuthAuthCodeUrlBuilder, OAuthAuthorizationData},
    error::OAuthError,
};
use self::{
    http_client::OAuthHttpClient,
    oidc_discovery::discover,
    registration::{register_client, ClientMetadata, ClientRegistrationResponse},
};
use super::{AuthData, SessionTokens};
use crate::{client::SessionChange, executor::spawn, Client, HttpError, RefreshTokenError, Result};

pub(crate) struct OAuthCtx {
    /// Lock and state when multiple processes may refresh an OAuth 2.0 session.
    #[cfg(feature = "e2e-encryption")]
    cross_process_token_refresh_manager: OnceCell<CrossProcessRefreshManager>,

    /// Deferred cross-process lock initializer.
    ///
    /// Note: only required because we're using the crypto store that might not
    /// be present before reloading a session.
    #[cfg(feature = "e2e-encryption")]
    deferred_cross_process_lock_init: Mutex<Option<String>>,

    /// Whether to allow HTTP issuer URLs.
    insecure_discover: bool,
}

impl OAuthCtx {
    pub(crate) fn new(insecure_discover: bool) -> Self {
        Self {
            insecure_discover,
            #[cfg(feature = "e2e-encryption")]
            cross_process_token_refresh_manager: Default::default(),
            #[cfg(feature = "e2e-encryption")]
            deferred_cross_process_lock_init: Default::default(),
        }
    }
}

pub(crate) struct OAuthAuthData {
    pub(crate) issuer: Url,
    pub(crate) client_id: ClientId,
    /// The data necessary to validate authorization responses.
    authorization_data: Mutex<HashMap<CsrfToken, AuthorizationValidationData>>,
}

#[cfg(not(tarpaulin_include))]
impl fmt::Debug for OAuthAuthData {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OAuthAuthData")
            .field("issuer", &self.issuer.as_str())
            .finish_non_exhaustive()
    }
}

/// A high-level authentication API to interact with an OAuth 2.0 authorization
/// server.
#[derive(Debug, Clone)]
pub struct OAuth {
    /// The underlying Matrix API client.
    client: Client,
    /// The HTTP client used for making OAuth 2.0 request.
    http_client: OAuthHttpClient,
}

impl OAuth {
    pub(crate) fn new(client: Client) -> Self {
        let http_client = OAuthHttpClient {
            inner: client.inner.http_client.inner.clone(),
            #[cfg(test)]
            insecure_rewrite_https_to_http: false,
        };
        Self { client, http_client }
    }

    /// Rewrite HTTPS requests to use HTTP instead.
    ///
    /// This is a workaround to bypass some checks that require an HTTPS URL,
    /// but we can only mock HTTP URLs.
    #[cfg(test)]
    pub(crate) fn insecure_rewrite_https_to_http(mut self) -> Self {
        self.http_client.insecure_rewrite_https_to_http = true;
        self
    }

    fn ctx(&self) -> &OAuthCtx {
        &self.client.auth_ctx().oauth
    }

    fn http_client(&self) -> &OAuthHttpClient {
        &self.http_client
    }

    /// Enable a cross-process store lock on the state store, to coordinate
    /// refreshes across different processes.
    #[cfg(feature = "e2e-encryption")]
    pub async fn enable_cross_process_refresh_lock(
        &self,
        lock_value: String,
    ) -> Result<(), OAuthError> {
        // FIXME: it must be deferred only because we're using the crypto store and it's
        // initialized only in `set_or_reload_session`, not if we use a dedicated store.
        let mut lock = self.ctx().deferred_cross_process_lock_init.lock().await;
        if lock.is_some() {
            return Err(CrossProcessRefreshLockError::DuplicatedLock.into());
        }
        *lock = Some(lock_value);

        Ok(())
    }

    /// Performs a deferred cross-process refresh-lock, if needs be, after an
    /// olm machine has been initialized.
    ///
    /// Must be called after [`BaseClient::set_or_reload_session`].
    #[cfg(feature = "e2e-encryption")]
    async fn deferred_enable_cross_process_refresh_lock(&self) {
        let deferred_init_lock = self.ctx().deferred_cross_process_lock_init.lock().await;

        // Don't `take()` the value, so that subsequent calls to
        // `enable_cross_process_refresh_lock` will keep on failing if we've enabled the
        // lock at least once.
        let Some(lock_value) = deferred_init_lock.as_ref() else {
            return;
        };

        // FIXME: We shouldn't be using the crypto store for that! see also https://github.com/matrix-org/matrix-rust-sdk/issues/2472
        let olm_machine_lock = self.client.olm_machine().await;
        let olm_machine =
            olm_machine_lock.as_ref().expect("there has to be an olm machine, hopefully?");
        let store = olm_machine.store();
        let lock =
            store.create_store_lock("oidc_session_refresh_lock".to_owned(), lock_value.clone());

        let manager = CrossProcessRefreshManager::new(store.clone(), lock);

        // This method is guarded with the `deferred_cross_process_lock_init` lock held,
        // so this `set` can't be an error.
        let _ = self.ctx().cross_process_token_refresh_manager.set(manager);
    }

    /// The OAuth 2.0 authentication data.
    ///
    /// Returns `None` if the client was not registered or if the registration
    /// was not restored with [`OAuth::restore_registered_client()`] or
    /// [`OAuth::restore_session()`].
    fn data(&self) -> Option<&OAuthAuthData> {
        let data = self.client.auth_ctx().auth_data.get()?;
        as_variant!(data, AuthData::OAuth)
    }

    /// Log in using a QR code.
    ///
    /// This method allows you to log in with a QR code, the existing device
    /// needs to display the QR code which this device can scan and call
    /// this method to log in.
    ///
    /// A successful login using this method will automatically mark the device
    /// as verified and transfer all end-to-end encryption related secrets, like
    /// the private cross-signing keys and the backup key from the existing
    /// device to the new device.
    ///
    /// # Example
    ///
    /// ```no_run
    /// use anyhow::bail;
    /// use futures_util::StreamExt;
    /// use matrix_sdk::{
    ///     authentication::oauth::{
    ///         registration::ClientMetadata,
    ///         qrcode::{LoginProgress, QrCodeData, QrCodeModeData},
    ///     },
    ///     ruma::serde::Raw,
    ///     Client,
    /// };
    /// # fn client_metadata() -> Raw<ClientMetadata> { unimplemented!() }
    /// # _ = async {
    /// # let bytes = unimplemented!();
    /// // You'll need to use a different library to scan and extract the raw bytes from the QR
    /// // code.
    /// let qr_code_data = QrCodeData::from_bytes(bytes)?;
    ///
    /// // Fetch the homeserver out of the parsed QR code data.
    /// let QrCodeModeData::Reciprocate{ server_name } = qr_code_data.mode_data else {
    ///     bail!("The QR code is invalid, we did not receive a homeserver in the QR code.");
    /// };
    ///
    /// // Build the client as usual.
    /// let client = Client::builder()
    ///     .server_name_or_homeserver_url(server_name)
    ///     .handle_refresh_tokens()
    ///     .build()
    ///     .await?;
    ///
    /// let oauth = client.oauth();
    /// let metadata: Raw<ClientMetadata> = client_metadata();
    ///
    /// // Subscribing to the progress is necessary since we need to input the check
    /// // code on the existing device.
    /// let login = oauth.login_with_qr_code(&qr_code_data, metadata.into());
    /// let mut progress = login.subscribe_to_progress();
    ///
    /// // Create a task which will show us the progress and tell us the check
    /// // code to input in the existing device.
    /// let task = tokio::spawn(async move {
    ///     while let Some(state) = progress.next().await {
    ///         match state {
    ///             LoginProgress::Starting => (),
    ///             LoginProgress::EstablishingSecureChannel { check_code } => {
    ///                 let code = check_code.to_digit();
    ///                 println!("Please enter the following code into the other device {code:02}");
    ///             },
    ///             LoginProgress::WaitingForToken { user_code } => {
    ///                 println!("Please use your other device to confirm the log in {user_code}")
    ///             },
    ///             LoginProgress::Done => break,
    ///         }
    ///     }
    /// });
    ///
    /// // Now run the future to complete the login.
    /// login.await?;
    /// task.abort();
    ///
    /// println!("Successfully logged in: {:?} {:?}", client.user_id(), client.device_id());
    /// # anyhow::Ok(()) };
    /// ```
    #[cfg(all(feature = "e2e-encryption", not(target_arch = "wasm32")))]
    pub fn login_with_qr_code<'a>(
        &'a self,
        data: &'a QrCodeData,
        registration_method: ClientRegistrationMethod,
    ) -> LoginWithQrCode<'a> {
        LoginWithQrCode::new(&self.client, registration_method, data)
    }

    /// Restore or register the OAuth 2.0 client for the server with the given
    /// metadata, with the given [`OAuthRegistrationStore`].
    ///
    /// If there is a client ID in the store, it is used to restore the client.
    /// Otherwise, the client is registered with the metadata in the store.
    ///
    /// Returns an error if there is an error while accessing the store, or
    /// while registering the client.
    #[cfg(not(target_arch = "wasm32"))]
    async fn restore_or_register_client(
        &self,
        server_metadata: &AuthorizationServerMetadata,
        registrations: &OAuthRegistrationStore,
    ) -> std::result::Result<(), OAuthClientRegistrationError> {
        if let Some(client_id) = registrations.client_id(&server_metadata.issuer).await? {
            self.restore_registered_client(server_metadata.issuer.clone(), client_id);

            tracing::info!("OAuth 2.0 configuration loaded from disk.");
            return Ok(());
        };

        tracing::info!("Registering this client for OAuth 2.0.");
        let response = self.register_client_inner(server_metadata, &registrations.metadata).await?;

        tracing::info!("Persisting OAuth 2.0 registration data.");
        registrations
            .set_and_write_client_id(response.client_id, server_metadata.issuer.clone())
            .await?;

        Ok(())
    }

    /// Restore or register the OAuth 2.0 client for the server with the given
    /// metadata, with the given [`ClientRegistrationMethod`].
    ///
    /// If we already have a client ID, this is a noop.
    ///
    /// Returns an error if there was a problem using the registration method.
    async fn use_registration_method(
        &self,
        server_metadata: &AuthorizationServerMetadata,
        method: &ClientRegistrationMethod,
    ) -> std::result::Result<(), OAuthError> {
        if self.client_id().is_some() {
            tracing::info!("OAuth 2.0 is already configured.");
            return Ok(());
        };

        match method {
            ClientRegistrationMethod::None => return Err(OAuthError::NotRegistered),
            ClientRegistrationMethod::ClientId(client_id) => {
                self.restore_registered_client(server_metadata.issuer.clone(), client_id.clone());
            }
            ClientRegistrationMethod::Metadata(client_metadata) => {
                self.register_client_inner(server_metadata, client_metadata).await?;
            }
            #[cfg(not(target_arch = "wasm32"))]
            ClientRegistrationMethod::Store(registrations) => {
                self.restore_or_register_client(server_metadata, registrations).await?
            }
        }

        Ok(())
    }

    /// The OAuth 2.0 authorization server used for authorization.
    ///
    /// Returns `None` if the client was not registered or if the registration
    /// was not restored with [`OAuth::restore_registered_client()`] or
    /// [`OAuth::restore_session()`].
    pub fn issuer(&self) -> Option<&Url> {
        self.data().map(|data| &data.issuer)
    }

    /// The account management actions supported by the authorization server's
    /// account management URL.
    ///
    /// Returns `Ok(None)` if the data was not found. Returns an error if the
    /// request to get the server metadata fails.
    pub async fn account_management_actions_supported(
        &self,
    ) -> Result<BTreeSet<AccountManagementAction>, OAuthError> {
        let server_metadata = self.server_metadata().await?;

        Ok(server_metadata.account_management_actions_supported)
    }

    /// Build the URL where the user can manage their account.
    ///
    /// This will always request the latest server metadata to get the account
    /// management URL.
    ///
    /// To avoid making a request each time, you can use
    /// [`OAuth::account_management_url()`].
    ///
    /// Returns an [`AccountManagementUrlBuilder`] if the URL was found. An
    /// optional action to perform can be added with `.action()`, and the final
    /// URL is obtained with `.build()`.
    ///
    /// Returns `Ok(None)` if the URL was not found.
    ///
    /// Returns an error if the request to get the server metadata fails or the
    /// URL could not be parsed.
    pub async fn fetch_account_management_url(
        &self,
    ) -> Result<Option<AccountManagementUrlBuilder>, OAuthError> {
        let server_metadata = self.server_metadata().await?;
        Ok(server_metadata.account_management_uri.map(AccountManagementUrlBuilder::new))
    }

    /// Get the account management URL where the user can manage their
    /// identity-related settings.
    ///
    /// This method will cache the URL for a while, if the cache is not
    /// populated it will request the server metadata, like a call to
    /// [`OAuth::fetch_account_management_url()`], and cache the resulting URL
    /// before returning it.
    ///
    /// Returns an [`AccountManagementUrlBuilder`] if the URL was found. An
    /// optional action to perform can be added with `.action()`, and the final
    /// URL is obtained with `.build()`.
    ///
    /// Returns `Ok(None)` if the URL was not found.
    ///
    /// Returns an error if the request to get the server metadata fails or the
    /// URL could not be parsed.
    pub async fn account_management_url(
        &self,
    ) -> Result<Option<AccountManagementUrlBuilder>, OAuthError> {
        const CACHE_KEY: &str = "SERVER_METADATA";

        let mut cache = self.client.inner.caches.server_metadata.lock().await;

        let metadata = if let Some(metadata) = cache.get(CACHE_KEY) {
            metadata
        } else {
            let server_metadata = self.server_metadata().await?;
            cache.insert(CACHE_KEY.to_owned(), server_metadata.clone());
            server_metadata
        };

        Ok(metadata.account_management_uri.map(AccountManagementUrlBuilder::new))
    }

    /// Discover the authentication issuer and retrieve the
    /// [`AuthorizationServerMetadata`] using the GET `/auth_metadata` endpoint
    /// defined in [MSC2965].
    ///
    /// **Note**: This endpoint is deprecated.
    ///
    /// MSC2956: https://github.com/matrix-org/matrix-spec-proposals/pull/2965
    async fn fallback_discover(
        &self,
    ) -> Result<Raw<AuthorizationServerMetadata>, OAuthDiscoveryError> {
        #[allow(deprecated)]
        let issuer =
            match self.client.send(get_authentication_issuer::msc2965::Request::new()).await {
                Ok(response) => response.issuer,
                Err(error)
                    if error
                        .as_client_api_error()
                        .is_some_and(|err| err.status_code == http::StatusCode::NOT_FOUND) =>
                {
                    return Err(OAuthDiscoveryError::NotSupported);
                }
                Err(error) => return Err(error.into()),
            };

        discover(self.http_client(), &issuer).await
    }

    /// Fetch the OAuth 2.0 authorization server metadata of the homeserver.
    ///
    /// Returns an error if a problem occurred when fetching or validating the
    /// metadata.
    pub async fn server_metadata(
        &self,
    ) -> Result<AuthorizationServerMetadata, OAuthDiscoveryError> {
        let is_endpoint_unsupported = |error: &HttpError| {
            error
                .as_client_api_error()
                .is_some_and(|err| err.status_code == http::StatusCode::NOT_FOUND)
        };

        let raw_metadata = match self
            .client
            .send(get_authorization_server_metadata::msc2965::Request::new())
            .await
        {
            Ok(response) => response.metadata,
            // If the endpoint returns a 404, i.e. the server doesn't support the endpoint, attempt
            // to use the equivalent, but deprecated, endpoint.
            Err(error) if is_endpoint_unsupported(&error) => {
                // TODO: remove this fallback behavior when the metadata endpoint has wider
                // support.
                self.fallback_discover().await?
            }
            Err(error) => return Err(error.into()),
        };

        let metadata = raw_metadata.deserialize()?;

        if self.ctx().insecure_discover {
            metadata.insecure_validate_urls()?;
        } else {
            metadata.validate_urls()?;
        }

        Ok(metadata)
    }

    /// The OAuth 2.0 unique identifier of this client obtained after
    /// registration.
    ///
    /// Returns `None` if the client was not registered or if the registration
    /// was not restored with [`OAuth::restore_registered_client()`] or
    /// [`OAuth::restore_session()`].
    pub fn client_id(&self) -> Option<&ClientId> {
        self.data().map(|data| &data.client_id)
    }

    /// The OAuth 2.0 user session of this client.
    ///
    /// Returns `None` if the client was not logged in with the OAuth 2.0 API.
    pub fn user_session(&self) -> Option<UserSession> {
        let meta = self.client.session_meta()?.to_owned();
        let tokens = self.client.session_tokens()?;
        let issuer = self.data()?.issuer.clone();
        Some(UserSession { meta, tokens, issuer })
    }

    /// The full OAuth 2.0 session of this client.
    ///
    /// Returns `None` if the client was not logged in with the OAuth 2.0 API.
    pub fn full_session(&self) -> Option<OAuthSession> {
        let user = self.user_session()?;
        let data = self.data()?;
        Some(OAuthSession { client_id: data.client_id.clone(), user })
    }

    /// Register a client with the OAuth 2.0 server.
    ///
    /// This should be called before any authorization request with an unknown
    /// authorization server. If the client is already registered with the
    /// server, it should use [`OAuth::restore_registered_client()`].
    ///
    /// Note that this method only supports public clients, i.e. clients without
    /// a secret.
    ///
    /// # Arguments
    ///
    /// * `client_metadata` - The serialized client metadata to register.
    ///
    /// The client ID in the response should be persisted for future use and
    /// reused for the same authorization server, identified by the
    /// [`OAuth::issuer()`], along with the client metadata sent to the server,
    /// even for different sessions or user accounts.
    ///
    /// # Panic
    ///
    /// Panics if the authentication data was already set.
    ///
    /// # Example
    ///
    /// ```no_run
    /// use matrix_sdk::{Client, ServerName};
    /// # use matrix_sdk::authentication::oauth::ClientId;
    /// # use matrix_sdk::authentication::oauth::registration::ClientMetadata;
    /// # use ruma::serde::Raw;
    /// # let client_metadata = unimplemented!();
    /// # fn persist_client_registration (_: &url::Url, _: &Raw<ClientMetadata>, _: &ClientId) {}
    /// # _ = async {
    /// let server_name = ServerName::parse("myhomeserver.org")?;
    /// let client = Client::builder().server_name(&server_name).build().await?;
    /// let oauth = client.oauth();
    ///
    /// if let Err(error) = oauth.server_metadata().await {
    ///     if error.is_not_supported() {
    ///         println!("OAuth 2.0 is not supported");
    ///     }
    ///
    ///     return Err(error.into());
    /// }
    ///
    /// let response = oauth
    ///     .register_client(&client_metadata)
    ///     .await?;
    ///
    /// println!(
    ///     "Registered with client_id: {}",
    ///     response.client_id.as_str()
    /// );
    ///
    /// // The API only supports clients without secrets.
    /// let client_id = response.client_id;
    /// let issuer = oauth.issuer().expect("issuer should be set after registration");
    ///
    /// persist_client_registration(issuer, &client_metadata, &client_id);
    /// # anyhow::Ok(()) };
    /// ```
    pub async fn register_client(
        &self,
        client_metadata: &Raw<ClientMetadata>,
    ) -> Result<ClientRegistrationResponse, OAuthError> {
        let server_metadata = self.server_metadata().await?;
        Ok(self.register_client_inner(&server_metadata, client_metadata).await?)
    }

    async fn register_client_inner(
        &self,
        server_metadata: &AuthorizationServerMetadata,
        client_metadata: &Raw<ClientMetadata>,
    ) -> Result<ClientRegistrationResponse, OAuthClientRegistrationError> {
        let registration_endpoint = server_metadata
            .registration_endpoint
            .as_ref()
            .ok_or(OAuthClientRegistrationError::NotSupported)?;

        let registration_response =
            register_client(self.http_client(), registration_endpoint, client_metadata).await?;

        // The format of the credentials changes according to the client metadata that
        // was sent. Public clients only get a client ID.
        self.restore_registered_client(
            server_metadata.issuer.clone(),
            registration_response.client_id.clone(),
        );

        Ok(registration_response)
    }

    /// Set the data of a client that is registered with an OAuth 2.0
    /// authorization server.
    ///
    /// This should be called when logging in with a server that is already
    /// known by the client.
    ///
    /// Note that this method only supports public clients, i.e. clients with
    /// no credentials.
    ///
    /// # Arguments
    ///
    /// * `issuer` - The authorization server that was used to register the
    ///   client.
    ///
    /// * `client_id` - The unique identifier to authenticate the client with
    ///   the server, obtained after registration.
    ///
    /// # Panic
    ///
    /// Panics if authentication data was already set.
    pub fn restore_registered_client(&self, issuer: Url, client_id: ClientId) {
        let data = OAuthAuthData { issuer, client_id, authorization_data: Default::default() };

        self.client
            .auth_ctx()
            .auth_data
            .set(AuthData::OAuth(data))
            .expect("Client authentication data was already set");
    }

    /// Restore a previously logged in session.
    ///
    /// This can be used to restore the client to a logged in state, including
    /// loading the sync state and the encryption keys from the store, if
    /// one was set up.
    ///
    /// # Arguments
    ///
    /// * `session` - The session to restore.
    /// * `room_load_settings` — Specify how many rooms must be restored; use
    ///   `::default()` if you don't know which value to pick.
    ///
    /// # Panic
    ///
    /// Panics if authentication data was already set.
    pub async fn restore_session(
        &self,
        session: OAuthSession,
        room_load_settings: RoomLoadSettings,
    ) -> Result<()> {
        let OAuthSession { client_id, user: UserSession { meta, tokens, issuer } } = session;

        let data = OAuthAuthData { issuer, client_id, authorization_data: Default::default() };

        self.client.auth_ctx().set_session_tokens(tokens.clone());
        self.client
            .base_client()
            .activate(
                meta,
                room_load_settings,
                #[cfg(feature = "e2e-encryption")]
                None,
            )
            .await?;
        #[cfg(feature = "e2e-encryption")]
        self.deferred_enable_cross_process_refresh_lock().await;

        self.client
            .inner
            .auth_ctx
            .auth_data
            .set(AuthData::OAuth(data))
            .expect("Client authentication data was already set");

        // Initialize the cross-process locking by saving our tokens' hash into the
        // database, if we've enabled the cross-process lock.

        #[cfg(feature = "e2e-encryption")]
        if let Some(cross_process_lock) = self.ctx().cross_process_token_refresh_manager.get() {
            cross_process_lock.restore_session(&tokens).await;

            let mut guard = cross_process_lock
                .spin_lock()
                .await
                .map_err(|err| crate::Error::OAuth(err.into()))?;

            // After we got the lock, it's possible that our session doesn't match the one
            // read from the database, because of a race: another process has
            // refreshed the tokens while we were waiting for the lock.
            //
            // In that case, if there's a mismatch, we reload the session and update the
            // hash. Otherwise, we save our hash into the database.

            if guard.hash_mismatch {
                Box::pin(self.handle_session_hash_mismatch(&mut guard))
                    .await
                    .map_err(|err| crate::Error::OAuth(err.into()))?;
            } else {
                guard
                    .save_in_memory_and_db(&tokens)
                    .await
                    .map_err(|err| crate::Error::OAuth(err.into()))?;
                // No need to call the save_session_callback here; it was the
                // source of the session, so it's already in
                // sync with what we had.
            }
        }

        #[cfg(feature = "e2e-encryption")]
        self.client.encryption().spawn_initialization_task(None);

        Ok(())
    }

    #[cfg(feature = "e2e-encryption")]
    async fn handle_session_hash_mismatch(
        &self,
        guard: &mut CrossProcessRefreshLockGuard,
    ) -> Result<(), CrossProcessRefreshLockError> {
        trace!("Handling hash mismatch.");

        let callback = self
            .client
            .auth_ctx()
            .reload_session_callback
            .get()
            .ok_or(CrossProcessRefreshLockError::MissingReloadSession)?;

        match callback(self.client.clone()) {
            Ok(tokens) => {
                guard.handle_mismatch(&tokens).await?;

                self.client.auth_ctx().set_session_tokens(tokens.clone());
                // The app's callback acted as authoritative here, so we're not
                // saving the data back into the app, as that would have no
                // effect.
            }
            Err(err) => {
                error!("when reloading OAuth 2.0 session tokens from callback: {err}");
            }
        }

        Ok(())
    }

    /// The scopes to request for logging in and the corresponding device ID.
    fn login_scopes(device_id: Option<OwnedDeviceId>) -> ([Scope; 2], OwnedDeviceId) {
        /// Scope to grand full access to the client-server API.
        const SCOPE_MATRIX_CLIENT_SERVER_API_FULL_ACCESS: &str =
            "urn:matrix:org.matrix.msc2967.client:api:*";
        /// Prefix of the scope to bind a device ID to an access token.
        const SCOPE_MATRIX_DEVICE_ID_PREFIX: &str = "urn:matrix:org.matrix.msc2967.client:device:";

        // Generate the device ID if it is not provided.
        let device_id = device_id.unwrap_or_else(DeviceId::new);

        (
            [
                Scope::new(SCOPE_MATRIX_CLIENT_SERVER_API_FULL_ACCESS.to_owned()),
                Scope::new(format!("{SCOPE_MATRIX_DEVICE_ID_PREFIX}{device_id}")),
            ],
            device_id,
        )
    }

    /// Login via OAuth 2.0 with the Authorization Code flow.
    ///
    /// This should be called after [`OAuth::register_client()`] or
    /// [`OAuth::restore_registered_client()`].
    ///
    /// [`OAuth::finish_login()`] must be called once the user has been
    /// redirected to the `redirect_uri`. [`OAuth::abort_login()`] should be
    /// called instead if the authorization should be aborted before completion.
    ///
    /// # Arguments
    ///
    /// * `registration_method` - The method to restore or register the client
    ///   with the server.
    ///
    /// * `redirect_uri` - The URI where the end user will be redirected after
    ///   authorizing the login. It must be one of the redirect URIs sent in the
    ///   client metadata during registration.
    ///
    /// * `device_id` - The unique ID that will be associated with the session.
    ///   If not set, a random one will be generated. It can be an existing
    ///   device ID from a previous login call. Note that this should be done
    ///   only if the client also holds the corresponding encryption keys.
    ///
    /// # Example
    ///
    /// ```no_run
    /// use anyhow::anyhow;
    /// use matrix_sdk::{
    ///     Client,
    ///     authentication::oauth::OAuthRegistrationStore,
    /// };
    /// # use ruma::serde::Raw;
    /// # use matrix_sdk::authentication::oauth::registration::ClientMetadata;
    /// # let homeserver = unimplemented!();
    /// # let redirect_uri = unimplemented!();
    /// # let issuer_info = unimplemented!();
    /// # let client_id = unimplemented!();
    /// # let store_path = unimplemented!();
    /// # async fn open_uri_and_wait_for_redirect(uri: url::Url) -> url::Url { unimplemented!() };
    /// # fn client_metadata() -> Raw<ClientMetadata> { unimplemented!() };
    /// # _ = async {
    /// # let client = Client::new(homeserver).await?;
    /// let oauth = client.oauth();
    ///
    /// let registration_store = OAuthRegistrationStore::new(
    ///     store_path,
    ///     client_metadata()
    /// ).await?;
    ///
    /// let auth_data = oauth.login(registration_store.into(), redirect_uri, None)
    ///                      .build()
    ///                      .await?;
    ///
    /// // Open auth_data.url and wait for response at the redirect URI.
    /// let redirected_to_uri = open_uri_and_wait_for_redirect(auth_data.url).await;
    ///
    /// oauth.finish_login(redirected_to_uri.into()).await?;
    ///
    /// // The session tokens can be persisted from the
    /// // `Client::session_tokens()` method.
    ///
    /// // You can now make requests to the Matrix API.
    /// let _me = client.whoami().await?;
    /// # anyhow::Ok(()) }
    /// ```
    pub fn login(
        &self,
        registration_method: ClientRegistrationMethod,
        redirect_uri: Url,
        device_id: Option<OwnedDeviceId>,
    ) -> OAuthAuthCodeUrlBuilder {
        let (scopes, device_id) = Self::login_scopes(device_id);

        OAuthAuthCodeUrlBuilder::new(
            self.clone(),
            registration_method,
            scopes.to_vec(),
            device_id,
            redirect_uri,
        )
    }

    /// Finish the login process.
    ///
    /// This method should be called after the URL returned by
    /// [`OAuthAuthCodeUrlBuilder::build()`] has been presented and the user has
    /// been redirected to the redirect URI after completing the authorization.
    ///
    /// If the authorization needs to be cancelled before its completion,
    /// [`OAuth::abort_login()`] should be used instead to clean up the local
    /// data.
    ///
    /// # Arguments
    ///
    /// * `url_or_query` - The URI where the user was redirected, or just its
    ///   query part.
    ///
    /// Returns an error if the authorization failed, if a request fails, or if
    /// the client was already logged in with a different session.
    pub async fn finish_login(&self, url_or_query: UrlOrQuery) -> Result<()> {
        let response = AuthorizationResponse::parse_url_or_query(&url_or_query)
            .map_err(|error| OAuthError::from(OAuthAuthorizationCodeError::from(error)))?;

        let auth_code = match response {
            AuthorizationResponse::Success(code) => code,
            AuthorizationResponse::Error(error) => {
                self.abort_login(&error.state).await;
                return Err(OAuthError::from(OAuthAuthorizationCodeError::from(error.error)).into());
            }
        };

        let device_id = self.finish_authorization(auth_code).await?;
        self.load_session(device_id).await
    }

    /// Load the session after login.
    ///
    /// Returns an error if the request to get the user ID fails, or if the
    /// client was already logged in with a different session.
    pub(crate) async fn load_session(&self, device_id: OwnedDeviceId) -> Result<()> {
        // Get the user ID.
        let whoami_res = self.client.whoami().await.map_err(crate::Error::from)?;

        let new_session = SessionMeta { user_id: whoami_res.user_id, device_id };

        if let Some(current_session) = self.client.session_meta() {
            if new_session != *current_session {
                return Err(OAuthError::SessionMismatch.into());
            }
        } else {
            self.client
                .base_client()
                .activate(
                    new_session,
                    RoomLoadSettings::default(),
                    #[cfg(feature = "e2e-encryption")]
                    None,
                )
                .await?;
            // At this point the Olm machine has been set up.

            // Enable the cross-process lock for refreshes, if needs be.
            #[cfg(feature = "e2e-encryption")]
            self.enable_cross_process_lock().await.map_err(OAuthError::from)?;

            #[cfg(feature = "e2e-encryption")]
            self.client.encryption().spawn_initialization_task(None);
        }

        Ok(())
    }

    #[cfg(feature = "e2e-encryption")]
    pub(crate) async fn enable_cross_process_lock(
        &self,
    ) -> Result<(), CrossProcessRefreshLockError> {
        // Enable the cross-process lock for refreshes, if needs be.
        self.deferred_enable_cross_process_refresh_lock().await;

        if let Some(cross_process_manager) = self.ctx().cross_process_token_refresh_manager.get() {
            if let Some(tokens) = self.client.session_tokens() {
                let mut cross_process_guard = cross_process_manager.spin_lock().await?;

                if cross_process_guard.hash_mismatch {
                    // At this point, we're finishing a login while another process had written
                    // something in the database. It's likely the information in the database is
                    // just outdated and wasn't properly updated, but display a warning, just in
                    // case this happens frequently.
                    warn!(
                        "unexpected cross-process hash mismatch when finishing login (see comment)"
                    );
                }

                cross_process_guard.save_in_memory_and_db(&tokens).await?;
            }
        }

        Ok(())
    }

    /// Finish the authorization process.
    ///
    /// This method should be called after the URL returned by
    /// [`OAuthAuthCodeUrlBuilder::build()`] has been presented and the user has
    /// been redirected to the redirect URI after a successful authorization.
    ///
    /// # Arguments
    ///
    /// * `auth_code` - The response received as part of the redirect URI when
    ///   the authorization was successful.
    ///
    /// Returns the device ID used in the authorized scope if it succeeds.
    /// Returns an error if a request fails.
    async fn finish_authorization(
        &self,
        auth_code: AuthorizationCode,
    ) -> Result<OwnedDeviceId, OAuthError> {
        let data = self.data().ok_or(OAuthError::NotAuthenticated)?;
        let client_id = data.client_id.clone();

        let validation_data = data
            .authorization_data
            .lock()
            .await
            .remove(&auth_code.state)
            .ok_or(OAuthAuthorizationCodeError::InvalidState)?;

        let token_uri = TokenUrl::from_url(validation_data.server_metadata.token_endpoint.clone());

        let response = OAuthClient::new(client_id)
            .set_token_uri(token_uri)
            .exchange_code(oauth2::AuthorizationCode::new(auth_code.code))
            .set_pkce_verifier(validation_data.pkce_verifier)
            .set_redirect_uri(Cow::Owned(validation_data.redirect_uri))
            .request_async(self.http_client())
            .await
            .map_err(OAuthAuthorizationCodeError::RequestToken)?;

        self.client.auth_ctx().set_session_tokens(SessionTokens {
            access_token: response.access_token().secret().clone(),
            refresh_token: response.refresh_token().map(RefreshToken::secret).cloned(),
        });

        Ok(validation_data.device_id)
    }

    /// Abort the login process.
    ///
    /// This method should be called if an authorization should be aborted
    /// before it is completed.
    ///
    /// If the authorization has been completed, [`OAuth::finish_login()`]
    /// should be used instead.
    ///
    /// # Arguments
    ///
    /// * `state` - The state provided in [`OAuthAuthorizationData`] after
    ///   building the authorization URL.
    pub async fn abort_login(&self, state: &CsrfToken) {
        if let Some(data) = self.data() {
            data.authorization_data.lock().await.remove(state);
        }
    }

    /// Request codes from the authorization server for logging in with another
    /// device.
    #[cfg(all(feature = "e2e-encryption", not(target_arch = "wasm32")))]
    async fn request_device_authorization(
        &self,
        server_metadata: &AuthorizationServerMetadata,
        device_id: Option<OwnedDeviceId>,
    ) -> Result<oauth2::StandardDeviceAuthorizationResponse, qrcode::DeviceAuthorizationOAuthError>
    {
        let (scopes, _) = Self::login_scopes(device_id);

        let client_id = self.client_id().ok_or(OAuthError::NotRegistered)?.clone();

        let device_authorization_url = server_metadata
            .device_authorization_endpoint
            .clone()
            .map(oauth2::DeviceAuthorizationUrl::from_url)
            .ok_or(qrcode::DeviceAuthorizationOAuthError::NoDeviceAuthorizationEndpoint)?;

        let response = OAuthClient::new(client_id)
            .set_device_authorization_url(device_authorization_url)
            .exchange_device_code()
            .add_scopes(scopes)
            .request_async(self.http_client())
            .await?;

        Ok(response)
    }

    /// Exchange the device code against an access token.
    #[cfg(all(feature = "e2e-encryption", not(target_arch = "wasm32")))]
    async fn exchange_device_code(
        &self,
        server_metadata: &AuthorizationServerMetadata,
        device_authorization_response: &oauth2::StandardDeviceAuthorizationResponse,
    ) -> Result<(), qrcode::DeviceAuthorizationOAuthError> {
        use oauth2::TokenResponse;

        let client_id = self.client_id().ok_or(OAuthError::NotRegistered)?.clone();

        let token_uri = TokenUrl::from_url(server_metadata.token_endpoint.clone());

        let response = OAuthClient::new(client_id)
            .set_token_uri(token_uri)
            .exchange_device_access_token(device_authorization_response)
            .request_async(self.http_client(), tokio::time::sleep, None)
            .await?;

        self.client.auth_ctx().set_session_tokens(SessionTokens {
            access_token: response.access_token().secret().to_owned(),
            refresh_token: response.refresh_token().map(|t| t.secret().to_owned()),
        });

        Ok(())
    }

    async fn refresh_access_token_inner(
        self,
        refresh_token: String,
        token_endpoint: Url,
        client_id: ClientId,
        #[cfg(feature = "e2e-encryption")] cross_process_lock: Option<CrossProcessRefreshLockGuard>,
    ) -> Result<(), OAuthError> {
        trace!(
            "Token refresh: attempting to refresh with refresh_token {:x}",
            hash_str(&refresh_token)
        );

        let token = RefreshToken::new(refresh_token.clone());
        let token_uri = TokenUrl::from_url(token_endpoint);

        let response = OAuthClient::new(client_id)
            .set_token_uri(token_uri)
            .exchange_refresh_token(&token)
            .request_async(self.http_client())
            .await
            .map_err(OAuthError::RefreshToken)?;

        let new_access_token = response.access_token().secret().clone();
        let new_refresh_token = response.refresh_token().map(RefreshToken::secret).cloned();

        trace!(
            "Token refresh: new refresh_token: {} / access_token: {:x}",
            new_refresh_token
                .as_deref()
                .map(|token| format!("{:x}", hash_str(token)))
                .unwrap_or_else(|| "<none>".to_owned()),
            hash_str(&new_access_token)
        );

        let tokens = SessionTokens {
            access_token: new_access_token,
            refresh_token: new_refresh_token.or(Some(refresh_token)),
        };

        #[cfg(feature = "e2e-encryption")]
        let tokens_clone = tokens.clone();

        self.client.auth_ctx().set_session_tokens(tokens);

        // Call the save_session_callback if set, while the optional lock is being held.
        if let Some(save_session_callback) = self.client.auth_ctx().save_session_callback.get() {
            // Satisfies the save_session_callback invariant: set_session_tokens has
            // been called just above.
            if let Err(err) = save_session_callback(self.client.clone()) {
                error!("when saving session after refresh: {err}");
            }
        }

        #[cfg(feature = "e2e-encryption")]
        if let Some(mut lock) = cross_process_lock {
            lock.save_in_memory_and_db(&tokens_clone).await?;
        }

        _ = self.client.auth_ctx().session_change_sender.send(SessionChange::TokensRefreshed);

        Ok(())
    }

    /// Refresh the access token.
    ///
    /// This should be called when the access token has expired. It should not
    /// be needed to call this manually if the [`Client`] was constructed with
    /// [`ClientBuilder::handle_refresh_tokens()`].
    ///
    /// This method is protected behind a lock, so calling this method several
    /// times at once will only call the endpoint once and all subsequent calls
    /// will wait for the result of the first call. The first call will
    /// return `Ok(Some(response))` or a [`RefreshTokenError`], while the others
    /// will return `Ok(None)` if the token was refreshed by the first call
    /// or the same [`RefreshTokenError`], if it failed.
    ///
    /// [`ClientBuilder::handle_refresh_tokens()`]: crate::ClientBuilder::handle_refresh_tokens()
    #[instrument(skip_all)]
    pub async fn refresh_access_token(&self) -> Result<(), RefreshTokenError> {
        macro_rules! fail {
            ($lock:expr, $err:expr) => {
                let error = $err;
                *$lock = Err(error.clone());
                return Err(error);
            };
        }

        let client = &self.client;

        let refresh_status_lock = client.auth_ctx().refresh_token_lock.clone().try_lock_owned();

        let Ok(mut refresh_status_guard) = refresh_status_lock else {
            debug!("another refresh is happening, waiting for result.");
            // There's already a request to refresh happening in the same process. Wait for
            // it to finish.
            let res = client.auth_ctx().refresh_token_lock.lock().await.clone();
            debug!("other refresh is a {}", if res.is_ok() { "success" } else { "failure " });
            return res;
        };

        debug!("no other refresh happening in background, starting.");

        #[cfg(feature = "e2e-encryption")]
        let cross_process_guard =
            if let Some(manager) = self.ctx().cross_process_token_refresh_manager.get() {
                let mut cross_process_guard = match manager
                    .spin_lock()
                    .await
                    .map_err(|err| RefreshTokenError::OAuth(Arc::new(err.into())))
                {
                    Ok(guard) => guard,
                    Err(err) => {
                        warn!("couldn't acquire cross-process lock (timeout)");
                        fail!(refresh_status_guard, err);
                    }
                };

                if cross_process_guard.hash_mismatch {
                    Box::pin(self.handle_session_hash_mismatch(&mut cross_process_guard))
                        .await
                        .map_err(|err| RefreshTokenError::OAuth(Arc::new(err.into())))?;
                    // Optimistic exit: assume that the underlying process did update fast enough.
                    // In the worst case, we'll do another refresh Soon™.
                    tracing::info!("other process handled refresh for us, assuming success");
                    *refresh_status_guard = Ok(());
                    return Ok(());
                }

                Some(cross_process_guard)
            } else {
                None
            };

        let Some(session_tokens) = self.client.session_tokens() else {
            warn!("invalid state: missing session tokens");
            fail!(refresh_status_guard, RefreshTokenError::RefreshTokenRequired);
        };

        let Some(refresh_token) = session_tokens.refresh_token else {
            warn!("invalid state: missing session tokens");
            fail!(refresh_status_guard, RefreshTokenError::RefreshTokenRequired);
        };

        let server_metadata = match self.server_metadata().await {
            Ok(metadata) => metadata,
            Err(err) => {
                warn!("couldn't get authorization server metadata: {err:?}");
                fail!(refresh_status_guard, RefreshTokenError::OAuth(Arc::new(err.into())));
            }
        };

        let Some(client_id) = self.client_id().cloned() else {
            warn!("invalid state: missing client ID");
            fail!(
                refresh_status_guard,
                RefreshTokenError::OAuth(Arc::new(OAuthError::NotAuthenticated))
            );
        };

        // Do not interrupt refresh access token requests and processing, by detaching
        // the request sending and response processing.
        // Make sure to keep the `refresh_status_guard` during the entire processing.

        let this = self.clone();

        spawn(async move {
            match this
                .refresh_access_token_inner(
                    refresh_token,
                    server_metadata.token_endpoint,
                    client_id,
                    #[cfg(feature = "e2e-encryption")]
                    cross_process_guard,
                )
                .await
            {
                Ok(()) => {
                    debug!("success refreshing a token");
                    *refresh_status_guard = Ok(());
                    Ok(())
                }

                Err(err) => {
                    let err = RefreshTokenError::OAuth(Arc::new(err));
                    warn!("error refreshing an OAuth 2.0 token: {err}");
                    fail!(refresh_status_guard, err);
                }
            }
        })
        .await
        .expect("joining")
    }

    /// Log out from the currently authenticated session.
    pub async fn logout(&self) -> Result<(), OAuthError> {
        let client_id = self.client_id().ok_or(OAuthError::NotAuthenticated)?.clone();

        let server_metadata = self.server_metadata().await?;
        let revocation_url = RevocationUrl::from_url(server_metadata.revocation_endpoint);

        let tokens = self.client.session_tokens().ok_or(OAuthError::NotAuthenticated)?;

        // Revoke the access token, it should revoke both tokens.
        OAuthClient::new(client_id)
            .set_revocation_url(revocation_url)
            .revoke_token(StandardRevocableToken::AccessToken(AccessToken::new(
                tokens.access_token,
            )))
            .map_err(OAuthTokenRevocationError::Url)?
            .request_async(self.http_client())
            .await
            .map_err(OAuthTokenRevocationError::Revoke)?;

        #[cfg(feature = "e2e-encryption")]
        if let Some(manager) = self.ctx().cross_process_token_refresh_manager.get() {
            manager.on_logout().await?;
        }

        Ok(())
    }
}

/// A full session for the OAuth 2.0 API.
#[derive(Debug, Clone)]
pub struct OAuthSession {
    /// The client ID obtained after registration.
    pub client_id: ClientId,

    /// The user session.
    pub user: UserSession,
}

/// A user session for the OAuth 2.0 API.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserSession {
    /// The Matrix user session info.
    #[serde(flatten)]
    pub meta: SessionMeta,

    /// The tokens used for authentication.
    #[serde(flatten)]
    pub tokens: SessionTokens,

    /// The OAuth 2.0 server used for this session.
    pub issuer: Url,
}

/// The data necessary to validate a response from the Token endpoint in the
/// Authorization Code flow.
#[derive(Debug)]
struct AuthorizationValidationData {
    /// The metadata of the server,
    server_metadata: AuthorizationServerMetadata,

    /// The device ID used in the scope.
    device_id: OwnedDeviceId,

    /// The URI where the end-user will be redirected after authorization.
    redirect_uri: RedirectUrl,

    /// A string to correlate the authorization request to the token request.
    pkce_verifier: PkceCodeVerifier,
}

/// The data returned by the server in the redirect URI after a successful
/// authorization.
#[derive(Debug, Clone)]
enum AuthorizationResponse {
    /// A successful response.
    Success(AuthorizationCode),

    /// An error response.
    Error(AuthorizationError),
}

impl AuthorizationResponse {
    /// Deserialize an `AuthorizationResponse` from a [`UrlOrQuery`].
    ///
    /// Returns an error if the URL or query doesn't have the expected format.
    fn parse_url_or_query(url_or_query: &UrlOrQuery) -> Result<Self, RedirectUriQueryParseError> {
        let query = url_or_query.query().ok_or(RedirectUriQueryParseError::MissingQuery)?;
        Self::parse_query(query)
    }

    /// Deserialize an `AuthorizationResponse` from the query part of a URI.
    ///
    /// Returns an error if the query doesn't have the expected format.
    fn parse_query(query: &str) -> Result<Self, RedirectUriQueryParseError> {
        // For some reason deserializing the enum with `serde(untagged)` doesn't work,
        // so let's try both variants separately.
        if let Ok(code) = serde_html_form::from_str(query) {
            return Ok(AuthorizationResponse::Success(code));
        }
        if let Ok(error) = serde_html_form::from_str(query) {
            return Ok(AuthorizationResponse::Error(error));
        }

        Err(RedirectUriQueryParseError::UnknownFormat)
    }
}

/// The data returned by the server in the redirect URI after a successful
/// authorization.
#[derive(Debug, Clone, Deserialize)]
struct AuthorizationCode {
    /// The code to use to retrieve the access token.
    code: String,
    /// The unique identifier for this transaction.
    state: CsrfToken,
}

/// The data returned by the server in the redirect URI after an authorization
/// error.
#[derive(Debug, Clone, Deserialize)]
struct AuthorizationError {
    /// The error.
    #[serde(flatten)]
    error: StandardErrorResponse<error::AuthorizationCodeErrorResponseType>,
    /// The unique identifier for this transaction.
    state: CsrfToken,
}

fn hash_str(x: &str) -> impl fmt::LowerHex {
    sha2::Sha256::new().chain_update(x).finalize()
}

/// The available methods to register or restore a client.
#[derive(Debug)]
pub enum ClientRegistrationMethod {
    /// No registration will be done.
    ///
    /// This should only be set if [`OAuth::register_client()`] or
    /// [`OAuth::restore_registered_client()`] was already called before.
    None,

    /// The given client ID will be used.
    ///
    /// This will call [`OAuth::restore_registered_client()`] internally.
    ClientId(ClientId),

    /// The client will register using dynamic client registration, with the
    /// given metadata.
    ///
    /// This will call [`OAuth::register_client()`] internally.
    Metadata(Raw<ClientMetadata>),

    /// Use an [`OAuthRegistrationStore`] to handle registrations.
    #[cfg(not(target_arch = "wasm32"))]
    Store(OAuthRegistrationStore),
}

impl From<ClientId> for ClientRegistrationMethod {
    fn from(value: ClientId) -> Self {
        Self::ClientId(value)
    }
}

impl From<Raw<ClientMetadata>> for ClientRegistrationMethod {
    fn from(value: Raw<ClientMetadata>) -> Self {
        Self::Metadata(value)
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl From<OAuthRegistrationStore> for ClientRegistrationMethod {
    fn from(value: OAuthRegistrationStore) -> Self {
        Self::Store(value)
    }
}

/// A full URL or just the query part of a URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UrlOrQuery {
    /// A full URL.
    Url(Url),

    /// The query part of a URL.
    Query(String),
}

impl UrlOrQuery {
    /// Get the query part of this [`UrlOrQuery`].
    ///
    /// If this is a [`Url`], this extracts the query.
    pub fn query(&self) -> Option<&str> {
        match self {
            Self::Url(url) => url.query(),
            Self::Query(query) => Some(query),
        }
    }
}

impl From<Url> for UrlOrQuery {
    fn from(value: Url) -> Self {
        Self::Url(value)
    }
}
