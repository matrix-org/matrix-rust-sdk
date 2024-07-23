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
//! logging in via OIDC when [`Oidc::fetch_authentication_issuer()`] succeeds.
//!
//! If the homeserver doesn't advertise its support for OIDC, but the issuer URL
//! is known by some other method, it can be provided manually during
//! registration.
//!
//! # Registration
//!
//! Registration is only required the first time a client encounters an issuer.
//!
//! If the issuer supports dynamic registration, it can be done by using
//! [`Oidc::register_client()`]. If dynamic registration is not available, the
//! homeserver should document how to obtain client credentials.
//!
//! To make the client aware of being registered successfully,
//! [`Oidc::restore_registered_client()`] needs to be called next.
//!
//! After client registration, the client credentials should be persisted and
//! reused for every session that interacts with that same issuer.
//!
//! # Login
//!
//! Before logging in, make sure to call [`Oidc::restore_registered_client()`]
//! (even after registering the client), as it is the first step to know how to
//! interact with the issuer.
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
//! - The client credentials obtained after client registration with the
//!   corresponding client metadata,
//! - The user session obtained after login.
//!
//! Both parts are usually stored separately because the client credentials can
//! be reused for any session with the same issuer, while the user session is
//! unique.
//!
//! _Note_ that the type returned by [`Oidc::full_session()`] is not
//! (de)serializable. This is due to some client credentials methods that
//! require a function to generate a JWT. The types of some fields can still be
//! (de)serialized.
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
//! # Insufficient scope error
//!
//! _Note: This is not fully specced yet._
//!
//! Some API calls that deal with sensitive data require more privileges than
//! others. In the current Matrix specification, those endpoints use the
//! User-Interactive Authentication API.
//!
//! The OAuth 2.0 specification has the concept of scopes, and an access token
//! is limited to a given scope when it is generated. Accessing those endpoints
//! require a privileged scope, so a new authorization request is necessary to
//! get a new access token.
//!
//! When the API endpoint returns an [`Error`] with an
//! [`AuthenticateError::InsufficientScope`], the [`Oidc::authorize_scope()`]
//! method can be used to authorize the required scope. It works just like
//! [`Oidc::login()`].
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

use std::{collections::HashMap, fmt, sync::Arc};

use as_variant::as_variant;
use eyeball::SharedObservable;
use futures_core::Stream;
use http::StatusCode;
pub use mas_oidc_client::{error, requests, types};
use mas_oidc_client::{
    requests::{
        account_management::{build_account_management_url, AccountManagementActionFull},
        authorization_code::AuthorizationValidationData,
    },
    types::{
        client_credentials::ClientCredentials,
        errors::{ClientError, ClientErrorCode::AccessDenied},
        iana::oauth::OAuthTokenTypeHint,
        oidc::{AccountManagementAction, VerifiedProviderMetadata},
        registration::{ClientRegistrationResponse, VerifiedClientMetadata},
        requests::Prompt,
        scope::{MatrixApiScopeToken, Scope, ScopeToken},
        IdToken,
    },
};
use matrix_sdk_base::{
    crypto::types::qr_login::QrCodeData, once_cell::sync::OnceCell, SessionMeta,
};
use rand::{rngs::StdRng, Rng, SeedableRng};
use ruma::api::client::discovery::get_authentication_issuer;
use serde::{Deserialize, Serialize};
use sha2::Digest as _;
use thiserror::Error;
use tokio::{spawn, sync::Mutex};
use tracing::{error, trace, warn};
use url::Url;

mod auth_code_builder;
mod backend;
mod cross_process;
mod data_serde;
mod end_session_builder;
pub mod registrations;
#[cfg(test)]
mod tests;

pub use self::{
    auth_code_builder::{OidcAuthCodeUrlBuilder, OidcAuthorizationData},
    cross_process::CrossProcessRefreshLockError,
    end_session_builder::{OidcEndSessionData, OidcEndSessionUrlBuilder},
};
use self::{
    backend::{server::OidcServer, OidcBackend},
    cross_process::{CrossProcessRefreshLockGuard, CrossProcessRefreshManager},
};
use crate::{
    authentication::{qrcode::LoginWithQrCode, AuthData},
    client::SessionChange,
    oidc::registrations::{ClientId, OidcRegistrations},
    Client, HttpError, RefreshTokenError, Result,
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
    pub(crate) credentials: ClientCredentials,
    pub(crate) metadata: VerifiedClientMetadata,
    pub(crate) tokens: OnceCell<SharedObservable<OidcSessionTokens>>,
    /// The data necessary to validate authorization responses.
    pub(crate) authorization_data: Mutex<HashMap<String, AuthorizationValidationData>>,
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

    /// The implementation of the OIDC backend.
    backend: Arc<dyn OidcBackend>,
}

impl Oidc {
    pub(crate) fn new(client: Client) -> Self {
        Self { client: client.clone(), backend: Arc::new(OidcServer::new(client)) }
    }

    fn ctx(&self) -> &OidcCtx {
        &self.client.inner.auth_ctx.oidc
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
    /// Returns `None` if the client registration was not restored with
    /// [`Oidc::restore_registered_client()`] or
    /// [`Oidc::restore_session()`].
    fn data(&self) -> Option<&OidcAuthData> {
        let data = self.client.inner.auth_ctx.auth_data.get()?;
        as_variant!(data, AuthData::Oidc)
    }

    /// Get the authentication issuer advertised by the homeserver.
    ///
    /// Returns an error if the request fails. An error with a
    /// `StatusCode::NOT_FOUND` should mean that the homeserver does not support
    /// authenticating via OpenID Connect ([MSC3861]).
    ///
    /// [MSC3861]: https://github.com/matrix-org/matrix-spec-proposals/pull/3861
    pub async fn fetch_authentication_issuer(&self) -> Result<String, HttpError> {
        let response =
            self.client.send(get_authentication_issuer::msc2965::Request::new(), None).await?;

        Ok(response.issuer)
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
    ///     authentication::qrcode::{LoginProgress, QrCodeData, QrCodeModeData},
    ///     Client,
    ///     oidc::types::registration::VerifiedClientMetadata,
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
    pub async fn url_for_oidc_login(
        &self,
        client_metadata: VerifiedClientMetadata,
        registrations: OidcRegistrations,
    ) -> Result<OidcAuthorizationData, OidcError> {
        let issuer = match self.fetch_authentication_issuer().await {
            Ok(issuer) => issuer,
            Err(error) => {
                if error
                    .as_client_api_error()
                    .is_some_and(|err| err.status_code == StatusCode::NOT_FOUND)
                {
                    return Err(OidcError::MissingAuthenticationIssuer);
                } else {
                    return Err(OidcError::UnknownError(Box::new(error)));
                }
            }
        };

        let redirect_uris =
            client_metadata.redirect_uris.clone().ok_or(OidcError::MissingRedirectUri)?;

        let redirect_url = redirect_uris.first().ok_or(OidcError::MissingRedirectUri)?;

        self.configure(issuer, client_metadata, registrations).await?;

        let mut data_builder = self.login(redirect_url.clone(), None)?;
        data_builder = data_builder.prompt(vec![Prompt::Consent]);
        let data = data_builder.build().await?;

        Ok(data)
    }

    /// A higher level wrapper around the methods to complete a login after the
    /// user has logged in through a webview. This method should be used in
    /// tandem with [`Oidc::url_for_oidc_login`].
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
        if self.client_credentials().is_some() {
            tracing::info!("OIDC is already configured.");
            return Ok(());
        };

        if self.load_client_registration(issuer.clone(), client_metadata.clone(), &registrations) {
            tracing::info!("OIDC configuration loaded from disk.");
            return Ok(());
        }

        tracing::info!("Registering this client for OIDC.");
        let registration_response =
            self.register_client(&issuer, client_metadata.clone(), None).await?;

        // The format of the credentials changes according to the client metadata that
        // was sent. Public clients only get a client ID.
        let credentials = ClientCredentials::None { client_id: registration_response.client_id };
        self.restore_registered_client(issuer, client_metadata, credentials);

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
        let issuer = Url::parse(self.issuer().ok_or(OidcError::MissingAuthenticationIssuer)?)
            .map_err(OidcError::Url)?;
        let client_id =
            self.client_credentials().ok_or(OidcError::NotRegistered)?.client_id().to_owned();

        registrations
            .set_and_write_client_id(ClientId(client_id), issuer)
            .map_err(|e| OidcError::UnknownError(Box::new(e)))?;

        Ok(())
    }

    /// Attempts to load an existing OIDC dynamic client registration for a
    /// given issuer.
    ///
    /// Returns `true` if an existing registration was found and `false` if not.
    fn load_client_registration(
        &self,
        issuer: String,
        oidc_metadata: VerifiedClientMetadata,
        registrations: &OidcRegistrations,
    ) -> bool {
        let Ok(issuer_url) = Url::parse(&issuer) else {
            error!("Failed to parse {issuer:?}");
            return false;
        };
        let Some(client_id) = registrations.client_id(&issuer_url) else {
            return false;
        };

        self.restore_registered_client(
            issuer,
            oidc_metadata,
            ClientCredentials::None { client_id: client_id.0 },
        );

        true
    }

    /// The OpenID Connect Provider used for authorization.
    ///
    /// Returns `None` if the client registration was not restored with
    /// [`Oidc::restore_registered_client()`] or
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
    pub async fn account_management_url(
        &self,
        action: Option<AccountManagementActionFull>,
    ) -> Result<Option<Url>, OidcError> {
        let provider_metadata = self.provider_metadata().await?;

        let Some(base_url) = provider_metadata.account_management_uri.clone() else {
            return Ok(None);
        };

        let id_token_hint =
            self.session_tokens().and_then(|t| t.latest_id_token).map(|t| t.to_string());

        let url = build_account_management_url(base_url, action, id_token_hint)?;

        Ok(Some(url))
    }

    /// Fetch the OpenID Connect metadata of the given issuer.
    ///
    /// Returns an error if fetching the metadata failed.
    pub async fn given_provider_metadata(
        &self,
        issuer: &str,
    ) -> Result<VerifiedProviderMetadata, OidcError> {
        self.backend.discover(issuer, self.ctx().insecure_discover).await
    }

    /// Fetch the OpenID Connect metadata of the issuer.
    ///
    /// Returns an error if the client registration was not restored, or if an
    /// error occurred when fetching the metadata.
    pub async fn provider_metadata(&self) -> Result<VerifiedProviderMetadata, OidcError> {
        let issuer = self.issuer().ok_or(OidcError::MissingAuthenticationIssuer)?;

        self.given_provider_metadata(issuer).await
    }

    /// The OpenID Connect metadata of this client used during registration.
    ///
    /// Returns `None` if the client registration was not restored with
    /// [`Oidc::restore_registered_client()`] or
    /// [`Oidc::restore_session()`].
    pub fn client_metadata(&self) -> Option<&VerifiedClientMetadata> {
        self.data().map(|data| &data.metadata)
    }

    /// The OpenID Connect credentials of this client obtained after
    /// registration.
    ///
    /// Returns `None` if the client registration was not restored with
    /// [`Oidc::restore_registered_client()`] or
    /// [`Oidc::restore_session()`].
    pub fn client_credentials(&self) -> Option<&ClientCredentials> {
        self.data().map(|data| &data.credentials)
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
    /// # fn persist_session(_: &matrix_sdk::oidc::OidcSession) {}
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

    /// The ID Token received after the latest authorization of this client, if
    /// any.
    ///
    /// Returns `None` if the client was not logged in with the OpenID Connect
    /// API, or if the issuer did not provide one.
    pub fn latest_id_token(&self) -> Option<IdToken<'static>> {
        self.session_tokens()?.latest_id_token
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
        Some(OidcSession {
            credentials: data.credentials.clone(),
            metadata: data.metadata.clone(),
            user,
        })
    }

    /// Register a client with an OpenID Connect Provider.
    ///
    /// This should be called before any authorization request with an unknown
    /// authentication issuer. If the client is already registered with the
    /// given issuer, it should use [`Oidc::restore_registered_client()`]
    /// directly.
    ///
    /// Note that the client should adapt the security measures enabled in its
    /// metadata according to the capabilities advertised in
    /// [`Oidc::given_provider_metadata()`].
    ///
    /// # Arguments
    ///
    /// * `issuer` - The OpenID Connect Provider to register with. Can be
    ///   obtained with [`Oidc::fetch_authentication_issuer()`].
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
    /// The credentials in the response should be persisted for future use and
    /// reused for the same issuer, along with the client metadata sent to the
    /// provider, even for different sessions or user accounts.
    ///
    /// # Example
    ///
    /// ```no_run
    /// use matrix_sdk::{Client, ServerName};
    /// use matrix_sdk::oidc::types::client_credentials::ClientCredentials;
    /// use matrix_sdk::oidc::types::registration::ClientMetadata;
    /// # use matrix_sdk::oidc::types::registration::VerifiedClientMetadata;
    /// # let client_metadata = ClientMetadata::default().validate().unwrap();
    /// # fn persist_client_registration (_: &str, _: &ClientMetadata, _: &ClientCredentials) {}
    /// # _ = async {
    /// let server_name = ServerName::parse("my_homeserver.org").unwrap();
    /// let client = Client::builder().server_name(&server_name).build().await?;
    /// let oidc = client.oidc();
    ///
    /// if let Ok(issuer) = oidc.fetch_authentication_issuer().await {
    ///     let response = oidc
    ///         .register_client(&issuer, client_metadata.clone(), None)
    ///         .await?;
    ///
    ///     println!(
    ///         "Registered with client_id: {}",
    ///         response.client_id
    ///     );
    ///
    ///     // In this case we registered a public client, so it has no secret.
    ///     let credentials = ClientCredentials::None {
    ///         client_id: response.client_id,
    ///     };
    ///
    ///     persist_client_registration(&issuer, &client_metadata, &credentials);
    /// }
    /// # anyhow::Ok(()) };
    /// ```
    ///
    /// [software statement]: https://datatracker.ietf.org/doc/html/rfc7591#autoid-8
    pub async fn register_client(
        &self,
        issuer: &str,
        client_metadata: VerifiedClientMetadata,
        software_statement: Option<String>,
    ) -> Result<ClientRegistrationResponse, OidcError> {
        let provider_metadata = self.given_provider_metadata(issuer).await?;

        let registration_endpoint = provider_metadata
            .registration_endpoint
            .as_ref()
            .ok_or(OidcError::NoRegistrationSupport)?;

        self.backend
            .register_client(registration_endpoint, client_metadata, software_statement)
            .await
    }

    /// Set the data of a client that is registered with an OpenID Connect
    /// Provider.
    ///
    /// This should be called after registration or when logging in with a
    /// provider that is already known by the client.
    ///
    /// # Arguments
    ///
    /// * `issuer` - The OpenID Connect Provider we're interacting with.
    ///
    /// * `client_metadata` - The [`VerifiedClientMetadata`] that was
    ///   registered.
    ///
    /// * `client_credentials` - The credentials necessary to authenticate the
    ///   client with the provider, obtained after registration.
    ///
    /// # Panic
    ///
    /// Panics if authentication data was already set.
    pub fn restore_registered_client(
        &self,
        issuer: String,
        client_metadata: VerifiedClientMetadata,
        client_credentials: ClientCredentials,
    ) {
        let data = OidcAuthData {
            issuer,
            credentials: client_credentials,
            metadata: client_metadata,
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
        let OidcSession { credentials, metadata, user: UserSession { meta, tokens, issuer } } =
            session;

        let data = OidcAuthData {
            issuer,
            credentials,
            metadata,
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

    /// Login via OpenID Connect with the Authorization Code flow.
    ///
    /// This should be called after the registered client has been restored with
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
    /// # use matrix_sdk::oidc::AuthorizationResponse;
    /// # use url::Url;
    /// # let homeserver = Url::parse("https://example.com").unwrap();
    /// # let redirect_uri = Url::parse("http://127.0.0.1/oidc").unwrap();
    /// # let redirected_to_uri = Url::parse("http://127.0.0.1/oidc").unwrap();
    /// # let issuer_info = unimplemented!();
    /// # let client_metadata = unimplemented!();
    /// # let client_credentials = unimplemented!();
    /// # _ = async {
    /// # let client = Client::new(homeserver).await?;
    /// let oidc = client.oidc();
    ///
    /// oidc.restore_registered_client(
    ///     issuer_info,
    ///     client_metadata,
    ///     client_credentials,
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
        // Generate the device ID if it is not provided.
        let device_id = if let Some(device_id) = device_id {
            device_id
        } else {
            rand::thread_rng()
                .sample_iter(&rand::distributions::Alphanumeric)
                .map(char::from)
                .take(10)
                .collect::<String>()
        };

        let scope = [
            ScopeToken::Openid,
            ScopeToken::MatrixApi(MatrixApiScopeToken::Full),
            ScopeToken::try_with_matrix_device(device_id).or(Err(OidcError::InvalidDeviceId))?,
        ]
        .into_iter()
        .collect();

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
            .await
            .map_err(crate::Error::from)?;
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

    /// Authorize a given scope with the Authorization Code flow.
    ///
    /// This should be used if a new scope is necessary to make a request. For
    /// example, if the homeserver returns an [`Error`] with an
    /// [`AuthenticateError::InsufficientScope`].
    ///
    /// This should be called after the registered client has been restored or
    /// the client was logged in.
    ///
    /// # Arguments
    ///
    /// * `scope` - The scope to authorize.
    ///
    /// * `redirect_uri` - The URI where the end user will be redirected after
    ///   authorizing the scope. It must be one of the redirect URIs sent in the
    ///   client metadata during registration.
    ///
    /// [`Error`]: ruma::api::client::error::Error
    /// [`AuthenticateError::InsufficientScope`]: ruma::api::client::error::AuthenticateError
    pub fn authorize_scope(&self, scope: Scope, redirect_uri: Url) -> OidcAuthCodeUrlBuilder {
        OidcAuthCodeUrlBuilder::new(self.clone(), scope, redirect_uri)
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

        let session_tokens = self
            .backend
            .trade_authorization_code_for_tokens(
                provider_metadata,
                data.credentials.clone(),
                data.metadata.clone(),
                auth_code,
                validation_data,
            )
            .await?;

        self.set_session_tokens(session_tokens);

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

    async fn refresh_access_token_inner(
        &self,
        refresh_token: String,
        latest_id_token: Option<IdToken<'static>>,
        lock: Option<CrossProcessRefreshLockGuard>,
    ) -> Result<(), OidcError> {
        // Do not interrupt refresh access token requests and processing, by detaching
        // the request sending and response processing.

        let provider_metadata = self.provider_metadata().await?;

        let this = self.clone();
        let data = self.data().ok_or(OidcError::NotAuthenticated)?;
        let credentials = data.credentials.clone();
        let metadata = data.metadata.clone();

        spawn(async move {
            trace!(
                "Token refresh: attempting to refresh with refresh_token {:x}",
                hash_str(&refresh_token)
            );

            match this
                .backend
                .refresh_access_token(
                    provider_metadata,
                    credentials,
                    &metadata,
                    refresh_token.clone(),
                    latest_id_token.clone(),
                )
                .await
                .map_err(OidcError::from)
            {
                Ok(new_tokens) => {
                    trace!(
                        "Token refresh: new refresh_token: {} / access_token: {:x}",
                        new_tokens
                            .refresh_token
                            .as_deref()
                            .map(|token| format!("{:x}", hash_str(token)))
                            .unwrap_or_else(|| "<none>".to_owned()),
                        hash_str(&new_tokens.access_token)
                    );

                    let tokens = OidcSessionTokens {
                        access_token: new_tokens.access_token,
                        refresh_token: new_tokens.refresh_token.clone().or(Some(refresh_token)),
                        latest_id_token,
                    };

                    this.set_session_tokens(tokens.clone());

                    // Call the save_session_callback if set, while the optional lock is being held.
                    if let Some(save_session_callback) =
                        this.client.inner.auth_ctx.save_session_callback.get()
                    {
                        // Satisfies the save_session_callback invariant: set_session_tokens has
                        // been called just above.
                        if let Err(err) = save_session_callback(this.client.clone()) {
                            error!("when saving session after refresh: {err}");
                        }
                    }

                    if let Some(mut lock) = lock {
                        lock.save_in_memory_and_db(&tokens).await?;
                    }

                    _ = this
                        .client
                        .inner
                        .auth_ctx
                        .session_change_sender
                        .send(SessionChange::TokensRefreshed);

                    Ok(())
                }

                Err(err) => Err(err),
            }
        })
        .await
        .expect("joining")?;

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
    pub async fn refresh_access_token(&self) -> Result<(), RefreshTokenError> {
        let client = &self.client;

        let refresh_status_lock = client.inner.auth_ctx.refresh_token_lock.try_lock();

        macro_rules! fail {
            ($lock:expr, $err:expr) => {
                let error = $err;
                *$lock = Err(error.clone());
                return Err(error);
            };
        }

        let Ok(mut refresh_status_guard) = refresh_status_lock else {
            // There's already a request to refresh happening in the same process. Wait for
            // it to finish.
            return client.inner.auth_ctx.refresh_token_lock.lock().await.clone();
        };

        let cross_process_guard =
            if let Some(manager) = self.ctx().cross_process_token_refresh_manager.get() {
                let mut cross_process_guard = match manager
                    .spin_lock()
                    .await
                    .map_err(|err| RefreshTokenError::Oidc(Arc::new(err.into())))
                {
                    Ok(guard) => guard,
                    Err(err) => {
                        fail!(refresh_status_guard, err);
                    }
                };

                if cross_process_guard.hash_mismatch {
                    Box::pin(self.handle_session_hash_mismatch(&mut cross_process_guard))
                        .await
                        .map_err(|err| RefreshTokenError::Oidc(Arc::new(err.into())))?;
                    // Optimistic exit: assume that the underlying process did update fast enough.
                    // In the worst case, we'll do another refresh Soon™.
                    *refresh_status_guard = Ok(());
                    return Ok(());
                }

                Some(cross_process_guard)
            } else {
                None
            };

        let Some(session_tokens) = self.session_tokens() else {
            fail!(refresh_status_guard, RefreshTokenError::RefreshTokenRequired);
        };
        let Some(refresh_token) = session_tokens.refresh_token else {
            fail!(refresh_status_guard, RefreshTokenError::RefreshTokenRequired);
        };

        match self
            .refresh_access_token_inner(
                refresh_token,
                session_tokens.latest_id_token,
                cross_process_guard,
            )
            .await
        {
            Ok(()) => {
                *refresh_status_guard = Ok(());
                Ok(())
            }
            Err(error) => {
                fail!(refresh_status_guard, RefreshTokenError::Oidc(error.into()));
            }
        }
    }

    /// Log out from the currently authenticated session.
    ///
    /// On success, if the provider supports [RP-Initiated Logout], an
    /// [`OidcEndSessionUrlBuilder`] will be provided to build the URL allowing
    /// the user to log out from their account in the provider's interface.
    ///
    /// [RP-Initiated Logout]: https://openid.net/specs/openid-connect-rpinitiated-1_0.html
    pub async fn logout(&self) -> Result<Option<OidcEndSessionUrlBuilder>, OidcError> {
        let provider_metadata = self.provider_metadata().await?;
        let client_credentials = self.client_credentials().ok_or(OidcError::NotAuthenticated)?;

        let revocation_endpoint =
            provider_metadata.revocation_endpoint.as_ref().ok_or(OidcError::NoRevocationSupport)?;

        let tokens = self.session_tokens().ok_or(OidcError::NotAuthenticated)?;

        // Revoke the access token.
        self.backend
            .revoke_token(
                client_credentials.clone(),
                revocation_endpoint,
                tokens.access_token,
                Some(OAuthTokenTypeHint::AccessToken),
            )
            .await?;

        // Revoke the refresh token, if any.
        if let Some(refresh_token) = tokens.refresh_token {
            self.backend
                .revoke_token(
                    client_credentials.clone(),
                    revocation_endpoint,
                    refresh_token,
                    Some(OAuthTokenTypeHint::RefreshToken),
                )
                .await?;
        }

        let end_session_builder =
            provider_metadata.end_session_endpoint.clone().map(|end_session_endpoint| {
                OidcEndSessionUrlBuilder::new(
                    self.clone(),
                    end_session_endpoint,
                    client_credentials.client_id().to_owned(),
                )
            });

        if let Some(manager) = self.ctx().cross_process_token_refresh_manager.get() {
            manager.on_logout().await?;
        }

        Ok(end_session_builder)
    }
}

/// A full session for the OpenID Connect API.
#[derive(Debug, Clone)]
pub struct OidcSession {
    /// The credentials obtained after registration.
    pub credentials: ClientCredentials,

    /// The client metadata sent for registration.
    pub metadata: VerifiedClientMetadata,

    /// The user session.
    pub user: UserSession,
}

/// A user session for the OpenID Connect API.
#[derive(Debug, Clone, Serialize)]
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
#[derive(Clone, Eq, PartialEq)]
#[allow(missing_debug_implementations)]
pub struct OidcSessionTokens {
    /// The access token used for this session.
    pub access_token: String,

    /// The token used for refreshing the access token, if any.
    pub refresh_token: Option<String>,

    /// The ID token returned by the provider during the latest authorization.
    pub latest_id_token: Option<IdToken<'static>>,
}

#[cfg(not(tarpaulin_include))]
impl fmt::Debug for OidcSessionTokens {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SessionTokens").finish_non_exhaustive()
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

    /// No authentication issuer was provided by the homeserver or by the user.
    #[error("client missing authentication issuer")]
    MissingAuthenticationIssuer,

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

fn rng() -> Result<StdRng, OidcError> {
    StdRng::from_rng(rand::thread_rng()).map_err(OidcError::Rand)
}

fn hash_str(x: &str) -> impl fmt::LowerHex {
    sha2::Sha256::new().chain_update(x).finalize()
}
