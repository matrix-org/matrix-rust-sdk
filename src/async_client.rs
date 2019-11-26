use futures::future::{BoxFuture, Future, FutureExt};
use std::convert::{TryFrom, TryInto};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use http::Method as HttpMethod;
use http::Response as HttpResponse;
use js_int::UInt;
use reqwest::header::{HeaderValue, InvalidHeaderValue};
use url::Url;

use ruma_api::Endpoint;
use ruma_events::collections::all::RoomEvent;
use ruma_events::room::message::MessageEventContent;
use ruma_events::EventResult;
pub use ruma_events::EventType;
use ruma_identifiers::RoomId;

use crate::api;
use crate::base_client::Client as BaseClient;
use crate::base_client::Room;
use crate::error::{Error, InnerError};
use crate::session::Session;
use crate::VERSION;

type RoomEventCallbackF =
    Box<dyn FnMut(Arc<RwLock<Room>>, Arc<EventResult<RoomEvent>>) -> BoxFuture<'static, ()> + Send>;

#[derive(Clone)]
pub struct AsyncClient {
    /// The URL of the homeserver to connect to.
    homeserver: Url,
    /// The underlying HTTP client.
    http_client: reqwest::Client,
    /// User session data.
    base_client: Arc<RwLock<BaseClient>>,
    /// The transaction id.
    transaction_id: Arc<AtomicU64>,
    /// Event futures
    event_futures: Arc<Mutex<Vec<RoomEventCallbackF>>>,
}

#[derive(Default, Debug)]
pub struct AsyncClientConfig {
    proxy: Option<reqwest::Proxy>,
    user_agent: Option<HeaderValue>,
    disable_ssl_verification: bool,
}

impl AsyncClientConfig {
    pub fn new() -> Self {
        Default::default()
    }

    pub fn proxy(mut self, proxy: &str) -> Result<Self, Error> {
        self.proxy = Some(reqwest::Proxy::all(proxy)?);
        Ok(self)
    }

    pub fn disable_ssl_verification(mut self) -> Self {
        self.disable_ssl_verification = true;
        self
    }

    pub fn user_agent(mut self, user_agent: &str) -> Result<Self, InvalidHeaderValue> {
        self.user_agent = Some(HeaderValue::from_str(user_agent)?);
        Ok(self)
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

use api::r0::send::send_message_event;
use api::r0::session::login;
use api::r0::sync::sync_events;

impl AsyncClient {
    /// Creates a new client for making HTTP requests to the given homeserver.
    pub fn new<U: TryInto<Url>>(
        homeserver_url: U,
        session: Option<Session>,
    ) -> Result<Self, Error> {
        let config = AsyncClientConfig::new();
        AsyncClient::new_with_config(homeserver_url, session, config)
    }

    pub fn new_with_config<U: TryInto<Url>>(
        homeserver_url: U,
        session: Option<Session>,
        config: AsyncClientConfig,
    ) -> Result<Self, Error> {
        let homeserver: Url = match homeserver_url.try_into() {
            Ok(u) => u,
            Err(e) => panic!("Error parsing homeserver url"),
        };

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

        let mut headers = reqwest::header::HeaderMap::new();

        let user_agent = match config.user_agent {
            Some(a) => a,
            None => HeaderValue::from_str(&format!("nio-rust {}", VERSION)).unwrap(),
        };

        headers.insert(reqwest::header::USER_AGENT, user_agent);

        let http_client = http_client.default_headers(headers).build().unwrap();

        Ok(Self {
            homeserver,
            http_client,
            base_client: Arc::new(RwLock::new(BaseClient::new(session))),
            transaction_id: Arc::new(AtomicU64::new(0)),
            event_futures: Arc::new(Mutex::new(Vec::new())),
        })
    }

    pub fn logged_in(&self) -> bool {
        self.base_client.read().unwrap().logged_in()
    }

    pub fn add_event_future<C: 'static>(
        &mut self,
        mut callback: impl FnMut(Arc<RwLock<Room>>, Arc<EventResult<RoomEvent>>) -> C + 'static + Send,
    ) where
        C: Future<Output = ()> + Send,
    {
        let mut futures = self.event_futures.lock().unwrap();

        let future = move |room, event| callback(room, event).boxed();

        futures.push(Box::new(future));
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

        let response = self.send(request).await?;
        let mut client = self.base_client.write().unwrap();
        client.receive_login_response(&response);

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

        let response = self.send(request).await?;

        for (room_id, room) in &response.rooms.join {
            let room_id = room_id.to_string();

            let matrix_room = {
                let mut client = self.base_client.write().unwrap();

                for event in &room.state.events {
                    if let EventResult::Ok(e) = event {
                        client.receive_joined_state_event(&room_id, &e);
                    }
                }

                client.joined_rooms.get(&room_id).unwrap().clone()
            };

            for event in &room.timeline.events {
                {
                    let mut client = self.base_client.write().unwrap();
                    client.receive_joined_timeline_event(&room_id, &event);
                }

                let event = Arc::new(event.clone());

                let callbacks = {
                    let mut cb_futures = self.event_futures.lock().unwrap();
                    let mut callbacks = Vec::new();

                    for cb in &mut cb_futures.iter_mut() {
                        callbacks.push(cb(matrix_room.clone(), event.clone()));
                    }

                    callbacks
                };

                for cb in callbacks {
                    cb.await;
                }
            }

            let mut client = self.base_client.write().unwrap();
            client.receive_sync_response(&response);
        }

        Ok(response)
    }

    async fn sync_forever() {}

    async fn send<Request: Endpoint>(&self, request: Request) -> Result<Request::Response, Error> {
        let request: http::Request<Vec<u8>> = request.try_into()?;
        let url = request.uri();
        let url = self
            .homeserver
            .join(url.path_and_query().unwrap().as_str())
            .unwrap();

        let request_builder = match Request::METADATA.method {
            HttpMethod::GET => self.http_client.get(url),
            HttpMethod::POST => {
                let body = request.body().clone();
                self.http_client.post(url).body(body)
            }
            HttpMethod::PUT => {
                let body = request.body().clone();
                self.http_client.put(url).body(body)
            }
            HttpMethod::DELETE => unimplemented!(),
            _ => panic!("Unsuported method"),
        };

        let request_builder = if Request::METADATA.requires_authentication {
            let client = self.base_client.read().unwrap();

            if let Some(ref session) = client.session {
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

    fn transaction_id(&self) -> u64 {
        self.transaction_id.fetch_add(1, Ordering::SeqCst)
    }

    pub async fn room_send(
        &mut self,
        room_id: &str,
        data: MessageEventContent,
    ) -> Result<send_message_event::Response, Error> {
        let request = send_message_event::Request {
            room_id: RoomId::try_from(room_id).unwrap(),
            event_type: EventType::RoomMessage,
            txn_id: self.transaction_id().to_string(),
            data,
        };

        let response = self.send(request).await?;
        Ok(response)
    }

    pub fn sync_token(&self) -> Option<String> {
        self.base_client.read().unwrap().sync_token.clone()
    }
}
