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

//! Actual implementation of the OIDC backend, using the mas_oidc_client
//! implementation.

use chrono::Utc;
use http::StatusCode;
use mas_oidc_client::{
    http_service::HttpService,
    jose::jwk::PublicJsonWebKeySet,
    requests::{
        authorization_code::{
            access_token_with_authorization_code, build_par_authorization_url,
            AuthorizationRequestData, AuthorizationValidationData,
        },
        discovery::{discover, insecure_discover},
        jose::{fetch_jwks, JwtVerificationData},
        refresh_token::refresh_access_token,
        registration::register_client,
        revocation::revoke_token,
    },
    types::{
        client_credentials::ClientCredentials,
        iana::oauth::OAuthTokenTypeHint,
        oidc::{ProviderMetadata, ProviderMetadataVerificationError, VerifiedProviderMetadata},
        registration::{ClientRegistrationResponse, VerifiedClientMetadata},
        IdToken,
    },
};
use ruma::api::client::discovery::{get_authentication_issuer, get_authorization_server_metadata};
use url::Url;

use super::{OidcBackend, OidcError, RefreshedSessionTokens};
use crate::{
    authentication::oidc::{rng, AuthorizationCode, OauthDiscoveryError, OidcSessionTokens},
    Client,
};

#[derive(Debug)]
pub(crate) struct OidcServer {
    client: Client,
}

impl OidcServer {
    pub(crate) fn new(client: Client) -> Self {
        Self { client }
    }

    fn http_service(&self) -> HttpService {
        HttpService::new(self.client.inner.http_client.clone())
    }

    /// Fetch the OpenID Connect JSON Web Key Set at the given URI.
    ///
    /// Returns an error if the client registration was not restored, or if an
    /// error occurred when fetching the data.
    async fn fetch_jwks(&self, uri: &Url) -> Result<PublicJsonWebKeySet, OidcError> {
        fetch_jwks(&self.http_service(), uri).await.map_err(Into::into)
    }
}

#[async_trait::async_trait]
impl OidcBackend for OidcServer {
    async fn discover(
        &self,
        insecure: bool,
    ) -> Result<VerifiedProviderMetadata, OauthDiscoveryError> {
        match self.client.send(get_authorization_server_metadata::msc2965::Request::new()).await {
            Ok(response) => {
                let metadata = response.metadata.deserialize_as::<ProviderMetadata>()?;

                let result = if insecure {
                    metadata.insecure_verify_metadata()
                } else {
                    // The mas-oidc-client method needs to compare the issuer for validation. It's a
                    // bit unnecessary because we take it from the metadata, oh well.
                    let issuer = metadata.issuer.clone().ok_or(
                        mas_oidc_client::error::DiscoveryError::Validation(
                            ProviderMetadataVerificationError::MissingIssuer,
                        ),
                    )?;
                    metadata.validate(&issuer)
                };

                return Ok(result.map_err(mas_oidc_client::error::DiscoveryError::Validation)?);
            }
            Err(error)
                if error
                    .as_client_api_error()
                    .is_some_and(|err| err.status_code == StatusCode::NOT_FOUND) =>
            {
                // Fallback to OIDC discovery.
            }
            Err(error) => return Err(error.into()),
        };

        // TODO: remove this fallback behavior when the metadata endpoint has wider
        // support.
        #[allow(deprecated)]
        let issuer =
            match self.client.send(get_authentication_issuer::msc2965::Request::new()).await {
                Ok(response) => response.issuer,
                Err(error)
                    if error
                        .as_client_api_error()
                        .is_some_and(|err| err.status_code == StatusCode::NOT_FOUND) =>
                {
                    return Err(OauthDiscoveryError::NotSupported);
                }
                Err(error) => return Err(error.into()),
            };

        if insecure {
            insecure_discover(&self.http_service(), &issuer).await.map_err(Into::into)
        } else {
            discover(&self.http_service(), &issuer).await.map_err(Into::into)
        }
    }

    async fn trade_authorization_code_for_tokens(
        &self,
        provider_metadata: VerifiedProviderMetadata,
        credentials: ClientCredentials,
        metadata: VerifiedClientMetadata,
        auth_code: AuthorizationCode,
        validation_data: AuthorizationValidationData,
    ) -> Result<OidcSessionTokens, OidcError> {
        let jwks = self.fetch_jwks(provider_metadata.jwks_uri()).await?;

        let id_token_verification_data = JwtVerificationData {
            issuer: provider_metadata.issuer(),
            jwks: &jwks,
            client_id: &credentials.client_id().to_owned(),
            signing_algorithm: metadata.id_token_signed_response_alg(),
        };

        let (response, id_token) = access_token_with_authorization_code(
            &self.http_service(),
            credentials.clone(),
            provider_metadata.token_endpoint(),
            auth_code.code,
            validation_data,
            Some(id_token_verification_data),
            Utc::now(),
            &mut rng()?,
        )
        .await?;

        Ok(OidcSessionTokens {
            access_token: response.access_token,
            refresh_token: response.refresh_token,
            latest_id_token: id_token,
        })
    }

    async fn refresh_access_token(
        &self,
        provider_metadata: VerifiedProviderMetadata,
        credentials: ClientCredentials,
        metadata: &VerifiedClientMetadata,
        refresh_token: String,
        latest_id_token: Option<IdToken<'static>>,
    ) -> Result<RefreshedSessionTokens, OidcError> {
        let jwks = self.fetch_jwks(provider_metadata.jwks_uri()).await?;

        let id_token_verification_data = JwtVerificationData {
            issuer: provider_metadata.issuer(),
            jwks: &jwks,
            client_id: &credentials.client_id().to_owned(),
            signing_algorithm: &metadata.id_token_signed_response_alg().clone(),
        };

        refresh_access_token(
            &self.http_service(),
            credentials,
            provider_metadata.token_endpoint(),
            refresh_token,
            None,
            Some(id_token_verification_data),
            latest_id_token.as_ref(),
            Utc::now(),
            &mut rng()?,
        )
        .await
        .map(|(response, _id_token)| RefreshedSessionTokens {
            access_token: response.access_token,
            refresh_token: response.refresh_token,
        })
        .map_err(Into::into)
    }

    async fn register_client(
        &self,
        registration_endpoint: &Url,
        client_metadata: VerifiedClientMetadata,
        software_statement: Option<String>,
    ) -> Result<ClientRegistrationResponse, OidcError> {
        register_client(
            &self.http_service(),
            registration_endpoint,
            client_metadata,
            software_statement,
        )
        .await
        .map_err(Into::into)
    }

    async fn build_par_authorization_url(
        &self,
        client_credentials: ClientCredentials,
        par_endpoint: &Url,
        authorization_endpoint: Url,
        authorization_data: AuthorizationRequestData,
    ) -> Result<(Url, AuthorizationValidationData), OidcError> {
        Ok(build_par_authorization_url(
            &self.http_service(),
            client_credentials,
            par_endpoint,
            authorization_endpoint,
            authorization_data,
            Utc::now(),
            &mut rng()?,
        )
        .await?)
    }

    async fn revoke_token(
        &self,
        client_credentials: ClientCredentials,
        revocation_endpoint: &Url,
        token: String,
        token_type_hint: Option<OAuthTokenTypeHint>,
    ) -> Result<(), OidcError> {
        Ok(revoke_token(
            &self.http_service(),
            client_credentials,
            revocation_endpoint,
            token,
            token_type_hint,
            Utc::now(),
            &mut rng()?,
        )
        .await?)
    }
}
