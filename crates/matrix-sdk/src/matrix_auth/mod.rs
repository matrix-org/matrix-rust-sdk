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

use ruma::{
    api::{
        client::{
            account::register,
            session::{get_login_types, logout, refresh_token, sso_login, sso_login_with_provider},
            uiaa::UserIdentifier,
        },
        OutgoingRequest, SendAccessToken,
    },
    serde::JsonObject,
};
use tracing::{info, instrument};

use crate::{
    config::RequestConfig,
    error::{HttpError, HttpResult},
    Client, Error, RefreshTokenError, Result, RumaApiError,
};

mod login_builder;

pub use self::login_builder::LoginBuilder;
#[cfg(feature = "sso-login")]
pub use self::login_builder::SsoLoginBuilder;

/// A high-level API to interact with the native Matrix authentication API.
///
/// To access this API, use [`Client::matrix_auth()`].
#[derive(Debug, Clone)]
pub struct MatrixAuth {
    client: Client,
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
        self.client.send(request, None).await
    }

    /// Get the URL to use to log in via Single Sign-On.
    ///
    /// Returns a URL that should be opened in a web browser to let the user
    /// log in.
    ///
    /// After a successful login, the loginToken received at the redirect URL
    /// should be used to log in with [`login_with_token`].
    ///
    /// # Arguments
    ///
    /// * `redirect_url` - The URL that will receive a `loginToken` after a
    ///   successful SSO login.
    ///
    /// * `idp_id` - The optional ID of the identity provider to log in with.
    ///
    /// [`login_with_token`]: #method.login_with_token
    pub async fn get_sso_login_url(
        &self,
        redirect_url: &str,
        idp_id: Option<&str>,
    ) -> Result<String> {
        let homeserver = self.client.homeserver().await;
        let server_versions = self.client.server_versions().await?;

        let request = if let Some(id) = idp_id {
            sso_login_with_provider::v3::Request::new(id.to_owned(), redirect_url.to_owned())
                .try_into_http_request::<Vec<u8>>(
                    homeserver.as_str(),
                    SendAccessToken::None,
                    server_versions,
                )
        } else {
            sso_login::v3::Request::new(redirect_url.to_owned()).try_into_http_request::<Vec<u8>>(
                homeserver.as_str(),
                SendAccessToken::None,
                server_versions,
            )
        };

        match request {
            Ok(req) => Ok(req.uri().to_string()),
            Err(err) => Err(Error::from(HttpError::from(err))),
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
    /// # use matrix_sdk::Session;
    /// use url::Url;
    /// # async {
    /// # fn get_credentials() -> (&'static str, &'static str) { ("", "") };
    /// # fn persist_session(_: Option<Session>) {};
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
    ///     if client.refresh_token().is_some()
    ///         && client.refresh_access_token().await.is_ok()
    ///     {
    ///         persist_session(client.session());
    ///         return Ok(());
    ///     }
    ///
    ///     let (user, password) = get_credentials();
    ///     client
    ///         .matrix_auth()
    ///         .login_username(user, password)
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
    pub async fn refresh_access_token(&self) -> HttpResult<Option<refresh_token::v3::Response>> {
        let client = &self.client;
        let lock = client.inner.refresh_token_lock.try_lock();

        if let Ok(mut guard) = lock {
            let Some(mut session_tokens) = client.session_tokens() else {
                *guard = Err(RefreshTokenError::RefreshTokenRequired);
                return Err(RefreshTokenError::RefreshTokenRequired.into());
            };

            let refresh_token = session_tokens
                .refresh_token
                .clone()
                .ok_or(RefreshTokenError::RefreshTokenRequired)?;
            let request = refresh_token::v3::Request::new(refresh_token);

            let res = client.send_inner(request, None, None, Default::default()).await;

            match res {
                Ok(res) => {
                    *guard = Ok(());

                    session_tokens.update_with_refresh_response(&res);

                    client.base_client().set_session_tokens(session_tokens);

                    // TODO: Let ffi client to know that tokens have changed

                    Ok(Some(res))
                }
                Err(error) => {
                    *guard = match error.as_ruma_api_error() {
                        Some(RumaApiError::ClientApi(api_error)) => {
                            Err(RefreshTokenError::ClientApi(api_error.to_owned()))
                        }
                        _ => Err(RefreshTokenError::UnableToRefreshToken),
                    };

                    Err(error)
                }
            }
        } else {
            match *client.inner.refresh_token_lock.lock().await {
                Ok(_) => Ok(None),
                Err(_) => Err(RefreshTokenError::UnableToRefreshToken.into()),
            }
        }
    }

    /// Register a user to the server.
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
    ///     ruma::api::client::{
    ///         account::register::v3::Request as RegistrationRequest, uiaa,
    ///     },
    ///     Client,
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
    pub async fn register(
        &self,
        request: register::v3::Request,
    ) -> HttpResult<register::v3::Response> {
        let homeserver = self.client.homeserver().await;
        info!("Registering to {homeserver}");

        let config = if self.client.inner.appservice_mode {
            Some(RequestConfig::short_retry().force_auth())
        } else {
            None
        };

        self.client.send(request, config).await
    }

    /// Log out the current user.
    pub async fn logout(&self) -> HttpResult<logout::v3::Response> {
        let request = logout::v3::Request::new();
        self.client.send(request, None).await
    }
}
