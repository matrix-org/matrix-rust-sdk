// Copyright 2025 The Matrix.org Foundation C.I.C.
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

use std::pin::Pin;

use futures_core::Future;
use mas_oidc_client::types::{
    oidc::VerifiedProviderMetadata,
    scope::{MatrixApiScopeToken, ScopeToken},
};
use oauth2::{
    basic::BasicClient, ClientId, DeviceAuthorizationUrl, EndpointNotSet, EndpointSet,
    HttpClientError, HttpRequest, Scope, StandardDeviceAuthorizationResponse, TokenResponse,
    TokenUrl,
};
use vodozemac::Curve25519PublicKey;

use super::DeviceAuthorizationOauthError;
use crate::{authentication::oidc::OidcSessionTokens, http_client::HttpClient};

/// An OAuth 2.0 specific HTTP client.
///
/// This is used to communicate with the OAuth 2.0 authorization server
/// exclusively.
pub(super) struct OauthClient {
    /// Oauth 2.0 Basic client.
    inner: BasicClient<EndpointNotSet, EndpointSet, EndpointNotSet, EndpointNotSet, EndpointSet>,
    http_client: HttpClient,
}

impl OauthClient {
    pub(super) fn new(
        client_id: String,
        server_metadata: &VerifiedProviderMetadata,
        http_client: HttpClient,
    ) -> Result<Self, DeviceAuthorizationOauthError> {
        let client_id = ClientId::new(client_id);

        let token_endpoint = TokenUrl::from_url(server_metadata.token_endpoint().clone());

        // We can use the device authorization endpoint to attempt to log in this new
        // device, though the other, existing device will do that using the
        // verification URL.
        let device_authorization_endpoint = server_metadata
            .device_authorization_endpoint
            .clone()
            .map(DeviceAuthorizationUrl::from_url)
            .ok_or(DeviceAuthorizationOauthError::NoDeviceAuthorizationEndpoint)?;

        let oauth2_client = BasicClient::new(client_id)
            .set_token_uri(token_endpoint)
            .set_device_authorization_url(device_authorization_endpoint);

        Ok(Self { inner: oauth2_client, http_client })
    }

    pub(super) async fn request_device_authorization(
        &self,
        device_id: Curve25519PublicKey,
    ) -> Result<StandardDeviceAuthorizationResponse, DeviceAuthorizationOauthError> {
        let scopes = [
            ScopeToken::MatrixApi(MatrixApiScopeToken::Full),
            ScopeToken::try_with_matrix_device(device_id.to_base64()).expect(
                "We should be able to create a scope token from a \
                 Curve25519 public key encoded as base64",
            ),
        ]
        .into_iter()
        .map(|scope| Scope::new(scope.to_string()));

        let details: StandardDeviceAuthorizationResponse = self
            .inner
            .exchange_device_code()
            .add_scopes(scopes)
            .request_async(&self.http_client)
            .await?;

        Ok(details)
    }

    pub(super) async fn wait_for_tokens(
        &self,
        details: &StandardDeviceAuthorizationResponse,
    ) -> Result<OidcSessionTokens, DeviceAuthorizationOauthError> {
        let response = self
            .inner
            .exchange_device_access_token(details)
            .request_async(&self.http_client, tokio::time::sleep, None)
            .await?;

        let tokens = OidcSessionTokens {
            access_token: response.access_token().secret().to_owned(),
            refresh_token: response.refresh_token().map(|t| t.secret().to_owned()),
            latest_id_token: None,
        };

        Ok(tokens)
    }
}

impl<'c> oauth2::AsyncHttpClient<'c> for HttpClient {
    type Error = HttpClientError<reqwest::Error>;

    type Future =
        Pin<Box<dyn Future<Output = Result<oauth2::HttpResponse, Self::Error>> + Send + Sync + 'c>>;

    fn call(&'c self, request: HttpRequest) -> Self::Future {
        Box::pin(async move {
            let response = self.inner.call(request).await?;

            Ok(response)
        })
    }
}
