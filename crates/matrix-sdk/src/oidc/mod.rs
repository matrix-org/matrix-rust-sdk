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

//! High-level OpenID Connect API using the Authorization Code flow.
//!
//! The OpenID Connect interactions with the Matrix API are currently a
//! work-in-progress and are defined by [MSC3861] and its sub-proposals. And
//! more documentation is available at [areweoidcyet.com].
//!
//! # OpenID Connect specification compliance
//!
//! The OpenID Connect client used in the SDK has not been certified for
//! compliance against the OpenID Connect specification.
//!
//! It implements only parts of the specification and is currently
//! limited to the Authorization Code flow. It also uses some OAuth 2.0
//! extensions.
//!
//! # Setup
//!
//! To enable support for OpenID Connect on the [`Client`], simply enable the
//! `experimental-oidc` cargo feature for the `matrix-sdk` crate. Then this
//! authentication API is available with [`Client::oidc()`].
//!
//! # Homeserver support
//!
//! It is recommended to build the Client with [`Client::builder()`] and then to
//! enable auto-discovery by calling [`ClientBuilder::server_name()`]. That will
//! allow to discover the OpenID Connect Provider that is advertised by the
//! homeserver. After building the client, you can check that the homeserver
//! supports logging in via OIDC when [`Oidc::authentication_server_info()`]
//! is set.
//!
//! If the homeserver doesn't advertise its support for OIDC, but the issuer URL
//! is known by some other method, it can be provided manually during
//! registration.
//!
//! # Registration
//!
//! Registration is only required the first time a client encounters an issuer.
//!
//! If the issuer supports dynamic registration, it can be done by using
//! [`Oidc::register_client()`]. If dynamic registration is not available, the
//! homeserver should document how to obtain client credentials.
//!
//! To make the client aware of being registered successfully,
//! [`Oidc::restore_registered_client()`] needs to be called next.
//!
//! After client registration, the client credentials should be persisted and
//! reused for every session that interacts with that same issuer.
//!
//! # Login
//!
//! Before logging in, make sure to call [`Oidc::restore_registered_client()`]
//! (even after registering the client), as it is the first step to know how to
//! interact with the issuer.
//!
//! With OIDC, logging into a Matrix account is simply logging in with a
//! predefined scope, part of it declaring the device ID of the session.
//!
//! [`Oidc::login()`] constructs an [`OidcAuthCodeUrlBuilder`] that can be
//! configured, and then calling [`OidcAuthCodeUrlBuilder::build()`] will
//! provide the URL to present to the user in a web browser. After
//! authenticating with the OIDC provider, the user will be redirected to the
//! provided redirect URI, with a code in the query that will allow to finish
//! the authorization process by calling [`Oidc::finish_authorization()`].
//!
//! # Persisting/restoring a session
//!
//! A full OIDC session requires two parts:
//!
//! - The client credentials obtained after client registration with the
//!   corresponding client metadata,
//! - The user session obtained after login.
//!
//! Both parts are usually stored separately because the client credentials can
//! be reused for any session with the same issuer, while the user session is
//! unique.
//!
//! _Note_ that the type returned by [`Oidc::full_session()`] is not
//! (de)serializable. This is due to some client credentials methods that
//! require a function to generate a JWT. The types of some fields can still be
//! (de)serialized.
//!
//! To restore a previous session, use [`Oidc::restore_session()`].
//!
//! # Refresh tokens
//!
//! The use of refresh tokens with OpenID Connect Providers is more common than
//! in the Matrix specification. For this reason, it is recommended to configure
//! the client with [`ClientBuilder::handle_refresh_tokens()`], to handle
//! refreshing tokens automatically.
//!
//! Applications should then listen to session tokens changes after logging in
//! with [`Oidc::session_tokens_stream()`] to be able to restore the session at
//! a later time, otherwise the end-user will need to login again.
//!
//! # Unknown token error
//!
//! A request to the Matrix API can return an [`Error`] with an
//! [`ErrorKind::UnknownToken`].
//!
//! The first step is to try to refresh the token with
//! [`Oidc::refresh_access_token()`]. This step is done automatically if the
//! client was built with [`ClientBuilder::handle_refresh_tokens()`].
//!
//! If refreshing the access token fails, the next step is to try to request a
//! new login authorization with [`Oidc::login()`], using the device ID from the
//! session. _Note_ that in this case [`Oidc::finish_login()`] must NOT be
//! called after [`Oidc::finish_authorization()`].
//!
//! If this fails again, the client should assume to be logged out, and all
//! local data should be erased.
//!
//! # Insufficient scope error
//!
//! _Note: This is not fully specced yet._
//!
//! Some API calls that deal with sensitive data require more privileges than
//! others. In the current Matrix specification, those endpoints use the
//! User-Interactive Authentication API.
//!
//! The OAuth 2.0 specification has the concept of scopes, and an access token
//! is limited to a given scope when it is generated. Accessing those endpoints
//! require a privileged scope, so a new authorization request is necessary to
//! get a new access token.
//!
//! When the API endpoint returns an [`Error`] with an
//! [`AuthenticateError::InsufficientScope`], the [`Oidc::authorize_scope()`]
//! method can be used to authorize the required scope. It works just like
//! [`Oidc::login()`].
//!
//! # Account management.
//!
//! The homeserver might advertise a URL that allows the user to manage their
//! account, alongside the issuer in the [`Oidc::authentication_server_info()`]
//! obtained during registration.
//!
//! # Logout
//!
//! To log the [`Client`] out of the session, simply call [`Oidc::logout()`].
//!
//! # Examples
//!
//! Most methods have examples, there is also an example CLI application that
//! supports all the actions described here, in [`examples/oidc-cli`].
//!
//! [MSC3861]: https://github.com/matrix-org/matrix-spec-proposals/pull/3861
//! [areweoidcyet.com]: https://areweoidcyet.com/
//! [`ClientBuilder::server_name()`]: crate::ClientBuilder::server_name()
//! [`ClientBuilder::handle_refresh_tokens()`]: crate::ClientBuilder::handle_refresh_tokens()
//! [`Error`]: ruma::api::client::error::Error
//! [`ErrorKind::UnknownToken`]: ruma::api::client::error::ErrorKind::UnknownToken
//! [`AuthenticateError::InsufficientScope`]: ruma::api::client::error::AuthenticateError
//! [`examples/oidc-cli`]: https://github.com/matrix-org/matrix-rust-sdk/tree/main/examples/oidc-cli

use std::{
    collections::{hash_map::DefaultHasher, HashMap},
    fmt,
    hash::{Hash, Hasher},
};

use chrono::Utc;
use eyeball::SharedObservable;
use futures_core::Stream;
pub use mas_oidc_client::{error, types};
use mas_oidc_client::{
    http_service::HttpService,
    jose::jwk::PublicJsonWebKeySet,
    requests::{
        authorization_code::{access_token_with_authorization_code, AuthorizationValidationData},
        discovery::discover,
        jose::{fetch_jwks, JwtVerificationData},
        refresh_token::refresh_access_token,
        registration::register_client,
        revocation::revoke_token,
    },
    types::{
        client_credentials::ClientCredentials,
        errors::ClientError,
        iana::oauth::OAuthTokenTypeHint,
        oidc::VerifiedProviderMetadata,
        registration::{ClientRegistrationResponse, VerifiedClientMetadata},
        requests::AccessTokenResponse,
        scope::{MatrixApiScopeToken, Scope, ScopeToken},
        IdToken,
    },
};
use matrix_sdk_base::{once_cell::sync::OnceCell, SessionMeta};
use rand::{rngs::StdRng, Rng, SeedableRng};
use ruma::api::client::discovery::discover_homeserver::AuthenticationServerInfo;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::Mutex;
use url::Url;

mod auth_code_builder;
mod data_serde;
mod end_session_builder;

pub use self::{
    auth_code_builder::{OidcAuthCodeUrlBuilder, OidcAuthorizationData},
    end_session_builder::{OidcEndSessionData, OidcEndSessionUrlBuilder},
};
use crate::{authentication::AuthData, client::SessionChange, Client, RefreshTokenError, Result};

pub(crate) struct OidcAuthData {
    pub(crate) issuer_info: AuthenticationServerInfo,
    pub(crate) credentials: ClientCredentials,
    pub(crate) metadata: VerifiedClientMetadata,
    pub(crate) tokens: OnceCell<SharedObservable<SessionTokens>>,
    /// The data necessary to validate authorization responses.
    pub(crate) authorization_data: Mutex<HashMap<String, AuthorizationValidationData>>,
}

impl fmt::Debug for OidcAuthData {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OidcAuthData")
            .field("issuer_info", &self.issuer_info)
            .finish_non_exhaustive()
    }
}

/// A high-level authentication API to interact with an OpenID Connect Provider.
#[derive(Debug, Clone)]
pub struct Oidc {
    /// The underlying Matrix API client.
    client: Client,
}

impl Oidc {
    pub(crate) fn new(client: Client) -> Self {
        Self { client }
    }

    /// Get the `HttpService` to make requests with.
    fn http_service(&self) -> HttpService {
        HttpService::new(self.client.inner.http_client.clone())
    }

    /// The OpenID Connect authentication data.
    ///
    /// Returns `None` if the client's registration was not restored yet.
    fn data(&self) -> Option<&OidcAuthData> {
        match self.client.inner.auth_data.get()? {
            AuthData::Oidc(data) => Some(data),
            _ => None,
        }
    }

    /// The authentication server info discovered from the homeserver.
    ///
    /// This will only be set if the homeserver supports authenticating via
    /// OpenID Connect ([MSC3861]) and this `Client` was constructed using
    /// auto-discovery by setting the homeserver with
    /// [`ClientBuilder::server_name()`].
    ///
    /// [MSC3861]: https://github.com/matrix-org/matrix-spec-proposals/pull/3861
    /// [`ClientBuilder::server_name()`]: crate::ClientBuilder::server_name()
    pub fn authentication_server_info(&self) -> Option<&AuthenticationServerInfo> {
        self.client.inner.auth_ctx.authentication_server_info.as_ref()
    }

    /// The OpenID Connect Provider used for authorization.
    ///
    /// Returns `None` if the client registration was not restored with
    /// [`Oidc::restore_registered_client()`] or
    /// [`Oidc::restore_session()`].
    pub fn issuer(&self) -> Option<&str> {
        self.data().map(|data| data.issuer_info.issuer.as_str())
    }

    /// The URL where the user can manage their account.
    ///
    /// Returns `Ok(None)` if the client registration was not restored with
    /// [`Oidc::restore_registered_client()`] or
    /// [`Oidc::restore_session()`], or if the homeserver doesn't advertise this
    /// URL. Returns an error if the URL could not be parsed
    pub fn account_management_url(&self) -> Result<Option<Url>, url::ParseError> {
        let Some(data) = self.data() else {
            return Ok(None);
        };
        let Some(account) = data.issuer_info.account.as_deref() else {
            return Ok(None);
        };

        let mut url = Url::parse(account)?;

        if let Some(id_token) = self.session_tokens().and_then(|t| t.latest_id_token) {
            url.query_pairs_mut().append_pair("id_token_hint", id_token.as_str());
        }

        Ok(Some(url))
    }

    /// Fetch the OpenID Connect metadata of the given issuer.
    ///
    /// Returns an error if fetching the metadata failed.
    pub async fn given_provider_metadata(
        &self,
        issuer: &str,
    ) -> Result<VerifiedProviderMetadata, OidcError> {
        discover(&self.http_service(), issuer).await.map_err(Into::into)
    }

    /// Fetch the OpenID Connect metadata of the issuer.
    ///
    /// Returns an error if the client registration was not restored, or if an
    /// error occurred when fetching the metadata.
    pub async fn provider_metadata(&self) -> Result<VerifiedProviderMetadata, OidcError> {
        let issuer = self.issuer().ok_or(OidcError::MissingAuthenticationIssuer)?;

        self.given_provider_metadata(issuer).await
    }

    /// Fetch the OpenID Connect JSON Web Key Set at the given URI.
    ///
    /// Returns an error if the client registration was not restored, or if an
    /// error occurred when fetching the data.
    async fn fetch_jwks(&self, jwks_uri: &Url) -> Result<PublicJsonWebKeySet, OidcError> {
        Ok(fetch_jwks(&self.http_service(), jwks_uri).await?)
    }

    /// The OpenID Connect metadata of this client used during registration.
    ///
    /// Returns `None` if the client registration was not restored with
    /// [`Oidc::restore_registered_client()`] or
    /// [`Oidc::restore_session()`].
    pub fn client_metadata(&self) -> Option<&VerifiedClientMetadata> {
        self.data().map(|data| &data.metadata)
    }

    /// The OpenID Connect credentials of this client obtained after
    /// registration.
    ///
    /// Returns `None` if the client registration was not restored with
    /// [`Oidc::restore_registered_client()`] or
    /// [`Oidc::restore_session()`].
    pub fn client_credentials(&self) -> Option<&ClientCredentials> {
        self.data().map(|data| &data.credentials)
    }

    /// Set the current session tokens.
    fn set_session_tokens(&self, session_tokens: SessionTokens) {
        if let Some(auth_data) = self.client.inner.auth_data.get() {
            let Some(data) = auth_data.as_oidc() else {
                panic!("Cannot call OpenID Connect API after logging in with another API");
            };

            if let Some(tokens) = data.tokens.get() {
                tokens.set_if_not_eq(session_tokens);
            } else {
                let _ = data.tokens.set(SharedObservable::new(session_tokens));
            }
        } else {
            // Other OIDC auth data should have already been set before the session tokens.
            unreachable!()
        }
    }

    /// The tokens received after authorization of this client.
    ///
    /// Returns `None` if the client was not logged in with the OpenID Connect
    /// API.
    pub fn session_tokens(&self) -> Option<SessionTokens> {
        Some(self.data()?.tokens.get()?.get())
    }

    /// Get changes to the session tokens as a [`Stream`].
    ///
    /// Returns `None` if the client was not logged in with the OpenID Connect
    /// API.
    ///
    /// After login, the tokens should only change when refreshing the access
    /// token or authorizing new scopes.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use futures_util::StreamExt;
    /// use matrix_sdk::Client;
    /// # fn persist_session(_: &matrix_sdk::matrix_auth::Session) {};
    /// # async {
    /// let homeserver = "http://example.com";
    /// let client = Client::builder()
    ///     .homeserver_url(homeserver)
    ///     .handle_refresh_tokens()
    ///     .build()
    ///     .await?;
    ///
    /// // Login with the OpenID Connect API…
    ///
    /// let oidc = client.oidc();
    /// let session = oidc.session().expect("Client should be logged in");
    /// persist_session(session);
    ///
    /// // Handle when at least one of the tokens changed.
    /// let mut tokens_stream =
    ///     oidc.session_tokens_stream().expect("Client should be logged in");
    /// loop {
    ///     if tokens_stream.next().await.is_some() {
    ///         let session = oidc.session().expect("Client should be logged in");
    ///         persist_session(session);
    ///     }
    /// }
    /// # anyhow::Ok(()) };
    /// ```
    pub fn session_tokens_stream(&self) -> Option<impl Stream<Item = SessionTokens>> {
        Some(self.data()?.tokens.get()?.subscribe())
    }

    /// Get the current access token for this session.
    ///
    /// Returns `None` if the client was not logged in with the OpenID Connect
    /// API.
    pub fn access_token(&self) -> Option<String> {
        self.session_tokens().map(|tokens| tokens.access_token)
    }

    /// Get the current refresh token for this session.
    ///
    /// Returns `None` if the client was not logged in with the OpenID Connect
    /// API, or if the access token cannot be refreshed.
    pub fn refresh_token(&self) -> Option<String> {
        self.session_tokens().and_then(|tokens| tokens.refresh_token)
    }

    /// The ID Token received after the latest authorization of this client, if
    /// any.
    ///
    /// Returns `None` if the client was not logged in with the OpenID Connect
    /// API, or if the issuer did not provide one.
    pub fn latest_id_token(&self) -> Option<IdToken<'static>> {
        self.session_tokens()?.latest_id_token
    }

    /// The OpenID Connect user session of this client.
    ///
    /// Returns `None` if the client was not logged in with the OpenID Connect
    /// API.
    pub fn user_session(&self) -> Option<UserSession> {
        let meta = self.client.session_meta()?.to_owned();
        let tokens = self.session_tokens()?;
        let issuer_info = self.data()?.issuer_info.clone();
        Some(UserSession { meta, tokens, issuer_info })
    }

    /// The full OpenID Connect session of this client.
    ///
    /// Returns `None` if the client was not logged in with the OpenID Connect
    /// API.
    pub fn full_session(&self) -> Option<FullSession> {
        let user = self.user_session()?;
        let data = self.data()?;
        let client = RegisteredClientData {
            credentials: data.credentials.clone(),
            metadata: data.metadata.clone(),
        };
        Some(FullSession { client, user })
    }

    /// Register a client with an OpenID Connect Provider.
    ///
    /// This should be called before any authorization request with an unknown
    /// authentication issuer. If the client is already registered with the
    /// given issuer, it should use [`Oidc::restore_registered_client()`]
    /// directly.
    ///
    /// Note that the client should adapt the security measures enabled in its
    /// metadata according to the capabilities advertised in
    /// [`Oidc::given_provider_metadata()`].
    ///
    /// # Arguments
    ///
    /// * `issuer` - The OpenID Connect Provider to register with. Can be
    ///   obtained with [`Oidc::authentication_server_info()`].
    ///
    /// * `client_metadata` - The [`VerifiedClientMetadata`] to register.
    ///
    /// * `software_statement` - A [software statement], a digitally signed
    ///   version of the metadata, as a JWT. Any claim in this JWT will override
    ///   the corresponding field in the client metadata. It must include a
    ///   `software_id` claim that is used to uniquely identify a client and
    ///   ensure the same `client_id` is returned on subsequent registration,
    ///   allowing to update the registered client metadata.
    ///
    /// The credentials in the response should be persisted for future use and
    /// reused for the same issuer, along with the client metadata sent to the
    /// provider, even for different sessions or user accounts.
    ///
    /// # Example
    ///
    /// ```no_run
    /// use matrix_sdk::Client;
    /// use matrix_sdk::oidc::types::client_credentials::ClientCredentials;
    /// # use matrix_sdk::oidc::types::registration::{ClientMetadata, VerifiedClientMetadata};
    /// # let metadata = ClientMetadata::default().validate().unwrap();
    /// # fn persist_client_registration (_: &str, _: &VerifiedClientMetadata, _: &ClientCredentials) {};
    /// # async {
    /// let client = Client::builder().server_name("my_homeserver.org").build().await?;
    /// let oidc = client.oidc();
    ///
    /// if let Some(info) = oidc.authentication_server_info() {
    ///     let response = oidc
    ///         .register_client(&info.issuer, metadata.clone(), None)
    ///         .await?;
    ///
    ///     println!(
    ///         "Registered with client_id: {}",
    ///         response.client_id
    ///     );
    ///
    ///     // In this case we registered a public client, so it has no secret.
    ///     let credentials = ClientCredentials::None {
    ///         client_id: response.client_id,
    ///     };
    ///
    ///     let client_data = RegisteredClientData {
    ///         credentials,
    ///         metadata,
    ///     };
    ///
    ///     persist_client_registration(&info.issuer, &client_data);
    /// }
    /// # anyhow::Ok(()) }
    /// ```
    ///
    /// [software statement]: https://datatracker.ietf.org/doc/html/rfc7591#autoid-8
    pub async fn register_client(
        &self,
        issuer: &str,
        client_metadata: VerifiedClientMetadata,
        software_statement: Option<String>,
    ) -> Result<ClientRegistrationResponse, OidcError> {
        let provider_metadata = self.given_provider_metadata(issuer).await?;
        let registration_endpoint = provider_metadata
            .registration_endpoint
            .as_ref()
            .ok_or(OidcError::NoRegistrationSupport)?;

        register_client(
            &self.http_service(),
            registration_endpoint,
            client_metadata,
            software_statement,
        )
        .await
        .map_err(Into::into)
    }

    /// Set the data of a client that is registered with an OpenID Connect
    /// Provider.
    ///
    /// This should be called after registration or when logging in with a
    /// provider that is already known by the client.
    ///
    /// # Arguments
    ///
    /// * `issuer` - The OpenID Connect Provider to interact with.
    ///
    /// * `client_metadata` - The [`VerifiedClientMetadata`] that was
    ///   registered.
    ///
    /// * `client_credentials` - The credentials necessary to authenticate the
    ///   client with the provider, obtained after registration.
    ///
    /// # Panic
    ///
    /// Panics if authentication data was already set.
    pub async fn restore_registered_client(
        &self,
        issuer_info: AuthenticationServerInfo,
        client_data: RegisteredClientData,
    ) {
        let RegisteredClientData { credentials, metadata } = client_data;
        let data = OidcAuthData {
            issuer_info,
            credentials,
            metadata,
            tokens: Default::default(),
            authorization_data: Default::default(),
        };

        self.client
            .inner
            .auth_data
            .set(AuthData::Oidc(data))
            .expect("Client authentication data was already set");
    }

    /// Restore a previously logged in session.
    ///
    /// This can be used to restore the client to a logged in state, including
    /// loading the sync state and the encryption keys from the store, if
    /// one was set up.
    ///
    /// # Arguments
    ///
    /// * `session` - The session to restore.
    ///
    /// # Panic
    ///
    /// Panics if authentication data was already set.
    pub async fn restore_session(&self, session: FullSession) -> Result<()> {
        let FullSession {
            client: RegisteredClientData { credentials, metadata },
            user: UserSession { meta, tokens, issuer_info },
        } = session;

        let data = OidcAuthData {
            issuer_info,
            credentials,
            metadata,
            tokens: SharedObservable::new(tokens).into(),
            authorization_data: Default::default(),
        };

        self.client.base_client().set_session_meta(meta).await?;
        self.client
            .inner
            .auth_data
            .set(AuthData::Oidc(data))
            .expect("Client authentication data was already set");

        Ok(())
    }

    /// Login via OpenID Connect with the Authorization Code flow.
    ///
    /// This should be called after the registered client has been restored with
    /// [`Oidc::restore_registered_client()`].
    ///
    /// If this is a brand new login, [`Oidc::finish_login()`] must be called
    /// after [`Oidc::finish_authorization()`], to finish loading the user
    /// session.
    ///
    /// # Arguments
    ///
    /// * `redirect_uri` - The URI where the end user will be redirected after
    ///   authorizing the login. It must be one of the redirect URIs sent in the
    ///   client metadata during registration.
    ///
    /// * `device_id` - The unique ID that will be associated with the session.
    ///   If not set, a random one will be generated. It can be an existing
    ///   device ID from a previous login call. Note that this should be done
    ///   only if the client also holds the corresponding encryption keys.
    ///
    /// # Errors
    ///
    /// Returns an error if the device ID is not valid.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use matrix_sdk::{Client};
    /// # use url::Url;
    /// # let homeserver = Url::parse("https://example.com").unwrap();
    /// # let redirect_uri = Url::parse("http://127.0.0.1/oidc").unwrap();
    /// # let redirected_to_uri = Url::parse("http://127.0.0.1/oidc").unwrap();
    /// # let issuer_info = unimplemented!();
    /// # let client_data = unimplemented!();
    /// # async {
    /// # let client = Client::new(homeserver).await?;
    /// let oidc = client.oidc();
    ///
    /// oidc.restore_registered_client(
    ///     issuer_info,
    ///     client_data,
    /// ).await;
    ///
    /// let auth_data = oidc.login(redirect_uri, None).build().await?;
    ///
    /// // Open auth_data.url and wait for response at the redirect URI.
    /// // The full URL obtained is called here `redirected_to_uri`.
    ///
    /// let auth_response = AuthorizationResponse::parse_uri(&redirected_to_uri)?;
    ///
    /// let code = match auth_response {
    ///     AuthorizationResponse::Success(code) => code,
    ///     AuthorizationResponse::Error(error) => {
    ///         return Err(format!("Authorization failed: {error}").into());
    ///     }
    /// };
    ///
    /// let _tokens_response = oidc.finish_authorization(code).await?;
    ///
    /// // Important! Without this we can't access the full session.
    /// oidc.finish_login().await?;
    ///
    /// // The session tokens can be persisted either from the response, or from
    /// // one of the `Oidc::session_tokens()` method.
    ///
    /// // You can now make any request compatible with the requested scope.
    /// let _me = client.whoami().await?;
    /// # anyhow::Ok(()) }
    /// ```
    pub fn login(
        &self,
        redirect_uri: Url,
        device_id: Option<String>,
    ) -> Result<OidcAuthCodeUrlBuilder, OidcError> {
        // Generate the device ID if it is not provided.
        let device_id = if let Some(device_id) = device_id {
            device_id
        } else {
            rand::thread_rng()
                .sample_iter(&rand::distributions::Alphanumeric)
                .map(char::from)
                .take(10)
                .collect::<String>()
        };

        let scope = [
            ScopeToken::Openid,
            ScopeToken::MatrixApi(MatrixApiScopeToken::Full),
            ScopeToken::try_with_matrix_device(device_id).or(Err(OidcError::InvalidDeviceId))?,
        ]
        .into_iter()
        .collect();

        Ok(OidcAuthCodeUrlBuilder::new(self.clone(), scope, redirect_uri))
    }

    /// Finish the login process.
    ///
    /// Must be called after [`Oidc::finish_authorization()`] after logging into
    /// a brand new session, to load the last part of the user session and
    /// complete the initialization of the [`Client`].
    ///
    /// # Panic
    ///
    /// Panics if the login was already completed.
    pub async fn finish_login(&self) -> Result<()> {
        // Get the user ID.
        let whoami_res = self.client.whoami().await.map_err(crate::Error::from)?;

        let session = SessionMeta {
            user_id: whoami_res.user_id,
            device_id: whoami_res.device_id.ok_or(OidcError::MissingDeviceId)?,
        };
        self.client.base_client().set_session_meta(session).await.map_err(crate::Error::from)?;

        Ok(())
    }

    /// Authorize a given scope with the Authorization Code flow.
    ///
    /// This should be used if a new scope is necessary to make a request. For
    /// example, if the homeserver returns an [`Error`] with an
    /// [`AuthenticateError::InsufficientScope`].
    ///
    /// This should be called after the registered client has been restored or
    /// the client was logged in.
    ///
    /// # Arguments
    ///
    /// * `scope` - The scope to authorize.
    ///
    /// * `redirect_uri` - The URI where the end user will be redirected after
    ///   authorizing the scope. It must be one of the redirect URIs sent in the
    ///   client metadata during registration.
    ///
    /// [`Error`]: ruma::api::client::error::Error
    /// [`AuthenticateError::InsufficientScope`]: ruma::api::client::error::AuthenticateError
    pub fn authorize_scope(&self, scope: Scope, redirect_uri: Url) -> OidcAuthCodeUrlBuilder {
        OidcAuthCodeUrlBuilder::new(self.clone(), scope, redirect_uri)
    }

    /// Finish the authorization process.
    ///
    /// This method should be called after the URL returned by
    /// [`OidcAuthCodeUrlBuilder::build()`] has been presented and the user has
    /// been redirected to the redirect URI after a successful authorization.
    ///
    /// If the authorization has not been successful,
    /// [`Oidc::abort_authorization()`] should be used instead to clean up the
    /// local data.
    ///
    /// # Arguments
    ///
    /// * `code` - The response received as part of the redirect URI when the
    ///   authorization was successful.
    ///
    /// Returns an error if a request fails.
    pub async fn finish_authorization(
        &self,
        code: AuthorizationCode,
    ) -> Result<AccessTokenResponse, OidcError> {
        let provider_metadata = self.provider_metadata().await?;
        let data = self.data().ok_or(OidcError::NotAuthenticated)?;
        let validation_data = data
            .authorization_data
            .lock()
            .await
            .remove(&code.state)
            .ok_or(OidcError::InvalidState)?;

        let jwks = self.fetch_jwks(provider_metadata.jwks_uri()).await?;
        let id_token_verification_data = JwtVerificationData {
            issuer: provider_metadata.issuer(),
            jwks: &jwks,
            client_id: &data.credentials.client_id().to_owned(),
            signing_algorithm: data.metadata.id_token_signed_response_alg(),
        };

        let (response, id_token) = access_token_with_authorization_code(
            &self.http_service(),
            data.credentials.clone(),
            provider_metadata.token_endpoint(),
            code.code,
            validation_data,
            Some(id_token_verification_data),
            Utc::now(),
            &mut rng()?,
        )
        .await?;

        self.set_session_tokens(SessionTokens {
            access_token: response.access_token.clone(),
            refresh_token: response.refresh_token.clone(),
            latest_id_token: id_token,
        });

        Ok(response)
    }

    /// Abort the authorization process.
    ///
    /// This method should be called after the URL returned by
    /// [`OidcAuthCodeUrlBuilder::build()`] has been presented and the user has
    /// been redirected to the redirect URI after a failed authorization, or if
    /// the authorization should be aborted before it is completed.
    ///
    /// If the authorization has been successful,
    /// [`Oidc::finish_authorization()`] should be used instead.
    ///
    /// # Arguments
    ///
    /// * `state` - The state received as part of the redirect URI when the
    ///   authorization failed, or the one provided in [`OidcAuthorizationData`]
    ///   after building the authorization URL.
    pub async fn abort_authorization(&self, state: &str) {
        if let Some(data) = self.data() {
            data.authorization_data.lock().await.remove(state);
        }
    }

    async fn refresh_access_token_inner(
        &self,
        latest_id_token: Option<IdToken<'static>>,
        refresh_token: String,
    ) -> Result<AccessTokenResponse, OidcError> {
        let provider_metadata = self.provider_metadata().await?;
        let data = self.data().ok_or(OidcError::NotAuthenticated)?;

        let jwks = self.fetch_jwks(provider_metadata.jwks_uri()).await?;
        let id_token_verification_data = JwtVerificationData {
            issuer: provider_metadata.issuer(),
            jwks: &jwks,
            client_id: &data.credentials.client_id().to_owned(),
            signing_algorithm: data.metadata.id_token_signed_response_alg(),
        };

        tracing::trace!(
            "Token refresh: attempting to refresh with refresh_token {}",
            hash(&refresh_token)
        );

        let (response, _id_token) = refresh_access_token(
            &self.http_service(),
            data.credentials.clone(),
            provider_metadata.token_endpoint(),
            refresh_token.clone(),
            None,
            Some(id_token_verification_data),
            latest_id_token.as_ref(),
            Utc::now(),
            &mut rng()?,
        )
        .await
        .map_err(OidcError::from)?;

        tracing::trace!(
            "Token refresh: new refresh_token: {:?} / access_token: {}",
            response.refresh_token.as_ref().map(hash),
            hash(&response.access_token)
        );

        self.set_session_tokens(SessionTokens {
            access_token: response.access_token.clone(),
            refresh_token: response.refresh_token.clone().or(Some(refresh_token)),
            latest_id_token,
        });

        _ = self.client.inner.session_change_sender.send(SessionChange::TokensRefreshed);

        Ok(response)
    }

    /// Refresh the access token.
    ///
    /// This should be called when the access token has expired. It should not
    /// be needed to call this manually if the [`Client`] was constructed with
    /// [`ClientBuilder::handle_refresh_tokens()`].
    ///
    /// This method is protected behind a lock, so calling this method several
    /// times at once will only call the endpoint once and all subsequent calls
    /// will wait for the result of the first call. The first call will
    /// return `Ok(Some(response))` or a [`RefreshTokenError`], while the others
    /// will return `Ok(None)` if the token was refreshed by the first call
    /// or the same [`RefreshTokenError`], if it failed.
    ///
    /// [`ClientBuilder::handle_refresh_tokens()`]: crate::ClientBuilder::handle_refresh_tokens()
    pub async fn refresh_access_token(
        &self,
    ) -> Result<Option<AccessTokenResponse>, RefreshTokenError> {
        let client = &self.client;
        let lock = client.inner.refresh_token_lock.try_lock();

        macro_rules! fail {
            ($lock:expr, $error:expr) => {
                let error = $error;
                *$lock = Err(error.clone());
                return Err(error);
            };
        }

        if let Ok(mut guard) = lock {
            let Some(session_tokens) = self.session_tokens() else {
                fail!(guard, RefreshTokenError::RefreshTokenRequired);
            };
            let Some(refresh_token) = session_tokens.refresh_token else {
                fail!(guard, RefreshTokenError::RefreshTokenRequired);
            };

            match self
                .refresh_access_token_inner(session_tokens.latest_id_token, refresh_token)
                .await
            {
                Ok(response) => {
                    *guard = Ok(());
                    Ok(Some(response))
                }
                Err(error) => {
                    fail!(guard, RefreshTokenError::Oidc(error.into()));
                }
            }
        } else {
            match client.inner.refresh_token_lock.lock().await.as_ref() {
                Ok(_) => Ok(None),
                Err(error) => Err(error.clone()),
            }
        }
    }

    /// Log out from the currently authenticated session.
    ///
    /// On success, if the provider supports [RP-Initiated Logout], an
    /// [`OidcEndSessionUrlBuilder`] will be provided to build the URL allowing
    /// the user to log out from their account in the provider's interface.
    ///
    /// [RP-Initiated Logout]: https://openid.net/specs/openid-connect-rpinitiated-1_0.html
    pub async fn logout(&self) -> Result<Option<OidcEndSessionUrlBuilder>, OidcError> {
        let provider_metadata = self.provider_metadata().await?;
        let client_credentials = self.client_credentials().ok_or(OidcError::NotAuthenticated)?;

        let revocation_endpoint =
            provider_metadata.revocation_endpoint.as_ref().ok_or(OidcError::NoRevocationSupport)?;

        let tokens = self.session_tokens().ok_or(OidcError::NotAuthenticated)?;
        let mut rng = rng()?;

        // Revoke the access token.
        revoke_token(
            &self.http_service(),
            client_credentials.clone(),
            revocation_endpoint,
            tokens.access_token,
            Some(OAuthTokenTypeHint::AccessToken),
            Utc::now(),
            &mut rng,
        )
        .await?;

        // Revoke the refresh token, if any.
        if let Some(refresh_token) = tokens.refresh_token {
            revoke_token(
                &self.http_service(),
                client_credentials.clone(),
                revocation_endpoint,
                refresh_token,
                Some(OAuthTokenTypeHint::RefreshToken),
                Utc::now(),
                &mut rng,
            )
            .await?;
        }

        let end_session_builder =
            provider_metadata.end_session_endpoint.clone().map(|end_session_endpoint| {
                OidcEndSessionUrlBuilder::new(
                    self.clone(),
                    end_session_endpoint,
                    client_credentials.client_id().to_owned(),
                )
            });

        Ok(end_session_builder)
    }
}

/// A full session for the OpenID Connect API.
#[derive(Debug, Clone)]
pub struct FullSession {
    /// The registered client data.
    pub client: RegisteredClientData,

    /// The user session.
    pub user: UserSession,
}

/// The data used to identify a registered client for the OpenID Connect API.
#[derive(Debug, Clone)]
pub struct RegisteredClientData {
    /// The credentials obtained after registration.
    pub credentials: ClientCredentials,

    /// The client metadata sent for registration.
    pub metadata: VerifiedClientMetadata,
}

/// A user session for the OpenID Connect API.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserSession {
    /// The Matrix user session info.
    #[serde(flatten)]
    pub meta: SessionMeta,

    /// The tokens used for authentication.
    #[serde(flatten)]
    pub tokens: SessionTokens,

    /// Information about the OpenID Connect provider used for this session.
    pub issuer_info: AuthenticationServerInfo,
}

/// The tokens for a user session obtained with the OpenID Connect API.
#[derive(Clone, Eq, PartialEq)]
#[allow(missing_debug_implementations)]
pub struct SessionTokens {
    /// The access token used for this session.
    pub access_token: String,

    /// The token used for refreshing the access token, if any.
    pub refresh_token: Option<String>,

    /// The ID token returned by the provider during the latest authorization.
    pub latest_id_token: Option<IdToken<'static>>,
}

impl fmt::Debug for SessionTokens {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SessionTokens").finish_non_exhaustive()
    }
}

/// The data returned by the provider in the redirect URI after a successful
/// authorization.
#[derive(Debug, Clone)]
pub enum AuthorizationResponse {
    /// A successful response.
    Success(AuthorizationCode),

    /// An error response.
    Error(AuthorizationError),
}

impl AuthorizationResponse {
    /// Deserialize an `AuthorizationResponse` from the given URI.
    ///
    /// Returns an error if the URL doesn't have the expected format.
    pub fn parse_uri(uri: &Url) -> Result<Self, RedirectUriQueryParseError> {
        let Some(query) = uri.query() else { return Err(RedirectUriQueryParseError::MissingQuery) };

        Self::parse_query(query)
    }

    /// Deserialize an `AuthorizationResponse` from the query part of a URI.
    ///
    /// Returns an error if the URL doesn't have the expected format.
    pub fn parse_query(query: &str) -> Result<Self, RedirectUriQueryParseError> {
        // For some reason deserializing the enum with `serde(untagged)` doesn't work,
        // so let's try both variants separately.
        if let Ok(code) = serde_html_form::from_str(query) {
            return Ok(AuthorizationResponse::Success(code));
        }
        if let Ok(error) = serde_html_form::from_str(query) {
            return Ok(AuthorizationResponse::Error(error));
        }

        Err(RedirectUriQueryParseError::UnknownFormat)
    }

    /// Access the state field, on either variant.
    pub fn state(&self) -> &str {
        match self {
            Self::Success(code) => &code.state,
            Self::Error(error) => &error.state,
        }
    }
}

/// The data returned by the provider in the redirect URI after a successful
/// authorization.
#[derive(Debug, Clone, Deserialize)]
pub struct AuthorizationCode {
    /// The code to use to retrieve the access token.
    pub code: String,
    /// The unique identifier for this transaction.
    pub state: String,
}

/// The data returned by the provider in the redirect URI after an authorization
/// error.
#[derive(Debug, Clone, Deserialize)]
pub struct AuthorizationError {
    /// The error.
    #[serde(flatten)]
    pub error: ClientError,
    /// The unique identifier for this transaction.
    pub state: String,
}

/// An error when trying to parse the query of a redirect URI.
#[derive(Debug, Clone, Error)]
pub enum RedirectUriQueryParseError {
    /// There is no query part in the URI.
    #[error("No query in URI")]
    MissingQuery,

    /// Deserialization failed.
    #[error("Query is not using one of the defined formats")]
    UnknownFormat,
}

/// All errors that can occur when using the OpenID Connect API.
#[derive(Debug, Error)]
pub enum OidcError {
    /// An error occurred when interacting with the provider.
    #[error(transparent)]
    Oidc(mas_oidc_client::error::Error),

    /// No authentication issuer was provided by the homeserver or by the user.
    #[error("client missing authentication issuer")]
    MissingAuthenticationIssuer,

    /// The OpenID Connect Provider doesn't support dynamic client registration.
    ///
    /// The provider probably offers another way to register clients.
    #[error("no dynamic registration support")]
    NoRegistrationSupport,

    /// The device ID was not returned by the homeserver after login.
    #[error("missing device ID in response")]
    MissingDeviceId,

    /// The client is not authenticated while the request requires it.
    #[error("client not authenticated")]
    NotAuthenticated,

    /// The state used to complete authorization doesn't match an original
    /// value.
    #[error("the supplied state is unexpected")]
    InvalidState,

    /// The device ID is invalid.
    #[error("invalid device ID")]
    InvalidDeviceId,

    /// The OpenID Connect Provider doesn't support token revocation, aka
    /// logging out.
    #[error("no token revocation support")]
    NoRevocationSupport,

    /// An error occurred generating a random value.
    #[error(transparent)]
    Rand(rand::Error),

    /// An unknown error occurred.
    #[error("unknown error")]
    UnknownError(#[source] Box<dyn std::error::Error + Send + Sync>),
}

impl<E> From<E> for OidcError
where
    E: Into<mas_oidc_client::error::Error>,
{
    fn from(value: E) -> Self {
        Self::Oidc(value.into())
    }
}

fn rng() -> Result<StdRng, OidcError> {
    StdRng::from_rng(rand::thread_rng()).map_err(OidcError::Rand)
}

fn hash<T: Hash>(x: &T) -> u64 {
    let mut hasher = DefaultHasher::new();
    x.hash(&mut hasher);
    hasher.finish()
}
