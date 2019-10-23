use std::collections::HashMap;
use std::convert::{TryFrom, TryInto};
use std::future::Future;
use std::pin::Pin;

use http::Method as HttpMethod;
use http::Response as HttpResponse;
use js_int::UInt;
use reqwest;
use url::Url;

use ruma_api::Endpoint;
use ruma_events::collections::all::RoomEvent;
use ruma_events::Event;
pub use ruma_events::EventType;

use crate::api;
use crate::base_client::Client as BaseClient;
use crate::error::{Error, InnerError};
use crate::session::Session;

pub struct AsyncClient {
    /// The URL of the homeserver to connect to.
    homeserver: Url,
    /// The underlying HTTP client.
    http_client: reqwest::Client,
    /// User session data.
    base_client: BaseClient,
    /// Event callbacks
    event_callbacks:
        HashMap<EventType, Box<dyn FnMut(RoomEvent) -> Pin<Box<dyn Future<Output = ()>>>>>,
}

#[derive(Default, Debug)]
pub struct AsyncClientConfig {
    proxy: Option<reqwest::Proxy>,
    use_sys_proxy: bool,
    user_agent: Option<String>,
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
        let http_client = reqwest::Client::new();

        Ok(Self {
            homeserver,
            http_client,
            base_client: BaseClient::new(session),
            event_callbacks: HashMap::new(),
        })
    }

    pub fn new_with_config(
        homeserver_url: &str,
        session: Option<Session>,
        config: AsyncClientConfig,
    ) -> Result<Self, url::ParseError> {
        let homeserver = Url::parse(homeserver_url)?;
        let http_client = reqwest::Client::builder();

        let http_client = if config.disable_ssl_verification {
            http_client.danger_accept_invalid_certs(true)
        } else {
            http_client
        };

        let http_client = match config.proxy {
            Some(p) => http_client.proxy(p),
            None => http_client,
        };

        let http_client = if config.use_sys_proxy {
            http_client.use_sys_proxy()
        } else {
            http_client
        };

        let mut headers = reqwest::header::HeaderMap::new();

        headers.insert(
            reqwest::header::USER_AGENT,
            reqwest::header::HeaderValue::from_static("ruma"),
        );

        let http_client = http_client.default_headers(headers).build().unwrap();

        Ok(Self {
            homeserver,
            http_client,
            base_client: BaseClient::new(session),
            event_callbacks: HashMap::new(),
        })
    }

    pub fn add_event_callback(
        &mut self,
        event_type: EventType,
        callback: impl FnMut(RoomEvent) -> Pin<Box<dyn Future<Output = ()>>> + 'static,
    ) {
        self.event_callbacks.insert(event_type, Box::new(callback));
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
        self.base_client.receive_login_response(&response);

        Ok(response)
    }

    pub async fn sync(
        &mut self,
        sync_settings: SyncSettings,
    ) -> Result<sync_events::Response, Error> {
        let request = sync_events::Request {
            filter: None,
            since: sync_settings.token,
            full_state: sync_settings.full_state,
            set_presence: None,
            timeout: sync_settings.timeout,
        };

        let response = self.send(request).await.unwrap();

        for (_, room) in &response.rooms.join {
            for event in &room.timeline.events {
                let event_type = match &event {
                    RoomEvent::CallAnswer(e) => e.event_type(),
                    RoomEvent::CallCandidates(e) => e.event_type(),
                    RoomEvent::CallHangup(e) => e.event_type(),
                    RoomEvent::CallInvite(e) => e.event_type(),
                    RoomEvent::RoomAliases(e) => e.event_type(),
                    RoomEvent::RoomAvatar(e) => e.event_type(),
                    RoomEvent::RoomCanonicalAlias(e) => e.event_type(),
                    RoomEvent::RoomCreate(e) => e.event_type(),
                    RoomEvent::RoomGuestAccess(e) => e.event_type(),
                    RoomEvent::RoomHistoryVisibility(e) => e.event_type(),
                    RoomEvent::RoomJoinRules(e) => e.event_type(),
                    RoomEvent::RoomMember(e) => e.event_type(),
                    RoomEvent::RoomMessage(e) => e.event_type(),
                    RoomEvent::RoomName(e) => e.event_type(),
                    RoomEvent::RoomPinnedEvents(e) => e.event_type(),
                    RoomEvent::RoomPowerLevels(e) => e.event_type(),
                    RoomEvent::RoomRedaction(e) => e.event_type(),
                    RoomEvent::RoomThirdPartyInvite(e) => e.event_type(),
                    RoomEvent::RoomTopic(e) => e.event_type(),
                    RoomEvent::CustomRoom(e) => e.event_type(),
                    RoomEvent::CustomState(e) => e.event_type(),
                };

                if self.event_callbacks.contains_key(&event_type) {
                    let cb = self.event_callbacks.get_mut(&event_type).unwrap();
                    cb(event.clone());
                }

                if self.event_callbacks.contains_key(&event_type) {
                    let cb = self.event_callbacks.get_mut(&event_type).unwrap();
                    let future = Pin::from(cb(event.clone()));
                    future.await;
                }
            }
        }

        Ok(response)
    }

    async fn send<Request: Endpoint>(&self, request: Request) -> Result<Request::Response, Error> {
        let request: http::Request<Vec<u8>> = request.try_into()?;
        let url = request.uri();
        let url = self.homeserver.join(url.path()).unwrap();

        let request_builder = match Request::METADATA.method {
            HttpMethod::GET => self.http_client.get(url),
            HttpMethod::POST => {
                let body = request.body().clone();
                self.http_client.post(url).body(body)
            }
            HttpMethod::PUT => unimplemented!(),
            HttpMethod::DELETE => unimplemented!(),
            _ => panic!("Unsuported method"),
        };

        let request_builder = if Request::METADATA.requires_authentication {
            if let Some(ref session) = self.base_client.session {
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
