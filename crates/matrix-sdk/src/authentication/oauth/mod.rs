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
//! Clients must register with the homeserver before being able to interact with
//! an OAuth 2.0 server.
//!
//! The registration consists in providing client metadata to the authorization
//! server, to declare the interactions that the client supports with the
//! homeserver. This step is important because the client cannot use a feature
//! that is not declared during registration. In return, the server assigns an
//! ID and eventually credentials to the client, which will allow to identify
//! the client when authorization requests are made.
//!
//! Note that only public clients are supported by this API, i.e. clients
//! without credentials.
//!
//! The registration step can be done automatically by providing a
//! [`ClientRegistrationData`] to the login method.
//!
//! If the server supports dynamic registration, registration can be performed
//! manually by using [`OAuth::register_client()`]. If dynamic registration is
//! not available, the homeserver should document how to obtain a client ID. The
//! client ID can then be provided with [`OAuth::restore_registered_client()`].
//!
//! # Login
//!
//! Currently, two login methods are supported by this API.
//!
//! ## Login with the Authorization Code flow
//!
//! The use of the Authorization Code flow is defined in [MSC2964] and [RFC
//! 6749][rfc6749-auth-code].
//!
//! This method requires to open a URL in the end-user's browser where
//! they will be able to log into their account in the server's web UI and grant
//! access to their Matrix account.
//!
//! [`OAuth::login()`] constructs an [`OAuthAuthCodeUrlBuilder`] that can be
//! configured, and then calling [`OAuthAuthCodeUrlBuilder::build()`] will
//! provide the URL to present to the user in a web browser.
//!
//! After authenticating with the server, the user will be redirected to the
//! provided redirect URI, with a code in the query that will allow to finish
//! the login process by calling [`OAuth::finish_login()`].
//!
//! If the login needs to be cancelled before its completion,
//! [`OAuth::abort_login()`] should be called to clean up the local data.
//!
//! ## Login by scanning a QR Code
//!
//! Logging in via a QR code is defined in [MSC4108]. It uses the Device
//! authorization flow specified in [RFC 8628].
//!
//! This method requires to have another logged-in Matrix device that can
//! display a QR Code.
//!
//! This login method is only available if the `e2e-encryption` cargo feature is
//! enabled. It is not available on WASM.
//!
//! After scanning the QR Code, [`OAuth::login_with_qr_code()`] can be called
//! with the QR Code's data. Then the different steps of the process need to be
//! followed with [`LoginWithQrCode::subscribe_to_progress()`].
//!
//! A successful login using this method will automatically mark the device as
//! verified and transfer all end-to-end encryption related secrets, like the
//! private cross-signing keys and the backup key from the existing device to
//! the new device.
//!
//! # Persisting/restoring a session
//!
//! The full session to persist can be obtained with [`OAuth::full_session()`].
//! The different parts can also be retrieved with [`Client::session_meta()`],
//! [`Client::session_tokens()`] and [`OAuth::client_id()`].
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
//! with [`Client::subscribe_to_session_changes()`] to persist them on every
//! change. If they are not persisted properly, the end-user will need to login
//! again.
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
//! account. It can be used to replace most of the Matrix APIs requiring
//! User-Interactive Authentication.
//!
//! An [`AccountManagementUrlBuilder`] can be obtained with
//! [`OAuth::account_management_url()`]. Then the action that the user wants to
//! perform can be customized with [`AccountManagementUrlBuilder::action()`].
//! Finally you can obtain the final URL to present to the user with
//! [`AccountManagementUrlBuilder::build()`].
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
//! [MSC2964]: https://github.com/matrix-org/matrix-spec-proposals/pull/2964
//! [rfc6749-auth-code]: https://datatracker.ietf.org/doc/html/rfc6749#section-4.1
//! [MSC4108]: https://github.com/matrix-org/matrix-spec-proposals/pull/4108
//! [RFC 8628]: https://datatracker.ietf.org/doc/html/rfc8628
//! [`ClientBuilder::handle_refresh_tokens()`]: crate::ClientBuilder::handle_refresh_tokens()
//! [`Error`]: ruma::api::client::error::Error
//! [`ErrorKind::UnknownToken`]: ruma::api::client::error::ErrorKind::UnknownToken
//! [`examples/oauth_cli`]: https://github.com/matrix-org/matrix-rust-sdk/tree/main/examples/oauth_cli

#[cfg(feature = "e2e-encryption")]
use std::time::Duration;
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
#[cfg(feature = "e2e-encryption")]
use matrix_sdk_base::crypto::types::qr_login::QrCodeData;
#[cfg(feature = "e2e-encryption")]
use matrix_sdk_base::once_cell::sync::OnceCell;
use matrix_sdk_base::{SessionMeta, store::RoomLoadSettings};
use oauth2::{
    AccessToken, PkceCodeVerifier, RedirectUrl, RefreshToken, RevocationUrl, Scope,
    StandardErrorResponse, StandardRevocableToken, TokenResponse, TokenUrl,
    basic::BasicClient as OAuthClient,
};
pub use oauth2::{ClientId, CsrfToken};
use ruma::{
    DeviceId, OwnedDeviceId,
    api::client::discovery::get_authorization_server_metadata::{
        self,
        v1::{AccountManagementAction, AuthorizationServerMetadata},
    },
    serde::Raw,
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
#[cfg(feature = "e2e-encryption")]
pub mod qrcode;
pub mod registration;
#[cfg(all(test, not(target_family = "wasm")))]
mod tests;

#[cfg(feature = "e2e-encryption")]
use self::cross_process::{CrossProcessRefreshLockGuard, CrossProcessRefreshManager};
#[cfg(feature = "e2e-encryption")]
use self::qrcode::{
    GrantLoginWithGeneratedQrCode, GrantLoginWithScannedQrCode, LoginWithGeneratedQrCode,
    LoginWithQrCode,
};
pub use self::{
    account_management_url::{AccountManagementActionFull, AccountManagementUrlBuilder},
    auth_code_builder::{OAuthAuthCodeUrlBuilder, OAuthAuthorizationData},
    error::OAuthError,
};
use self::{
    http_client::OAuthHttpClient,
    registration::{ClientMetadata, ClientRegistrationResponse, register_client},
};
use super::{AuthData, SessionTokens};
use crate::{Client, HttpError, RefreshTokenError, Result, client::SessionChange, executor::spawn};

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
    pub(crate) client_id: ClientId,
    /// The data necessary to validate authorization responses.
    authorization_data: Mutex<HashMap<CsrfToken, AuthorizationValidationData>>,
}

#[cfg(not(tarpaulin_include))]
impl fmt::Debug for OAuthAuthData {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OAuthAuthData").finish_non_exhaustive()
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

    /// Log in this device using a QR code.
    ///
    /// # Arguments
    ///
    /// * `registration_data` - The data to restore or register the client with
    ///   the server. If this is not provided, an error will occur unless
    ///   [`OAuth::register_client()`] or [`OAuth::restore_registered_client()`]
    ///   was called previously.
    #[cfg(feature = "e2e-encryption")]
    pub fn login_with_qr_code<'a>(
        &'a self,
        registration_data: Option<&'a ClientRegistrationData>,
    ) -> LoginWithQrCodeBuilder<'a> {
        LoginWithQrCodeBuilder { client: &self.client, registration_data }
    }

    /// Grant login to a new device using a QR code.
    #[cfg(feature = "e2e-encryption")]
    pub fn grant_login_with_qr_code<'a>(&'a self) -> GrantLoginWithQrCodeBuilder<'a> {
        GrantLoginWithQrCodeBuilder::new(&self.client)
    }

    /// Restore or register the OAuth 2.0 client for the server with the given
    /// metadata, with the given optional [`ClientRegistrationData`].
    ///
    /// If we already have a client ID, this is a noop.
    ///
    /// Returns an error if there was a problem using the registration method.
    async fn use_registration_data(
        &self,
        server_metadata: &AuthorizationServerMetadata,
        data: Option<&ClientRegistrationData>,
    ) -> std::result::Result<(), OAuthError> {
        if self.client_id().is_some() {
            tracing::info!("OAuth 2.0 is already configured.");
            return Ok(());
        }

        let Some(data) = data else {
            return Err(OAuthError::NotRegistered);
        };

        if let Some(static_registrations) = &data.static_registrations {
            let client_id = static_registrations
                .get(&self.client.homeserver())
                .or_else(|| static_registrations.get(&server_metadata.issuer));

            if let Some(client_id) = client_id {
                self.restore_registered_client(client_id.clone());
                return Ok(());
            }
        }

        self.register_client_inner(server_metadata, &data.metadata).await?;

        Ok(())
    }

    /// The account management actions supported by the authorization server's
    /// account management URL.
    ///
    /// Returns an error if the request to get the server metadata fails.
    pub async fn account_management_actions_supported(
        &self,
    ) -> Result<BTreeSet<AccountManagementAction>, OAuthError> {
        let server_metadata = self.server_metadata().await?;

        Ok(server_metadata.account_management_actions_supported)
    }

    /// Get the account management URL where the user can manage their
    /// identity-related settings.
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

        let response =
            self.client.send(get_authorization_server_metadata::v1::Request::new()).await.map_err(
                |error| {
                    // If the endpoint returns a 404, i.e. the server doesn't support the endpoint.
                    if is_endpoint_unsupported(&error) {
                        OAuthDiscoveryError::NotSupported
                    } else {
                        error.into()
                    }
                },
            )?;

        let metadata = response.metadata.deserialize()?;

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
    /// Returns `None` if the client was not logged in.
    pub fn user_session(&self) -> Option<UserSession> {
        let meta = self.client.session_meta()?.to_owned();
        let tokens = self.client.session_tokens()?;
        Some(UserSession { meta, tokens })
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
    /// This should be called before any authorization request with an
    /// authorization server that supports dynamic client registration. If the
    /// client registered with the server manually, it should use
    /// [`OAuth::restore_registered_client()`].
    ///
    /// Note that this method only supports public clients, i.e. clients without
    /// a secret.
    ///
    /// # Arguments
    ///
    /// * `client_metadata` - The serialized client metadata to register.
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
    /// # fn persist_client_registration (_: url::Url, _: &ClientId) {}
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
    ///
    /// persist_client_registration(client.homeserver(), &client_id);
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
        self.restore_registered_client(registration_response.client_id.clone());

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
    /// * `client_id` - The unique identifier to authenticate the client with
    ///   the server, obtained after registration.
    ///
    /// # Panic
    ///
    /// Panics if authentication data was already set.
    pub fn restore_registered_client(&self, client_id: ClientId) {
        let data = OAuthAuthData { client_id, authorization_data: Default::default() };

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
        let OAuthSession { client_id, user: UserSession { meta, tokens } } = session;

        let data = OAuthAuthData { client_id, authorization_data: Default::default() };

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
                .map_err(|err| crate::Error::OAuth(Box::new(err.into())))?;

            // After we got the lock, it's possible that our session doesn't match the one
            // read from the database, because of a race: another process has
            // refreshed the tokens while we were waiting for the lock.
            //
            // In that case, if there's a mismatch, we reload the session and update the
            // hash. Otherwise, we save our hash into the database.

            if guard.hash_mismatch {
                Box::pin(self.handle_session_hash_mismatch(&mut guard))
                    .await
                    .map_err(|err| crate::Error::OAuth(Box::new(err.into())))?;
            } else {
                guard
                    .save_in_memory_and_db(&tokens)
                    .await
                    .map_err(|err| crate::Error::OAuth(Box::new(err.into())))?;
                // No need to call the save_session_callback here; it was the
                // source of the session, so it's already in
                // sync with what we had.
            }
        }

        #[cfg(feature = "e2e-encryption")]
        self.client.encryption().spawn_initialization_task(None).await;

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
    fn login_scopes(
        device_id: Option<OwnedDeviceId>,
        additional_scopes: Option<Vec<Scope>>,
    ) -> (Vec<Scope>, OwnedDeviceId) {
        /// Scope to grand full access to the client-server API.
        const SCOPE_MATRIX_CLIENT_SERVER_API_FULL_ACCESS: &str =
            "urn:matrix:org.matrix.msc2967.client:api:*";
        /// Prefix of the scope to bind a device ID to an access token.
        const SCOPE_MATRIX_DEVICE_ID_PREFIX: &str = "urn:matrix:org.matrix.msc2967.client:device:";

        // Generate the device ID if it is not provided.
        let device_id = device_id.unwrap_or_else(DeviceId::new);

        let mut scopes = vec![
            Scope::new(SCOPE_MATRIX_CLIENT_SERVER_API_FULL_ACCESS.to_owned()),
            Scope::new(format!("{SCOPE_MATRIX_DEVICE_ID_PREFIX}{device_id}")),
        ];

        if let Some(extra_scopes) = additional_scopes {
            scopes.extend(extra_scopes);
        }

        (scopes, device_id)
    }

    /// Log in via OAuth 2.0 with the Authorization Code flow.
    ///
    /// This method requires to open a URL in the end-user's browser where they
    /// will be able to log into their account in the server's web UI and grant
    /// access to their Matrix account.
    ///
    /// The [`OAuthAuthCodeUrlBuilder`] that is returned allows to customize a
    /// few settings before calling `.build()` to obtain the URL to open in the
    /// browser of the end-user.
    ///
    /// [`OAuth::finish_login()`] must be called once the user has been
    /// redirected to the `redirect_uri`. [`OAuth::abort_login()`] should be
    /// called instead if the authorization should be aborted before completion.
    ///
    /// # Arguments
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
    /// * `registration_data` - The data to restore or register the client with
    ///   the server. If this is not provided, an error will occur unless
    ///   [`OAuth::register_client()`] or [`OAuth::restore_registered_client()`]
    ///   was called previously.
    ///
    /// * `additional_scopes` - Additional scopes to request from the
    ///   authorization server, e.g. "urn:matrix:client:com.example.msc9999.foo".
    ///   The scopes for API access and the device ID according to the
    ///   [specification](https://spec.matrix.org/v1.15/client-server-api/#allocated-scope-tokens)
    ///   are always requested.
    ///
    /// # Example
    ///
    /// ```no_run
    /// use matrix_sdk::{
    ///     authentication::oauth::registration::ClientMetadata,
    ///     ruma::serde::Raw,
    /// };
    /// use url::Url;
    /// # use matrix_sdk::Client;
    /// # let client: Client = unimplemented!();
    /// # let redirect_uri = unimplemented!();
    /// # async fn open_uri_and_wait_for_redirect(uri: Url) -> Url { unimplemented!() };
    /// # fn client_metadata() -> Raw<ClientMetadata> { unimplemented!() };
    /// # _ = async {
    /// let oauth = client.oauth();
    /// let client_metadata: Raw<ClientMetadata> = client_metadata();
    /// let registration_data = client_metadata.into();
    ///
    /// let auth_data = oauth.login(redirect_uri, None, Some(registration_data), None)
    ///                      .build()
    ///                      .await?;
    ///
    /// // Open auth_data.url and wait for response at the redirect URI.
    /// let redirected_to_uri: Url = open_uri_and_wait_for_redirect(auth_data.url).await;
    ///
    /// oauth.finish_login(redirected_to_uri.into()).await?;
    ///
    /// // The session tokens can be persisted from the
    /// // `OAuth::full_session()` method.
    ///
    /// // You can now make requests to the Matrix API.
    /// let _me = client.whoami().await?;
    /// # anyhow::Ok(()) }
    /// ```
    pub fn login(
        &self,
        redirect_uri: Url,
        device_id: Option<OwnedDeviceId>,
        registration_data: Option<ClientRegistrationData>,
        additional_scopes: Option<Vec<Scope>>,
    ) -> OAuthAuthCodeUrlBuilder {
        let (scopes, device_id) = Self::login_scopes(device_id, additional_scopes);

        OAuthAuthCodeUrlBuilder::new(
            self.clone(),
            scopes.to_vec(),
            device_id,
            redirect_uri,
            registration_data,
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
            self.client.encryption().spawn_initialization_task(None).await;
        }

        Ok(())
    }

    #[cfg(feature = "e2e-encryption")]
    pub(crate) async fn enable_cross_process_lock(
        &self,
    ) -> Result<(), CrossProcessRefreshLockError> {
        // Enable the cross-process lock for refreshes, if needs be.
        self.deferred_enable_cross_process_refresh_lock().await;

        if let Some(cross_process_manager) = self.ctx().cross_process_token_refresh_manager.get()
            && let Some(tokens) = self.client.session_tokens()
        {
            let mut cross_process_guard = cross_process_manager.spin_lock().await?;

            if cross_process_guard.hash_mismatch {
                // At this point, we're finishing a login while another process had written
                // something in the database. It's likely the information in the database is
                // just outdated and wasn't properly updated, but display a warning, just in
                // case this happens frequently.
                warn!("unexpected cross-process hash mismatch when finishing login (see comment)");
            }

            cross_process_guard.save_in_memory_and_db(&tokens).await?;
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
    /// This method should be called if a login should be aborted before it is
    /// completed.
    ///
    /// If the login has been completed, [`OAuth::finish_login()`] should be
    /// used instead.
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
    #[cfg(feature = "e2e-encryption")]
    async fn request_device_authorization(
        &self,
        server_metadata: &AuthorizationServerMetadata,
        device_id: Option<OwnedDeviceId>,
    ) -> Result<oauth2::StandardDeviceAuthorizationResponse, qrcode::DeviceAuthorizationOAuthError>
    {
        let (scopes, _) = Self::login_scopes(device_id, None);

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
    #[cfg(feature = "e2e-encryption")]
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
            tracing::debug!("call save_session_callback");
            if let Err(err) = save_session_callback(self.client.clone()) {
                error!("when saving session after refresh: {err}");
            }
        }

        #[cfg(feature = "e2e-encryption")]
        if let Some(mut lock) = cross_process_lock {
            lock.save_in_memory_and_db(&tokens_clone).await?;
        }

        tracing::debug!("broadcast session changed");
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
    /// will wait for the result of the first call.
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

/// Builder for QR login futures.
#[cfg(feature = "e2e-encryption")]
#[derive(Debug)]
pub struct LoginWithQrCodeBuilder<'a> {
    /// The underlying Matrix API client.
    client: &'a Client,

    /// The data to restore or register the client with the server.
    registration_data: Option<&'a ClientRegistrationData>,
}

#[cfg(feature = "e2e-encryption")]
impl<'a> LoginWithQrCodeBuilder<'a> {
    /// This method allows you to log in with a scanned QR code.
    ///
    /// The existing device needs to display the QR code which this device can
    /// scan and call this method to log in.
    ///
    /// A successful login using this method will automatically mark the device
    /// as verified and transfer all end-to-end encryption related secrets, like
    /// the private cross-signing keys and the backup key from the existing
    /// device to the new device.
    ///
    /// For the reverse flow where this device generates the QR code for the
    /// existing device to scan, use [`LoginWithQrCodeBuilder::generate`].
    ///
    /// # Arguments
    ///
    /// * `data` - The data scanned from a QR code.
    ///
    /// # Example
    ///
    /// ```no_run
    /// use anyhow::bail;
    /// use futures_util::StreamExt;
    /// use matrix_sdk::{
    ///     authentication::oauth::{
    ///         registration::ClientMetadata,
    ///         qrcode::{LoginProgress, QrCodeData, QrCodeModeData, QrProgress},
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
    /// let client_metadata: Raw<ClientMetadata> = client_metadata();
    /// let registration_data = client_metadata.into();
    ///
    /// // Subscribing to the progress is necessary since we need to input the check
    /// // code on the existing device.
    /// let login = oauth.login_with_qr_code(Some(&registration_data)).scan(&qr_code_data);
    /// let mut progress = login.subscribe_to_progress();
    ///
    /// // Create a task which will show us the progress and tell us the check
    /// // code to input in the existing device.
    /// let task = tokio::spawn(async move {
    ///     while let Some(state) = progress.next().await {
    ///         match state {
    ///             LoginProgress::Starting | LoginProgress::SyncingSecrets => (),
    ///             LoginProgress::EstablishingSecureChannel(QrProgress { check_code }) => {
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
    pub fn scan(self, data: &'a QrCodeData) -> LoginWithQrCode<'a> {
        LoginWithQrCode::new(self.client, data, self.registration_data)
    }

    /// This method allows you to log in by generating a QR code.
    ///
    /// This device needs to call this method to generate and display the
    /// QR code which the existing device can scan and grant the log in.
    ///
    /// A successful login using this method will automatically mark the device
    /// as verified and transfer all end-to-end encryption related secrets, like
    /// the private cross-signing keys and the backup key from the existing
    /// device to the new device.
    ///
    /// For the reverse flow where the existing device generates the QR code
    /// for this device to scan, use [`LoginWithQrCodeBuilder::scan`].
    ///
    /// # Example
    ///
    /// ```no_run
    /// use anyhow::bail;
    /// use futures_util::StreamExt;
    /// use matrix_sdk::{
    ///     authentication::oauth::{
    ///         registration::ClientMetadata,
    ///         qrcode::{GeneratedQrProgress, LoginProgress, QrCodeData, QrCodeModeData},
    ///     },
    ///     ruma::serde::Raw,
    ///     Client,
    /// };
    /// use std::{error::Error, io::stdin};
    /// # fn client_metadata() -> Raw<ClientMetadata> { unimplemented!() }
    /// # _ = async {
    /// // Build the client as usual.
    /// let client = Client::builder()
    ///     .server_name_or_homeserver_url("matrix.org")
    ///     .handle_refresh_tokens()
    ///     .build()
    ///     .await?;
    ///
    /// let oauth = client.oauth();
    /// let client_metadata: Raw<ClientMetadata> = client_metadata();
    /// let registration_data = client_metadata.into();
    ///
    /// // Subscribing to the progress is necessary since we need to display the
    /// // QR code and prompt for the check code.
    /// let login = oauth.login_with_qr_code(Some(&registration_data)).generate();
    /// let mut progress = login.subscribe_to_progress();
    ///
    /// // Create a task which will show us the progress and allows us to display
    /// // the QR code and prompt for the check code.
    /// let task = tokio::spawn(async move {
    ///     while let Some(state) = progress.next().await {
    ///         match state {
    ///             LoginProgress::Starting | LoginProgress::SyncingSecrets => (),
    ///             LoginProgress::EstablishingSecureChannel(GeneratedQrProgress::QrReady(qr)) => {
    ///                 println!("Please use your other device to scan the QR code {:?}", qr)
    ///             }
    ///             LoginProgress::EstablishingSecureChannel(GeneratedQrProgress::QrScanned(cctx)) => {
    ///                 println!("Please enter the code displayed on your other device");
    ///                 let mut s = String::new();
    ///                 stdin().read_line(&mut s)?;
    ///                 let check_code = s.trim().parse::<u8>()?;
    ///                 cctx.send(check_code).await?
    ///             }
    ///             LoginProgress::WaitingForToken { user_code } => {
    ///                 println!("Please use your other device to confirm the log in {user_code}")
    ///             },
    ///             LoginProgress::Done => break,
    ///         }
    ///     }
    ///     Ok::<(), Box<dyn Error + Send + Sync>>(())
    /// });
    ///
    /// // Now run the future to complete the login.
    /// login.await?;
    /// task.abort();
    ///
    /// println!("Successfully logged in: {:?} {:?}", client.user_id(), client.device_id());
    /// # anyhow::Ok(()) };
    /// ```
    pub fn generate(self) -> LoginWithGeneratedQrCode<'a> {
        LoginWithGeneratedQrCode::new(self.client, self.registration_data)
    }
}

/// Builder for QR login grant handlers.
#[cfg(feature = "e2e-encryption")]
#[derive(Debug)]
pub struct GrantLoginWithQrCodeBuilder<'a> {
    /// The underlying Matrix API client.
    client: &'a Client,
    /// The duration to wait for the homeserver to create the new device after
    /// consenting the login before giving up.
    device_creation_timeout: Duration,
}

#[cfg(feature = "e2e-encryption")]
impl<'a> GrantLoginWithQrCodeBuilder<'a> {
    /// Create a new builder with the default device creation timeout.
    fn new(client: &'a Client) -> Self {
        Self { client, device_creation_timeout: Duration::from_secs(10) }
    }

    /// Set the device creation timeout.
    ///
    /// # Arguments
    ///
    /// * `device_creation_timeout` - The duration to wait for the homeserver to
    ///   create the new device after consenting the login before giving up.
    pub fn device_creation_timeout(mut self, device_creation_timeout: Duration) -> Self {
        self.device_creation_timeout = device_creation_timeout;
        self
    }

    /// This method allows you to grant login to a new device by scanning a
    /// QR code generated by the new device.
    ///
    /// The new device needs to display the QR code which this device can
    /// scan and call this method to grant the login.
    ///
    /// A successful login grant using this method will automatically mark the
    /// new device as verified and transfer all end-to-end encryption
    /// related secrets, like the private cross-signing keys and the backup
    /// key from this device device to the new device.
    ///
    /// For the reverse flow where this device generates the QR code
    /// for the new device to scan, use
    /// [`GrantLoginWithQrCodeBuilder::generate`].
    ///
    /// # Arguments
    ///
    /// * `data` - The data scanned from a QR code.
    ///
    /// # Example
    ///
    /// ```no_run
    /// use anyhow::bail;
    /// use futures_util::StreamExt;
    /// use matrix_sdk::{
    ///     Client, authentication::oauth::{
    ///         qrcode::{GrantLoginProgress, QrCodeData, QrCodeModeData, QrProgress},
    ///     }
    /// };
    /// use std::{error::Error, io::stdin};
    /// # _ = async {
    /// # let bytes = unimplemented!();
    /// // You'll need to use a different library to scan and extract the raw bytes from the QR
    /// // code.
    /// let qr_code_data = QrCodeData::from_bytes(bytes)?;
    ///
    /// // Build the client as usual.
    /// let client = Client::builder()
    ///     .server_name_or_homeserver_url("matrix.org")
    ///     .handle_refresh_tokens()
    ///     .build()
    ///     .await?;
    ///
    /// let oauth = client.oauth();
    ///
    /// // Subscribing to the progress is necessary to capture
    /// // the checkcode in order to display it to the other device and to obtain the verification URL to
    /// // open it in a browser so the user can consent to the new login.
    /// let mut grant = oauth.grant_login_with_qr_code().scan(&qr_code_data);
    /// let mut progress = grant.subscribe_to_progress();
    ///
    /// // Create a task which will show us the progress and allows us to receive
    /// // and feed back data.
    /// let task = tokio::spawn(async move {
    ///     while let Some(state) = progress.next().await {
    ///         match state {
    ///             GrantLoginProgress::Starting | GrantLoginProgress::SyncingSecrets => (),
    ///             GrantLoginProgress::EstablishingSecureChannel(QrProgress { check_code }) => {
    ///                 println!("Please enter the checkcode on your other device: {:?}", check_code);
    ///             }
    ///             GrantLoginProgress::WaitingForAuth { verification_uri } => {
    ///                 println!("Please open {verification_uri} to confirm the new login")
    ///             },
    ///             GrantLoginProgress::Done => break,
    ///         }
    ///     }
    ///     Ok::<(), Box<dyn Error + Send + Sync>>(())
    /// });
    ///
    /// // Now run the future to grant the login.
    /// grant.await?;
    /// task.abort();
    ///
    /// println!("Successfully granted login");
    /// # anyhow::Ok(()) };
    /// ```
    pub fn scan(self, data: &'a QrCodeData) -> GrantLoginWithScannedQrCode<'a> {
        GrantLoginWithScannedQrCode::new(self.client, data, self.device_creation_timeout)
    }

    /// This method allows you to grant login to a new device by generating a QR
    /// code on this device to be scanned by the new device.
    ///
    /// This device needs to call this method to generate and display the
    /// QR code which the new device can scan to initiate the grant process.
    ///
    /// A successful login grant using this method will automatically mark the
    /// new device as verified and transfer all end-to-end encryption
    /// related secrets, like the private cross-signing keys and the backup
    /// key from this device device to the new device.
    ///
    /// For the reverse flow where the new device generates the QR code
    /// for this device to scan, use [`GrantLoginWithQrCodeBuilder::scan`].
    ///
    /// # Example
    ///
    /// ```no_run
    /// use anyhow::bail;
    /// use futures_util::StreamExt;
    /// use matrix_sdk::{
    ///     Client, authentication::oauth::{
    ///         qrcode::{GeneratedQrProgress, GrantLoginProgress}
    ///     }
    /// };
    /// use std::{error::Error, io::stdin};
    /// # _ = async {
    /// // Build the client as usual.
    /// let client = Client::builder()
    ///     .server_name_or_homeserver_url("matrix.org")
    ///     .handle_refresh_tokens()
    ///     .build()
    ///     .await?;
    ///
    /// let oauth = client.oauth();
    ///
    /// // Subscribing to the progress is necessary since we need to capture the
    /// // QR code, feed the checkcode back in and obtain the verification URL to
    /// // open it in a browser so the user can consent to the new login.
    /// let mut grant = oauth.grant_login_with_qr_code().generate();
    /// let mut progress = grant.subscribe_to_progress();
    ///
    /// // Create a task which will show us the progress and allows us to receive
    /// // and feed back data.
    /// let task = tokio::spawn(async move {
    ///     while let Some(state) = progress.next().await {
    ///         match state {
    ///             GrantLoginProgress::Starting | GrantLoginProgress::SyncingSecrets => (),
    ///             GrantLoginProgress::EstablishingSecureChannel(GeneratedQrProgress::QrReady(qr_code_data)) => {
    ///                 println!("Please scan the QR code on your other device: {:?}", qr_code_data);
    ///             }
    ///             GrantLoginProgress::EstablishingSecureChannel(GeneratedQrProgress::QrScanned(checkcode_sender)) => {
    ///                 println!("Please enter the code displayed on your other device");
    ///                 let mut s = String::new();
    ///                 stdin().read_line(&mut s)?;
    ///                 let check_code = s.trim().parse::<u8>()?;
    ///                 checkcode_sender.send(check_code).await?;
    ///             }
    ///             GrantLoginProgress::WaitingForAuth { verification_uri } => {
    ///                 println!("Please open {verification_uri} to confirm the new login")
    ///             },
    ///             GrantLoginProgress::Done => break,
    ///         }
    ///     }
    ///     Ok::<(), Box<dyn Error + Send + Sync>>(())
    /// });
    ///
    /// // Now run the future to grant the login.
    /// grant.await?;
    /// task.abort();
    ///
    /// println!("Successfully granted login");
    /// # anyhow::Ok(()) };
    /// ```
    pub fn generate(self) -> GrantLoginWithGeneratedQrCode<'a> {
        GrantLoginWithGeneratedQrCode::new(self.client, self.device_creation_timeout)
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

/// Data to register or restore a client.
#[derive(Debug, Clone)]
pub struct ClientRegistrationData {
    /// The metadata to use to register the client when using dynamic client
    /// registration.
    pub metadata: Raw<ClientMetadata>,

    /// Static registrations for servers that don't support dynamic registration
    /// but provide a client ID out-of-band.
    ///
    /// The keys of the map should be the URLs of the homeservers, but keys
    /// using `issuer` URLs are also supported.
    pub static_registrations: Option<HashMap<Url, ClientId>>,
}

impl ClientRegistrationData {
    /// Construct a [`ClientRegistrationData`] with the given metadata and no
    /// static registrations.
    pub fn new(metadata: Raw<ClientMetadata>) -> Self {
        Self { metadata, static_registrations: None }
    }
}

impl From<Raw<ClientMetadata>> for ClientRegistrationData {
    fn from(value: Raw<ClientMetadata>) -> Self {
        Self::new(value)
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
