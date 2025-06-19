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
#![cfg_attr(not(target_family = "wasm"), deny(clippy::future_not_send))]

#[cfg(feature = "sso-login")]
use std::future::Future;
use std::future::IntoFuture;

use matrix_sdk_common::boxed_into_future;
use ruma::{
    api::client::{session::login, uiaa::UserIdentifier},
    assign,
    serde::JsonObject,
};
use tracing::{info, instrument};

use super::MatrixAuth;
#[cfg(feature = "sso-login")]
use crate::utils::local_server::LocalServerBuilder;
use crate::{Result, config::RequestConfig};

/// The login method.
///
/// See also [`LoginInfo`][login::v3::LoginInfo] and [the spec].
///
/// [the spec]: https://spec.matrix.org/v1.3/client-server-api/#post_matrixclientv3login
enum LoginMethod {
    /// Login type `m.login.password`
    UserPassword {
        id: UserIdentifier,
        password: String,
    },
    /// Login type `m.token`
    Token(String),
    Custom(login::v3::LoginInfo),
}

impl LoginMethod {
    fn id(&self) -> Option<&UserIdentifier> {
        match self {
            LoginMethod::UserPassword { id, .. } => Some(id),
            LoginMethod::Token(_) | LoginMethod::Custom(_) => None,
        }
    }

    fn tracing_desc(&self) -> &'static str {
        match self {
            LoginMethod::UserPassword { .. } => "identifier and password",
            LoginMethod::Token(_) => "token",
            LoginMethod::Custom(_) => "custom",
        }
    }

    fn into_login_info(self) -> login::v3::LoginInfo {
        match self {
            LoginMethod::UserPassword { id, password } => {
                login::v3::LoginInfo::Password(login::v3::Password::new(id, password))
            }
            LoginMethod::Token(token) => login::v3::LoginInfo::Token(login::v3::Token::new(token)),
            LoginMethod::Custom(login_info) => login_info,
        }
    }
}

/// Builder type used to configure optional settings for logging in with a
/// username or token.
///
/// Created with [`MatrixAuth::login_username`] or [`MatrixAuth::login_token`].
/// Finalized with [`.send()`](Self::send).
#[allow(missing_debug_implementations)]
pub struct LoginBuilder {
    auth: MatrixAuth,
    login_method: LoginMethod,
    device_id: Option<String>,
    initial_device_display_name: Option<String>,
    request_refresh_token: bool,
}

impl LoginBuilder {
    fn new(auth: MatrixAuth, login_method: LoginMethod) -> Self {
        Self {
            auth,
            login_method,
            device_id: None,
            initial_device_display_name: None,
            request_refresh_token: false,
        }
    }

    pub(super) fn new_password(auth: MatrixAuth, id: UserIdentifier, password: String) -> Self {
        Self::new(auth, LoginMethod::UserPassword { id, password })
    }

    pub(super) fn new_token(auth: MatrixAuth, token: String) -> Self {
        Self::new(auth, LoginMethod::Token(token))
    }

    pub(super) fn new_custom(
        auth: MatrixAuth,
        login_type: &str,
        data: JsonObject,
    ) -> serde_json::Result<Self> {
        let login_info = login::v3::LoginInfo::new(login_type, data)?;
        Ok(Self::new(auth, LoginMethod::Custom(login_info)))
    }

    /// Set the device ID.
    ///
    /// The device ID is a unique ID that will be associated with this session.
    /// If not set, the homeserver will create one. Can be an existing device ID
    /// from a previous login call. Note that this should be done only if the
    /// client also holds the corresponding encryption keys.
    pub fn device_id(mut self, value: &str) -> Self {
        self.device_id = Some(value.to_owned());
        self
    }

    /// Set the initial device display name.
    ///
    /// The device display name is the public name that will be associated with
    /// the device ID. Only necessary the first time you log in with this device
    /// ID. It can be changed later.
    pub fn initial_device_display_name(mut self, value: &str) -> Self {
        self.initial_device_display_name = Some(value.to_owned());
        self
    }

    /// Advertise support for [refreshing access tokens].
    ///
    /// By default, the `Client` won't handle refreshing access tokens, so
    /// [`Client::refresh_access_token()`] or
    /// [`MatrixAuth::refresh_access_token()`] needs to be called manually.
    ///
    /// This behavior can be changed by calling
    /// [`handle_refresh_tokens()`] when building the `Client`.
    ///
    /// *Note* that refreshing access tokens might not be supported or might be
    /// enforced by the homeserver regardless of this setting.
    ///
    /// [refreshing access tokens]: https://spec.matrix.org/v1.3/client-server-api/#refreshing-access-tokens
    /// [`Client::refresh_access_token()`]: crate::Client::refresh_access_token
    /// [`handle_refresh_tokens()`]: crate::ClientBuilder::handle_refresh_tokens
    pub fn request_refresh_token(mut self) -> Self {
        self.request_refresh_token = true;
        self
    }

    /// Send the login request.
    ///
    /// Instead of calling this function and `.await`ing its return value, you
    /// can also `.await` the `LoginBuilder` directly.
    ///
    /// # Panics
    ///
    /// Panics if a session was already restored or logged in.
    #[instrument(
        target = "matrix_sdk::client",
        name = "login",
        skip_all,
        fields(method = self.login_method.tracing_desc()),
    )]
    pub async fn send(self) -> Result<login::v3::Response> {
        let client = &self.auth.client;
        let homeserver = client.homeserver();
        info!(homeserver = homeserver.as_str(), identifier = ?self.login_method.id(), "Logging in");

        let login_info = self.login_method.into_login_info();

        let request = assign!(login::v3::Request::new(login_info.clone()), {
            device_id: self.device_id.map(Into::into),
            initial_device_display_name: self.initial_device_display_name,
            refresh_token: self.request_refresh_token,
        });

        let response =
            client.send(request).with_request_config(RequestConfig::short_retry()).await?;
        self.auth
            .receive_login_response(
                &response,
                #[cfg(feature = "e2e-encryption")]
                Some(login_info),
            )
            .await?;

        Ok(response)
    }
}

impl IntoFuture for LoginBuilder {
    type Output = Result<login::v3::Response>;
    boxed_into_future!();

    fn into_future(self) -> Self::IntoFuture {
        Box::pin(self.send())
    }
}

/// Builder type used to configure optional settings for logging in via SSO.
///
/// Created with [`MatrixAuth::login_sso`]. Finalized with
/// [`.send()`](Self::send).
#[cfg(feature = "sso-login")]
#[allow(missing_debug_implementations)]
pub struct SsoLoginBuilder<F> {
    auth: MatrixAuth,
    use_sso_login_url: F,
    device_id: Option<String>,
    initial_device_display_name: Option<String>,
    server_builder: Option<LocalServerBuilder>,
    identity_provider_id: Option<String>,
    request_refresh_token: bool,
}

#[cfg(feature = "sso-login")]
impl<F, Fut> SsoLoginBuilder<F>
where
    F: FnOnce(String) -> Fut + Send,
    Fut: Future<Output = Result<()>> + Send,
{
    pub(super) fn new(auth: MatrixAuth, use_sso_login_url: F) -> Self {
        Self {
            auth,
            use_sso_login_url,
            device_id: None,
            initial_device_display_name: None,
            server_builder: None,
            identity_provider_id: None,
            request_refresh_token: false,
        }
    }

    /// Set the device ID.
    ///
    /// The device ID is a unique ID that will be associated with this session.
    /// If not set, the homeserver will create one. Can be an existing device ID
    /// from a previous login call. Note that this should be done only if the
    /// client also holds the corresponding encryption keys.
    pub fn device_id(mut self, value: &str) -> Self {
        self.device_id = Some(value.to_owned());
        self
    }

    /// Set the initial device display name.
    ///
    /// The device display name is the public name that will be associated with
    /// the device ID. Only necessary the first time you login with this device
    /// ID. It can be changed later.
    pub fn initial_device_display_name(mut self, value: &str) -> Self {
        self.initial_device_display_name = Some(value.to_owned());
        self
    }

    /// Customize the settings used to construct the server where the end-user
    /// will be redirected.
    ///
    /// If this is not set, the default settings of [`LocalServerBuilder`] will
    /// be used.
    pub fn server_builder(mut self, builder: LocalServerBuilder) -> Self {
        self.server_builder = Some(builder);
        self
    }

    /// Set the ID of the identity provider to log in with.
    pub fn identity_provider_id(mut self, value: &str) -> Self {
        self.identity_provider_id = Some(value.to_owned());
        self
    }

    /// Advertise support for [refreshing access tokens].
    ///
    /// By default, the `Client` won't handle refreshing access tokens, so
    /// [`Client::refresh_access_token()`] or
    /// [`MatrixAuth::refresh_access_token()`] needs to be called
    /// manually.
    ///
    /// This behavior can be changed by calling
    /// [`handle_refresh_tokens()`] when building the `Client`.
    ///
    /// *Note* that refreshing access tokens might not be supported or might be
    /// enforced by the homeserver regardless of this setting.
    ///
    /// [refreshing access tokens]: https://spec.matrix.org/v1.3/client-server-api/#refreshing-access-tokens
    /// [`Client::refresh_access_token()`]: crate::Client::refresh_access_token
    /// [`handle_refresh_tokens()`]: crate::ClientBuilder::handle_refresh_tokens
    pub fn request_refresh_token(mut self) -> Self {
        self.request_refresh_token = true;
        self
    }

    /// Send the login request.
    ///
    /// Instead of calling this function and `.await`ing its return value, you
    /// can also `.await` the `SsoLoginBuilder` directly.
    ///
    /// # Panics
    ///
    /// Panics if a session was already restored or logged in.
    #[instrument(target = "matrix_sdk::client", name = "login", skip_all, fields(method = "sso"))]
    pub async fn send(self) -> Result<login::v3::Response> {
        use std::io::Error as IoError;

        use serde::Deserialize;

        let client = &self.auth.client;
        let homeserver = client.homeserver();
        info!(%homeserver, "Logging in");

        #[derive(Deserialize)]
        struct QueryParameters {
            #[serde(rename = "loginToken")]
            login_token: String,
        }

        let server_builder = self.server_builder.unwrap_or_default();
        let (redirect_url, server_handle) = server_builder.spawn().await?;

        let sso_url = self
            .auth
            .get_sso_login_url(redirect_url.as_str(), self.identity_provider_id.as_deref())
            .await?;

        (self.use_sso_login_url)(sso_url).await?;

        let query_string =
            server_handle.await.ok_or_else(|| IoError::other("Could not get the loginToken"))?;
        let token = serde_html_form::from_str::<QueryParameters>(&query_string)
            .map_err(IoError::other)?
            .login_token;

        let login_builder = LoginBuilder {
            device_id: self.device_id,
            initial_device_display_name: self.initial_device_display_name,
            request_refresh_token: self.request_refresh_token,
            ..LoginBuilder::new_token(self.auth, token)
        };
        login_builder.send().await
    }
}

#[cfg(feature = "sso-login")]
impl<F, Fut> IntoFuture for SsoLoginBuilder<F>
where
    F: FnOnce(String) -> Fut + Send + 'static,
    Fut: Future<Output = Result<()>> + Send + 'static,
{
    type Output = Result<login::v3::Response>;
    boxed_into_future!();

    fn into_future(self) -> Self::IntoFuture {
        Box::pin(self.send())
    }
}
