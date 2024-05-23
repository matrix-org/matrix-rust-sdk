// Copyright 2024 The Matrix.org Foundation C.I.C.
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
use mas_oidc_client::types::scope::{MatrixApiScopeToken, ScopeToken};
use openidconnect::{
    core::{
        CoreAuthDisplay, CoreAuthPrompt, CoreClaimName, CoreClaimType, CoreClient,
        CoreClientAuthMethod, CoreDeviceAuthorizationResponse, CoreErrorResponseType,
        CoreGenderClaim, CoreGrantType, CoreJsonWebKey, CoreJweContentEncryptionAlgorithm,
        CoreJweKeyManagementAlgorithm, CoreResponseMode, CoreResponseType, CoreRevocableToken,
        CoreRevocationErrorResponse, CoreSubjectIdentifierType, CoreTokenIntrospectionResponse,
        CoreTokenResponse,
    },
    AdditionalProviderMetadata, AuthType, ClientId, ClientSecret, DeviceAuthorizationUrl,
    EmptyAdditionalClaims, EndpointMaybeSet, EndpointNotSet, EndpointSet, HttpClientError,
    HttpRequest, IssuerUrl, OAuth2TokenResponse, ProviderMetadata, Scope, StandardErrorResponse,
};
use vodozemac::Curve25519PublicKey;

use super::DeviceAuhorizationOidcError;
use crate::{http_client::HttpClient, oidc::OidcSessionTokens};

// Obtain the device_authorization_url from the OIDC metadata provider.
#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
struct DeviceEndpointProviderMetadata {
    device_authorization_endpoint: DeviceAuthorizationUrl,
}
impl AdditionalProviderMetadata for DeviceEndpointProviderMetadata {}

type DeviceProviderMetadata = ProviderMetadata<
    DeviceEndpointProviderMetadata,
    CoreAuthDisplay,
    CoreClientAuthMethod,
    CoreClaimName,
    CoreClaimType,
    CoreGrantType,
    CoreJweContentEncryptionAlgorithm,
    CoreJweKeyManagementAlgorithm,
    CoreJsonWebKey,
    CoreResponseMode,
    CoreResponseType,
    CoreSubjectIdentifierType,
>;

/// OpenID Connect Core client.
pub type OidcClientInner<
    HasAuthUrl = EndpointSet,
    HasDeviceAuthUrl = EndpointSet,
    HasIntrospectionUrl = EndpointNotSet,
    HasRevocationUrl = EndpointNotSet,
    HasTokenUrl = EndpointMaybeSet,
    HasUserInfoUrl = EndpointMaybeSet,
> = openidconnect::Client<
    EmptyAdditionalClaims,
    CoreAuthDisplay,
    CoreGenderClaim,
    CoreJweContentEncryptionAlgorithm,
    CoreJsonWebKey,
    CoreAuthPrompt,
    StandardErrorResponse<CoreErrorResponseType>,
    CoreTokenResponse,
    CoreTokenIntrospectionResponse,
    CoreRevocableToken,
    CoreRevocationErrorResponse,
    HasAuthUrl,
    HasDeviceAuthUrl,
    HasIntrospectionUrl,
    HasRevocationUrl,
    HasTokenUrl,
    HasUserInfoUrl,
>;

/// An OIDC specific HTTP client.
///
/// This is used to communicate with the OIDC provider exclusively.
pub(super) struct OidcClient {
    inner: OidcClientInner,
    http_client: HttpClient,
}

impl OidcClient {
    pub(super) async fn new(
        client_id: String,
        issuer_url: String,
        http_client: HttpClient,
        client_secret: Option<&str>,
    ) -> Result<Self, DeviceAuhorizationOidcError> {
        let client_id = ClientId::new(client_id);
        let issuer_url = IssuerUrl::new(issuer_url)?;
        let client_secret = client_secret.map(|s| ClientSecret::new(s.to_owned()));

        // We're fetching the provider metadata which will contain the device
        // authorization endpoint. We can use this endpoint to attempt to log in
        // this new device, though the other, existing device will do that using the
        // verification URL.
        let provider_metadata =
            DeviceProviderMetadata::discover_async(issuer_url, &http_client).await?;
        let device_authorization_endpoint =
            provider_metadata.additional_metadata().device_authorization_endpoint.clone();

        let oidc_client =
            CoreClient::from_provider_metadata(provider_metadata, client_id.clone(), client_secret)
                .set_device_authorization_url(device_authorization_endpoint)
                .set_auth_type(AuthType::RequestBody);

        Ok(OidcClient { inner: oidc_client, http_client })
    }

    pub(super) async fn request_device_authorization(
        &self,
        device_id: Curve25519PublicKey,
    ) -> Result<CoreDeviceAuthorizationResponse, DeviceAuhorizationOidcError> {
        let scopes = [
            ScopeToken::Openid,
            ScopeToken::MatrixApi(MatrixApiScopeToken::Full),
            ScopeToken::try_with_matrix_device(device_id.to_base64()).expect(
                "We should be able to create a scope token from a \
                 Curve25519 public key encoded as base64",
            ),
        ]
        .into_iter()
        .map(|scope| Scope::new(scope.to_string()));

        let details: CoreDeviceAuthorizationResponse = self
            .inner
            .exchange_device_code()
            .add_scopes(scopes)
            .request_async(&self.http_client)
            .await?;

        Ok(details)
    }

    pub(super) async fn wait_for_tokens(
        &self,
        details: &CoreDeviceAuthorizationResponse,
    ) -> Result<OidcSessionTokens, DeviceAuhorizationOidcError> {
        let response = self
            .inner
            .exchange_device_access_token(details)?
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

impl<'c> openidconnect::AsyncHttpClient<'c> for HttpClient {
    type Error = HttpClientError<reqwest::Error>;

    type Future = Pin<
        Box<
            dyn Future<Output = Result<openidconnect::HttpResponse, Self::Error>>
                + Send
                + Sync
                + 'c,
        >,
    >;

    fn call(&'c self, request: HttpRequest) -> Self::Future {
        Box::pin(async move {
            let response = self.inner.call(request).await?;

            Ok(response)
        })
    }
}
