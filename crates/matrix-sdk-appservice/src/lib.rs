// Copyright 2021 Famedly GmbH
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

//! Matrix [Application Service] library
//!
//! The appservice crate aims to provide a batteries-included experience by
//! being a thin wrapper around the [`matrix_sdk`]. That means that we
//!
//! - [x] ship with functionality to configure your webserver crate or simply
//!   run the webserver for you
//! - [x] receive and validate requests from the homeserver correctly
//! - [x] allow calling the homeserver with proper user identity assertion
//! - [x] have consistent room state by leveraging matrix-sdk's state store
//! - [ ] provide E2EE support by leveraging matrix-sdk's crypto store
//!
//! # Status
//!
//! The crate is in an experimental state. Follow
//! [matrix-org/matrix-rust-sdk#228] for progress.
//!
//! # Registration
//!
//! The crate relies on the appservice registration being always in sync with
//! the actual registration used by the homeserver. That's because it's required
//! for the access tokens and because membership states for appservice users are
//! determined based on the registered namespaces.
//!
//! # Quickstart
//!
//! ```no_run
//! # async {
//! #
//! use matrix_sdk_appservice::{
//!     ruma::events::room::member::SyncRoomMemberEvent, AppService,
//!     AppServiceRegistration,
//! };
//!
//! let homeserver_url = "http://127.0.0.1:8008";
//! let server_name = "localhost";
//! let registration = AppServiceRegistration::try_from_yaml_file(
//!     "./tests/registration.yaml",
//! )?;
//!
//! let mut appservice = AppService::builder(
//!     homeserver_url.try_into()?,
//!     server_name.try_into()?,
//!     registration,
//! )
//! .build()
//! .await?;
//! appservice.user(None).await?.add_event_handler(
//!     |_ev: SyncRoomMemberEvent| async {
//!         // do stuff
//!     },
//! );
//!
//! let (host, port) = appservice.registration().get_host_and_port()?;
//! appservice.run(host, port).await?;
//! #
//! # Ok::<(), Box<dyn std::error::Error + 'static>>(())
//! # };
//! ```
//!
//! Check the [examples directory] for fully working examples.
//!
//! [Application Service]: https://matrix.org/docs/spec/application_service/r0.1.2
//! [matrix-org/matrix-rust-sdk#228]: https://github.com/matrix-org/matrix-rust-sdk/issues/228
//! [examples directory]: https://github.com/matrix-org/matrix-rust-sdk/tree/main/crates/matrix-sdk-appservice/examples

use std::{fmt::Debug, sync::Arc};

use axum::body::HttpBody;
use dashmap::DashMap;
pub use error::Error;
use event_handler::AppserviceFn;
pub use matrix_sdk;
#[doc(no_inline)]
pub use matrix_sdk::ruma;
use matrix_sdk::{config::RequestConfig, reqwest::Url, Client, ClientBuilder};
use ruma::{
    api::{
        appservice::{
            event::push_events,
            query::{query_room_alias::v1 as query_room, query_user_id::v1 as query_user},
        },
        client::{account::register, sync::sync_events},
    },
    assign,
    events::{room::member::MembershipState, AnyStateEvent, AnyTimelineEvent},
    DeviceId, OwnedRoomId, OwnedServerName,
};
use serde::Deserialize;
use thiserror::Error;
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

mod error;
pub mod event_handler;
pub mod registration;
pub mod user;
mod webserver;

pub use registration::AppServiceRegistration;
use registration::NamespaceCache;
pub use user::UserBuilder;
pub use webserver::AppServiceRouter;

pub type Result<T, E = Error> = std::result::Result<T, E>;

const USER_KEY: &[u8] = b"appservice.users.";
const USER_MEMBER: &[u8] = b"appservice.users.membership.";

type Localpart = String;

/// AppService
#[derive(Debug, Clone)]
pub struct AppService {
    homeserver_url: Url,
    server_name: OwnedServerName,
    registration: Arc<AppServiceRegistration>,
    namespaces: Arc<NamespaceCache>,
    clients: Arc<DashMap<Localpart, Client>>,
    event_handler: event_handler::EventHandler,
    default_request_config: Option<RequestConfig>,
}

/// Builder for an AppService
#[derive(Debug, Clone)]
pub struct AppServiceBuilder {
    homeserver_url: Url,
    server_name: OwnedServerName,
    registration: AppServiceRegistration,
    client_builder: Option<ClientBuilder>,
    default_request_config: Option<RequestConfig>,
}

impl AppServiceBuilder {
    /// Create a new AppService builder.
    ///
    /// # Arguments
    ///
    /// * `homeserver_url` - The homeserver that the client should connect to.
    /// * `server_name` - The server name to use when constructing user ids from
    ///   the localpart.
    /// * `registration` - The [AppService Registration] to use when interacting
    ///   with the homeserver.
    ///
    /// [AppService Registration]: https://matrix.org/docs/spec/application_service/r0.1.2#registration
    pub fn new(
        homeserver_url: Url,
        server_name: OwnedServerName,
        registration: AppServiceRegistration,
    ) -> Self {
        AppServiceBuilder {
            homeserver_url,
            server_name,
            registration,
            client_builder: None,
            default_request_config: None,
        }
    }

    /// Set the client builder to use for the appservice user.
    pub fn client_builder(mut self, client_builder: ClientBuilder) -> Self {
        self.client_builder = Some(client_builder);
        self
    }

    /// Set the default `[RequestConfig]` to use for appservice users.
    pub fn default_request_config(mut self, default_request_config: RequestConfig) -> Self {
        self.default_request_config = Some(default_request_config);
        self
    }

    /// Build the AppService.
    ///
    /// This will also construct an appservice [`user()`][AppService::user]
    /// for the `sender_localpart` of the given registration. This
    /// user can be used to register an event handler for all incoming
    /// events. Other appservice users only receive events if they're known to
    /// be a member of a room.
    pub async fn build(self) -> Result<AppService> {
        let homeserver_url = self.homeserver_url;
        let server_name = self.server_name;
        let registration = Arc::new(self.registration);
        let namespaces = Arc::new(NamespaceCache::from_registration(&registration)?);
        let clients = Arc::new(DashMap::new());
        let sender_localpart = registration.sender_localpart.clone();
        let event_handler = event_handler::EventHandler::default();
        let default_request_config = self.default_request_config;

        let appservice = AppService {
            homeserver_url,
            server_name,
            registration,
            namespaces,
            clients,
            event_handler,
            default_request_config,
        };
        if let Some(client_builder) = self.client_builder {
            appservice
                .user_builder(&sender_localpart)
                .client_builder(client_builder)
                .build()
                .await?;
        } else {
            appservice.user_builder(&sender_localpart).build().await?;
        }
        Ok(appservice)
    }
}

impl AppService {
    /// Create a new [`AppServiceBuilder`].
    pub fn builder(
        homeserver_url: Url,
        server_name: OwnedServerName,
        registration: AppServiceRegistration,
    ) -> AppServiceBuilder {
        AppServiceBuilder::new(homeserver_url, server_name, registration)
    }

    /// Create an appservice user client.
    ///
    /// Will create and return a client that's configured to [assert the
    /// identity] on outgoing homeserver requests that need authentication.
    ///
    /// This method is a singleton that saves the client internally for re-use
    /// based on the `localpart`. The cached client can be retrieved by calling
    /// this method again.
    ///
    /// Note that if you want to do actions like joining rooms with a
    /// user it needs to be registered first.
    /// [`register_user()`][Self::register_user] can be used
    /// for that purpose.
    ///
    /// # Arguments
    ///
    /// * `localpart` - Used for constructing the user accordingly. If `None` is
    ///   given it uses the `sender_localpart` from the registration.
    ///
    /// [registration]: https://matrix.org/docs/spec/application_service/r0.1.2#registration
    /// [assert the identity]: https://matrix.org/docs/spec/application_service/r0.1.2#identity-assertion
    pub async fn user(&self, localpart: Option<&str>) -> Result<Client> {
        let localpart = localpart.unwrap_or_else(|| self.registration.sender_localpart.as_ref());
        let builder = match self.default_request_config {
            Some(config) => self
                .user_builder(localpart)
                .client_builder(Client::builder().request_config(config)),
            None => self.user_builder(localpart),
        };
        builder.build().await
    }

    /// Same as [`user()`][Self::user] but with
    /// the ability to pass in a [`ClientBuilder`].
    ///
    /// Since this method is a singleton follow-up calls with different
    /// [`ClientBuilder`]s will be ignored.
    pub async fn user_with_client_builder(
        &self,
        localpart: Option<&str>,
        builder: ClientBuilder,
    ) -> Result<Client> {
        let localpart = localpart.unwrap_or_else(|| self.registration.sender_localpart.as_ref());
        self.user_builder(localpart).client_builder(builder).build().await
    }

    /// Create a new appservice user builder for the given `localpart`.
    pub fn user_builder<'a>(&'a self, localpart: &'a str) -> UserBuilder<'a> {
        UserBuilder::new(self, localpart)
    }

    /// Get the map containing all constructed appservice user clients.
    pub fn users(&self) -> Arc<DashMap<Localpart, Client>> {
        self.clients.clone()
    }

    /// Register a responder for queries about the existence of a user with a
    /// given mxid.
    ///
    /// See [GET /_matrix/app/v1/users/{userId}](https://matrix.org/docs/spec/application_service/r0.1.2#get-matrix-app-v1-users-userid).
    ///
    /// # Examples
    /// ```no_run
    /// # use matrix_sdk_appservice::AppService;
    /// # fn run(appservice: AppService) {
    /// appservice.register_user_query(Box::new(|appservice, req| {
    ///     Box::pin(async move {
    ///         println!("Got request for {}", req.user_id);
    ///         true
    ///     })
    /// }));
    /// # }
    /// ```
    pub async fn register_user_query(&self, handler: AppserviceFn<query_user::Request, bool>) {
        *self.event_handler.users.lock().await = Some(handler);
    }

    /// Register a responder for queries about the existence of a room with the
    /// given alias.
    ///
    /// See [GET /_matrix/app/v1/rooms/{roomAlias}](https://matrix.org/docs/spec/application_service/r0.1.2#get-matrix-app-v1-rooms-roomalias).
    ///
    /// # Examples
    /// ```no_run
    /// # use matrix_sdk_appservice::AppService;
    /// # fn run(appservice: AppService) {
    /// appservice.register_room_query(Box::new(|appservice, req| {
    ///     Box::pin(async move {
    ///         println!("Got request for {}", req.room_alias);
    ///         true
    ///     })
    /// }));
    /// # }
    /// ```
    pub async fn register_room_query(&self, handler: AppserviceFn<query_room::Request, bool>) {
        *self.event_handler.rooms.lock().await = Some(handler);
    }

    /// Register an appservice user by sending a [`register::v3::Request`] to
    /// the homeserver.
    ///
    /// # Arguments
    ///
    /// * `localpart` - The localpart of the user to register. Must be covered
    ///   by the namespaces in the registration in order to succeed.
    ///
    /// # Returns
    /// This function may return a UIAA response, which should be checked for
    /// with [`Error::as_uiaa_response()`].
    pub async fn register_user(&self, localpart: &str, device_id: Option<&DeviceId>) -> Result<()> {
        if self.is_user_registered(localpart).await? {
            return Ok(());
        }
        let request = assign!(register::v3::Request::new(), {
            username: Some(localpart.to_owned()),
            login_type: Some(register::LoginType::ApplicationService),
            device_id: device_id.map(ToOwned::to_owned),
        });

        let client = self.user(None).await?;
        client.register(request).await?;
        self.set_user_registered(localpart).await?;

        Ok(())
    }

    /// Add the given localpart to the database of registered localparts.
    async fn set_user_registered(&self, localpart: impl AsRef<str>) -> Result<()> {
        let client = self.user(None).await?;
        client
            .store()
            .set_custom_value(
                &[USER_KEY, localpart.as_ref().as_bytes()].concat(),
                vec![u8::from(true)],
            )
            .await?;
        Ok(())
    }

    /// Get whether a localpart is listed in the database as registered.
    async fn is_user_registered(&self, localpart: impl AsRef<str>) -> Result<bool> {
        let client = self.user(None).await?;
        let key = [USER_KEY, localpart.as_ref().as_bytes()].concat();
        let store = client.store().get_custom_value(&key).await?;
        let registered = store.is_some_and(|vec| vec.first().copied() == Some(u8::from(true)));
        Ok(registered)
    }

    /// Get the [`AppServiceRegistration`].
    pub fn registration(&self) -> &AppServiceRegistration {
        &self.registration
    }

    /// Compare the given `hs_token` against the registration's `hs_token`.
    ///
    /// Returns `true` if the tokens match, `false` otherwise.
    pub fn compare_hs_token(&self, hs_token: impl AsRef<str>) -> bool {
        self.registration.hs_token == hs_token.as_ref()
    }

    /// Check if given `user_id` is in any of the [`AppServiceRegistration`]'s
    /// `users` namespaces.
    pub fn user_id_is_in_namespace(&self, user_id: impl AsRef<str>) -> bool {
        let user_id = user_id.as_ref();
        self.namespaces.users.iter().any(|regex| regex.is_match(user_id))
    }

    /// Returns a [`Service`][tower::Service] that processes appservice
    /// requests.
    pub fn service<B>(&self) -> AppServiceRouter<B>
    where
        B: HttpBody + Send + 'static,
        B::Data: Send,
        B::Error: Into<axum::BoxError>,
    {
        webserver::router(self.clone())
    }

    /// Receive an incoming [transaction], pushing the contained events to
    /// active clients.
    ///
    /// [transaction]: https://spec.matrix.org/v1.2/application-service-api/#put_matrixappv1transactionstxnid
    async fn receive_transaction(&self, transaction: push_events::v1::Request) -> Result<()> {
        let sender_localpart_client = self.user(None).await?;

        // Find membership events affecting members in our namespace, and update
        // membership accordingly
        for raw_event in transaction.events.iter() {
            let res = raw_event.deserialize();
            let Ok(AnyTimelineEvent::State(AnyStateEvent::RoomMember(event))) = res else {
                continue;
            };
            if !self.user_id_is_in_namespace(event.state_key()) {
                continue;
            }
            let localpart = event.state_key().localpart();
            sender_localpart_client
                .store()
                .set_custom_value(
                    &[USER_MEMBER, event.room_id().as_bytes(), b".", localpart.as_bytes()].concat(),
                    event.membership().to_string().into_bytes(),
                )
                .await?;
        }

        /// Helper type for extracting the room id for an event
        #[derive(Debug, Deserialize)]
        struct EventRoomId {
            room_id: Option<OwnedRoomId>,
        }

        // Spawn a task for each client that constructs and pushes a sync event
        let mut tasks: Vec<JoinHandle<_>> = Vec::new();
        let transaction = Arc::new(transaction);
        for user_client in self.clients.iter() {
            let client = sender_localpart_client.clone();
            let user_client = user_client.clone();
            let transaction = transaction.clone();
            let sender_localpart = self.registration.sender_localpart.clone();

            let task = tokio::spawn(async move {
                let Some(user_id) = user_client.user_id() else {
                    // The client is not logged in, skipping
                    return Ok(());
                };
                let user_localpart = user_id.localpart();
                let mut response = sync_events::v3::Response::new(transaction.txn_id.to_string());

                // Clients expect events to be grouped per room, where the
                // group also denotes what the client's membership of the given
                // room is. We take all the events in the transaction and sort
                // them into appropriate groups.
                //
                // We special-case the `sender_localpart` user which receives all events and
                // by falling back to a membership of "join" if it's unknown.
                for raw_event in &transaction.events {
                    let Some(room_id) = raw_event.deserialize_as::<EventRoomId>()?.room_id else {
                        warn!("Transaction contained event with no ID");
                        continue;
                    };
                    let key = &[USER_MEMBER, room_id.as_bytes(), b".", user_localpart.as_bytes()]
                        .concat();
                    let membership = match client.store().get_custom_value(key).await? {
                        Some(value) => String::from_utf8(value).ok().map(MembershipState::from),
                        // Assume the `sender_localpart` user is in every known room
                        None if user_localpart == sender_localpart => Some(MembershipState::Join),
                        None => None,
                    };

                    match membership {
                        Some(MembershipState::Join) => {
                            let room = response.rooms.join.entry(room_id).or_default();
                            room.timeline.events.push(raw_event.clone().cast())
                        }
                        Some(MembershipState::Leave | MembershipState::Ban) => {
                            let room = response.rooms.leave.entry(room_id).or_default();
                            room.timeline.events.push(raw_event.clone().cast())
                        }
                        Some(MembershipState::Knock) => {
                            response.rooms.knock.entry(room_id).or_default();
                        }
                        Some(MembershipState::Invite) => {
                            response.rooms.invite.entry(room_id).or_default();
                        }
                        Some(unknown) => debug!("Unknown membership type: {unknown}"),
                        None => debug!("Assuming {user_localpart} is not in {room_id}"),
                    }
                }
                user_client.receive_transaction(&transaction.txn_id, response).await?;
                Ok::<_, Error>(())
            });

            tasks.push(task);
        }
        for task in tasks {
            if let Err(e) = task.await {
                warn!("Joining sync task failed: {e}");
            }
        }
        Ok(())
    }

    /// Convenience method that runs an http server.
    ///
    /// This is a blocking call that tries to listen on the provided host and
    /// port.
    pub async fn run(&self, host: impl Into<String>, port: impl Into<u16>) -> Result<()> {
        let host = host.into();
        let port = port.into();
        info!(host, port, "Starting AppService");

        webserver::run_server(self.clone(), host, port).await?;
        Ok(())
    }

    /// Set the default RequestConfig
    pub fn set_default_request_config(
        &mut self,
        request_config: Option<RequestConfig>,
    ) -> Result<()> {
        self.default_request_config = request_config;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::{
        future,
        sync::{Arc, Mutex},
    };

    use http::{Method, Request};
    use hyper::Body;
    use matrix_sdk::{
        config::RequestConfig,
        ruma::{api::appservice::Registration, events::room::member::OriginalSyncRoomMemberEvent},
        Client, RoomMemberships,
    };
    use matrix_sdk_test::{appservice::TransactionBuilder, async_test, TimelineTestEvent};
    use ruma::{
        api::{appservice::event::push_events, MatrixVersion},
        events::AnyTimelineEvent,
        room_id,
        serde::Raw,
    };
    use serde_json::json;
    use tower::{Service, ServiceExt};
    use wiremock::{
        matchers::{body_json, header, method, path},
        Mock, MockServer, ResponseTemplate,
    };

    use super::*;

    fn registration_string() -> String {
        include_str!("../tests/registration.yaml").to_owned()
    }

    async fn appservice(
        homeserver_url: Option<String>,
        registration: Option<Registration>,
    ) -> Result<AppService> {
        let _ = tracing_subscriber::fmt::try_init();

        let registration = match registration {
            Some(registration) => registration.into(),
            None => AppServiceRegistration::try_from_yaml_str(registration_string()).unwrap(),
        };

        let homeserver_url = homeserver_url.unwrap_or_else(|| "http://localhost:1234".to_owned());
        let server_name = "localhost";

        let client_builder = Client::builder()
            .request_config(RequestConfig::default().disable_retry())
            .server_versions([MatrixVersion::V1_0]);

        AppServiceBuilder::new(homeserver_url.parse()?, server_name.parse()?, registration)
            .client_builder(client_builder)
            .build()
            .await
    }

    #[async_test]
    async fn test_register_user() -> Result<()> {
        let server = MockServer::start().await;
        let appservice = appservice(Some(server.uri()), None).await?;

        let localpart = "someone";
        Mock::given(method("POST"))
            .and(path("/_matrix/client/r0/register"))
            .and(header(
                "authorization",
                format!("Bearer {}", appservice.registration().as_token).as_str(),
            ))
            .and(body_json(json!({
                "username": localpart.to_owned(),
                "type": "m.login.application_service"
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "access_token": "abc123",
                "device_id": "GHTYAJCE",
                "user_id": format!("@{localpart}:localhost"),
            })))
            .mount(&server)
            .await;

        appservice.register_user(localpart, None).await?;

        Ok(())
    }

    #[async_test]
    async fn test_put_transaction() -> Result<()> {
        let uri = "/_matrix/app/v1/transactions/1?access_token=hs_token";

        let mut transaction_builder = TransactionBuilder::new();
        transaction_builder.add_timeline_event(TimelineTestEvent::Member);
        let transaction = transaction_builder.build_transaction();

        let response = appservice(None, None)
            .await?
            .service()
            .oneshot(
                Request::builder()
                    .method(Method::PUT)
                    .uri(uri)
                    .body(Body::from(transaction))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), 200);

        Ok(())
    }

    #[async_test]
    async fn test_put_transaction_with_repeating_txn_id() -> Result<()> {
        let uri = "/_matrix/app/v1/transactions/1?access_token=hs_token";

        let mut transaction_builder = TransactionBuilder::new();
        transaction_builder.add_timeline_event(TimelineTestEvent::Member);
        let transaction = transaction_builder.build_transaction();

        let appservice = appservice(None, None).await?;

        #[allow(clippy::mutex_atomic)]
        let on_state_member = Arc::new(Mutex::new(false));
        appservice.user(None).await?.add_event_handler({
            let on_state_member = on_state_member.clone();
            move |_ev: OriginalSyncRoomMemberEvent| {
                *on_state_member.lock().unwrap() = true;
                future::ready(())
            }
        });

        let mut service = appservice.service();

        let response = service
            .call(
                Request::builder()
                    .method(Method::PUT)
                    .uri(uri)
                    .body(Body::from(transaction.clone()))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), 200);
        {
            let on_room_member_called = *on_state_member.lock().unwrap();
            assert!(on_room_member_called);
        }

        // Reset this to check that next time it doesnt get called
        {
            let mut on_room_member_called = on_state_member.lock().unwrap();
            *on_room_member_called = false;
        }

        let response = service
            .call(
                Request::builder()
                    .method(Method::PUT)
                    .uri(uri)
                    .body(Body::from(transaction))
                    .unwrap(),
            )
            .await
            .unwrap();

        // According to https://spec.matrix.org/v1.2/application-service-api/#pushing-events
        // This should noop and return 200.
        assert_eq!(response.status(), 200);
        {
            let on_room_member_called = *on_state_member.lock().unwrap();
            // This time we should not have called the event handler.
            assert!(!on_room_member_called);
        }

        Ok(())
    }

    #[async_test]
    async fn test_get_user() -> Result<()> {
        let appservice = appservice(None, None).await?;
        appservice.register_user_query(Box::new(|_, _| Box::pin(async move { true }))).await;

        let uri = "/_matrix/app/v1/users/%40_botty_1:dev.famedly.local?access_token=hs_token";

        let response = appservice
            .service()
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(response.status(), 200);

        Ok(())
    }

    #[async_test]
    async fn test_get_room() -> Result<()> {
        let appservice = appservice(None, None).await?;
        appservice.register_room_query(Box::new(|_, _| Box::pin(async move { true }))).await;

        let uri = "/_matrix/app/v1/rooms/%23magicforest:example.com?access_token=hs_token";

        let response = appservice
            .service()
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(response.status(), 200);

        Ok(())
    }

    #[async_test]
    async fn test_invalid_access_token() -> Result<()> {
        let uri = "/_matrix/app/v1/transactions/1?access_token=invalid_token";

        let mut transaction_builder = TransactionBuilder::new();
        let transaction =
            transaction_builder.add_timeline_event(TimelineTestEvent::Member).build_transaction();

        let appservice = appservice(None, None).await?;

        let response = appservice
            .service()
            .oneshot(
                Request::builder()
                    .method(Method::PUT)
                    .uri(uri)
                    .body(Body::from(transaction))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), 401);

        Ok(())
    }

    #[async_test]
    async fn test_no_access_token() -> Result<()> {
        let uri = "/_matrix/app/v1/transactions/1";

        let mut transaction_builder = TransactionBuilder::new();
        transaction_builder.add_timeline_event(TimelineTestEvent::Member);
        let transaction = transaction_builder.build_transaction();

        let appservice = appservice(None, None).await?;

        let response = appservice
            .service()
            .oneshot(
                Request::builder()
                    .method(Method::PUT)
                    .uri(uri)
                    .body(Body::from(transaction))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), 401);

        Ok(())
    }

    #[async_test]
    async fn test_event_handler() -> Result<()> {
        let appservice = appservice(None, None).await?;

        #[allow(clippy::mutex_atomic)]
        let on_state_member = Arc::new(Mutex::new(false));
        appservice.user(None).await?.add_event_handler({
            let on_state_member = on_state_member.clone();
            move |_ev: OriginalSyncRoomMemberEvent| {
                *on_state_member.lock().unwrap() = true;
                future::ready(())
            }
        });

        let uri = "/_matrix/app/v1/transactions/1?access_token=hs_token";

        let mut transaction_builder = TransactionBuilder::new();
        transaction_builder.add_timeline_event(TimelineTestEvent::Member);
        let transaction = transaction_builder.build_transaction();

        appservice
            .service()
            .oneshot(
                Request::builder()
                    .method(Method::PUT)
                    .uri(uri)
                    .body(Body::from(transaction))
                    .unwrap(),
            )
            .await
            .unwrap();

        let on_room_member_called = *on_state_member.lock().unwrap();
        assert!(on_room_member_called);

        Ok(())
    }

    #[async_test]
    async fn test_appservice_on_sub_path() -> Result<()> {
        let room_id = room_id!("!SVkFJHzfwvuaIEawgC:localhost");
        let uri_1 = "/sub_path/_matrix/app/v1/transactions/1?access_token=hs_token";
        let uri_2 = "/sub_path/_matrix/app/v1/transactions/2?access_token=hs_token";

        let mut transaction_builder = TransactionBuilder::new();
        transaction_builder.add_timeline_event(TimelineTestEvent::Member);
        let transaction_1 = transaction_builder.build_transaction();

        let mut transaction_builder = TransactionBuilder::new();
        transaction_builder.add_timeline_event(TimelineTestEvent::MemberNameChange);
        let transaction_2 = transaction_builder.build_transaction();

        let appservice = appservice(None, None).await?;
        let mut service = axum::Router::new().nest_service("/sub_path", appservice.service());

        service
            .call(
                Request::builder()
                    .method(Method::PUT)
                    .uri(uri_1)
                    .body(Body::from(transaction_1))?,
            )
            .await
            .unwrap();
        service
            .call(
                Request::builder()
                    .method(Method::PUT)
                    .uri(uri_2)
                    .body(Body::from(transaction_2))?,
            )
            .await
            .unwrap();

        let members = appservice
            .user(None)
            .await?
            .get_room(room_id)
            .expect("Expected room to be available")
            .members_no_sync(RoomMemberships::empty())
            .await?;

        assert_eq!(members[0].display_name().unwrap(), "changed");

        Ok(())
    }

    #[async_test]
    async fn test_receive_transaction() -> Result<()> {
        tracing_subscriber::fmt().try_init().ok();
        let json = vec![
            Raw::new(&json!({
                "content": {
                    "avatar_url": null,
                    "displayname": "Appservice",
                    "membership": "join"
                },
                "event_id": "$151800140479rdvjg:localhost",
                "membership": "join",
                "origin_server_ts": 151800140,
                "sender": "@_appservice:localhost",
                "state_key": "@_appservice:localhost",
                "type": "m.room.member",
                "room_id": "!coolplace:localhost",
                "unsigned": {
                    "age": 2970366
                }
            }))?
            .cast::<AnyTimelineEvent>(),
            Raw::new(&json!({
                "content": {
                    "avatar_url": null,
                    "displayname": "Appservice",
                    "membership": "join"
                },
                "event_id": "$151800140491rfbja:localhost",
                "membership": "join",
                "origin_server_ts": 151800140,
                "sender": "@_appservice:localhost",
                "state_key": "@_appservice:localhost",
                "type": "m.room.member",
                "room_id": "!boringplace:localhost",
                "unsigned": {
                    "age": 2970366
                }
            }))?
            .cast::<AnyTimelineEvent>(),
            Raw::new(&json!({
                "content": {
                    "avatar_url": null,
                    "displayname": "Alice",
                    "membership": "join"
                },
                "event_id": "$151800140517rfvjc:localhost",
                "membership": "join",
                "origin_server_ts": 151800140,
                "sender": "@_appservice_alice:localhost",
                "state_key": "@_appservice_alice:localhost",
                "type": "m.room.member",
                "room_id": "!coolplace:localhost",
                "unsigned": {
                    "age": 2970366
                }
            }))?
            .cast::<AnyTimelineEvent>(),
            Raw::new(&json!({
                "content": {
                    "avatar_url": null,
                    "displayname": "Bob",
                    "membership": "invite"
                },
                "event_id": "$151800140594rfvjc:localhost",
                "membership": "invite",
                "origin_server_ts": 151800174,
                "sender": "@_appservice_bob:localhost",
                "state_key": "@_appservice_bob:localhost",
                "type": "m.room.member",
                "room_id": "!boringplace:localhost",
                "unsigned": {
                    "age": 2970366
                }
            }))?
            .cast::<AnyTimelineEvent>(),
        ];
        let appservice = appservice(None, None).await?;

        let alice = appservice.user(Some("_appservice_alice")).await?;
        let bob = appservice.user(Some("_appservice_bob")).await?;
        appservice
            .receive_transaction(push_events::v1::Request::new("dontcare".into(), json))
            .await?;
        let coolplace = room_id!("!coolplace:localhost");
        let boringplace = room_id!("!boringplace:localhost");
        assert!(
            alice.get_joined_room(coolplace).is_some(),
            "Alice's membership in coolplace should be join"
        );
        assert!(
            bob.get_invited_room(boringplace).is_some(),
            "Bob's membership in boringplace should be invite"
        );
        assert!(alice.get_room(boringplace).is_none(), "Alice should not know about boringplace");
        assert!(bob.get_room(coolplace).is_none(), "Bob should not know about coolplace");
        Ok(())
    }

    mod registration {
        use ruma::api::appservice::Registration;

        use crate::{tests::registration_string, AppServiceRegistration, Result};

        #[test]
        fn test_registration() -> Result<()> {
            let registration: Registration = serde_yaml::from_str(&registration_string())?;
            let registration: AppServiceRegistration = registration.into();

            assert_eq!(registration.id, "appservice");

            Ok(())
        }

        #[test]
        fn test_registration_from_yaml_file() -> Result<()> {
            let registration =
                AppServiceRegistration::try_from_yaml_file("./tests/registration.yaml")?;

            assert_eq!(registration.id, "appservice");

            Ok(())
        }

        #[test]
        fn test_registration_from_yaml_str() -> Result<()> {
            let registration = AppServiceRegistration::try_from_yaml_str(registration_string())?;

            assert_eq!(registration.id, "appservice");

            Ok(())
        }
    }
}
