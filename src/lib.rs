//! Crate `ruma_client` is a [Matrix](https://matrix.org/) client library.
//!
use std::convert::{TryFrom, TryInto};

use http::Method as HttpMethod;
use http::Response as HttpResponse;
use js_int::UInt;
use reqwest;
use ruma_api::Endpoint;
use url::Url;

use crate::error::InnerError;

pub use crate::{error::Error, session::Session};
pub use ruma_client_api as api;
pub use ruma_events as events;

//pub mod api;
mod error;
mod session;

#[derive(Debug)]
pub struct AsyncClient {
    /// The URL of the homeserver to connect to.
    homeserver: Url,
    /// The underlying HTTP client.
    client: reqwest::Client,
    /// User session data.
    session: Option<Session>,
}

#[derive(Default, Debug)]
pub struct AsyncClientConfig {
    proxy: Option<reqwest::Proxy>,
    use_sys_proxy: bool,
    disable_ssl_verification: bool,
}

impl AsyncClientConfig {
    pub fn new() -> Self {
        Default::default()
    }

    pub fn proxy(mut self, proxy: &str) -> Result<Self, Error> {
        if self.use_sys_proxy {
            return Err(Error(InnerError::ConfigurationError(
                "Using the system proxy has been previously configured.".to_string(),
            )));
        }
        self.proxy = Some(reqwest::Proxy::all(proxy)?);
        Ok(self)
    }

    pub fn use_sys_proxy(mut self) -> Result<Self, Error> {
        if self.proxy.is_some() {
            return Err(Error(InnerError::ConfigurationError(
                "A proxy has already been configured.".to_string(),
            )));
        }
        self.use_sys_proxy = true;
        Ok(self)
    }

    pub fn disable_ssl_verification(mut self) -> Self {
        self.disable_ssl_verification = true;
        self
    }
}

#[derive(Debug, Default)]
pub struct SyncSettings {
    pub(crate) timeout: Option<UInt>,
    pub(crate) token: Option<String>,
    pub(crate) full_state: Option<bool>,
}

impl SyncSettings {
    pub fn new() -> Self {
        Default::default()
    }

    pub fn token<S: Into<String>>(mut self, token: S) -> Self {
        self.token = Some(token.into());
        self
    }

    pub fn timeout<T: TryInto<UInt>>(mut self, timeout: T) -> Result<Self, js_int::TryFromIntError>
    where
        js_int::TryFromIntError:
            std::convert::From<<T as std::convert::TryInto<js_int::UInt>>::Error>,
    {
        self.timeout = Some(timeout.try_into()?);
        Ok(self)
    }

    pub fn full_state(mut self, full_state: bool) -> Self {
        self.full_state = Some(full_state);
        self
    }
}

use api::r0::session::login;
use api::r0::sync::sync_events;

impl AsyncClient {
    /// Creates a new client for making HTTP requests to the given homeserver.
    pub fn new(homeserver_url: &str, session: Option<Session>) -> Result<Self, url::ParseError> {
        let homeserver = Url::parse(homeserver_url)?;
        let client = reqwest::Client::new();

        Ok(Self {
            homeserver,
            client,
            session,
        })
    }

    pub fn new_with_config(
        homeserver_url: &str,
        session: Option<Session>,
        config: AsyncClientConfig,
    ) -> Result<Self, url::ParseError> {
        let homeserver = Url::parse(homeserver_url)?;
        let client = reqwest::Client::builder();

        let client = if config.disable_ssl_verification {
            client.danger_accept_invalid_certs(true)
        } else {
            client
        };

        let client = match config.proxy {
            Some(p) => client.proxy(p),
            None => client,
        };

        let client = if config.use_sys_proxy {
            client.use_sys_proxy()
        } else {
            client
        };

        let mut headers = reqwest::header::HeaderMap::new();

        headers.insert(reqwest::header::USER_AGENT, reqwest::header::HeaderValue::from_static("ruma"));

        let client = client.default_headers(headers).build().unwrap();

        Ok(Self {
            homeserver,
            client,
            session,
        })
    }

    pub async fn login<S: Into<String>>(
        &mut self,
        user: S,
        password: S,
        device_id: Option<S>,
    ) -> Result<login::Response, Error> {
        let request = login::Request {
            address: None,
            login_type: login::LoginType::Password,
            medium: None,
            device_id: device_id.map(|d| d.into()),
            password: password.into(),
            user: user.into(),
        };

        let response = self.send(request).await.unwrap();

        let session = Session {
            access_token: response.access_token.clone(),
            device_id: response.device_id.clone(),
            user_id: response.user_id.clone(),
        };

        self.session = Some(session.clone());

        Ok(response)
    }

    pub async fn sync(&self, sync_settings: SyncSettings) -> Result<sync_events::Response, Error> {
        let request = sync_events::Request {
            filter: None,
            since: sync_settings.token,
            full_state: sync_settings.full_state,
            set_presence: None,
            timeout: sync_settings.timeout,
        };

        let response = self.send(request).await.unwrap();

        Ok(response)
    }

    async fn send<Request: Endpoint>(&self, request: Request) -> Result<Request::Response, Error> {
        let request: http::Request<Vec<u8>> = request.try_into()?;
        let url = request.uri();
        let url = self.homeserver.join(url.path()).unwrap();

        let request_builder = match Request::METADATA.method {
            HttpMethod::GET => self.client.get(url),
            HttpMethod::POST => {
                let body = request.body().clone();
                self.client.post(url).body(body)
            }
            HttpMethod::PUT => unimplemented!(),
            HttpMethod::DELETE => unimplemented!(),
            _ => panic!("Unsuported method"),
        };

        let request_builder = if Request::METADATA.requires_authentication {
            if let Some(ref session) = self.session {
                request_builder.bearer_auth(&session.access_token)
            } else {
                return Err(Error(InnerError::AuthenticationRequired));
            }
        } else {
            request_builder
        };

        let response = request_builder.send().await?;

        let status = response.status();
        let body = response.bytes().await?.as_ref().to_owned();
        let response = HttpResponse::builder().status(status).body(body).unwrap();
        let response = Request::Response::try_from(response)?;

        Ok(response)
    }
}
