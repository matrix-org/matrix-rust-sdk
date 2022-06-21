#![cfg_attr(not(target_arch = "wasm32"), deny(clippy::future_not_send))]

#[cfg(all(feature = "sso-login", not(target_arch = "wasm32")))]
use std::future::Future;

use ruma::{
    api::client::{session::login, uiaa::UserIdentifier},
    assign,
};
use tracing::{info, instrument};

use super::Client;
use crate::{config::RequestConfig, Result};

/// The login method.
///
/// See also [`LoginInfo`][login::v3::LoginInfo] and [the spec].
///
/// [the spec]: https://spec.matrix.org/v1.3/client-server-api/#post_matrixclientv3login
enum LoginMethod<'a> {
    /// Login type `m.login.password`
    UserPassword { id: UserIdentifier<'a>, password: &'a str },
    /// Login type `m.token`
    Token(&'a str),
}

impl<'a> LoginMethod<'a> {
    fn id(&self) -> Option<&UserIdentifier<'a>> {
        match self {
            LoginMethod::UserPassword { id, .. } => Some(id),
            LoginMethod::Token(_) => None,
        }
    }

    fn tracing_desc(&self) -> &'static str {
        match self {
            LoginMethod::UserPassword { .. } => "identifier and password",
            LoginMethod::Token(_) => "token",
        }
    }

    fn to_login_info(&self) -> login::v3::LoginInfo<'a> {
        match self {
            LoginMethod::UserPassword { id, password } => {
                login::v3::LoginInfo::Password(login::v3::Password::new(id.clone(), password))
            }
            LoginMethod::Token(token) => login::v3::LoginInfo::Token(login::v3::Token::new(token)),
        }
    }
}

/// Builder type used to configure optional settings for logging in with a
/// username or token.
///
/// Created with [`Client::login_username`] or [`Client::login_token`].
/// Finalized with [`.send()`](Self::send).
#[allow(missing_debug_implementations)]
pub struct LoginBuilder<'a> {
    client: Client,
    login_method: LoginMethod<'a>,
    device_id: Option<&'a str>,
    initial_device_display_name: Option<&'a str>,
}

impl<'a> LoginBuilder<'a> {
    fn new(client: Client, login_method: LoginMethod<'a>) -> Self {
        Self { client, login_method, device_id: None, initial_device_display_name: None }
    }

    pub(super) fn new_password(client: Client, id: UserIdentifier<'a>, password: &'a str) -> Self {
        Self::new(client, LoginMethod::UserPassword { id, password })
    }

    pub(super) fn new_token(client: Client, token: &'a str) -> Self {
        Self::new(client, LoginMethod::Token(token))
    }

    /// Set the device ID.
    ///
    /// The device ID is a unique ID that will be associated with this session.
    /// If not set, the homeserver will create one. Can be an existing device ID
    /// from a previous login call. Note that this should be done only if the
    /// client also holds the corresponding encryption keys.
    pub fn device_id(mut self, value: &'a str) -> Self {
        self.device_id = Some(value);
        self
    }

    /// Set the initial device display name.
    ///
    /// The device display name is the public name that will be associated with
    /// the device ID. Only necessary the first time you login with this device
    /// ID. It can be changed later.
    pub fn initial_device_display_name(mut self, value: &'a str) -> Self {
        self.initial_device_display_name = Some(value);
        self
    }

    /// Send the login request.
    #[instrument(
        target = "matrix_sdk::client",
        name = "login",
        skip_all,
        fields(method = self.login_method.tracing_desc()),
    )]
    pub async fn send(self) -> Result<login::v3::Response> {
        let homeserver = self.client.homeserver().await;
        info!(homeserver = homeserver.as_str(), identifier = ?self.login_method.id(), "Logging in");

        let request = assign!(login::v3::Request::new(self.login_method.to_login_info()), {
            device_id: self.device_id.map(Into::into),
            initial_device_display_name: self.initial_device_display_name,
        });

        let response = self.client.send(request, Some(RequestConfig::short_retry())).await?;
        self.client.receive_login_response(&response).await?;

        Ok(response)
    }
}

/// Builder type used to configure optional settings for logging in via SSO.
///
/// Created with [`Client::login_sso`].
/// Finalized with [`.send()`](Self::send).
#[cfg(all(feature = "sso-login", not(target_arch = "wasm32")))]
#[allow(missing_debug_implementations)]
pub struct SsoLoginBuilder<'a, F> {
    client: Client,
    use_sso_login_url: F,
    device_id: Option<&'a str>,
    initial_device_display_name: Option<&'a str>,
    server_url: Option<&'a str>,
    server_response: Option<&'a str>,
    identity_provider_id: Option<&'a str>,
}

#[cfg(all(feature = "sso-login", not(target_arch = "wasm32")))]
impl<'a, F, Fut> SsoLoginBuilder<'a, F>
where
    F: FnOnce(String) -> Fut + Send,
    Fut: Future<Output = Result<()>> + Send,
{
    pub(super) fn new(client: Client, use_sso_login_url: F) -> Self {
        Self {
            client,
            use_sso_login_url,
            device_id: None,
            initial_device_display_name: None,
            server_url: None,
            server_response: None,
            identity_provider_id: None,
        }
    }

    /// Set the device ID.
    ///
    /// The device ID is a unique ID that will be associated with this session.
    /// If not set, the homeserver will create one. Can be an existing device ID
    /// from a previous login call. Note that this should be done only if the
    /// client also holds the corresponding encryption keys.
    pub fn device_id(mut self, value: &'a str) -> Self {
        self.device_id = Some(value);
        self
    }

    /// Set the initial device display name.
    ///
    /// The device display name is the public name that will be associated with
    /// the device ID. Only necessary the first time you login with this device
    /// ID. It can be changed later.
    pub fn initial_device_display_name(mut self, value: &'a str) -> Self {
        self.initial_device_display_name = Some(value);
        self
    }

    /// Set the local URL the server is going to try to bind to.
    ///
    /// Usually something like `http://localhost:3030`. If not set, the server
    /// will try to open a random port on `127.0.0.1`.
    pub fn server_url(mut self, value: &'a str) -> Self {
        self.server_url = Some(value);
        self
    }

    /// Set the text to be shown at the end of the login process.
    ///
    /// This configures the text that will be shown on the webpage at the end of
    /// the login process. This can be an HTML page. If not set, a default text
    /// will be displayed.
    pub fn server_response(mut self, value: &'a str) -> Self {
        self.server_response = Some(value);
        self
    }

    /// Set the ID of the identity provider to log in with.
    pub fn identity_provider_id(mut self, value: &'a str) -> Self {
        self.identity_provider_id = Some(value);
        self
    }

    /// Send the login request.
    #[instrument(target = "matrix_sdk::client", name = "login", skip_all, fields(method = "sso"))]
    pub async fn send(self) -> Result<login::v3::Response> {
        use std::{
            collections::HashMap,
            io::{Error as IoError, ErrorKind as IoErrorKind},
            ops::Range,
            sync::{Arc, Mutex},
        };

        use rand::{thread_rng, Rng};
        use tokio::{net::TcpListener, sync::oneshot};
        use tokio_stream::wrappers::TcpListenerStream;
        use url::Url;
        use warp::Filter;

        /// The range of ports the SSO server will try to bind to randomly.
        ///
        /// This is used to avoid binding to a port blocked by browsers.
        /// See <https://fetch.spec.whatwg.org/#port-blocking>.
        const SSO_SERVER_BIND_RANGE: Range<u16> = 20000..30000;
        /// The number of times the SSO server will try to bind to a random port
        const SSO_SERVER_BIND_TRIES: u8 = 10;

        let homeserver = self.client.homeserver().await;
        info!("Logging in to {}", homeserver);

        let (signal_tx, signal_rx) = oneshot::channel();
        let (data_tx, data_rx) = oneshot::channel();
        let data_tx_mutex = Arc::new(Mutex::new(Some(data_tx)));

        let mut redirect_url = match self.server_url {
            Some(s) => Url::parse(s)?,
            None => {
                Url::parse("http://127.0.0.1:0/").expect("Couldn't parse good known localhost URL")
            }
        };

        let response = match self.server_response {
            Some(s) => s.to_string(),
            None => String::from(
                "The Single Sign-On login process is complete. You can close this page now.",
            ),
        };

        let route = warp::get().and(warp::query::<HashMap<String, String>>()).map(
            move |p: HashMap<String, String>| {
                if let Some(data_tx) = data_tx_mutex.lock().unwrap().take() {
                    if let Some(token) = p.get("loginToken") {
                        data_tx.send(Some(token.to_owned())).unwrap();
                    } else {
                        data_tx.send(None).unwrap();
                    }
                }
                http::Response::builder().body(response.clone())
            },
        );

        let listener = {
            if redirect_url.port().expect("The redirect URL doesn't include a port") == 0 {
                let host = redirect_url.host_str().expect("The redirect URL doesn't have a host");
                let mut n = 0u8;
                let mut port = 0u16;
                let mut res = Err(IoError::new(IoErrorKind::Other, ""));

                while res.is_err() && n < SSO_SERVER_BIND_TRIES {
                    port = thread_rng().gen_range(SSO_SERVER_BIND_RANGE);
                    res = TcpListener::bind((host, port)).await;
                    n += 1;
                }
                match res {
                    Ok(s) => {
                        redirect_url
                            .set_port(Some(port))
                            .expect("Could not set new port on redirect URL");
                        s
                    }
                    Err(err) => return Err(err.into()),
                }
            } else {
                match TcpListener::bind(redirect_url.as_str()).await {
                    Ok(s) => s,
                    Err(err) => return Err(err.into()),
                }
            }
        };

        let server = warp::serve(route).serve_incoming_with_graceful_shutdown(
            TcpListenerStream::new(listener),
            async {
                signal_rx.await.ok();
            },
        );

        tokio::spawn(server);

        let sso_url =
            self.client.get_sso_login_url(redirect_url.as_str(), self.identity_provider_id).await?;

        match (self.use_sso_login_url)(sso_url).await {
            Ok(t) => t,
            Err(err) => return Err(err),
        };

        let token = match data_rx.await {
            Ok(Some(t)) => t,
            Ok(None) => {
                return Err(IoError::new(IoErrorKind::Other, "Could not get the loginToken").into())
            }
            Err(err) => return Err(IoError::new(IoErrorKind::Other, format!("{}", err)).into()),
        };

        let _ = signal_tx.send(());

        let login_builder = LoginBuilder {
            device_id: self.device_id,
            initial_device_display_name: self.initial_device_display_name,
            ..LoginBuilder::new_token(self.client, &token)
        };
        login_builder.send().await
    }
}
