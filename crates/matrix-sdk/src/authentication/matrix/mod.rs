// Copyright 2020 Damir Jelić
// Copyright 2020 The Matrix.org Foundation C.I.C.
// Copyright 2022 Famedly GmbH
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

//! Types to interact with the native Matrix authentication API.

#[cfg(feature = "sso-login")]
use std::future::Future;
use std::{borrow::Cow, fmt};

use matrix_sdk_base::{SessionMeta, store::RoomLoadSettings};
use ruma::{
    api::{
        OutgoingRequest,
        auth_scheme::SendAccessToken,
        client::{
            account::register,
            session::{
                get_login_types, login, logout, refresh_token, sso_login, sso_login_with_provider,
            },
            uiaa::UserIdentifier,
        },
    },
    serde::JsonObject,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tracing::{debug, error, info, instrument};
use url::Url;

use crate::{
    Client, Error, RefreshTokenError, Result,
    authentication::AuthData,
    client::SessionChange,
    error::{HttpError, HttpResult},
};

mod login_builder;

pub use self::login_builder::LoginBuilder;
#[cfg(feature = "sso-login")]
pub use self::login_builder::SsoLoginBuilder;
use super::SessionTokens;

/// A high-level API to interact with the native Matrix authentication API.
///
/// To access this API, use [`Client::matrix_auth()`].
#[derive(Debug, Clone)]
pub struct MatrixAuth {
    client: Client,
}

/// Errors that can occur when using the SSO API.
#[derive(Debug, Error)]
pub enum SsoError {
    /// The supplied callback URL used to complete SSO is invalid.
    #[error("callback URL invalid")]
    CallbackUrlInvalid,
}

impl MatrixAuth {
    pub(crate) fn new(client: Client) -> Self {
        Self { client }
    }

    /// Gets the homeserver’s supported login types.
    ///
    /// This should be the first step when trying to log in so you can call the
    /// appropriate method for the next step.
    pub async fn get_login_types(&self) -> HttpResult<get_login_types::v3::Response> {
        let request = get_login_types::v3::Request::new();
        self.client.send(request).await
    }

    /// Get the URL to use to log in via Single Sign-On.
    ///
    /// Returns a URL that should be opened in a web browser to let the user
    /// log in.
    ///
    /// After a successful login, the loginToken received at the redirect URL
    /// should be used to log in with [`login_token`].
    ///
    /// # Arguments
    ///
    /// * `redirect_url` - The URL that will receive a `loginToken` after a
    ///   successful SSO login.
    ///
    /// * `idp_id` - The optional ID of the identity provider to log in with.
    ///
    /// [`login_token`]: #method.login_token
    pub async fn get_sso_login_url(
        &self,
        redirect_url: &str,
        idp_id: Option<&str>,
    ) -> Result<String> {
        let homeserver = self.client.homeserver();
        let supported_versions = self.client.supported_versions().await?;

        let request = if let Some(id) = idp_id {
            sso_login_with_provider::v3::Request::new(id.to_owned(), redirect_url.to_owned())
                .try_into_http_request::<Vec<u8>>(
                    homeserver.as_str(),
                    SendAccessToken::None,
                    Cow::Owned(supported_versions),
                )
        } else {
            sso_login::v3::Request::new(redirect_url.to_owned()).try_into_http_request::<Vec<u8>>(
                homeserver.as_str(),
                SendAccessToken::None,
                Cow::Owned(supported_versions),
            )
        };

        match request {
            Ok(req) => Ok(req.uri().to_string()),
            Err(err) => Err(Error::from(HttpError::IntoHttp(err))),
        }
    }

    /// Log into the server with a username and password.
    ///
    /// This can be used for the first login as well as for subsequent logins,
    /// note that if the device ID isn't provided a new device will be created.
    ///
    /// If this isn't the first login, a device ID should be provided through
    /// [`LoginBuilder::device_id`] to restore the correct stores.
    ///
    /// Alternatively the [`restore_session`] method can be used to restore a
    /// logged-in client without the password.
    ///
    /// # Arguments
    ///
    /// * `user` - The user ID or user ID localpart of the user that should be
    ///   logged into the homeserver.
    ///
    /// * `password` - The password of the user.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use url::Url;
    /// # let homeserver = Url::parse("http://example.com").unwrap();
    /// # futures_executor::block_on(async {
    /// use matrix_sdk::Client;
    ///
    /// let client = Client::new(homeserver).await?;
    /// let user = "example";
    ///
    /// let response = client
    ///     .matrix_auth()
    ///     .login_username(user, "wordpass")
    ///     .initial_device_display_name("My bot")
    ///     .await?;
    ///
    /// println!(
    ///     "Logged in as {user}, got device_id {} and access_token {}",
    ///     response.device_id, response.access_token,
    /// );
    /// # anyhow::Ok(()) });
    /// ```
    ///
    /// [`restore_session`]: #method.restore_session
    pub fn login_username(&self, id: impl AsRef<str>, password: &str) -> LoginBuilder {
        self.login_identifier(UserIdentifier::UserIdOrLocalpart(id.as_ref().to_owned()), password)
    }

    /// Log into the server with a user identifier and password.
    ///
    /// This is a more general form of [`login_username`][Self::login_username]
    /// that also accepts third-party identifiers instead of just the user ID or
    /// its localpart.
    pub fn login_identifier(&self, id: UserIdentifier, password: &str) -> LoginBuilder {
        LoginBuilder::new_password(self.clone(), id, password.to_owned())
    }

    /// Log into the server with a custom login type.
    ///
    /// # Arguments
    ///
    /// * `login_type` - Identifier of the custom login type, e.g.
    ///   `org.matrix.login.jwt`
    ///
    /// * `data` - The additional data which should be attached to the login
    ///   request.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use url::Url;
    /// # let homeserver = Url::parse("http://example.com").unwrap();
    /// # async {
    /// use matrix_sdk::Client;
    ///
    /// let client = Client::new(homeserver).await?;
    /// let user = "example";
    ///
    /// let response = client
    ///     .matrix_auth()
    ///     .login_custom(
    ///         "org.matrix.login.jwt",
    ///         [("token".to_owned(), "jwt_token_content".into())]
    ///             .into_iter()
    ///             .collect(),
    ///     )?
    ///     .initial_device_display_name("My bot")
    ///     .await?;
    ///
    /// println!(
    ///     "Logged in as {user}, got device_id {} and access_token {}",
    ///     response.device_id, response.access_token,
    /// );
    /// # anyhow::Ok(()) };
    /// ```
    pub fn login_custom(
        &self,
        login_type: &str,
        data: JsonObject,
    ) -> serde_json::Result<LoginBuilder> {
        LoginBuilder::new_custom(self.clone(), login_type, data)
    }

    /// Log into the server with a token.
    ///
    /// This token is usually received in the SSO flow after following the URL
    /// provided by [`get_sso_login_url`], note that this is not the access
    /// token of a session.
    ///
    /// This should only be used for the first login.
    ///
    /// The [`restore_session`] method should be used to restore a logged-in
    /// client after the first login.
    ///
    /// A device ID should be provided through [`LoginBuilder::device_id`] to
    /// restore the correct stores, if the device ID isn't provided a new
    /// device will be created.
    ///
    /// # Arguments
    ///
    /// * `token` - A login token.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use matrix_sdk::Client;
    /// # use url::Url;
    /// # let homeserver = Url::parse("https://example.com").unwrap();
    /// # let redirect_url = "http://localhost:1234";
    /// # let login_token = "token";
    /// # async {
    /// let client = Client::new(homeserver).await.unwrap();
    /// let auth = client.matrix_auth();
    /// let sso_url = auth.get_sso_login_url(redirect_url, None);
    ///
    /// // Let the user authenticate at the SSO URL.
    /// // Receive the loginToken param at the redirect_url.
    ///
    /// let response = auth
    ///     .login_token(login_token)
    ///     .initial_device_display_name("My app")
    ///     .await
    ///     .unwrap();
    ///
    /// println!(
    ///     "Logged in as {}, got device_id {} and access_token {}",
    ///     response.user_id, response.device_id, response.access_token,
    /// );
    /// # };
    /// ```
    ///
    /// [`get_sso_login_url`]: #method.get_sso_login_url
    /// [`restore_session`]: #method.restore_session
    pub fn login_token(&self, token: &str) -> LoginBuilder {
        LoginBuilder::new_token(self.clone(), token.to_owned())
    }

    /// A higher level wrapper around the methods to complete an SSO login after
    /// the user has logged in through a webview. This method should be used
    /// in tandem with [`MatrixAuth::get_sso_login_url`].
    ///
    /// # Arguments
    ///
    /// * `callback_url` - The received callback URL carrying the login token.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use matrix_sdk::Client;
    /// # use url::Url;
    /// # let homeserver = Url::parse("https://example.com").unwrap();
    /// # let redirect_url = "http://localhost:1234";
    /// # let callback_url = Url::parse("http://localhost:1234?loginToken=token").unwrap();
    /// # async {
    /// let client = Client::new(homeserver).await.unwrap();
    /// let auth = client.matrix_auth();
    /// let sso_url = auth.get_sso_login_url(redirect_url, None);
    ///
    /// // Let the user authenticate at the SSO URL.
    /// // Receive the callback_url.
    ///
    /// let response = auth
    ///     .login_with_sso_callback(callback_url)
    ///     .unwrap()
    ///     .initial_device_display_name("My app")
    ///     .await
    ///     .unwrap();
    ///
    /// println!(
    ///     "Logged in as {}, got device_id {} and access_token {}",
    ///     response.user_id, response.device_id, response.access_token,
    /// );
    /// # };
    /// ```
    pub fn login_with_sso_callback(&self, callback_url: Url) -> Result<LoginBuilder, SsoError> {
        #[derive(Deserialize)]
        struct QueryParameters {
            #[serde(rename = "loginToken")]
            login_token: Option<String>,
        }

        let query_string = callback_url.query().unwrap_or_default();
        let query: QueryParameters =
            serde_html_form::from_str(query_string).map_err(|_| SsoError::CallbackUrlInvalid)?;
        let token = query.login_token.ok_or(SsoError::CallbackUrlInvalid)?;

        Ok(self.login_token(token.as_str()))
    }

    /// Log into the server via Single Sign-On.
    ///
    /// This takes care of the whole SSO flow:
    ///   * Spawn a local http server
    ///   * Provide a callback to open the SSO login URL in a web browser
    ///   * Wait for the local http server to get the loginToken
    ///   * Call [`login_token`]
    ///
    /// If cancellation is needed the method should be wrapped in a cancellable
    /// task. **Note** that users with root access to the system have the
    /// ability to snoop in on the data/token that is passed to the local
    /// HTTP server that will be spawned.
    ///
    /// If you need more control over the SSO login process, you should use
    /// [`get_sso_login_url`] and [`login_token`] directly.
    ///
    /// This should only be used for the first login.
    ///
    /// The [`restore_session`] method should be used to restore a logged-in
    /// client after the first login.
    ///
    /// # Arguments
    ///
    /// * `use_sso_login_url` - A callback that will receive the SSO Login URL.
    ///   It should usually be used to open the SSO URL in a browser and must
    ///   return `Ok(())` if the URL was successfully opened. If it returns
    ///   `Err`, the error will be forwarded.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use matrix_sdk::Client;
    /// # use url::Url;
    /// # let homeserver = Url::parse("https://example.com").unwrap();
    /// # async {
    /// let client = Client::new(homeserver).await.unwrap();
    ///
    /// let response = client
    ///     .matrix_auth()
    ///     .login_sso(|sso_url| async move {
    ///         // Open sso_url
    ///         Ok(())
    ///     })
    ///     .initial_device_display_name("My app")
    ///     .await
    ///     .unwrap();
    ///
    /// println!(
    ///     "Logged in as {}, got device_id {} and access_token {}",
    ///     response.user_id, response.device_id, response.access_token
    /// );
    /// # };
    /// ```
    ///
    /// [`get_sso_login_url`]: #method.get_sso_login_url
    /// [`login_token`]: #method.login_token
    /// [`restore_session`]: #method.restore_session
    #[cfg(feature = "sso-login")]
    pub fn login_sso<F, Fut>(&self, use_sso_login_url: F) -> SsoLoginBuilder<F>
    where
        F: FnOnce(String) -> Fut + Send,
        Fut: Future<Output = Result<()>> + Send,
    {
        SsoLoginBuilder::new(self.clone(), use_sso_login_url)
    }

    /// Is the client logged in using the native Matrix authentication API.
    pub fn logged_in(&self) -> bool {
        self.client
            .auth_ctx()
            .auth_data
            .get()
            .is_some_and(|auth_data| matches!(auth_data, AuthData::Matrix))
    }

    /// Refresh the access token.
    ///
    /// When support for [refreshing access tokens] is activated on both the
    /// homeserver and the client, access tokens have an expiration date and
    /// need to be refreshed periodically. To activate support for refresh
    /// tokens in the [`Client`], it needs to be done at login with the
    /// [`LoginBuilder::request_refresh_token()`] method, or during account
    /// registration.
    ///
    /// This method doesn't need to be called if
    /// [`ClientBuilder::handle_refresh_tokens()`] is called during construction
    /// of the `Client`. Otherwise, it should be called once when a refresh
    /// token is available and an [`UnknownToken`] error is received.
    /// If this call fails with another [`UnknownToken`] error, it means that
    /// the session needs to be logged in again.
    ///
    /// It can also be called at any time when a refresh token is available, it
    /// will invalidate the previous access token.
    ///
    /// The new tokens in the response will be used by the `Client` and should
    /// be persisted to be able to [restore the session]. The response will
    /// always contain an access token that replaces the previous one. It
    /// can also contain a refresh token, in which case it will also replace
    /// the previous one.
    ///
    /// This method is protected behind a lock, so calling this method several
    /// times at once will only call the endpoint once and all subsequent calls
    /// will wait for the result of the first call. The first call will
    /// return `Ok(Some(response))` or the [`HttpError`] returned by the
    /// endpoint, while the others will return `Ok(None)` if the token was
    /// refreshed by the first call or a [`RefreshTokenError`] error, if it
    /// failed.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use matrix_sdk::{Client, Error};
    /// use url::Url;
    /// # async {
    /// # fn get_credentials() -> (&'static str, &'static str) { ("", "") };
    /// # fn persist_session(_: Option<matrix_sdk::AuthSession>) {};
    ///
    /// let homeserver = Url::parse("http://example.com")?;
    /// let client = Client::new(homeserver).await?;
    ///
    /// let (user, password) = get_credentials();
    /// let response = client
    ///     .matrix_auth()
    ///     .login_username(user, password)
    ///     .initial_device_display_name("My App")
    ///     .request_refresh_token()
    ///     .send()
    ///     .await?;
    ///
    /// persist_session(client.session());
    ///
    /// // Handle when an `M_UNKNOWN_TOKEN` error is encountered.
    /// async fn on_unknown_token_err(client: &Client) -> Result<(), Error> {
    ///     let auth = client.matrix_auth();
    ///
    ///     if client
    ///         .session_tokens()
    ///         .and_then(|tokens| tokens.refresh_token)
    ///         .is_some()
    ///         && auth.refresh_access_token().await.is_ok()
    ///     {
    ///         persist_session(client.session());
    ///         return Ok(());
    ///     }
    ///
    ///     let (user, password) = get_credentials();
    ///     auth.login_username(user, password)
    ///         .request_refresh_token()
    ///         .send()
    ///         .await?;
    ///
    ///     persist_session(client.session());
    ///
    ///     Ok(())
    /// }
    /// # anyhow::Ok(()) };
    /// ```
    ///
    /// [refreshing access tokens]: https://spec.matrix.org/v1.3/client-server-api/#refreshing-access-tokens
    /// [`UnknownToken`]: ruma::api::client::error::ErrorKind::UnknownToken
    /// [restore the session]: Client::restore_session
    /// [`ClientBuilder::handle_refresh_tokens()`]: crate::ClientBuilder::handle_refresh_tokens
    pub async fn refresh_access_token(&self) -> Result<(), RefreshTokenError> {
        macro_rules! fail {
            ($lock:expr, $err:expr) => {
                let error = $err;
                *$lock = Err(error.clone());
                return Err(error);
            };
        }

        let refresh_token_lock = &self.client.auth_ctx().refresh_token_lock;
        let Ok(mut guard) = refresh_token_lock.try_lock() else {
            // Somebody else is also doing a token refresh; wait for it to finish first.
            return refresh_token_lock.lock().await.clone();
        };

        let Some(mut session_tokens) = self.client.session_tokens() else {
            fail!(guard, RefreshTokenError::RefreshTokenRequired);
        };
        let Some(refresh_token) = session_tokens.refresh_token.clone() else {
            fail!(guard, RefreshTokenError::RefreshTokenRequired);
        };

        let request = refresh_token::v3::Request::new(refresh_token);
        let res = self.client.send_inner(request, None, Default::default()).await;

        match res {
            Ok(res) => {
                *guard = Ok(());

                session_tokens.access_token = res.access_token;
                if let Some(refresh_token) = res.refresh_token {
                    session_tokens.refresh_token = Some(refresh_token);
                }

                self.client.auth_ctx().set_session_tokens(session_tokens);

                if let Some(save_session_callback) =
                    self.client.inner.auth_ctx.save_session_callback.get()
                    && let Err(err) = save_session_callback(self.client.clone())
                {
                    error!("when saving session after refresh: {err}");
                }

                _ = self
                    .client
                    .inner
                    .auth_ctx
                    .session_change_sender
                    .send(SessionChange::TokensRefreshed);

                Ok(())
            }
            Err(error) => {
                let error = RefreshTokenError::MatrixAuth(error.into());
                fail!(guard, error);
            }
        }
    }

    /// Register a user to the server.
    ///
    /// If registration was successful and a session token was returned by the
    /// server, the client session is set (the client is logged in).
    ///
    /// # Arguments
    ///
    /// * `registration` - The easiest way to create this request is using the
    ///   [`register::v3::Request`] itself.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use matrix_sdk::{
    ///     Client,
    ///     ruma::api::client::{
    ///         account::register::v3::Request as RegistrationRequest, uiaa,
    ///     },
    /// };
    /// # use url::Url;
    /// # let homeserver = Url::parse("http://example.com").unwrap();
    /// # async {
    ///
    /// let mut request = RegistrationRequest::new();
    /// request.username = Some("user".to_owned());
    /// request.password = Some("password".to_owned());
    /// request.auth = Some(uiaa::AuthData::FallbackAcknowledgement(
    ///     uiaa::FallbackAcknowledgement::new("foobar".to_owned()),
    /// ));
    ///
    /// let client = Client::new(homeserver).await.unwrap();
    /// client.matrix_auth().register(request).await;
    /// # };
    /// ```
    #[instrument(skip_all)]
    pub async fn register(&self, request: register::v3::Request) -> Result<register::v3::Response> {
        let homeserver = self.client.homeserver();
        info!("Registering to {homeserver}");

        #[cfg(feature = "e2e-encryption")]
        let login_info = match (&request.username, &request.password) {
            (Some(u), Some(p)) => Some(login::v3::LoginInfo::Password(login::v3::Password::new(
                UserIdentifier::UserIdOrLocalpart(u.into()),
                p.clone(),
            ))),
            _ => None,
        };

        let response = self.client.send(request).await?;
        if let Some(session) = MatrixSession::from_register_response(&response) {
            let _ = self
                .set_session(
                    session,
                    RoomLoadSettings::default(),
                    #[cfg(feature = "e2e-encryption")]
                    login_info,
                )
                .await;
        }
        Ok(response)
    }
    /// Log out the current user.
    pub async fn logout(&self) -> HttpResult<logout::v3::Response> {
        let request = logout::v3::Request::new();
        self.client.send(request).await
    }

    /// Get the whole native Matrix authentication session info of this client.
    ///
    /// Will be `None` if the client has not been logged in with the native
    /// Matrix Authentication API.
    ///
    /// Can be used with [`MatrixAuth::restore_session`] to restore a previously
    /// logged-in session.
    pub fn session(&self) -> Option<MatrixSession> {
        let meta = self.client.session_meta()?;
        let tokens = self.client.session_tokens()?;
        Some(MatrixSession { meta: meta.to_owned(), tokens })
    }

    /// Restore a previously logged in session.
    ///
    /// This can be used to restore the client to a logged in state, loading all
    /// the stored state and encryption keys.
    ///
    /// Alternatively, if the whole session isn't stored the [`login`] method
    /// can be used with a device ID.
    ///
    /// # Arguments
    ///
    /// * `session` - A session that the user already has from a previous login
    ///   call.
    ///
    /// * `room_load_settings` — Specify how many rooms must be restored; use
    ///   `::default()` if you don't know which value to pick.
    ///
    /// # Panics
    ///
    /// Panics if a session was already restored or logged in.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use matrix_sdk::{
    ///     Client, SessionMeta, SessionTokens,
    ///     authentication::matrix::MatrixSession,
    ///     ruma::{device_id, user_id},
    /// };
    /// # use url::Url;
    /// # async {
    ///
    /// let homeserver = Url::parse("http://example.com")?;
    /// let client = Client::new(homeserver).await?;
    ///
    /// let session = MatrixSession {
    ///     meta: SessionMeta {
    ///         user_id: user_id!("@example:localhost").to_owned(),
    ///         device_id: device_id!("MYDEVICEID").to_owned(),
    ///     },
    ///     tokens: SessionTokens {
    ///         access_token: "My-Token".to_owned(),
    ///         refresh_token: None,
    ///     },
    /// };
    ///
    /// client.restore_session(session).await?;
    /// # anyhow::Ok(()) };
    /// ```
    ///
    /// The `MatrixSession` object can also be created from the response the
    /// [`LoginBuilder::send()`] method returns:
    ///
    /// ```no_run
    /// use matrix_sdk::{Client, store::RoomLoadSettings};
    /// use url::Url;
    /// # async {
    ///
    /// let homeserver = Url::parse("http://example.com")?;
    /// let client = Client::new(homeserver).await?;
    /// let auth = client.matrix_auth();
    ///
    /// let response = auth.login_username("example", "my-password").send().await?;
    ///
    /// // Persist the `MatrixSession` so it can later be used to restore the login.
    ///
    /// auth.restore_session((&response).into(), RoomLoadSettings::default())
    ///     .await?;
    /// # anyhow::Ok(()) };
    /// ```
    ///
    /// [`login`]: #method.login
    /// [`LoginBuilder::send()`]: crate::authentication::matrix::LoginBuilder::send
    #[instrument(skip_all)]
    pub async fn restore_session(
        &self,
        session: MatrixSession,
        room_load_settings: RoomLoadSettings,
    ) -> Result<()> {
        debug!("Restoring Matrix auth session");
        self.set_session(
            session,
            room_load_settings,
            #[cfg(feature = "e2e-encryption")]
            None,
        )
        .await?;
        debug!("Done restoring Matrix auth session");
        Ok(())
    }

    /// Receive a login response and update the homeserver and the base client
    /// if needed.
    ///
    /// # Arguments
    ///
    /// * `response` - A successful login response.
    pub(crate) async fn receive_login_response(
        &self,
        response: &login::v3::Response,
        #[cfg(feature = "e2e-encryption")] login_info: Option<login::v3::LoginInfo>,
    ) -> Result<()> {
        self.client.maybe_update_login_well_known(response.well_known.as_ref());

        self.set_session(
            response.into(),
            RoomLoadSettings::default(),
            #[cfg(feature = "e2e-encryption")]
            login_info,
        )
        .await?;

        Ok(())
    }

    /// Set the Matrix authentication session.
    ///
    /// # Arguments
    ///
    /// * `session` — The session being opened.
    ///
    /// * `room_load_settings` — Specify how much rooms must be restored; use
    ///   `::default()` if you don't know which value to pick.
    ///
    /// # Panic
    ///
    /// Panics if authentication data was already set.
    async fn set_session(
        &self,
        session: MatrixSession,
        room_load_settings: RoomLoadSettings,
        #[cfg(feature = "e2e-encryption")] login_info: Option<login::v3::LoginInfo>,
    ) -> Result<()> {
        // This API doesn't have any data but by setting this variant we protect the
        // user from using both authentication APIs at once.
        self.client
            .auth_ctx()
            .auth_data
            .set(AuthData::Matrix)
            .expect("Client authentication data was already set");
        self.client.auth_ctx().set_session_tokens(session.tokens);
        self.client
            .base_client()
            .activate(
                session.meta,
                room_load_settings,
                #[cfg(feature = "e2e-encryption")]
                None,
            )
            .await?;

        #[cfg(feature = "e2e-encryption")]
        {
            use ruma::api::client::uiaa::{AuthData, Password};

            let auth_data = match login_info {
                Some(login::v3::LoginInfo::Password(login::v3::Password {
                    identifier: Some(identifier),
                    password,
                    ..
                })) => Some(AuthData::Password(Password::new(identifier, password))),
                // Other methods can't be immediately translated to an auth.
                _ => None,
            };

            self.client.encryption().spawn_initialization_task(auth_data).await;
        }

        Ok(())
    }
}

/// A user session using the native Matrix authentication API.
///
/// # Examples
///
/// ```
/// use matrix_sdk::{
///     SessionMeta, SessionTokens, authentication::matrix::MatrixSession,
/// };
/// use ruma::{device_id, user_id};
///
/// let session = MatrixSession {
///     meta: SessionMeta {
///         user_id: user_id!("@example:localhost").to_owned(),
///         device_id: device_id!("MYDEVICEID").to_owned(),
///     },
///     tokens: SessionTokens {
///         access_token: "My-Token".to_owned(),
///         refresh_token: None,
///     },
/// };
///
/// assert_eq!(session.meta.device_id.as_str(), "MYDEVICEID");
/// ```
#[derive(Clone, Eq, Hash, PartialEq, Serialize, Deserialize)]
pub struct MatrixSession {
    /// The Matrix user session info.
    #[serde(flatten)]
    pub meta: SessionMeta,

    /// The tokens used for authentication.
    #[serde(flatten)]
    pub tokens: SessionTokens,
}

#[cfg(not(tarpaulin_include))]
impl fmt::Debug for MatrixSession {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MatrixSession").field("meta", &self.meta).finish_non_exhaustive()
    }
}

impl From<&login::v3::Response> for MatrixSession {
    fn from(response: &login::v3::Response) -> Self {
        let login::v3::Response { user_id, access_token, device_id, refresh_token, .. } = response;
        Self {
            meta: SessionMeta { user_id: user_id.clone(), device_id: device_id.clone() },
            tokens: SessionTokens {
                access_token: access_token.clone(),
                refresh_token: refresh_token.clone(),
            },
        }
    }
}

impl MatrixSession {
    #[allow(clippy::question_mark)] // clippy falsely complains about the let-unpacking
    fn from_register_response(response: &register::v3::Response) -> Option<Self> {
        let register::v3::Response { user_id, access_token, device_id, refresh_token, .. } =
            response;
        Some(Self {
            meta: SessionMeta { user_id: user_id.clone(), device_id: device_id.clone()? },
            tokens: SessionTokens {
                access_token: access_token.clone()?,
                refresh_token: refresh_token.clone(),
            },
        })
    }
}
