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
//! **Note:** Non-exclusive registration namespaces are not yet supported and
//! hence might lead to undefined behavior.
//!
//! # Quickstart
//!
//! ```no_run
//! # async {
//! #
//! use matrix_sdk_appservice::{
//!     ruma::events::room::member::SyncRoomMemberEvent,
//!     AppService, AppServiceRegistration
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
//!     ")?;
//!
//! let mut appservice = AppService::new(homeserver_url, server_name, registration).await?;
//! appservice
//!     .virtual_user(None)
//!     .await?
//!     .register_event_handler(|_ev: SyncRoomMemberEvent| async {
//!         // do stuff
//!     })
//!     .await;
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

use std::{convert::TryInto, sync::Arc};

use dashmap::DashMap;
pub use error::Error;
use event_handler::AppserviceFn;
pub use matrix_sdk;
#[doc(no_inline)]
pub use matrix_sdk::ruma;
use matrix_sdk::{bytes::Bytes, reqwest::Url, Client, ClientBuilder};
use ruma::{
    api::{
        appservice::{
            event::push_events,
            query::{query_room_alias::v1 as query_room, query_user_id::v1 as query_user},
        },
        client::{account::register, sync::sync_events},
    },
    assign,
    events::{room::member::MembershipState, AnyRoomEvent, AnyStateEvent},
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
pub const USER_MEMBER: &[u8] = b"appservice.users.membership.";

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
    /// appservice.register_user_query(Box::new(|appservice, req| Box::pin(async move {
    ///     println!("Got request for {}", req.user_id);
    ///     true
    /// })));
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
    /// appservice.register_room_query(Box::new(|appservice, req| Box::pin(async move {
    ///     println!("Got request for {}", req.room_alias);
    ///     true
    /// })));
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
    pub async fn receive_transaction(
        &self,
        transaction: push_events::v1::IncomingRequest,
    ) -> Result<()> {
        let sender_localpart_client = self.virtual_user(None).await?;

        // Find membership events affecting members in our namespace, and update
        // membership accordingly
        for event in transaction.events.iter() {
            let event = match event.deserialize() {
                Ok(AnyRoomEvent::State(AnyStateEvent::RoomMember(event))) => event,
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
                warn!("Joining sync task failed: {}", e);
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
        info!("Starting AppService on {}:{}", &host, &port);

        webserver::run_server(self.clone(), host, port).await?;
        Ok(())
    }
}

/// Ruma always expects the path to start with `/_matrix`, so we transform
/// accordingly. Handles [legacy routes] and appservice being located on a sub
/// path.
///
/// [legacy routes]: https://matrix.org/docs/spec/application_service/r0.1.2#legacy-routes
// TODO: consider ruma PR
pub(crate) fn transform_request_path(
    mut request: http::Request<Bytes>,
) -> Result<http::Request<Bytes>> {
    let uri = request.uri();
    // remove trailing slash from path
    let path = uri.path().trim_end_matches('/').to_owned();

    if !path.starts_with("/_matrix/app/v1/") {
        let path = match path {
            // special-case paths without value at the end
            _ if path.ends_with("/_matrix/app/unstable/thirdparty/user") => {
                "/_matrix/app/v1/thirdparty/user".to_owned()
            }
            _ if path.ends_with("/_matrix/app/unstable/thirdparty/location") => {
                "/_matrix/app/v1/thirdparty/location".to_owned()
            }
            // regular paths with values at the end
            _ => {
                let mut path = path.split('/').into_iter().rev();
                let value = match path.next() {
                    Some(value) => value,
                    None => return Err(Error::UriEmptyPath),
                };

                let mut path = match path.next() {
                    Some(path_segment)
                        if ["transactions", "users", "rooms"].contains(&path_segment) =>
                    {
                        format!("/_matrix/app/v1/{}/{}", path_segment, value)
                    }
                    Some(path_segment) => match path.next() {
                        Some(path_segment2) if path_segment2 == "thirdparty" => {
                            format!("/_matrix/app/v1/thirdparty/{}/{}", path_segment, value)
                        }
                        _ => return Err(Error::UriPathUnknown),
                    },
                    None => return Err(Error::UriEmptyPath),
                };

                if let Some(query) = uri.query() {
                    path.push('?');
                    path.push_str(query);
                }

                path
            }
        };

        let mut parts = uri.clone().into_parts();
        parts.path_and_query = Some(path.parse()?);

        let uri = parts.try_into().map_err(http::Error::from)?;
        *request.uri_mut() = uri;
    }

    Ok(request)
}
