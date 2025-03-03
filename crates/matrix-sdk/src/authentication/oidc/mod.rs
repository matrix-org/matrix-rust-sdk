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

//! High-level OpenID Connect API using the Authorization Code flow.
//!
//! The OpenID Connect interactions with the Matrix API are currently a
//! work-in-progress and are defined by [MSC3861] and its sub-proposals. And
//! more documentation is available at [areweoidcyet.com].
//!
//! # OpenID Connect specification compliance
//!
//! The OpenID Connect client used in the SDK has not been certified for
//! compliance against the OpenID Connect specification.
//!
//! It implements only parts of the specification and is currently
//! limited to the Authorization Code flow. It also uses some OAuth 2.0
//! extensions.
//!
//! # Setup
//!
//! To enable support for OpenID Connect on the [`Client`], simply enable the
//! `experimental-oidc` cargo feature for the `matrix-sdk` crate. Then this
//! authentication API is available with [`Client::oidc()`].
//!
//! # Homeserver support
//!
//! After building the client, you can check that the homeserver supports
//! logging in via OAuth 2.0 when [`Oidc::provider_metadata()`] succeeds.
//!
//! # Registration
//!
//! Registration is only required the first time a client encounters an issuer.
//!
//! Note that only public clients are supported by this API, i.e. clients
//! without credentials.
//!
//! If the issuer supports dynamic registration, it can be done by using
//! [`Oidc::register_client()`]. After registration, the client ID should be
//! persisted and reused for every session that interacts with that same issuer.
//!
//! If dynamic registration is not available, the homeserver should document how
//! to obtain a client ID.
//!
//! To provide the client ID and metadata if dynamic registration is not
//! available, or if the client is already registered with the issuer, call
//! [`Oidc::restore_registered_client()`].
//!
//! # Login
//!
//! Before logging in, make sure to register the client or to restore its
//! registration, as it is the first step to know how to interact with the
//! issuer.
//!
//! With OIDC, logging into a Matrix account is simply logging in with a
//! predefined scope, part of it declaring the device ID of the session.
//!
//! [`Oidc::login()`] constructs an [`OidcAuthCodeUrlBuilder`] that can be
//! configured, and then calling [`OidcAuthCodeUrlBuilder::build()`] will
//! provide the URL to present to the user in a web browser. After
//! authenticating with the OIDC provider, the user will be redirected to the
//! provided redirect URI, with a code in the query that will allow to finish
//! the authorization process by calling [`Oidc::finish_authorization()`].
//!
//! When the login is successful, you must then call [`Oidc::finish_login()`].
//!
//! # Persisting/restoring a session
//!
//! A full OIDC session requires two parts:
//!
//! - The client ID obtained after client registration with the corresponding
//!   client metadata,
//! - The user session obtained after login.
//!
//! Both parts are usually stored separately because the client ID can be reused
//! for any session with the same issuer, while the user session is unique.
//!
//! _Note_ that the type returned by [`Oidc::full_session()`] is not
//! (de)serializable. This is done on purpose because the client ID and metadata
//! should be stored separately than the user session, as they should be reused
//! for the same provider across with different user sessions.
//!
//! To restore a previous session, use [`Oidc::restore_session()`].
//!
//! # Refresh tokens
//!
//! The use of refresh tokens with OpenID Connect Providers is more common than
//! in the Matrix specification. For this reason, it is recommended to configure
//! the client with [`ClientBuilder::handle_refresh_tokens()`], to handle
//! refreshing tokens automatically.
//!
//! Applications should then listen to session tokens changes after logging in
//! with [`Oidc::session_tokens_stream()`] to be able to restore the session at
//! a later time, otherwise the end-user will need to login again.
//!
//! # Unknown token error
//!
//! A request to the Matrix API can return an [`Error`] with an
//! [`ErrorKind::UnknownToken`].
//!
//! The first step is to try to refresh the token with
//! [`Oidc::refresh_access_token()`]. This step is done automatically if the
//! client was built with [`ClientBuilder::handle_refresh_tokens()`].
//!
//! If refreshing the access token fails, the next step is to try to request a
//! new login authorization with [`Oidc::login()`], using the device ID from the
//! session. _Note_ that in this case [`Oidc::finish_login()`] must NOT be
//! called after [`Oidc::finish_authorization()`].
//!
//! If this fails again, the client should assume to be logged out, and all
//! local data should be erased.
//!
//! # Account management.
//!
//! The homeserver or provider might advertise a URL that allows the user to
//! manage their account, it can be obtained with
//! [`Oidc::account_management_url()`].
//!
//! # Logout
//!
//! To log the [`Client`] out of the session, simply call [`Oidc::logout()`]. If
//! the provider supports it, it will return a URL to present to the user if
//! they also want to log out from their account on the provider's website.
//!
//! # Examples
//!
//! Most methods have examples, there is also an example CLI application that
//! supports all the actions described here, in [`examples/oidc_cli`].
//!
//! [MSC3861]: https://github.com/matrix-org/matrix-spec-proposals/pull/3861
//! [areweoidcyet.com]: https://areweoidcyet.com/
//! [`ClientBuilder::server_name()`]: crate::ClientBuilder::server_name()
//! [`ClientBuilder::handle_refresh_tokens()`]: crate::ClientBuilder::handle_refresh_tokens()
//! [`Error`]: ruma::api::client::error::Error
//! [`ErrorKind::UnknownToken`]: ruma::api::client::error::ErrorKind::UnknownToken
//! [`AuthenticateError::InsufficientScope`]: ruma::api::client::error::AuthenticateError
//! [`examples/oidc_cli`]: https://github.com/matrix-org/matrix-rust-sdk/tree/main/examples/oidc_cli

use std::{collections::HashMap, fmt, future::Future, pin::Pin, sync::Arc};

use as_variant::as_variant;
use chrono::Utc;
use eyeball::SharedObservable;
use futures_core::Stream;
pub use mas_oidc_client::{error, requests, types};
use mas_oidc_client::{
    http_service::HttpService,
    requests::{
        account_management::{build_account_management_url, AccountManagementActionFull},
        authorization_code::{access_token_with_authorization_code, AuthorizationValidationData},
        discovery::{discover, insecure_discover},
        refresh_token::refresh_access_token,
        registration::register_client,
        revocation::revoke_token,
    },
    types::{
        client_credentials::ClientCredentials,
        errors::{ClientError, ClientErrorCode::AccessDenied},
        iana::oauth::OAuthTokenTypeHint,
        oidc::{
            AccountManagementAction, ProviderMetadata, ProviderMetadataVerificationError,
            VerifiedProviderMetadata,
        },
        registration::{ClientRegistrationResponse, VerifiedClientMetadata},
        requests::Prompt,
        scope::{MatrixApiScopeToken, ScopeToken},
    },
};
#[cfg(feature = "e2e-encryption")]
use matrix_sdk_base::crypto::types::qr_login::QrCodeData;
use matrix_sdk_base::{once_cell::sync::OnceCell, SessionMeta};
use oauth2::{AsyncHttpClient, HttpClientError, HttpRequest, HttpResponse};
use rand::{rngs::StdRng, Rng, SeedableRng};
use ruma::api::client::discovery::{get_authentication_issuer, get_authorization_server_metadata};
use serde::{Deserialize, Serialize};
use sha2::Digest as _;
use thiserror::Error;
use tokio::{spawn, sync::Mutex};
use tracing::{debug, error, info, instrument, trace, warn};
use url::Url;

mod auth_code_builder;
mod cross_process;
#[cfg(all(feature = "e2e-encryption", not(target_arch = "wasm32")))]
pub mod qrcode;
pub mod registrations;
#[cfg(test)]
mod tests;

pub use self::{
    auth_code_builder::{OidcAuthCodeUrlBuilder, OidcAuthorizationData},
    cross_process::CrossProcessRefreshLockError,
};
use self::{
    cross_process::{CrossProcessRefreshLockGuard, CrossProcessRefreshManager},
    qrcode::LoginWithQrCode,
    registrations::{ClientId, OidcRegistrations},
};
use super::AuthData;
use crate::{
    client::SessionChange, http_client::HttpClient, Client, HttpError, RefreshTokenError, Result,
};

pub(crate) struct OidcCtx {
    /// Lock and state when multiple processes may refresh an OIDC session.
    cross_process_token_refresh_manager: OnceCell<CrossProcessRefreshManager>,

    /// Deferred cross-process lock initializer.
    ///
    /// Note: only required because we're using the crypto store that might not
    /// be present before reloading a session.
    deferred_cross_process_lock_init: Mutex<Option<String>>,

    /// Whether to allow HTTP issuer URLs.
    insecure_discover: bool,
}

impl OidcCtx {
    pub(crate) fn new(insecure_discover: bool) -> Self {
        Self {
            insecure_discover,
            cross_process_token_refresh_manager: Default::default(),
            deferred_cross_process_lock_init: Default::default(),
        }
    }
}

pub(crate) struct OidcAuthData {
    pub(crate) issuer: String,
    pub(crate) client_id: ClientId,
    pub(crate) tokens: OnceCell<SharedObservable<OidcSessionTokens>>,
    /// The data necessary to validate authorization responses.
    pub(crate) authorization_data: Mutex<HashMap<String, AuthorizationValidationData>>,
}

impl OidcAuthData {
    /// Get the credentials of client.
    fn credentials(&self) -> ClientCredentials {
        ClientCredentials::None { client_id: self.client_id.0.clone() }
    }
}

#[cfg(not(tarpaulin_include))]
impl fmt::Debug for OidcAuthData {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OidcAuthData").field("issuer", &self.issuer).finish_non_exhaustive()
    }
}

/// A high-level authentication API to interact with an OpenID Connect Provider.
#[derive(Debug, Clone)]
pub struct Oidc {
    /// The underlying Matrix API client.
    client: Client,
}

impl Oidc {
    pub(crate) fn new(client: Client) -> Self {
        Self { client }
    }

    fn ctx(&self) -> &OidcCtx {
        &self.client.inner.auth_ctx.oidc
    }

    fn http_client(&self) -> &HttpClient {
        &self.client.inner.http_client
    }

    fn http_service(&self) -> HttpService {
        HttpService::new(self.http_client().clone())
    }

    /// Enable a cross-process store lock on the state store, to coordinate
    /// refreshes across different processes.
    pub async fn enable_cross_process_refresh_lock(
        &self,
        lock_value: String,
    ) -> Result<(), OidcError> {
        // FIXME: it must be deferred only because we're using the crypto store and it's
        // initialized only in `set_session_meta`, not if we use a dedicated
        // store.
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
    /// Must be called after `set_session_meta`.
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

    /// The OpenID Connect authentication data.
    ///
    /// Returns `None` if the client was not registered or if the registration
    /// was not restored with [`Oidc::restore_registered_client()`] or
    /// [`Oidc::restore_session()`].
    fn data(&self) -> Option<&OidcAuthData> {
        let data = self.client.inner.auth_ctx.auth_data.get()?;
        as_variant!(data, AuthData::Oidc)
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
    ///     authentication::oidc::{
    ///         types::registration::VerifiedClientMetadata,
    ///         qrcode::{LoginProgress, QrCodeData, QrCodeModeData},
    ///     },
    ///     Client,
    /// };
    /// # fn client_metadata() -> VerifiedClientMetadata { unimplemented!() }
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
    /// let oidc = client.oidc();
    /// let metadata: VerifiedClientMetadata = client_metadata();
    ///
    /// // Subscribing to the progress is necessary since we need to input the check
    /// // code on the existing device.
    /// let login = oidc.login_with_qr_code(&qr_code_data, metadata);
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
        client_metadata: VerifiedClientMetadata,
    ) -> LoginWithQrCode<'a> {
        LoginWithQrCode::new(&self.client, client_metadata, data)
    }

    /// A higher level wrapper around the configuration and login methods that
    /// will take some client metadata, register the client if needed and begin
    /// the login process, returning the authorization data required to show a
    /// webview for a user to login to their account. Call
    /// [`Oidc::login_with_oidc_callback`] to finish the process when the
    /// webview is complete.
    pub async fn url_for_oidc(
        &self,
        client_metadata: VerifiedClientMetadata,
        registrations: OidcRegistrations,
        prompt: Prompt,
    ) -> Result<OidcAuthorizationData, OidcError> {
        let metadata = self.provider_metadata().await?;

        let redirect_uris =
            client_metadata.redirect_uris.clone().ok_or(OidcError::MissingRedirectUri)?;

        let redirect_url = redirect_uris.first().ok_or(OidcError::MissingRedirectUri)?;

        self.configure(metadata.issuer().to_owned(), client_metadata, registrations).await?;

        let mut data_builder = self.login(redirect_url.clone(), None)?;
        data_builder = data_builder.prompt(vec![prompt]);
        let data = data_builder.build().await?;

        Ok(data)
    }

    /// A higher level wrapper around the methods to complete a login after the
    /// user has logged in through a webview. This method should be used in
    /// tandem with [`Oidc::url_for_oidc`].
    pub async fn login_with_oidc_callback(
        &self,
        authorization_data: &OidcAuthorizationData,
        callback_url: Url,
    ) -> Result<()> {
        let response = AuthorizationResponse::parse_uri(&callback_url)
            .or(Err(OidcError::InvalidCallbackUrl))?;

        let code = match response {
            AuthorizationResponse::Success(code) => code,
            AuthorizationResponse::Error(err) => {
                if err.error.error == AccessDenied {
                    // The user cancelled the login in the web view.
                    return Err(OidcError::CancelledAuthorization.into());
                }
                return Err(OidcError::Authorization(err).into());
            }
        };

        // This check will also be done in `finish_authorization`, however it requires
        // the client to have called `abort_authorization` which we can't guarantee so
        // lets double check with their supplied authorization data to be safe.
        if code.state != authorization_data.state {
            return Err(OidcError::InvalidState.into());
        };

        self.finish_authorization(code).await?;
        self.finish_login().await?;

        Ok(())
    }

    /// Higher level wrapper that restores the OIDC client with automatic
    /// static/dynamic client registration.
    async fn configure(
        &self,
        issuer: String,
        client_metadata: VerifiedClientMetadata,
        registrations: OidcRegistrations,
    ) -> std::result::Result<(), OidcError> {
        if self.client_id().is_some() {
            tracing::info!("OIDC is already configured.");
            return Ok(());
        };

        if self.load_client_registration(issuer.clone(), &registrations) {
            tracing::info!("OIDC configuration loaded from disk.");
            return Ok(());
        }

        tracing::info!("Registering this client for OIDC.");
        self.register_client(client_metadata, None).await?;

        tracing::info!("Persisting OIDC registration data.");
        self.store_client_registration(&registrations)
            .map_err(|e| OidcError::UnknownError(Box::new(e)))?;

        Ok(())
    }

    /// Stores the current OIDC dynamic client registration so it can be re-used
    /// if we ever log in via the same issuer again.
    fn store_client_registration(
        &self,
        registrations: &OidcRegistrations,
    ) -> std::result::Result<(), OidcError> {
        let issuer = Url::parse(self.issuer().expect("issuer should be set after registration"))
            .map_err(OidcError::Url)?;
        let client_id = self.client_id().ok_or(OidcError::NotRegistered)?.to_owned();

        registrations
            .set_and_write_client_id(client_id, issuer)
            .map_err(|e| OidcError::UnknownError(Box::new(e)))?;

        Ok(())
    }

    /// Attempts to load an existing OIDC dynamic client registration for a
    /// given issuer.
    ///
    /// Returns `true` if an existing registration was found and `false` if not.
    fn load_client_registration(&self, issuer: String, registrations: &OidcRegistrations) -> bool {
        let Ok(issuer_url) = Url::parse(&issuer) else {
            error!("Failed to parse {issuer:?}");
            return false;
        };
        let Some(client_id) = registrations.client_id(&issuer_url) else {
            return false;
        };

        self.restore_registered_client(issuer, client_id);

        true
    }

    /// The OpenID Connect Provider used for authorization.
    ///
    /// Returns `None` if the client was not registered or if the registration
    /// was not restored with [`Oidc::restore_registered_client()`] or
    /// [`Oidc::restore_session()`].
    pub fn issuer(&self) -> Option<&str> {
        self.data().map(|data| data.issuer.as_str())
    }

    /// The account management actions supported by the provider's account
    /// management URL.
    ///
    /// Returns `Ok(None)` if the data was not found. Returns an error if the
    /// request to get the provider metadata fails.
    pub async fn account_management_actions_supported(
        &self,
    ) -> Result<Option<Vec<AccountManagementAction>>, OidcError> {
        let provider_metadata = self.provider_metadata().await?;

        Ok(provider_metadata.account_management_actions_supported.clone())
    }

    /// Build the URL where the user can manage their account.
    ///
    /// # Arguments
    ///
    /// * `action` - An optional action that wants to be performed by the user
    ///   when they open the URL. The list of supported actions by the account
    ///   management URL can be found in the [`VerifiedProviderMetadata`], or
    ///   directly with [`Oidc::account_management_actions_supported()`].
    ///
    /// Returns `Ok(None)` if the URL was not found. Returns an error if the
    /// request to get the provider metadata fails or the URL could not be
    /// parsed.
    pub async fn fetch_account_management_url(
        &self,
        action: Option<AccountManagementActionFull>,
    ) -> Result<Option<Url>, OidcError> {
        let provider_metadata = self.provider_metadata().await?;
        self.management_url_from_provider_metadata(provider_metadata, action)
    }

    fn management_url_from_provider_metadata(
        &self,
        provider_metadata: VerifiedProviderMetadata,
        action: Option<AccountManagementActionFull>,
    ) -> Result<Option<Url>, OidcError> {
        let Some(base_url) = provider_metadata.account_management_uri.clone() else {
            return Ok(None);
        };

        let url = build_account_management_url(base_url, action, None)?;

        Ok(Some(url))
    }

    /// Get the account management URL where the user can manage their
    /// identity-related settings.
    ///
    /// # Arguments
    ///
    /// * `action` - An optional action that wants to be performed by the user
    ///   when they open the URL. The list of supported actions by the account
    ///   management URL can be found in the [`VerifiedProviderMetadata`], or
    ///   directly with [`Oidc::account_management_actions_supported()`].
    ///
    /// Returns `Ok(None)` if the URL was not found. Returns an error if the
    /// request to get the provider metadata fails or the URL could not be
    /// parsed.
    ///
    /// This method will cache the URL for a while, if the cache is not
    /// populated it will internally call
    /// [`Oidc::fetch_account_management_url()`] and cache the resulting URL
    /// before returning it.
    pub async fn account_management_url(
        &self,
        action: Option<AccountManagementActionFull>,
    ) -> Result<Option<Url>, OidcError> {
        const CACHE_KEY: &str = "PROVIDER_METADATA";

        let mut cache = self.client.inner.caches.provider_metadata.lock().await;

        let metadata = if let Some(metadata) = cache.get("PROVIDER_METADATA") {
            metadata
        } else {
            let provider_metadata = self.provider_metadata().await?;
            cache.insert(CACHE_KEY.to_owned(), provider_metadata.clone());
            provider_metadata
        };

        self.management_url_from_provider_metadata(metadata, action)
    }

    /// Fetch the OAuth 2.0 server metadata of the homeserver.
    ///
    /// Returns an error if a problem occurred when fetching or validating the
    /// metadata.
    pub async fn provider_metadata(&self) -> Result<VerifiedProviderMetadata, OauthDiscoveryError> {
        match self.client.send(get_authorization_server_metadata::msc2965::Request::new()).await {
            Ok(response) => {
                let metadata = response.metadata.deserialize_as::<ProviderMetadata>()?;

                let result = if self.ctx().insecure_discover {
                    metadata.insecure_verify_metadata()
                } else {
                    // The mas-oidc-client method needs to compare the issuer for validation. It's a
                    // bit unnecessary because we take it from the metadata, oh well.
                    let issuer =
                        metadata.issuer.clone().ok_or(error::DiscoveryError::Validation(
                            ProviderMetadataVerificationError::MissingIssuer,
                        ))?;
                    metadata.validate(&issuer)
                };

                return Ok(result.map_err(error::DiscoveryError::Validation)?);
            }
            Err(error)
                if error
                    .as_client_api_error()
                    .is_some_and(|err| err.status_code == http::StatusCode::NOT_FOUND) =>
            {
                // Fallback to OIDC discovery.
            }
            Err(error) => return Err(error.into()),
        };

        // TODO: remove this fallback behavior when the metadata endpoint has wider
        // support.
        #[allow(deprecated)]
        let issuer =
            match self.client.send(get_authentication_issuer::msc2965::Request::new()).await {
                Ok(response) => response.issuer,
                Err(error)
                    if error
                        .as_client_api_error()
                        .is_some_and(|err| err.status_code == http::StatusCode::NOT_FOUND) =>
                {
                    return Err(OauthDiscoveryError::NotSupported);
                }
                Err(error) => return Err(error.into()),
            };

        if self.ctx().insecure_discover {
            insecure_discover(&self.http_service(), &issuer).await.map_err(Into::into)
        } else {
            discover(&self.http_service(), &issuer).await.map_err(Into::into)
        }
    }

    /// The OpenID Connect unique identifier of this client obtained after
    /// registration.
    ///
    /// Returns `None` if the client was not registered or if the registration
    /// was not restored with [`Oidc::restore_registered_client()`] or
    /// [`Oidc::restore_session()`].
    pub fn client_id(&self) -> Option<&ClientId> {
        self.data().map(|data| &data.client_id)
    }

    /// Set the current session tokens.
    ///
    /// # Panics
    ///
    /// Will panic if no OIDC client has been configured yet.
    pub(crate) fn set_session_tokens(&self, session_tokens: OidcSessionTokens) {
        let data =
            self.data().expect("Cannot call OpenID Connect API after logging in with another API");
        if let Some(tokens) = data.tokens.get() {
            tokens.set_if_not_eq(session_tokens);
        } else {
            let _ = data.tokens.set(SharedObservable::new(session_tokens));
        }
    }

    /// The tokens received after authorization of this client.
    ///
    /// Returns `None` if the client was not logged in with the OpenID Connect
    /// API.
    pub fn session_tokens(&self) -> Option<OidcSessionTokens> {
        Some(self.data()?.tokens.get()?.get())
    }

    /// Get changes to the session tokens as a [`Stream`].
    ///
    /// Returns `None` if the client was not logged in with the OpenID Connect
    /// API.
    ///
    /// After login, the tokens should only change when refreshing the access
    /// token or authorizing new scopes.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use futures_util::StreamExt;
    /// use matrix_sdk::Client;
    /// # fn persist_session(_: &matrix_sdk::authentication::oidc::OidcSession) {}
    /// # _ = async {
    /// let homeserver = "http://example.com";
    /// let client = Client::builder()
    ///     .homeserver_url(homeserver)
    ///     .handle_refresh_tokens()
    ///     .build()
    ///     .await?;
    ///
    /// // Login with the OpenID Connect API…
    ///
    /// let oidc = client.oidc();
    /// let session = oidc.full_session().expect("Client should be logged in");
    /// persist_session(&session);
    ///
    /// // Handle when at least one of the tokens changed.
    /// let mut tokens_stream =
    ///     oidc.session_tokens_stream().expect("Client should be logged in");
    /// loop {
    ///     if tokens_stream.next().await.is_some() {
    ///         let session =
    ///             oidc.full_session().expect("Client should be logged in");
    ///         persist_session(&session);
    ///     }
    /// }
    /// # anyhow::Ok(()) };
    /// ```
    pub fn session_tokens_stream(&self) -> Option<impl Stream<Item = OidcSessionTokens>> {
        Some(self.data()?.tokens.get()?.subscribe())
    }

    /// Get the current access token for this session.
    ///
    /// Returns `None` if the client was not logged in with the OpenID Connect
    /// API.
    pub fn access_token(&self) -> Option<String> {
        self.session_tokens().map(|tokens| tokens.access_token)
    }

    /// Get the current refresh token for this session.
    ///
    /// Returns `None` if the client was not logged in with the OpenID Connect
    /// API, or if the access token cannot be refreshed.
    pub fn refresh_token(&self) -> Option<String> {
        self.session_tokens().and_then(|tokens| tokens.refresh_token)
    }

    /// The OpenID Connect user session of this client.
    ///
    /// Returns `None` if the client was not logged in with the OpenID Connect
    /// API.
    pub fn user_session(&self) -> Option<UserSession> {
        let meta = self.client.session_meta()?.to_owned();
        let tokens = self.session_tokens()?;
        let issuer = self.data()?.issuer.clone();
        Some(UserSession { meta, tokens, issuer })
    }

    /// The full OpenID Connect session of this client.
    ///
    /// Returns `None` if the client was not logged in with the OpenID Connect
    /// API.
    pub fn full_session(&self) -> Option<OidcSession> {
        let user = self.user_session()?;
        let data = self.data()?;
        Some(OidcSession { client_id: data.client_id.clone(), user })
    }

    /// Register a client with the OAuth 2.0 server.
    ///
    /// This should be called before any authorization request with an unknown
    /// authorization server. If the client is already registered with the
    /// given issuer, it should use [`Oidc::restore_registered_client()`].
    ///
    /// Note that this method only supports public clients, i.e. clients with
    /// the `token_endpoint_auth_method` field in [`VerifiedClientMetadata`] set
    /// to `none`.
    ///
    /// The client should adapt the security measures enabled in its metadata
    /// according to the capabilities advertised in
    /// [`Oidc::provider_metadata()`].
    ///
    /// # Arguments
    ///
    /// * `client_metadata` - The [`VerifiedClientMetadata`] to register.
    ///
    /// * `software_statement` - A [software statement], a digitally signed
    ///   version of the metadata, as a JWT. Any claim in this JWT will override
    ///   the corresponding field in the client metadata. It must include a
    ///   `software_id` claim that is used to uniquely identify a client and
    ///   ensure the same `client_id` is returned on subsequent registration,
    ///   allowing to update the registered client metadata.
    ///
    /// The client ID in the response should be persisted for future use and
    /// reused for the same authorization server, identified by the
    /// [`Oidc::issuer()`], along with the client metadata sent to the provider,
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
    /// use matrix_sdk::authentication::oidc::registrations::ClientId;
    /// use matrix_sdk::authentication::oidc::types::registration::ClientMetadata;
    /// # let client_metadata = ClientMetadata::default().validate().unwrap();
    /// # fn persist_client_registration (_: &str, _: &ClientMetadata, _: &ClientId) {}
    /// # _ = async {
    /// let server_name = ServerName::parse("myhomeserver.org")?;
    /// let client = Client::builder().server_name(&server_name).build().await?;
    /// let oidc = client.oidc();
    ///
    /// if let Err(error) = oidc.provider_metadata().await {
    ///     if error.is_not_supported() {
    ///         println!("OAuth 2.0 is not supported");
    ///     }
    ///
    ///     return Err(error.into());
    /// }
    ///
    /// let response = oidc
    ///     .register_client(client_metadata.clone(), None)
    ///     .await?;
    ///
    /// println!(
    ///     "Registered with client_id: {}",
    ///     response.client_id
    /// );
    ///
    /// // The API only supports clients without secrets.
    /// let client_id = ClientId(response.client_id);
    /// let issuer = oidc.issuer().expect("issuer should be set after registration");
    ///
    /// persist_client_registration(issuer, &client_metadata, &client_id);
    /// # anyhow::Ok(()) };
    /// ```
    ///
    /// [software statement]: https://datatracker.ietf.org/doc/html/rfc7591#autoid-8
    pub async fn register_client(
        &self,
        client_metadata: VerifiedClientMetadata,
        software_statement: Option<String>,
    ) -> Result<ClientRegistrationResponse, OidcError> {
        let provider_metadata = self.provider_metadata().await?;

        let registration_endpoint = provider_metadata
            .registration_endpoint
            .as_ref()
            .ok_or(OidcError::NoRegistrationSupport)?;

        let registration_response = register_client(
            &self.http_service(),
            registration_endpoint,
            client_metadata,
            software_statement,
        )
        .await?;

        // The format of the credentials changes according to the client metadata that
        // was sent. Public clients only get a client ID.
        self.restore_registered_client(
            provider_metadata.issuer().to_owned(),
            ClientId(registration_response.client_id.clone()),
        );

        Ok(registration_response)
    }

    /// Set the data of a client that is registered with an OpenID Connect
    /// Provider.
    ///
    /// This should be called when logging in with a provider that is already
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
    ///   the provider, obtained after registration.
    ///
    /// # Panic
    ///
    /// Panics if authentication data was already set.
    pub fn restore_registered_client(&self, issuer: String, client_id: ClientId) {
        let data = OidcAuthData {
            issuer,
            client_id,
            tokens: Default::default(),
            authorization_data: Default::default(),
        };

        self.client
            .inner
            .auth_ctx
            .auth_data
            .set(AuthData::Oidc(data))
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
    ///
    /// # Panic
    ///
    /// Panics if authentication data was already set.
    pub async fn restore_session(&self, session: OidcSession) -> Result<()> {
        let OidcSession { client_id, user: UserSession { meta, tokens, issuer } } = session;

        let data = OidcAuthData {
            issuer,
            client_id,
            tokens: SharedObservable::new(tokens.clone()).into(),
            authorization_data: Default::default(),
        };

        self.client
            .set_session_meta(
                meta,
                #[cfg(feature = "e2e-encryption")]
                None,
            )
            .await?;
        self.deferred_enable_cross_process_refresh_lock().await;

        self.client
            .inner
            .auth_ctx
            .auth_data
            .set(AuthData::Oidc(data))
            .expect("Client authentication data was already set");

        // Initialize the cross-process locking by saving our tokens' hash into the
        // database, if we've enabled the cross-process lock.

        if let Some(cross_process_lock) = self.ctx().cross_process_token_refresh_manager.get() {
            cross_process_lock.restore_session(&tokens).await;

            let mut guard = cross_process_lock
                .spin_lock()
                .await
                .map_err(|err| crate::Error::Oidc(err.into()))?;

            // After we got the lock, it's possible that our session doesn't match the one
            // read from the database, because of a race: another process has
            // refreshed the tokens while we were waiting for the lock.
            //
            // In that case, if there's a mismatch, we reload the session and update the
            // hash. Otherwise, we save our hash into the database.

            if guard.hash_mismatch {
                Box::pin(self.handle_session_hash_mismatch(&mut guard))
                    .await
                    .map_err(|err| crate::Error::Oidc(err.into()))?;
            } else {
                guard
                    .save_in_memory_and_db(&tokens)
                    .await
                    .map_err(|err| crate::Error::Oidc(err.into()))?;
                // No need to call the save_session_callback here; it was the
                // source of the session, so it's already in
                // sync with what we had.
            }
        }

        #[cfg(feature = "e2e-encryption")]
        self.client.encryption().spawn_initialization_task(None);

        Ok(())
    }

    async fn handle_session_hash_mismatch(
        &self,
        guard: &mut CrossProcessRefreshLockGuard,
    ) -> Result<(), CrossProcessRefreshLockError> {
        trace!("Handling hash mismatch.");

        let callback = self
            .client
            .inner
            .auth_ctx
            .reload_session_callback
            .get()
            .ok_or(CrossProcessRefreshLockError::MissingReloadSession)?;

        match callback(self.client.clone()) {
            Ok(tokens) => {
                let crate::authentication::SessionTokens::Oidc(tokens) = tokens else {
                    return Err(CrossProcessRefreshLockError::InvalidSessionTokens);
                };

                guard.handle_mismatch(&tokens).await?;

                self.set_session_tokens(tokens.clone());
                // The app's callback acted as authoritative here, so we're not
                // saving the data back into the app, as that would have no
                // effect.
            }
            Err(err) => {
                error!("when reloading OIDC session tokens from callback: {err}");
            }
        }

        Ok(())
    }

    /// The scopes to request for logging in.
    fn login_scopes(device_id: Option<String>) -> Result<[ScopeToken; 3], OidcError> {
        // Generate the device ID if it is not provided.
        let device_id = device_id.unwrap_or_else(|| {
            rand::thread_rng()
                .sample_iter(&rand::distributions::Alphanumeric)
                .map(char::from)
                .take(10)
                .collect::<String>()
        });

        Ok([
            ScopeToken::Openid,
            ScopeToken::MatrixApi(MatrixApiScopeToken::Full),
            ScopeToken::try_with_matrix_device(device_id).or(Err(OidcError::InvalidDeviceId))?,
        ])
    }

    /// Login via OpenID Connect with the Authorization Code flow.
    ///
    /// This should be called after [`Oidc::register_client()`] or
    /// [`Oidc::restore_registered_client()`].
    ///
    /// If this is a brand new login, [`Oidc::finish_login()`] must be called
    /// after [`Oidc::finish_authorization()`], to finish loading the user
    /// session.
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
    /// # Errors
    ///
    /// Returns an error if the device ID is not valid.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use anyhow::anyhow;
    /// use matrix_sdk::{Client};
    /// # use matrix_sdk::authentication::oidc::AuthorizationResponse;
    /// # use url::Url;
    /// # let homeserver = Url::parse("https://example.com").unwrap();
    /// # let redirect_uri = Url::parse("http://127.0.0.1/oidc").unwrap();
    /// # let redirected_to_uri = Url::parse("http://127.0.0.1/oidc").unwrap();
    /// # let issuer_info = unimplemented!();
    /// # let client_id = unimplemented!();
    /// # _ = async {
    /// # let client = Client::new(homeserver).await?;
    /// let oidc = client.oidc();
    ///
    /// oidc.restore_registered_client(
    ///     issuer_info,
    ///     client_id,
    /// );
    ///
    /// let auth_data = oidc.login(redirect_uri, None)?.build().await?;
    ///
    /// // Open auth_data.url and wait for response at the redirect URI.
    /// // The full URL obtained is called here `redirected_to_uri`.
    ///
    /// let auth_response = AuthorizationResponse::parse_uri(&redirected_to_uri)?;
    ///
    /// let code = match auth_response {
    ///     AuthorizationResponse::Success(code) => code,
    ///     AuthorizationResponse::Error(error) => {
    ///         return Err(anyhow!("Authorization failed: {:?}", error));
    ///     }
    /// };
    ///
    /// let _tokens_response = oidc.finish_authorization(code).await?;
    ///
    /// // Important! Without this we can't access the full session.
    /// oidc.finish_login().await?;
    ///
    /// // The session tokens can be persisted either from the response, or from
    /// // one of the `Oidc::session_tokens()` method.
    ///
    /// // You can now make any request compatible with the requested scope.
    /// let _me = client.whoami().await?;
    /// # anyhow::Ok(()) }
    /// ```
    pub fn login(
        &self,
        redirect_uri: Url,
        device_id: Option<String>,
    ) -> Result<OidcAuthCodeUrlBuilder, OidcError> {
        let scope = Self::login_scopes(device_id)?.into_iter().collect();

        Ok(OidcAuthCodeUrlBuilder::new(self.clone(), scope, redirect_uri))
    }

    /// Finish the login process.
    ///
    /// Must be called after [`Oidc::finish_authorization()`] after logging into
    /// a brand new session, to load the last part of the user session and
    /// complete the initialization of the [`Client`].
    ///
    /// # Panic
    ///
    /// Panics if the login was already completed.
    pub async fn finish_login(&self) -> Result<()> {
        // Get the user ID.
        let whoami_res = self.client.whoami().await.map_err(crate::Error::from)?;

        let session = SessionMeta {
            user_id: whoami_res.user_id,
            device_id: whoami_res.device_id.ok_or(OidcError::MissingDeviceId)?,
        };

        self.client
            .set_session_meta(
                session,
                #[cfg(feature = "e2e-encryption")]
                None,
            )
            .await?;
        // At this point the Olm machine has been set up.

        // Enable the cross-process lock for refreshes, if needs be.
        self.enable_cross_process_lock().await.map_err(OidcError::from)?;

        #[cfg(feature = "e2e-encryption")]
        self.client.encryption().spawn_initialization_task(None);

        Ok(())
    }

    pub(crate) async fn enable_cross_process_lock(
        &self,
    ) -> Result<(), CrossProcessRefreshLockError> {
        // Enable the cross-process lock for refreshes, if needs be.
        self.deferred_enable_cross_process_refresh_lock().await;

        if let Some(cross_process_manager) = self.ctx().cross_process_token_refresh_manager.get() {
            if let Some(tokens) = self.session_tokens() {
                let mut cross_process_guard = cross_process_manager.spin_lock().await?;

                if cross_process_guard.hash_mismatch {
                    // At this point, we're finishing a login while another process had written
                    // something in the database. It's likely the information in the database it
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
    /// [`OidcAuthCodeUrlBuilder::build()`] has been presented and the user has
    /// been redirected to the redirect URI after a successful authorization.
    ///
    /// If the authorization has not been successful,
    /// [`Oidc::abort_authorization()`] should be used instead to clean up the
    /// local data.
    ///
    /// # Arguments
    ///
    /// * `auth_code` - The response received as part of the redirect URI when
    ///   the authorization was successful.
    ///
    /// Returns an error if a request fails.
    pub async fn finish_authorization(
        &self,
        auth_code: AuthorizationCode,
    ) -> Result<(), OidcError> {
        let data = self.data().ok_or(OidcError::NotAuthenticated)?;
        let validation_data = data
            .authorization_data
            .lock()
            .await
            .remove(&auth_code.state)
            .ok_or(OidcError::InvalidState)?;

        let provider_metadata = self.provider_metadata().await?;

        let (response, _) = access_token_with_authorization_code(
            &self.http_service(),
            data.credentials(),
            provider_metadata.token_endpoint(),
            auth_code.code,
            validation_data,
            None,
            Utc::now(),
            &mut rng()?,
        )
        .await?;

        self.set_session_tokens(OidcSessionTokens {
            access_token: response.access_token,
            refresh_token: response.refresh_token,
        });

        Ok(())
    }

    /// Abort the authorization process.
    ///
    /// This method should be called after the URL returned by
    /// [`OidcAuthCodeUrlBuilder::build()`] has been presented and the user has
    /// been redirected to the redirect URI after a failed authorization, or if
    /// the authorization should be aborted before it is completed.
    ///
    /// If the authorization has been successful,
    /// [`Oidc::finish_authorization()`] should be used instead.
    ///
    /// # Arguments
    ///
    /// * `state` - The state received as part of the redirect URI when the
    ///   authorization failed, or the one provided in [`OidcAuthorizationData`]
    ///   after building the authorization URL.
    pub async fn abort_authorization(&self, state: &str) {
        if let Some(data) = self.data() {
            data.authorization_data.lock().await.remove(state);
        }
    }

    /// Request codes from the authorization server for logging in with another
    /// device.
    #[cfg(all(feature = "e2e-encryption", not(target_arch = "wasm32")))]
    async fn request_device_authorization(
        &self,
        device_id: Option<String>,
    ) -> Result<oauth2::StandardDeviceAuthorizationResponse, qrcode::DeviceAuthorizationOauthError>
    {
        let scopes = Self::login_scopes(device_id)?
            .into_iter()
            .map(|scope| oauth2::Scope::new(scope.to_string()));

        let client_id =
            oauth2::ClientId::new(self.client_id().ok_or(OidcError::NotRegistered)?.0.clone());

        let server_metadata = self.provider_metadata().await.map_err(OidcError::from)?;
        let device_authorization_url = server_metadata
            .device_authorization_endpoint
            .clone()
            .map(oauth2::DeviceAuthorizationUrl::from_url)
            .ok_or(qrcode::DeviceAuthorizationOauthError::NoDeviceAuthorizationEndpoint)?;

        let response = oauth2::basic::BasicClient::new(client_id)
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
        device_authorization_response: &oauth2::StandardDeviceAuthorizationResponse,
    ) -> Result<(), qrcode::DeviceAuthorizationOauthError> {
        use oauth2::TokenResponse;

        let client_id =
            oauth2::ClientId::new(self.client_id().ok_or(OidcError::NotRegistered)?.0.clone());

        let server_metadata = self.provider_metadata().await.map_err(OidcError::from)?;
        let token_uri = oauth2::TokenUrl::from_url(server_metadata.token_endpoint().clone());

        let response = oauth2::basic::BasicClient::new(client_id)
            .set_token_uri(token_uri)
            .exchange_device_access_token(device_authorization_response)
            .request_async(self.http_client(), tokio::time::sleep, None)
            .await?;

        self.set_session_tokens(OidcSessionTokens {
            access_token: response.access_token().secret().to_owned(),
            refresh_token: response.refresh_token().map(|t| t.secret().to_owned()),
        });

        Ok(())
    }

    async fn refresh_access_token_inner(
        self,
        refresh_token: String,
        provider_metadata: VerifiedProviderMetadata,
        credentials: ClientCredentials,
        cross_process_lock: Option<CrossProcessRefreshLockGuard>,
    ) -> Result<(), OidcError> {
        trace!(
            "Token refresh: attempting to refresh with refresh_token {:x}",
            hash_str(&refresh_token)
        );

        let (response, _) = refresh_access_token(
            &self.http_service(),
            credentials,
            provider_metadata.token_endpoint(),
            refresh_token.clone(),
            None,
            None,
            None,
            Utc::now(),
            &mut rng()?,
        )
        .await?;

        trace!(
            "Token refresh: new refresh_token: {} / access_token: {:x}",
            response
                .refresh_token
                .as_deref()
                .map(|token| format!("{:x}", hash_str(token)))
                .unwrap_or_else(|| "<none>".to_owned()),
            hash_str(&response.access_token)
        );

        let tokens = OidcSessionTokens {
            access_token: response.access_token,
            refresh_token: response.refresh_token.or(Some(refresh_token)),
        };

        self.set_session_tokens(tokens.clone());

        // Call the save_session_callback if set, while the optional lock is being held.
        if let Some(save_session_callback) = self.client.inner.auth_ctx.save_session_callback.get()
        {
            // Satisfies the save_session_callback invariant: set_session_tokens has
            // been called just above.
            if let Err(err) = save_session_callback(self.client.clone()) {
                error!("when saving session after refresh: {err}");
            }
        }

        if let Some(mut lock) = cross_process_lock {
            lock.save_in_memory_and_db(&tokens).await?;
        }

        _ = self.client.inner.auth_ctx.session_change_sender.send(SessionChange::TokensRefreshed);

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

        let refresh_status_lock = client.inner.auth_ctx.refresh_token_lock.clone().try_lock_owned();

        let Ok(mut refresh_status_guard) = refresh_status_lock else {
            debug!("another refresh is happening, waiting for result.");
            // There's already a request to refresh happening in the same process. Wait for
            // it to finish.
            let res = client.inner.auth_ctx.refresh_token_lock.lock().await.clone();
            debug!("other refresh is a {}", if res.is_ok() { "success" } else { "failure " });
            return res;
        };

        debug!("no other refresh happening in background, starting.");

        let cross_process_guard =
            if let Some(manager) = self.ctx().cross_process_token_refresh_manager.get() {
                let mut cross_process_guard = match manager
                    .spin_lock()
                    .await
                    .map_err(|err| RefreshTokenError::Oidc(Arc::new(err.into())))
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
                        .map_err(|err| RefreshTokenError::Oidc(Arc::new(err.into())))?;
                    // Optimistic exit: assume that the underlying process did update fast enough.
                    // In the worst case, we'll do another refresh Soon™.
                    info!("other process handled refresh for us, assuming success");
                    *refresh_status_guard = Ok(());
                    return Ok(());
                }

                Some(cross_process_guard)
            } else {
                None
            };

        let Some(session_tokens) = self.session_tokens() else {
            warn!("invalid state: missing session tokens");
            fail!(refresh_status_guard, RefreshTokenError::RefreshTokenRequired);
        };

        let Some(refresh_token) = session_tokens.refresh_token else {
            warn!("invalid state: missing session tokens");
            fail!(refresh_status_guard, RefreshTokenError::RefreshTokenRequired);
        };

        let provider_metadata = match self.provider_metadata().await {
            Ok(metadata) => metadata,
            Err(err) => {
                warn!("couldn't get authorization server metadata: {err:?}");
                fail!(refresh_status_guard, RefreshTokenError::Oidc(Arc::new(err.into())));
            }
        };

        let Some(auth_data) = self.data() else {
            warn!("invalid state: missing auth data");
            fail!(
                refresh_status_guard,
                RefreshTokenError::Oidc(Arc::new(OidcError::NotAuthenticated))
            );
        };

        let credentials = auth_data.credentials();

        // Do not interrupt refresh access token requests and processing, by detaching
        // the request sending and response processing.
        // Make sure to keep the `refresh_status_guard` during the entire processing.

        let this = self.clone();

        spawn(async move {
            match this
                .refresh_access_token_inner(
                    refresh_token,
                    provider_metadata,
                    credentials,
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
                    let err = RefreshTokenError::Oidc(Arc::new(err));
                    warn!("error refreshing an oidc token: {err}");
                    fail!(refresh_status_guard, err);
                }
            }
        })
        .await
        .expect("joining")
    }

    /// Log out from the currently authenticated session.
    pub async fn logout(&self) -> Result<(), OidcError> {
        let provider_metadata = self.provider_metadata().await?;
        let client_credentials = self.data().ok_or(OidcError::NotAuthenticated)?.credentials();

        let revocation_endpoint =
            provider_metadata.revocation_endpoint.as_ref().ok_or(OidcError::NoRevocationSupport)?;

        let tokens = self.session_tokens().ok_or(OidcError::NotAuthenticated)?;

        // Revoke the access token, it should revoke both tokens.
        revoke_token(
            &self.http_service(),
            client_credentials,
            revocation_endpoint,
            tokens.access_token,
            Some(OAuthTokenTypeHint::AccessToken),
            Utc::now(),
            &mut rng()?,
        )
        .await?;

        if let Some(manager) = self.ctx().cross_process_token_refresh_manager.get() {
            manager.on_logout().await?;
        }

        Ok(())
    }
}

/// A full session for the OpenID Connect API.
#[derive(Debug, Clone)]
pub struct OidcSession {
    /// The client ID obtained after registration.
    pub client_id: ClientId,

    /// The user session.
    pub user: UserSession,
}

/// A user session for the OpenID Connect API.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserSession {
    /// The Matrix user session info.
    #[serde(flatten)]
    pub meta: SessionMeta,

    /// The tokens used for authentication.
    #[serde(flatten)]
    pub tokens: OidcSessionTokens,

    /// The OpenID Connect provider used for this session.
    pub issuer: String,
}

/// The tokens for a user session obtained with the OpenID Connect API.
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[allow(missing_debug_implementations)]
pub struct OidcSessionTokens {
    /// The access token used for this session.
    pub access_token: String,

    /// The token used for refreshing the access token, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
}

#[cfg(not(tarpaulin_include))]
impl fmt::Debug for OidcSessionTokens {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OidcSessionTokens").finish_non_exhaustive()
    }
}

/// The data returned by the provider in the redirect URI after a successful
/// authorization.
#[derive(Debug, Clone)]
pub enum AuthorizationResponse {
    /// A successful response.
    Success(AuthorizationCode),

    /// An error response.
    Error(AuthorizationError),
}

impl AuthorizationResponse {
    /// Deserialize an `AuthorizationResponse` from the given URI.
    ///
    /// Returns an error if the URL doesn't have the expected format.
    pub fn parse_uri(uri: &Url) -> Result<Self, RedirectUriQueryParseError> {
        let Some(query) = uri.query() else { return Err(RedirectUriQueryParseError::MissingQuery) };

        Self::parse_query(query)
    }

    /// Deserialize an `AuthorizationResponse` from the query part of a URI.
    ///
    /// Returns an error if the URL doesn't have the expected format.
    pub fn parse_query(query: &str) -> Result<Self, RedirectUriQueryParseError> {
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

/// The data returned by the provider in the redirect URI after a successful
/// authorization.
#[derive(Debug, Clone, Deserialize)]
pub struct AuthorizationCode {
    /// The code to use to retrieve the access token.
    pub code: String,
    /// The unique identifier for this transaction.
    pub state: String,
}

/// The data returned by the provider in the redirect URI after an authorization
/// error.
#[derive(Debug, Clone, Deserialize)]
pub struct AuthorizationError {
    /// The error.
    #[serde(flatten)]
    pub error: ClientError,
    /// The unique identifier for this transaction.
    pub state: String,
}

/// An error when trying to parse the query of a redirect URI.
#[derive(Debug, Clone, Error)]
pub enum RedirectUriQueryParseError {
    /// There is no query part in the URI.
    #[error("No query in URI")]
    MissingQuery,

    /// Deserialization failed.
    #[error("Query is not using one of the defined formats")]
    UnknownFormat,
}

/// All errors that can occur when using the OpenID Connect API.
#[derive(Debug, Error)]
pub enum OidcError {
    /// An error occurred when interacting with the provider.
    #[error(transparent)]
    Oidc(error::Error),

    /// An error occurred when discovering the authorization server's issuer.
    #[error("authorization server discovery failed: {0}")]
    Discovery(#[from] OauthDiscoveryError),

    /// The OpenID Connect Provider doesn't support dynamic client registration.
    ///
    /// The provider probably offers another way to register clients.
    #[error("no dynamic registration support")]
    NoRegistrationSupport,

    /// The client has not registered while the operation requires it.
    #[error("client not registered")]
    NotRegistered,

    /// The supplied redirect URIs are missing or empty.
    #[error("missing or empty redirect URIs")]
    MissingRedirectUri,

    /// The device ID was not returned by the homeserver after login.
    #[error("missing device ID in response")]
    MissingDeviceId,

    /// The client is not authenticated while the request requires it.
    #[error("client not authenticated")]
    NotAuthenticated,

    /// The state used to complete authorization doesn't match an original
    /// value.
    #[error("the supplied state is unexpected")]
    InvalidState,

    /// The user cancelled authorization in the web view.
    #[error("authorization cancelled")]
    CancelledAuthorization,

    /// The login was completed with an invalid callback.
    #[error("the supplied callback URL is invalid")]
    InvalidCallbackUrl,

    /// An error occurred during authorization.
    #[error("authorization failed")]
    Authorization(AuthorizationError),

    /// The device ID is invalid.
    #[error("invalid device ID")]
    InvalidDeviceId,

    /// The OpenID Connect Provider doesn't support token revocation, aka
    /// logging out.
    #[error("no token revocation support")]
    NoRevocationSupport,

    /// An error occurred generating a random value.
    #[error(transparent)]
    Rand(rand::Error),

    /// An error occurred parsing a URL.
    #[error(transparent)]
    Url(url::ParseError),

    /// An error occurred caused by the cross-process locks.
    #[error(transparent)]
    LockError(#[from] CrossProcessRefreshLockError),

    /// An unknown error occurred.
    #[error("unknown error")]
    UnknownError(#[source] Box<dyn std::error::Error + Send + Sync>),
}

impl<E> From<E> for OidcError
where
    E: Into<error::Error>,
{
    fn from(value: E) -> Self {
        Self::Oidc(value.into())
    }
}

/// All errors that can occur when discovering the OAuth 2.0 server metadata.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum OauthDiscoveryError {
    /// OAuth 2.0 is not supported by the homeserver.
    #[error("OAuth 2.0 is not supported by the homeserver")]
    NotSupported,

    /// An error occurred when making a request to the homeserver.
    #[error(transparent)]
    Http(#[from] HttpError),

    /// The server metadata is invalid.
    #[error(transparent)]
    Json(#[from] serde_json::Error),

    /// An error occurred when making a request to the OpenID Connect provider.
    #[error(transparent)]
    Oidc(#[from] error::DiscoveryError),
}

impl OauthDiscoveryError {
    /// Whether this error occurred because OAuth 2.0 is not supported by the
    /// homeserver.
    pub fn is_not_supported(&self) -> bool {
        matches!(self, Self::NotSupported)
    }
}

fn rng() -> Result<StdRng, OidcError> {
    StdRng::from_rng(rand::thread_rng()).map_err(OidcError::Rand)
}

fn hash_str(x: &str) -> impl fmt::LowerHex {
    sha2::Sha256::new().chain_update(x).finalize()
}

impl<'c> AsyncHttpClient<'c> for HttpClient {
    type Error = HttpClientError<reqwest::Error>;

    type Future =
        Pin<Box<dyn Future<Output = Result<HttpResponse, Self::Error>> + Send + Sync + 'c>>;

    fn call(&'c self, request: HttpRequest) -> Self::Future {
        Box::pin(async move {
            let response = self.inner.call(request).await?;

            Ok(response)
        })
    }
}
