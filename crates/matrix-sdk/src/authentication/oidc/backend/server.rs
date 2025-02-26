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

use std::{future::Future, pin::Pin};

use chrono::Utc;
use http::StatusCode;
use mas_oidc_client::{
    http_service::HttpService,
    requests::{
        authorization_code::{access_token_with_authorization_code, AuthorizationValidationData},
        discovery::{discover, insecure_discover},
        refresh_token::refresh_access_token,
        registration::register_client,
        revocation::revoke_token,
    },
    types::{
        client_credentials::ClientCredentials,
        iana::oauth::OAuthTokenTypeHint,
        oidc::{ProviderMetadata, ProviderMetadataVerificationError, VerifiedProviderMetadata},
        registration::{ClientRegistrationResponse, VerifiedClientMetadata},
    },
};
use oauth2::{AsyncHttpClient, HttpClientError, HttpRequest, HttpResponse};
use ruma::api::client::discovery::{get_authentication_issuer, get_authorization_server_metadata};
use url::Url;

use super::{OidcBackend, OidcError, RefreshedSessionTokens};
use crate::{
    authentication::oidc::{rng, AuthorizationCode, OauthDiscoveryError, OidcSessionTokens},
    http_client::HttpClient,
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

    fn http_client(&self) -> &HttpClient {
        &self.client.inner.http_client
    }

    fn http_service(&self) -> HttpService {
        HttpService::new(self.http_client().clone())
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
        auth_code: AuthorizationCode,
        validation_data: AuthorizationValidationData,
    ) -> Result<OidcSessionTokens, OidcError> {
        let (response, _) = access_token_with_authorization_code(
            &self.http_service(),
            credentials.clone(),
            provider_metadata.token_endpoint(),
            auth_code.code,
            validation_data,
            None,
            Utc::now(),
            &mut rng()?,
        )
        .await?;

        Ok(OidcSessionTokens {
            access_token: response.access_token,
            refresh_token: response.refresh_token,
        })
    }

    async fn refresh_access_token(
        &self,
        provider_metadata: VerifiedProviderMetadata,
        credentials: ClientCredentials,
        refresh_token: String,
    ) -> Result<RefreshedSessionTokens, OidcError> {
        refresh_access_token(
            &self.http_service(),
            credentials,
            provider_metadata.token_endpoint(),
            refresh_token,
            None,
            None,
            None,
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

    #[cfg(all(feature = "e2e-encryption", not(target_arch = "wasm32")))]
    async fn request_device_authorization(
        &self,
        device_authorization_endpoint: Url,
        client_id: oauth2::ClientId,
        scopes: Vec<oauth2::Scope>,
    ) -> Result<
        oauth2::StandardDeviceAuthorizationResponse,
        oauth2::basic::BasicRequestTokenError<HttpClientError<reqwest::Error>>,
    > {
        let device_authorization_url =
            oauth2::DeviceAuthorizationUrl::from_url(device_authorization_endpoint);

        oauth2::basic::BasicClient::new(client_id)
            .set_device_authorization_url(device_authorization_url)
            .exchange_device_code()
            .add_scopes(scopes)
            .request_async(self.http_client())
            .await
    }

    #[cfg(all(feature = "e2e-encryption", not(target_arch = "wasm32")))]
    async fn exchange_device_code(
        &self,
        token_endpoint: Url,
        client_id: oauth2::ClientId,
        device_authorization_response: &oauth2::StandardDeviceAuthorizationResponse,
    ) -> Result<
        OidcSessionTokens,
        oauth2::RequestTokenError<HttpClientError<reqwest::Error>, oauth2::DeviceCodeErrorResponse>,
    > {
        use oauth2::TokenResponse;

        let token_uri = oauth2::TokenUrl::from_url(token_endpoint);

        let response = oauth2::basic::BasicClient::new(client_id)
            .set_token_uri(token_uri)
            .exchange_device_access_token(device_authorization_response)
            .request_async(self.http_client(), tokio::time::sleep, None)
            .await?;

        let tokens = OidcSessionTokens {
            access_token: response.access_token().secret().to_owned(),
            refresh_token: response.refresh_token().map(|t| t.secret().to_owned()),
        };

        Ok(tokens)
    }
}

impl<'c> AsyncHttpClient<'c> for HttpClient {
    type Error = HttpClientError<reqwest::Error>;

    type Future =
        Pin<Box<dyn Future<Output = Result<HttpResponse, Self::Error>> + Send + Sync + 'c>>;

    fn call(&'c self, request: HttpRequest) -> Self::Future {
        Box::pin(async move {
            let response = self.inner.call(request).await?;

            Ok(response)
        })
    }
}
