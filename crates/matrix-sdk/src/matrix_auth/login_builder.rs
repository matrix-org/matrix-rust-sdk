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
#![cfg_attr(not(target_arch = "wasm32"), deny(clippy::future_not_send))]

use std::{
    future::{Future, IntoFuture},
    pin::Pin,
};

use ruma::{
    api::client::{session::login, uiaa::UserIdentifier},
    assign,
    serde::JsonObject,
};
use tracing::{info, instrument};

use super::MatrixAuth;
use crate::{config::RequestConfig, Result};

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
    #[instrument(
        target = "matrix_sdk::client",
        name = "login",
        skip_all,
        fields(method = self.login_method.tracing_desc()),
    )]
    pub async fn send(self) -> Result<login::v3::Response> {
        let client = &self.auth.client;
        let homeserver = client.homeserver().await;
        info!(homeserver = homeserver.as_str(), identifier = ?self.login_method.id(), "Logging in");

        let request = assign!(login::v3::Request::new(self.login_method.into_login_info()), {
            device_id: self.device_id.map(Into::into),
            initial_device_display_name: self.initial_device_display_name,
            refresh_token: self.request_refresh_token,
        });

        let response = client.send(request, Some(RequestConfig::short_retry())).await?;
        client.receive_login_response(&response).await?;

        Ok(response)
    }
}

impl IntoFuture for LoginBuilder {
    type Output = Result<login::v3::Response>;
    // TODO: Use impl Trait once allowed in this position on stable
    type IntoFuture = Pin<Box<dyn Future<Output = Self::Output>>>;

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
    server_url: Option<String>,
    server_response: Option<String>,
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
            server_url: None,
            server_response: None,
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

    /// Set the local URL the server is going to try to bind to.
    ///
    /// Usually something like `http://localhost:3030`. If not set, the server
    /// will try to open a random port on `127.0.0.1`.
    pub fn server_url(mut self, value: &str) -> Self {
        self.server_url = Some(value.to_owned());
        self
    }

    /// Set the text to be shown at the end of the login process.
    ///
    /// This configures the text that will be shown on the webpage at the end of
    /// the login process. This can be an HTML page. If not set, a default text
    /// will be displayed.
    pub fn server_response(mut self, value: &str) -> Self {
        self.server_response = Some(value.to_owned());
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
    #[instrument(target = "matrix_sdk::client", name = "login", skip_all, fields(method = "sso"))]
    pub async fn send(self) -> Result<login::v3::Response> {
        use std::{
            convert::Infallible,
            io::{Error as IoError, ErrorKind as IoErrorKind},
            ops::Range,
            sync::{Arc, Mutex},
        };

        use http::{Method, StatusCode};
        use hyper::{server::conn::AddrIncoming, service::service_fn};
        use rand::{thread_rng, Rng};
        use serde::Deserialize;
        use tokio::{net::TcpListener, sync::oneshot};
        use tracing::debug;
        use url::Url;

        /// The range of ports the SSO server will try to bind to randomly.
        ///
        /// This is used to avoid binding to a port blocked by browsers.
        /// See <https://fetch.spec.whatwg.org/#port-blocking>.
        const SSO_SERVER_BIND_RANGE: Range<u16> = 20000..30000;
        /// The number of times the SSO server will try to bind to a random port
        const SSO_SERVER_BIND_TRIES: u8 = 10;

        let client = &self.auth.client;
        let homeserver = client.homeserver().await;
        info!(%homeserver, "Logging in");

        let (signal_tx, signal_rx) = oneshot::channel();
        let (data_tx, data_rx) = oneshot::channel();
        let data_tx_mutex = Arc::new(Mutex::new(Some(data_tx)));

        let mut redirect_url = match self.server_url {
            Some(s) => Url::parse(&s)?,
            None => {
                Url::parse("http://127.0.0.1:0/").expect("Couldn't parse good known localhost URL")
            }
        };

        let response = self.server_response.unwrap_or_else(|| {
            "The Single Sign-On login process is complete. You can close this page now.".to_owned()
        });

        #[derive(Deserialize)]
        struct QueryParameters {
            #[serde(rename = "loginToken")]
            login_token: Option<String>,
        }

        let handle_request = move |request: http::Request<_>| {
            if request.method() != Method::HEAD && request.method() != Method::GET {
                return Err(StatusCode::METHOD_NOT_ALLOWED);
            }

            if let Some(data_tx) = data_tx_mutex.lock().unwrap().take() {
                let query_string = request.uri().query().unwrap_or("");
                let query: QueryParameters =
                    serde_html_form::from_str(query_string).map_err(|_| {
                        debug!("Failed to deserialize query parameters");
                        StatusCode::BAD_REQUEST
                    })?;

                data_tx.send(query.login_token).unwrap();
            }

            Ok(http::Response::new(response.clone()))
        };

        let listener = {
            if redirect_url.port().expect("The redirect URL doesn't include a port") == 0 {
                let host = redirect_url.host_str().expect("The redirect URL doesn't have a host");
                let mut n = 0u8;

                loop {
                    let port = thread_rng().gen_range(SSO_SERVER_BIND_RANGE);
                    match TcpListener::bind((host, port)).await {
                        Ok(l) => {
                            redirect_url
                                .set_port(Some(port))
                                .expect("Could not set new port on redirect URL");
                            break l;
                        }
                        Err(_) if n < SSO_SERVER_BIND_TRIES => {
                            n += 1;
                        }
                        Err(e) => {
                            return Err(e.into());
                        }
                    }
                }
            } else {
                TcpListener::bind(redirect_url.as_str()).await?
            }
        };

        let incoming = AddrIncoming::from_listener(listener).unwrap();
        let server = hyper::Server::builder(incoming)
            .serve(tower::make::Shared::new(service_fn(move |request| {
                let handle_request = handle_request.clone();
                async move {
                    match handle_request(request) {
                        Ok(res) => Ok::<_, Infallible>(res.map(hyper::Body::from)),
                        Err(status_code) => {
                            let mut res = http::Response::new(hyper::Body::default());
                            *res.status_mut() = status_code;
                            Ok(res)
                        }
                    }
                }
            })))
            .with_graceful_shutdown(async {
                signal_rx.await.ok();
            });

        tokio::spawn(server);

        let sso_url = self
            .auth
            .get_sso_login_url(redirect_url.as_str(), self.identity_provider_id.as_deref())
            .await?;

        (self.use_sso_login_url)(sso_url).await?;

        let token = data_rx
            .await
            .map_err(|e| IoError::new(IoErrorKind::Other, format!("{e}")))?
            .ok_or_else(|| IoError::new(IoErrorKind::Other, "Could not get the loginToken"))?;

        let _ = signal_tx.send(());

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
    // TODO: Use impl Trait once allowed in this position on stable
    type IntoFuture = Pin<Box<dyn Future<Output = Self::Output>>>;

    fn into_future(self) -> Self::IntoFuture {
        Box::pin(self.send())
    }
}
