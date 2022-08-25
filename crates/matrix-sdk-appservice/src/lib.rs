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
//! - [x] allow calling the homeserver with proper virtual user identity
//!   assertion
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
//! for the access tokens and because membership states for virtual users are
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
//! let registration = AppServiceRegistration::try_from_yaml_str(
//!     r"
//!         id: appservice
//!         url: http://127.0.0.1:9009
//!         as_token: as_token
//!         hs_token: hs_token
//!         sender_localpart: _appservice
//!         namespaces:
//!           users:
//!           - exclusive: true
//!             regex: '@_appservice_.*'
//!     ",
//! )?;
//!
//! let mut appservice =
//!     AppService::new(homeserver_url, server_name, registration).await?;
//! appservice.virtual_user(None).await?.add_event_handler(
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

use std::sync::Arc;

use dashmap::DashMap;
pub use error::Error;
use event_handler::AppserviceFn;
pub use matrix_sdk;
#[doc(no_inline)]
pub use matrix_sdk::ruma;
use matrix_sdk::{reqwest::Url, Client, ClientBuilder};
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
    DeviceId, IdParseError, OwnedRoomId, OwnedServerName,
};
use serde::Deserialize;
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

mod error;
pub mod event_handler;
pub mod registration;
pub mod virtual_user;
mod webserver;

pub use registration::AppServiceRegistration;
use registration::NamespaceCache;
pub use virtual_user::VirtualUserBuilder;

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
}

impl AppService {
    /// Create a new AppService.
    ///
    /// This will also construct a [`virtual_user()`][Self::virtual_user] for
    /// the `sender_localpart` of the given registration. This virtual user can
    /// be used to register an event handler for all incoming events. Other
    /// virtual users only receive events if they're known to be a member of a
    /// room.
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
    pub async fn new(
        homeserver_url: impl TryInto<Url, Error = url::ParseError>,
        server_name: impl TryInto<OwnedServerName, Error = IdParseError>,
        registration: AppServiceRegistration,
    ) -> Result<Self> {
        let appservice =
            Self::with_client_builder(homeserver_url, server_name, registration, Client::builder())
                .await?;

        Ok(appservice)
    }

    /// Same as [`new()`][Self::new] but lets you provide a [`ClientBuilder`]
    /// for the virtual user that gets constructed for the `sender_localpart`.
    pub async fn with_client_builder(
        homeserver_url: impl TryInto<Url, Error = url::ParseError>,
        server_name: impl TryInto<OwnedServerName, Error = IdParseError>,
        registration: AppServiceRegistration,
        builder: ClientBuilder,
    ) -> Result<Self> {
        let homeserver_url = homeserver_url.try_into()?;
        let server_name = server_name.try_into()?;
        let registration = Arc::new(registration);
        let namespaces = Arc::new(NamespaceCache::from_registration(&registration)?);
        let clients = Arc::new(DashMap::new());
        let sender_localpart = registration.sender_localpart.clone();
        let event_handler = event_handler::EventHandler::default();

        let appservice = AppService {
            homeserver_url,
            server_name,
            registration,
            namespaces,
            clients,
            event_handler,
        };

        appservice.virtual_user_builder(&sender_localpart).client_builder(builder).build().await?;

        Ok(appservice)
    }

    /// Create a virtual user client.
    ///
    /// Will create and return a client that's configured to [assert the
    /// identity] on outgoing homeserver requests that need authentication.
    ///
    /// This method is a singleton that saves the client internally for re-use
    /// based on the `localpart`. The cached client can be retrieved by calling
    /// this method again.
    ///
    /// Note that if you want to do actions like joining rooms with a virtual
    /// user it needs to be registered first.
    /// [`register_virtual_user()`][Self::register_virtual_user] can be used
    /// for that purpose.
    ///
    /// # Arguments
    ///
    /// * `localpart` - Used for constructing the virtual user accordingly. If
    ///   `None` is given it uses the `sender_localpart` from the registration.
    ///
    /// [registration]: https://matrix.org/docs/spec/application_service/r0.1.2#registration
    /// [assert the identity]: https://matrix.org/docs/spec/application_service/r0.1.2#identity-assertion
    pub async fn virtual_user(&self, localpart: Option<&str>) -> Result<Client> {
        let localpart = localpart.unwrap_or_else(|| self.registration.sender_localpart.as_ref());
        self.virtual_user_builder(localpart).build().await
    }

    /// Same as [`virtual_user()`][Self::virtual_user] but with
    /// the ability to pass in a [`ClientBuilder`].
    ///
    /// Since this method is a singleton follow-up calls with different
    /// [`ClientBuilder`]s will be ignored.
    pub async fn virtual_user_with_client_builder(
        &self,
        localpart: Option<&str>,
        builder: ClientBuilder,
    ) -> Result<Client> {
        let localpart = localpart.unwrap_or_else(|| self.registration.sender_localpart.as_ref());
        self.virtual_user_builder(localpart).client_builder(builder).build().await
    }

    /// Create a new virtual user builder for the given `localpart`.
    pub fn virtual_user_builder<'a>(&'a self, localpart: &'a str) -> VirtualUserBuilder<'a> {
        VirtualUserBuilder::new(self, localpart)
    }

    /// Get the map containing all constructed virtual user clients.
    pub fn virtual_users(&self) -> Arc<DashMap<Localpart, Client>> {
        self.clients.clone()
    }

    /// Register a responder for queries about the existence of a user with a
    /// given mxid.
    ///
    /// See [GET /_matrix/app/v1/users/{userId}](https://matrix.org/docs/spec/application_service/r0.1.2#get-matrix-app-v1-users-userid).
    ///
    /// # Example
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
    pub async fn register_user_query(
        &self,
        handler: AppserviceFn<query_user::IncomingRequest, bool>,
    ) {
        *self.event_handler.users.lock().await = Some(handler);
    }

    /// Register a responder for queries about the existence of a room with the
    /// given alias.
    ///
    /// See [GET /_matrix/app/v1/rooms/{roomAlias}](https://matrix.org/docs/spec/application_service/r0.1.2#get-matrix-app-v1-rooms-roomalias).
    ///
    /// # Example
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
    pub async fn register_room_query(
        &self,
        handler: AppserviceFn<query_room::IncomingRequest, bool>,
    ) {
        *self.event_handler.rooms.lock().await = Some(handler);
    }

    /// Register a virtual user by sending a [`register::v3::Request`] to the
    /// homeserver.
    ///
    /// # Arguments
    ///
    /// * `localpart` - The localpart of the user to register. Must be covered
    ///   by the namespaces in the registration in order to succeed.
    ///
    /// # Returns
    /// This function may return a UIAA response, which should be checked for
    /// with [`Error::uiaa_response()`].
    pub async fn register_virtual_user<'a>(
        &self,
        localpart: &'a str,
        device_id: Option<&'a DeviceId>,
    ) -> Result<()> {
        if self.is_user_registered(localpart).await? {
            return Ok(());
        }
        let request = assign!(register::v3::Request::new(), {
            username: Some(localpart),
            login_type: Some(&register::LoginType::ApplicationService),
            device_id,
        });

        let client = self.virtual_user(None).await?;
        client.register(request).await?;
        self.set_user_registered(localpart).await?;

        Ok(())
    }

    /// Add the given localpart to the database of registered localparts.
    async fn set_user_registered(&self, localpart: impl AsRef<str>) -> Result<()> {
        let client = self.virtual_user(None).await?;
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
        let client = self.virtual_user(None).await?;
        let key = [USER_KEY, localpart.as_ref().as_bytes()].concat();
        let store = client.store().get_custom_value(&key).await?;
        let registered =
            store.and_then(|vec| vec.first().copied()).map_or(false, |b| b == u8::from(true));
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
    /// `users` namespaces
    pub fn user_id_is_in_namespace(&self, user_id: impl AsRef<str>) -> bool {
        for regex in &self.namespaces.users {
            if regex.is_match(user_id.as_ref()) {
                return true;
            }
        }

        false
    }

    /// Returns a [`warp::Filter`] to be used as [`warp::serve()`] route.
    ///
    /// Note that if you handle any of the [application-service-specific
    /// routes], including the legacy routes, you will break the appservice
    /// functionality.
    ///
    /// Hint: [`warp::Filter`]s can be converted to an `hyper::Service` using
    /// [`warp::service`], which allows using it with tower-compatible
    /// frameworks such as axum.
    ///
    /// [application-service-specific routes]: https://spec.matrix.org/unstable/application-service-api/#legacy-routes
    pub fn warp_filter(&self) -> warp::filters::BoxedFilter<(impl warp::Reply,)> {
        webserver::warp_filter(self.clone())
    }

    /// Receive an incoming [transaction], pushing the contained events to
    /// active virtual clients.
    ///
    /// [transaction]: https://spec.matrix.org/v1.2/application-service-api/#put_matrixappv1transactionstxnid
    async fn receive_transaction(
        &self,
        transaction: push_events::v1::IncomingRequest,
    ) -> Result<()> {
        let sender_localpart_client = self.virtual_user(None).await?;

        // Find membership events affecting members in our namespace, and update
        // membership accordingly
        for event in transaction.events.iter() {
            let event = match event.deserialize() {
                Ok(AnyTimelineEvent::State(AnyStateEvent::RoomMember(event))) => event,
                _ => continue,
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
        for virtual_user_client in self.clients.iter() {
            let client = sender_localpart_client.clone();
            let virtual_user_client = virtual_user_client.clone();
            let transaction = transaction.clone();
            let sender_localpart = self.registration.sender_localpart.clone();

            let task = tokio::spawn(async move {
                let virtual_user_localpart = match virtual_user_client.user_id() {
                    Some(user_id) => user_id.localpart(),
                    // The client is not logged in, skipping
                    None => return Ok(()),
                };
                let mut response = sync_events::v3::Response::new(transaction.txn_id.to_string());

                // Clients expect events to be grouped per room, where the
                // group also denotes what the client's membership of the given
                // room is. We take all the events in the transaction and sort
                // them into appropriate groups.
                //
                // We special-case the `sender_localpart` user which receives all events and
                // by falling back to a membership of "join" if it's unknown.
                for raw_event in &transaction.events {
                    let room_id = match raw_event.deserialize_as::<EventRoomId>()?.room_id {
                        Some(room_id) => room_id,
                        None => {
                            warn!("Transaction contained event with no ID");
                            continue;
                        }
                    };
                    let key =
                        &[USER_MEMBER, room_id.as_bytes(), b".", virtual_user_localpart.as_bytes()]
                            .concat();
                    let membership = match client.store().get_custom_value(key).await? {
                        Some(value) => String::from_utf8(value).ok().map(MembershipState::from),
                        // Assume the `sender_localpart` user is in every known room
                        None if virtual_user_localpart == sender_localpart => {
                            Some(MembershipState::Join)
                        }
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
                        None => debug!("Assuming {virtual_user_localpart} is not in {room_id}"),
                    }
                }
                virtual_user_client.receive_transaction(&transaction.txn_id, response).await?;
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
}

#[cfg(test)]
mod tests {
    use std::{
        future,
        sync::{Arc, Mutex},
    };

    use matrix_sdk::{
        config::RequestConfig,
        ruma::{api::appservice::Registration, events::room::member::OriginalSyncRoomMemberEvent},
        Client,
    };
    use matrix_sdk_test::{appservice::TransactionBuilder, async_test, TimelineTestEvent};
    use ruma::{
        api::{appservice::event::push_events, MatrixVersion},
        events::AnyTimelineEvent,
        room_id,
        serde::Raw,
    };
    use serde_json::json;
    use warp::{Filter, Reply};
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

        AppService::with_client_builder(
            homeserver_url.as_ref(),
            server_name,
            registration,
            client_builder,
        )
        .await
    }

    #[async_test]
    async fn test_register_virtual_user() -> Result<()> {
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

        appservice.register_virtual_user(localpart, None).await?;

        Ok(())
    }

    #[async_test]
    async fn test_put_transaction() -> Result<()> {
        let uri = "/_matrix/app/v1/transactions/1?access_token=hs_token";

        let mut transaction_builder = TransactionBuilder::new();
        transaction_builder.add_timeline_event(TimelineTestEvent::Member);
        let transaction = transaction_builder.build_json_transaction();

        let appservice = appservice(None, None).await?;

        let status = warp::test::request()
            .method("PUT")
            .path(uri)
            .json(&transaction)
            .filter(&appservice.warp_filter())
            .await
            .unwrap()
            .into_response()
            .status();

        assert_eq!(status, 200);

        Ok(())
    }

    #[async_test]
    async fn test_put_transaction_with_repeating_txn_id() -> Result<()> {
        let uri = "/_matrix/app/v1/transactions/1?access_token=hs_token";

        let mut transaction_builder = TransactionBuilder::new();
        transaction_builder.add_timeline_event(TimelineTestEvent::Member);
        let transaction = transaction_builder.build_json_transaction();

        let appservice = appservice(None, None).await?;

        #[allow(clippy::mutex_atomic)]
        let on_state_member = Arc::new(Mutex::new(false));
        appservice.virtual_user(None).await?.add_event_handler({
            let on_state_member = on_state_member.clone();
            move |_ev: OriginalSyncRoomMemberEvent| {
                *on_state_member.lock().unwrap() = true;
                future::ready(())
            }
        });

        let status = warp::test::request()
            .method("PUT")
            .path(uri)
            .json(&transaction)
            .filter(&appservice.warp_filter())
            .await
            .unwrap()
            .into_response()
            .status();

        assert_eq!(status, 200);
        {
            let on_room_member_called = *on_state_member.lock().unwrap();
            assert!(on_room_member_called);
        }

        // Reset this to check that next time it doesnt get called
        {
            let mut on_room_member_called = on_state_member.lock().unwrap();
            *on_room_member_called = false;
        }

        let status = warp::test::request()
            .method("PUT")
            .path(uri)
            .json(&transaction)
            .filter(&appservice.warp_filter())
            .await
            .unwrap()
            .into_response()
            .status();

        // According to https://spec.matrix.org/v1.2/application-service-api/#pushing-events
        // This should noop and return 200.
        assert_eq!(status, 200);
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

        let uri = "/_matrix/app/v1/users/%40_botty_1%3Adev.famedly.local?access_token=hs_token";

        let status = warp::test::request()
            .method("GET")
            .path(uri)
            .filter(&appservice.warp_filter())
            .await
            .unwrap()
            .into_response()
            .status();

        assert_eq!(status, 200);

        Ok(())
    }

    #[async_test]
    async fn test_get_room() -> Result<()> {
        let appservice = appservice(None, None).await?;
        appservice.register_room_query(Box::new(|_, _| Box::pin(async move { true }))).await;

        let uri = "/_matrix/app/v1/rooms/%23magicforest%3Aexample.com?access_token=hs_token";

        let status = warp::test::request()
            .method("GET")
            .path(uri)
            .filter(&appservice.warp_filter())
            .await
            .unwrap()
            .into_response()
            .status();

        assert_eq!(status, 200);

        Ok(())
    }

    #[async_test]
    async fn test_invalid_access_token() -> Result<()> {
        let uri = "/_matrix/app/v1/transactions/1?access_token=invalid_token";

        let mut transaction_builder = TransactionBuilder::new();
        let transaction = transaction_builder
            .add_timeline_event(TimelineTestEvent::Member)
            .build_json_transaction();

        let appservice = appservice(None, None).await?;

        let status = warp::test::request()
            .method("PUT")
            .path(uri)
            .json(&transaction)
            .filter(&appservice.warp_filter())
            .await
            .unwrap()
            .into_response()
            .status();

        assert_eq!(status, 401);

        Ok(())
    }

    #[async_test]
    async fn test_no_access_token() -> Result<()> {
        let uri = "/_matrix/app/v1/transactions/1";

        let mut transaction_builder = TransactionBuilder::new();
        transaction_builder.add_timeline_event(TimelineTestEvent::Member);
        let transaction = transaction_builder.build_json_transaction();

        let appservice = appservice(None, None).await?;

        {
            let status = warp::test::request()
                .method("PUT")
                .path(uri)
                .json(&transaction)
                .filter(&appservice.warp_filter())
                .await
                .unwrap()
                .into_response()
                .status();

            assert_eq!(status, 401);
        }

        Ok(())
    }

    #[async_test]
    async fn test_event_handler() -> Result<()> {
        let appservice = appservice(None, None).await?;

        #[allow(clippy::mutex_atomic)]
        let on_state_member = Arc::new(Mutex::new(false));
        appservice.virtual_user(None).await?.add_event_handler({
            let on_state_member = on_state_member.clone();
            move |_ev: OriginalSyncRoomMemberEvent| {
                *on_state_member.lock().unwrap() = true;
                future::ready(())
            }
        });

        let uri = "/_matrix/app/v1/transactions/1?access_token=hs_token";

        let mut transaction_builder = TransactionBuilder::new();
        transaction_builder.add_timeline_event(TimelineTestEvent::Member);
        let transaction = transaction_builder.build_json_transaction();

        warp::test::request()
            .method("PUT")
            .path(uri)
            .json(&transaction)
            .filter(&appservice.warp_filter())
            .await
            .unwrap();

        let on_room_member_called = *on_state_member.lock().unwrap();
        assert!(on_room_member_called);

        Ok(())
    }

    #[async_test]
    async fn test_unrelated_path() -> Result<()> {
        let appservice = appservice(None, None).await?;

        let status = {
            let consumer_filter = warp::any()
                .and(appservice.warp_filter())
                .or(warp::get().and(warp::path("unrelated").map(warp::reply)));

            let response = warp::test::request()
                .method("GET")
                .path("/unrelated")
                .filter(&consumer_filter)
                .await?
                .into_response();

            response.status()
        };

        assert_eq!(status, 200);

        Ok(())
    }

    #[async_test]
    async fn test_appservice_on_sub_path() -> Result<()> {
        let room_id = room_id!("!SVkFJHzfwvuaIEawgC:localhost");
        let uri_1 = "/sub_path/_matrix/app/v1/transactions/1?access_token=hs_token";
        let uri_2 = "/sub_path/_matrix/app/v1/transactions/2?access_token=hs_token";

        let mut transaction_builder = TransactionBuilder::new();
        transaction_builder.add_timeline_event(TimelineTestEvent::Member);
        let transaction_1 = transaction_builder.build_json_transaction();

        let mut transaction_builder = TransactionBuilder::new();
        transaction_builder.add_timeline_event(TimelineTestEvent::MemberNameChange);
        let transaction_2 = transaction_builder.build_json_transaction();

        let appservice = appservice(None, None).await?;

        {
            warp::test::request()
                .method("PUT")
                .path(uri_1)
                .json(&transaction_1)
                .filter(&warp::path("sub_path").and(appservice.warp_filter()))
                .await?;

            warp::test::request()
                .method("PUT")
                .path(uri_2)
                .json(&transaction_2)
                .filter(&warp::path("sub_path").and(appservice.warp_filter()))
                .await?;
        };

        let members = appservice
            .virtual_user(None)
            .await?
            .get_room(room_id)
            .expect("Expected room to be available")
            .members_no_sync()
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

        let alice = appservice.virtual_user(Some("_appservice_alice")).await?;
        let bob = appservice.virtual_user(Some("_appservice_bob")).await?;
        appservice
            .receive_transaction(push_events::v1::IncomingRequest::new("dontcare".into(), json))
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
        use super::*;

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
