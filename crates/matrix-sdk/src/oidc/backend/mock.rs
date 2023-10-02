// Copyright 2023 The Matrix.org Foundation C.I.C.
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
// See the License for that specific language governing permissions and
// limitations under the License.

//! Test implementation of the OIDC backend.

use std::sync::{Arc, Mutex};

use http::StatusCode;
use mas_oidc_client::{
    error::{
        DiscoveryError,
        Error::{self as OidcClientError, Discovery},
        ErrorBody as OidcErrorBody, HttpError as OidcHttpError, TokenRefreshError,
        TokenRequestError,
    },
    requests::authorization_code::{AuthorizationRequestData, AuthorizationValidationData},
    types::{
        client_credentials::ClientCredentials,
        errors::ClientErrorCode,
        iana::oauth::OAuthTokenTypeHint,
        oidc::{ProviderMetadata, ProviderMetadataVerificationError, VerifiedProviderMetadata},
        registration::{ClientRegistrationResponse, VerifiedClientMetadata},
        IdToken,
    },
};
use url::Url;

use super::{OidcBackend, OidcError, RefreshedSessionTokens};
use crate::oidc::{AuthorizationCode, OidcSessionTokens};

pub(crate) const ISSUER_URL: &str = "https://oidc.example.com/issuer";
pub(crate) const AUTHORIZATION_URL: &str = "https://oidc.example.com/authorization";
pub(crate) const REVOCATION_URL: &str = "https://oidc.example.com/revocation";
pub(crate) const TOKEN_URL: &str = "https://oidc.example.com/token";
pub(crate) const JWKS_URL: &str = "https://oidc.example.com/jwks";

#[derive(Debug)]
pub(crate) struct MockImpl {
    /// Must be an HTTPS URL.
    issuer: String,

    /// Must be an HTTPS URL.
    authorization_endpoint: String,

    /// Must be an HTTPS URL.
    token_endpoint: String,

    /// Must be an HTTPS URL.
    jwks_uri: String,

    /// Must be an HTTPS URL.
    revocation_endpoint: String,

    /// The next session tokens that will be returned by a login or refresh.
    next_session_tokens: Option<OidcSessionTokens>,

    /// The next refresh token that's expected for a refresh.
    expected_refresh_token: Option<String>,

    /// Number of refreshes that effectively happened.
    pub num_refreshes: Arc<Mutex<u32>>,

    /// Tokens that have been revoked with `revoke_token`.
    pub revoked_tokens: Arc<Mutex<Vec<String>>>,

    /// Should we only accept insecure flags during discovery?
    is_insecure: bool,
}

impl MockImpl {
    pub fn new() -> Self {
        Self {
            issuer: ISSUER_URL.to_owned(),
            authorization_endpoint: AUTHORIZATION_URL.to_owned(),
            token_endpoint: TOKEN_URL.to_owned(),
            jwks_uri: JWKS_URL.to_owned(),
            revocation_endpoint: REVOCATION_URL.to_owned(),
            next_session_tokens: None,
            expected_refresh_token: None,
            num_refreshes: Default::default(),
            revoked_tokens: Default::default(),
            is_insecure: false,
        }
    }

    pub fn next_session_tokens(mut self, next_session_tokens: OidcSessionTokens) -> Self {
        self.next_session_tokens = Some(next_session_tokens);
        self
    }

    pub fn expected_refresh_token(mut self, refresh_token: String) -> Self {
        self.expected_refresh_token = Some(refresh_token);
        self
    }

    pub fn mark_insecure(mut self) -> Self {
        self.is_insecure = true;
        self
    }
}

#[async_trait::async_trait]
impl OidcBackend for MockImpl {
    async fn discover(
        &self,
        issuer: &str,
        insecure: bool,
    ) -> Result<VerifiedProviderMetadata, OidcError> {
        if insecure != self.is_insecure {
            return Err(OidcError::Oidc(Discovery(DiscoveryError::Validation(
                ProviderMetadataVerificationError::UrlNonHttpsScheme(
                    "mocking backend",
                    Url::parse(&self.issuer).unwrap(),
                ),
            ))));
        }

        Ok(ProviderMetadata {
            issuer: Some(self.issuer.clone()),
            authorization_endpoint: Some(Url::parse(&self.authorization_endpoint).unwrap()),
            revocation_endpoint: Some(Url::parse(&self.revocation_endpoint).unwrap()),
            token_endpoint: Some(Url::parse(&self.token_endpoint).unwrap()),
            jwks_uri: Some(Url::parse(&self.jwks_uri).unwrap()),
            response_types_supported: Some(vec![]),
            subject_types_supported: Some(vec![]),
            id_token_signing_alg_values_supported: Some(vec![]),
            ..Default::default()
        }
        .validate(issuer)
        .map_err(DiscoveryError::from)?)
    }

    async fn trade_authorization_code_for_tokens(
        &self,
        _provider_metadata: VerifiedProviderMetadata,
        _credentials: ClientCredentials,
        _metadata: VerifiedClientMetadata,
        _auth_code: AuthorizationCode,
        _validation_data: AuthorizationValidationData,
    ) -> Result<OidcSessionTokens, OidcError> {
        Ok(self
            .next_session_tokens
            .as_ref()
            .expect("missing next session tokens in testing")
            .clone())
    }

    async fn register_client(
        &self,
        _registration_endpoint: &Url,
        _client_metadata: VerifiedClientMetadata,
        _software_statement: Option<String>,
    ) -> Result<ClientRegistrationResponse, OidcError> {
        todo!()
    }

    async fn build_par_authorization_url(
        &self,
        _client_credentials: ClientCredentials,
        _par_endpoint: &Url,
        _authorization_endpoint: Url,
        _authorization_data: AuthorizationRequestData,
    ) -> Result<(Url, AuthorizationValidationData), OidcError> {
        todo!()
    }

    async fn revoke_token(
        &self,
        _client_credentials: ClientCredentials,
        _revocation_endpoint: &Url,
        token: String,
        _token_type_hint: Option<OAuthTokenTypeHint>,
    ) -> Result<(), OidcError> {
        self.revoked_tokens.lock().unwrap().push(token);
        Ok(())
    }

    async fn refresh_access_token(
        &self,
        _provider_metadata: VerifiedProviderMetadata,
        _credentials: ClientCredentials,
        _metadata: &VerifiedClientMetadata,
        refresh_token: String,
        _latest_id_token: Option<IdToken<'static>>,
    ) -> Result<RefreshedSessionTokens, OidcError> {
        if Some(refresh_token) != self.expected_refresh_token {
            Err(OidcError::Oidc(OidcClientError::TokenRefresh(TokenRefreshError::Token(
                TokenRequestError::Http(OidcHttpError {
                    body: Some(OidcErrorBody {
                        error: ClientErrorCode::InvalidGrant,
                        error_description: None,
                    }),
                    status: StatusCode::from_u16(400).unwrap(),
                }),
            ))))
        } else {
            *self.num_refreshes.lock().unwrap() += 1;
            let next_tokens = self.next_session_tokens.clone().expect("missing next tokens");
            Ok(RefreshedSessionTokens {
                access_token: next_tokens.access_token,
                refresh_token: next_tokens.refresh_token,
            })
        }
    }
}
