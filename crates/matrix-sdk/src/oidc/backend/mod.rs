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

//! Trait for defining an implementation for an OIDC backend.
//!
//! Used mostly for testing purposes.

use mas_oidc_client::{
    requests::authorization_code::{AuthorizationRequestData, AuthorizationValidationData},
    types::{
        client_credentials::ClientCredentials,
        iana::oauth::OAuthTokenTypeHint,
        oidc::VerifiedProviderMetadata,
        registration::{ClientRegistrationResponse, VerifiedClientMetadata},
        IdToken,
    },
};
use url::Url;

use super::{AuthorizationCode, OidcError, OidcSessionTokens};

pub(crate) mod server;

#[cfg(test)]
pub(crate) mod mock;

pub(super) struct RefreshedSessionTokens {
    pub access_token: String,
    pub refresh_token: Option<String>,
}

#[async_trait::async_trait]
pub(super) trait OidcBackend: std::fmt::Debug + Send + Sync {
    async fn discover(&self, issuer: &str) -> Result<VerifiedProviderMetadata, OidcError>;

    async fn register_client(
        &self,
        registration_endpoint: &Url,
        client_metadata: VerifiedClientMetadata,
        software_statement: Option<String>,
    ) -> Result<ClientRegistrationResponse, OidcError>;

    async fn trade_authorization_code_for_tokens(
        &self,
        provider_metadata: VerifiedProviderMetadata,
        credentials: ClientCredentials,
        metadata: VerifiedClientMetadata,
        auth_code: AuthorizationCode,
        validation_data: AuthorizationValidationData,
    ) -> Result<OidcSessionTokens, OidcError>;

    async fn refresh_access_token(
        &self,
        provider_metadata: VerifiedProviderMetadata,
        credentials: ClientCredentials,
        metadata: &VerifiedClientMetadata,
        refresh_token: String,
        latest_id_token: Option<IdToken<'static>>,
    ) -> Result<RefreshedSessionTokens, OidcError>;

    async fn build_par_authorization_url(
        &self,
        client_credentials: ClientCredentials,
        par_endpoint: &Url,
        authorization_endpoint: Url,
        authorization_data: AuthorizationRequestData,
    ) -> Result<(Url, AuthorizationValidationData), OidcError>;

    async fn revoke_token(
        &self,
        client_credentials: ClientCredentials,
        revocation_endpoint: &Url,
        token: String,
        token_type_hint: Option<OAuthTokenTypeHint>,
    ) -> Result<(), OidcError>;
}
