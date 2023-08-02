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

//! High-level OpenID Connect API.
//!
//! # OpenID Connect specification compliance
//!
//! The OpenID Connect client used in the SDK has not been certified for
//! compliance against the OpendID Connect specification.
//!
//! It doesn't implement everything from the specification and is currently
//! limited to the Authorization Code and Client Credentials flows.
//!
//! In the future, it should also support some OAuth2.0 extensions like the
//! Device Code flow.
//!
//! # Setup
//!
//! To enable support for OpenID Connect on the [`Client`], two steps are
//! needed:
//!
//! 1. Add this to your Cargo manifest:
//!   
//!   ```toml
//!   [patch.crates-io.x25519-dalek]
//!   git = "https://github.com/A6GibKm/x25519-dalek"
//!   rev = "9f19028c34107eea87d37bcee2eb2b350ec34cfe"
//!   ```
//!
//!   It is necessary to solve a dependency pinning issue with the SDK's
//!   dependencies.
//!
//! 2. Enable the `experimental-oidc` cargo feature for the `matrix-sdk`
//!   crate.
//!
//! # Homeserver support
//!
//! It is recommended to build the Client with [`Client::builder()`] and then to
//! enable auto-discovery by calling [`ClientBuilder::server_name()`]. That will
//! allow to discover the OpenID Connect Provider that is advertised
//! by the homeserver.
//!
//! After building the client, you can check that the homeserver supports
//! logging in via OIDC when [`Client::authentication_issuer()`] is set.
//!
//! If the homeserver doesn't advertise its support for OIDC, but the issuer URL
//! is known with some other method, it can be provided manually during
//! registration.
//!
//! # Registration
//!
//! Registration is only required the first time a client encounters an issuer
//! (identified by its URL). After client registration, the client credentials
//! should be persisted and reused for every session that interacts with that
//! same issuer.
//!
//! If the issuer supports dynamic registration, it can be done by using
//! [`Oidc::register_client()`]. Otherwise the client credentials probably need
//! to be obtained directly from the issuer or the homeserver.
//!
//! # Login
//!
//! [`Oidc::login()`] allows to login via OIDC. It works like the other
//! login methods by creating an [`OidcLoginBuilder`] that can be configured,
//! and then calling one of the methods to proceed to logging in.
//!
//! Several authorization methods are available, that usually depend on the
//! client you want to build:
//!
//! - The Authorization Code flow should be used by clients that can open a URL
//!   in a browser directly on the device where the client is installed, which
//!   is generally the case for clients with a GUI.
//! - The Client Credentials flow should be used by clients that run on machines
//!   where no browser is available and no interactive login is wanted, which is
//!   generally the case for bots.
//! - (Coming Soon) The Device Authorization flow should be used by clients that
//!   run on machines where no browser is available and an interactive login is
//!   wanted, by opening a URL in the browser of another device.
//!
//! # Refresh tokens
//!
//! The use of refresh tokens with OpenID Connect Providers is more common than
//! in the Matrix specification. For this reason, it is recommended to configure
//! the client with [`ClientBuilder::handle_refresh_tokens()`], to handle
//! refreshing tokens automatically.
//!
//! Applications should then listen to session tokens changes after logging in
//! with [`Client::session_tokens_changed_stream()`] or
//! [`Client::session_tokens_stream()`] to be able to restore the session at a
//! later time, otherwise the end-user will need to login again.
//!
//! # Insufficient scope
//!
//! Some API calls that deal with sensitive data require more priviledges than
//! others. In the current Matrix specification, those endpoints use the
//! User-Interactive Authentication API.
//!
//! The OAuth 2.0 specification has the concept of scopes, and an access token
//! is limited to a given scope when it is generated. Accessing those endpoints
//! require a priviledged scope, so a new authorization request is necessary to
//! get a new access token.
//!
//! When the API endpoint returns an [`Error`] with an
//! [`AuthenticateError::InsufficientScope`], one of the
//! `Oidc::authorize_scope_with_*()` methods can be used to authorize the
//! required scope.
//!
//! _Note: This should work but is not fully specced yet._
//!
//! # Logout
//!
//! Coming soon.
//!
//! # Examples
//!
//! Most methods have examples, there is also an example CLI application that
//! supports all the actions described here in `examples/oidc-cli`.
//!
//! [`ClientBuilder::server_name()`]: crate::ClientBuilder::server_name()
//! [`ClientBuilder::handle_refresh_tokens()`]: crate::ClientBuilder::handle_refresh_tokens()
//! [`Error`]: ruma::api::client::error::Error
//! [`AuthenticateError::InsufficientScope`]: ruma::api::client::error::AuthenticateError

use chrono::Utc;
pub use mas_oidc_client::*;
use mas_oidc_client::{
    http_service::HttpService,
    requests::{
        authorization_code::{
            access_token_with_authorization_code, build_authorization_url, AuthorizationRequestData,
        },
        client_credentials::access_token_with_client_credentials,
        discovery::discover,
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
        scope::{Scope, ScopeToken},
        IdToken,
    },
};
use serde::Deserialize;
use thiserror::Error;
use tokio::sync::RwLock;
use tracing::{info, instrument};
use url::Url;

use crate::{matrix_auth::SessionTokens, Client, Result};

/// A high-level API to interact with an OpenID Connect Provider.
#[derive(Debug, Clone)]
pub struct Oidc {
    /// The underlying HTTP client.
    client: Client,
}

impl Oidc {
    pub(crate) fn new(client: Client) -> Self {
        Self { client }
    }

    fn http_service(&self) -> HttpService {
        HttpService::new(self.client.inner.http_client.clone())
    }

    /// The OpenID Connect Provider to interact with.
    pub async fn issuer(&self) -> Option<String> {
        self.client.inner.authentication_issuer.read().await.clone()
    }

    /// The OpenID Connect metadata of the issuer.
    pub async fn provider_metadata(&self) -> Result<VerifiedProviderMetadata, OidcError> {
        let issuer = self.issuer().await.ok_or(OidcError::MissingAuthenticationIssuer)?;

        discover(&self.http_service(), &issuer).await.map_err(Into::into)
    }

    /// The OpenID Connect metadata of this client.
    ///
    /// Returns `None` if the client registration was not restored.
    pub fn client_metadata(&self) -> Option<&VerifiedClientMetadata> {
        self.client.inner.oidc_data.get().map(|data| &data.metadata)
    }

    /// The OpenID Connect credentials of this client.
    ///
    /// Returns `None` if the client registration was not restored.
    pub fn client_credentials(&self) -> Option<ClientCredentials> {
        self.client.inner.oidc_data.get().map(|data| data.credentials.clone())
    }

    /// The ID Token received after the latest authentication of this client.
    ///
    /// Returns `None` if the client authentication was not restored.
    pub async fn latest_id_token(&self) -> Option<IdToken<'static>> {
        let data = self.client.inner.oidc_data.get()?;
        data.latest_id_token.read().await.clone()
    }

    /// Register the client with an OpenID Connect Provider.
    ///
    /// This should be called before any authorization request with an unknown
    /// authentication issuer.
    ///
    /// Note that the client metadata should adapt the security measures enabled
    /// in its metadata according to the capabilities advertised in the provider
    /// metadata.
    ///
    /// # Arguments
    ///
    /// * `client_metadata` - The [`VerifiedClientMetadata`] to register.
    ///
    /// * `issuer` - The OpenID Connect Provider to register with. This is
    ///   required if it is not returned by [`Oidc::issuer()`].
    ///
    /// The credentials in the response should be stored for future use and
    /// reused for the same issuer, even with other sessions.
    ///
    /// # Example
    ///
    /// ```no_run
    /// use matrix_sdk::Client;
    /// use matrix_sdk::oidc::types::client_credentials::ClientCredentials;
    /// # use matrix_sdk::oidc::types::registration::ClientMetadata;
    /// # use futures::executor::block_on;
    /// # let user_id = ruma::user_id!("@user:example.com");
    /// # let client_metadata = ClientMetadata::default().validate().unwrap();
    /// # block_on(async {
    /// let client = Client::builder().user_id(user_id).build().await?;
    /// let oidc = client.oidc();
    ///
    /// if oidc.issuer().await.is_some() {
    ///     let response = oidc
    ///         .register_client(client_metadata.clone(), None)
    ///         .await?;
    ///
    ///     println!(
    ///         "Registered with client_id: {}",
    ///         response.client_id
    ///     );
    ///
    ///     // In this case we registered a public client, so it has no secret.
    ///     let client_credentials = ClientCredentials::None {
    ///         client_id: response.client_id,
    ///     };
    ///
    ///     oidc.set_registered_client_data(
    ///         client_metadata,
    ///         client_credentials,
    ///         None,
    ///     ).await;
    /// }
    /// # anyhow::Ok(()) })
    /// ```
    pub async fn register_client(
        &self,
        client_metadata: VerifiedClientMetadata,
        issuer: Option<String>,
    ) -> Result<ClientRegistrationResponse, OidcError> {
        if let Some(issuer) = issuer {
            *self.client.inner.authentication_issuer.write().await = Some(issuer);
        }

        let provider_metadata = self.provider_metadata().await?;
        let registration_endpoint = provider_metadata
            .registration_endpoint
            .as_ref()
            .ok_or(OidcError::NoRegistrationSupport)?;

        register_client(&self.http_service(), registration_endpoint, client_metadata)
            .await
            .map_err(Into::into)
    }

    /// Set the data of a client that is already registered with an OpenID
    /// Connect Provider.
    ///
    /// This should be called after registering or when interacting with a
    /// provider that is already known by the client.
    ///
    /// # Arguments
    ///
    /// * `client_metadata` - The [`VerifiedClientMetadata`] that was
    ///   registered.
    ///
    /// * `client_credentials` - The credentials necessary to authenticate the
    ///   client with the provider.
    ///
    /// * `issuer` - The OpenID Connect Provider to use. This is required if it
    ///   is not returned by [`Oidc::issuer()`].
    ///
    /// # Panic
    ///
    /// Panics if the data was already set.
    pub async fn set_registered_client_data(
        &self,
        client_metadata: VerifiedClientMetadata,
        client_credentials: ClientCredentials,
        issuer: Option<String>,
    ) {
        if let Some(issuer) = issuer {
            *self.client.inner.authentication_issuer.write().await = Some(issuer);
        }

        let data = OidcData {
            credentials: client_credentials,
            metadata: client_metadata,
            latest_id_token: RwLock::new(None),
        };

        self.client.inner.oidc_data.set(data).expect("OIDC Client was already restored");
    }

    async fn load_session_meta(&self) -> Result<(), OidcError> {
        // Get the user ID.
        let whoami_res = self.client.whoami().await.map_err(crate::Error::from)?;

        let session = matrix_sdk_base::SessionMeta {
            user_id: whoami_res.user_id,
            device_id: whoami_res.device_id.ok_or(OidcError::MissingDeviceId)?,
        };
        self.client.base_client().set_session_meta(session).await.map_err(crate::Error::from)?;

        Ok(())
    }

    /// Login via OpenID Connect.
    ///
    /// This should be called after the registered client has been restored with
    /// [`Oidc::set_registered_client_data()`].
    pub fn login(&self) -> OidcLoginBuilder {
        OidcLoginBuilder::new(self.clone())
    }

    /// Get the URL that should be presented to authorize a given scope with the
    /// Authorization Code flow.
    ///
    /// This URL should be presented to the user and once they are redirected to
    /// the `redirect_uri`, the authorization can be finished by calling
    /// [`Oidc::finish_authorization_with_authorization_code()`].
    ///
    /// This should be used if a new scope is necessary to make a request.
    ///
    /// This should be called after the registered client has been restored with
    /// [`Oidc::set_registered_client_data()`].
    ///
    /// # Arguments
    ///
    /// * `scope` - The scope to authorize.
    ///
    /// * `redirect_uri` - The URI where the end user will be redirected after
    ///   authorizing the login. It must be one of the redirect URIs sent in the
    ///   client metadata during registration.
    ///
    /// # Example
    ///
    /// ```no_run
    /// use matrix_sdk::{Client, Error};
    /// # use futures::executor::block_on;
    /// # use url::Url;
    /// # let homeserver = Url::parse("https://example.com").unwrap();
    /// # let redirect_uri = Url::parse("http://127.0.0.1/oidc").unwrap();
    /// # let scope = "scope".parse().unwrap();
    /// # block_on(async {
    /// # let client = Client::new(homeserver).await?;
    /// let oidc = client.oidc();
    ///
    /// let auth_data =
    ///     oidc.url_for_scope_with_authorization_code(scope, redirect_uri).await?;
    /// // Open auth_data.url and wait for response at the redirect URI.
    ///
    /// # let (code, redirect_state) = (String::new(), String::new());
    ///
    /// // Alternatively, the state can be used as an identifier to handle several
    /// // authorization requests with a single redirect URI. In this instance the
    /// // redirect_state should be compared to auth_data.state before proceeding.
    ///
    /// let _response = oidc
    ///     .finish_authorization_with_authorization_code(code, redirect_state)
    ///     .await?;
    ///
    /// // The access and refresh tokens can be persisted either from the response,
    /// // or from one of the `Client::session_tokens*` methods.
    ///
    /// // You can now make any request compatible with the requested scope.
    /// let _me = client.whoami().await?;
    /// # anyhow::Ok(()) })
    /// ```
    ///
    /// [`ClientErrorCode`]: matrix_sdk::oidc::types::errors::ClientErrorCode
    pub async fn url_for_scope_with_authorization_code(
        &self,
        scope: &Scope,
        redirect_uri: &Url,
    ) -> Result<OidcAuthorizationData, OidcError> {
        let provider_metadata = self.provider_metadata().await?;
        let client_credentials = self.client_credentials().ok_or(OidcError::NotAuthenticated)?;

        let authorization_data = AuthorizationRequestData {
            client_id: client_credentials.client_id(),
            code_challenge_methods_supported: provider_metadata
                .code_challenge_methods_supported
                .as_deref(),
            scope,
            redirect_uri,
            prompt: None,
        };

        // TODO: use PAR if possible.

        let (url, validation_data) = build_authorization_url(
            provider_metadata.authorization_endpoint().clone(),
            authorization_data,
            &mut rand::thread_rng(),
        )?;

        let state = validation_data.state.clone();

        self.client.inner.oidc_validation_data.lock().await.insert(state.clone(), validation_data);

        Ok(OidcAuthorizationData { url, state })
    }

    /// Finish authorizing a given scope by using the Authorization Code flow.
    ///
    /// This method should be called after the URL returned by
    /// [`Oidc::url_for_scope_with_authorization_code()`] has been presented and
    /// the user has been redirected to the redirect URI.
    ///
    /// # Arguments
    ///
    /// * `code` - The code received as part of the redirect URI when the
    ///   authorization was successful.
    ///
    /// * `state` - The unique string received as part of the redirect URI when
    ///   the authorization was successful
    ///
    /// [`ClientErrorCode`]: matrix_sdk::oidc::types::errors::ClientErrorCode
    pub async fn finish_authorization_with_authorization_code(
        &self,
        auth_response: AuthorizationResponse,
    ) -> Result<AccessTokenResponse, OidcError> {
        let provider_metadata = self.provider_metadata().await?;
        let client_credentials = self.client_credentials().ok_or(OidcError::NotAuthenticated)?;

        let validation_data = self
            .client
            .inner
            .oidc_validation_data
            .lock()
            .await
            .remove(&auth_response.state)
            .ok_or(OidcError::InvalidState)?;

        let (response, id_token) = access_token_with_authorization_code(
            &self.http_service(),
            client_credentials.clone(),
            provider_metadata.token_endpoint(),
            auth_response.code,
            validation_data,
            None,
            Utc::now(),
            &mut rand::thread_rng(),
        )
        .await?;

        let tokens = SessionTokens {
            access_token: response.access_token.clone(),
            refresh_token: response.refresh_token.clone(),
        };
        self.client.matrix_auth().set_session_tokens(tokens);

        if let Some(id_token) = id_token {
            let data = self
                .client
                .inner
                .oidc_data
                .get()
                .expect("OpenID Connect data is set if client credentials were found");
            *data.latest_id_token.write().await = Some(id_token);
        }

        Ok(response)
    }

    /// Authorize the given scope by using the Client Credentials flow.
    ///
    /// This should be used if a new scope is necessary to make a request.
    ///
    /// This should be called after the registered client has been restored with
    /// [`Oidc::set_registered_client_data()`].
    ///
    /// # Arguments
    ///
    /// * `scope` - The scope to authorize.
    ///
    /// # Example
    ///
    /// ```no_run
    /// use matrix_sdk::{Client, Error};
    /// # use futures::executor::block_on;
    /// # use url::Url;
    /// # let homeserver = Url::parse("https://example.com").unwrap();
    /// # let scope = "scope".parse().unwrap();
    /// # block_on(async {
    /// # let client = Client::new(homeserver).await?;
    /// let oidc = client.oidc();
    ///
    /// let _response = oidc.authorize_scope_with_client_credentials(scope).await?;
    ///
    /// // The access and refresh tokens can be persisted either from the response,
    /// // or from one of the `Client::session_tokens*` methods.
    ///
    /// // You can now make any request compatible with the requested scope.
    /// let _me = client.whoami().await?;
    /// # anyhow::Ok(()) })
    /// ```
    pub async fn authorize_scope_with_client_credentials(
        &self,
        scope: Scope,
    ) -> Result<AccessTokenResponse, OidcError> {
        let provider_metadata = self.provider_metadata().await?;
        let client_credentials = self.client_credentials().ok_or(OidcError::NotAuthenticated)?;

        let response = access_token_with_client_credentials(
            &self.http_service(),
            client_credentials,
            provider_metadata.token_endpoint(),
            Some(scope),
            Utc::now(),
            &mut rand::thread_rng(),
        )
        .await?;

        let tokens = SessionTokens {
            access_token: response.access_token.clone(),
            refresh_token: response.refresh_token.clone(),
        };
        self.client.matrix_auth().set_session_tokens(tokens);

        Ok(response)
    }

    /// Refresh the access token.
    ///
    /// This should be called when the access token has expired.
    pub async fn refresh_access_token(&self) -> Result<AccessTokenResponse, OidcError> {
        let provider_metadata = self.provider_metadata().await?;
        let client_credentials = self.client_credentials().ok_or(OidcError::NotAuthenticated)?;

        let refresh_token =
            self.client.matrix_auth().refresh_token().ok_or(OidcError::MissingRefreshToken)?;

        let (response, _id_token) = refresh_access_token(
            &self.http_service(),
            client_credentials,
            provider_metadata.token_endpoint(),
            refresh_token.clone(),
            None,
            None,
            self.latest_id_token().await.as_ref(),
            Utc::now(),
            &mut rand::thread_rng(),
        )
        .await?;

        let mut tokens =
            SessionTokens { access_token: response.access_token.clone(), refresh_token: None };

        tokens.refresh_token = Some(response.refresh_token.clone().unwrap_or(refresh_token));

        self.client.matrix_auth().set_session_tokens(tokens);

        Ok(response)
    }

    /// Log out from the currently authenticated session.
    pub async fn logout(&self) -> Result<(), OidcError> {
        let provider_metadata = self.provider_metadata().await?;
        let client_credentials = self.client_credentials().ok_or(OidcError::NotAuthenticated)?;

        let revocation_endpoint =
            provider_metadata.revocation_endpoint.as_ref().ok_or(OidcError::NoRevocationSupport)?;

        let tokens =
            self.client.matrix_auth().session_tokens().ok_or(OidcError::NotAuthenticated)?;

        // Revoke the access token.
        revoke_token(
            &self.http_service(),
            client_credentials.clone(),
            revocation_endpoint,
            tokens.access_token,
            Some(OAuthTokenTypeHint::AccessToken),
            Utc::now(),
            &mut rand::thread_rng(),
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
                &mut rand::thread_rng(),
            )
            .await?;
        }

        Ok(())
    }
}

/// Builder type used to configure optional settings for logging in with an
/// OpenID Connect Provider via the Authorization Code flow.
///
/// Created with [`Oidc::login`]. Finalized with
/// [`.finish_login_with_authorization_code()`](Self::finish_login_with_authorization_code) or
/// [`.login_with_client_credentials()`](Self::login_with_client_credentials).
#[allow(missing_debug_implementations)]
pub struct OidcLoginBuilder {
    oidc: Oidc,
    device_id: Option<String>,
}

impl OidcLoginBuilder {
    pub(crate) fn new(oidc: Oidc) -> Self {
        Self { oidc, device_id: None }
    }

    /// Set the device ID.
    ///
    /// The device ID is a unique ID that will be associated with this session.
    /// If not set, the client will create one. Can be an existing device ID
    /// from a previous login call. Note that this should be done only if the
    /// client also holds the corresponding encryption keys.
    ///
    /// _Note_: Due to current limitations, the device ID should be an ASCII
    /// alphanumeric string with at least 10 characters.
    pub fn device_id(mut self, value: String) -> Self {
        self.device_id = Some(value);
        self
    }

    /// Get the URL that should be presented to login via the Authorization Code
    /// flow.
    ///
    /// This URL should be presented to the user and once they are redirected to
    /// the `redirect_uri`, the login can be finished by calling
    /// [`OidcLoginBuilder::finish_login_with_authorization_code()`].
    ///
    /// # Arguments
    ///
    /// * `redirect_uri` - The URI where the end user will be redirected after
    ///   authorizing the login. It must be one of the redirect URIs sent in the
    ///   client metadata during registration.
    ///
    /// # Example
    ///
    /// ```no_run
    /// use matrix_sdk::{Client, Error};
    /// # use futures::executor::block_on;
    /// # use url::Url;
    /// # let homeserver = Url::parse("https://example.com").unwrap();
    /// # let redirect_uri = Url::parse("http://127.0.0.1/oidc").unwrap();
    /// # block_on(async {
    /// # let client = Client::new(homeserver).await?;
    /// let oidc = client.oidc();
    ///
    /// let auth_data = oidc
    ///     .login()
    ///     .url_for_login_with_authorization_code(&redirect_uri)
    ///     .await?;
    /// // Open auth_data.url and wait for response at the redirect URI.
    ///
    /// # let (code, redirect_state) = (String::new(), String::new());
    ///
    /// // Alternatively, the state can be used as an identifier to handle several
    /// // authorization requests with a single redirect URI. In this instance the
    /// // redirect_state should be compared to auth_data.state before proceeding.
    ///
    /// let _response = oidc
    ///     .login()
    ///     .finish_login_with_authorization_code(code, redirect_state)
    ///     .await?;
    ///
    /// // The access and refresh tokens can be persisted either from the response,
    /// // or from one of the `Client::session_tokens*` methods.
    ///
    /// // You can now make any request compatible with the requested scope.
    /// let _me = client.whoami().await?;
    /// # anyhow::Ok(()) })
    /// ```
    ///
    /// [`ClientErrorCode`]: matrix_sdk::oidc::types::errors::ClientErrorCode
    #[instrument(target = "matrix_sdk::client", skip_all)]
    pub async fn url_for_login_with_authorization_code(
        self,
        redirect_uri: &Url,
    ) -> Result<OidcAuthorizationData, OidcError> {
        let issuer = self.oidc.issuer().await.ok_or(OidcError::MissingAuthenticationIssuer)?;
        info!(issuer, "Logging in via OpenID Connect with the Authorization Code flow");

        let scope = generate_scope(self.device_id)?;

        self.oidc.url_for_scope_with_authorization_code(&scope, redirect_uri).await
    }

    /// Finish logging in via the Authorization Code flow.
    ///
    /// This method should be called after the URL returned by
    /// [`OidcLoginBuilder::url_for_login_with_authorization_code()`] has been
    /// presented and the user has been redirected to the redirect URI.
    ///
    /// # Arguments
    ///
    /// * `code` - The code received as part of the redirect URI when login was
    ///   successful.
    ///
    /// * `state` - The unique string received as part of the redirect URI when
    ///   login was successful
    ///
    /// [`ClientErrorCode`]: matrix_sdk::oidc::types::errors::ClientErrorCode
    #[instrument(target = "matrix_sdk::client", skip_all)]
    pub async fn finish_login_with_authorization_code(
        &self,
        auth_response: AuthorizationResponse,
    ) -> Result<AccessTokenResponse, OidcError> {
        let response =
            self.oidc.finish_authorization_with_authorization_code(auth_response).await?;
        self.oidc.load_session_meta().await?;

        Ok(response)
    }

    /// Login via the Client Credentials flow.
    ///
    /// # Arguments
    ///
    /// * `scope` - The scope to authorize.
    ///
    /// # Example
    ///
    /// ```no_run
    /// use matrix_sdk::{Client, Error};
    /// # use futures::executor::block_on;
    /// # use url::Url;
    /// # let homeserver = Url::parse("https://example.com").unwrap();
    /// # block_on(async {
    /// # let client = Client::new(homeserver).await?;
    /// let oidc = client.oidc();
    ///
    /// let _response = oidc.login_with_client_credentials().await?;
    ///
    /// // The access and refresh tokens can be persisted either from the response,
    /// // or from one of the `Client::session_tokens*` methods.
    ///
    /// // You can now make any request compatible with the requested scope.
    /// let _me = client.whoami().await?;
    /// # anyhow::Ok(()) })
    /// ```
    #[instrument(target = "matrix_sdk::client", skip_all)]
    pub async fn login_with_client_credentials(self) -> Result<AccessTokenResponse, OidcError> {
        let issuer = self.oidc.issuer().await.ok_or(OidcError::MissingAuthenticationIssuer)?;
        info!(issuer, "Logging in via OpenID Connect with the Client Credentials flow");

        let scope = generate_scope(self.device_id)?;

        let response = self.oidc.authorize_scope_with_client_credentials(scope).await?;

        self.oidc.load_session_meta().await?;

        Ok(response)
    }
}

#[derive(Debug)]
pub(crate) struct OidcData {
    pub(crate) credentials: ClientCredentials,
    pub(crate) metadata: VerifiedClientMetadata,
    pub(crate) latest_id_token: RwLock<Option<IdToken<'static>>>,
}

/// The data needed to perform authorization using OpenID Connect.
#[derive(Debug, Clone)]
pub struct OidcAuthorizationData {
    /// The URL that should be presented.
    pub url: Url,
    /// A unique identifier for the request.
    pub state: String,
}

fn generate_scope(device_id: Option<String>) -> Result<Scope, OidcError> {
    let device_id = if let Some(device_id) = device_id {
        device_id
    } else {
        use rand::Rng;

        rand::thread_rng()
            .sample_iter(&rand::distributions::Alphanumeric)
            .map(char::from)
            .take(10)
            .collect::<String>()
    };

    Ok([
        ScopeToken::Openid,
        ScopeToken::MatrixApi,
        ScopeToken::try_with_matrix_device(device_id).or(Err(OidcError::InvalidDeviceId))?,
    ]
    .into_iter()
    .collect())
}

/// The data returned by the provider in the redirect URI after a successful
/// authorization.
#[derive(Debug, Deserialize)]
pub struct AuthorizationResponse {
    /// The code to use to retrieve the access token.
    pub code: String,
    /// The unique identifier for this transaction.
    pub state: String,
}

impl AuthorizationResponse {
    /// Deserialize an `AuthorizationResponse` from the given URI.
    ///
    /// If deserialization fails, the query should contain an
    /// [`AuthorizationError`].
    pub fn parse_uri(uri: &Url) -> Result<Self, RedirectUriQueryParseError> {
        let Some(query) = uri.query() else { return Err(RedirectUriQueryParseError::MissingQuery) };

        Ok(Self::parse_query(query)?)
    }

    /// Deserialize an `AuthorizationResponse` from the query part of a URI.
    ///
    /// If this fails, the query should contain an [`AuthorizationError`].
    pub fn parse_query(query: &str) -> Result<Self, serde_html_form::de::Error> {
        serde_html_form::from_str(query)
    }
}

/// The data returned by the provider in the redirect URI after an authorization
/// error.
#[derive(Debug, Deserialize)]
pub struct AuthorizationError {
    /// The error.
    #[serde(flatten)]
    pub error: ClientError,
    /// The unique identifier for this transaction.
    pub state: String,
}

impl AuthorizationError {
    /// Deserialize an `AuthorizationError` from the given URI.
    pub fn parse_uri(uri: &Url) -> Result<Self, RedirectUriQueryParseError> {
        let Some(query) = uri.query() else { return Err(RedirectUriQueryParseError::MissingQuery) };

        Ok(Self::parse_query(query)?)
    }

    /// Deserialize an `AuthorizationError` from the query part of a URI.
    pub fn parse_query(query: &str) -> Result<Self, serde_html_form::de::Error> {
        serde_html_form::from_str(query)
    }
}

/// An error when trying to parse the query of a redirect URI.
#[derive(Debug, Clone, Error)]
pub enum RedirectUriQueryParseError {
    /// There is no query part in the URI.
    #[error("No query in URI")]
    MissingQuery,

    /// Deserialization failed.
    #[error(transparent)]
    HtmlForm(#[from] serde_html_form::de::Error),
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

    /// The client tried to refresh an access token when no refresh token is
    /// available.
    #[error("missing refresh token")]
    MissingRefreshToken,

    /// The device ID is invalid.
    #[error("invalid device ID")]
    InvalidDeviceId,

    /// The OpenID Connect Provider doesn't support token revocation, aka
    /// logging out.
    #[error("no token revocation support")]
    NoRevocationSupport,

    /// An error occurred setting up the Client.
    #[error(transparent)]
    Sdk(#[from] crate::Error),

    /// An unkown error occurred.
    #[error("unknown error: {0}")]
    UnknownError(Box<dyn std::error::Error + Send + Sync>),
}

impl<E> From<E> for OidcError
where
    E: Into<mas_oidc_client::error::Error>,
{
    fn from(value: E) -> Self {
        Self::Oidc(value.into())
    }
}
