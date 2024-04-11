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

use std::{future::IntoFuture, pin::Pin};

use eyeball::SharedObservable;
use futures_core::{Future, Stream};
use mas_oidc_client::types::{
    client_credentials::ClientCredentials,
    registration::VerifiedClientMetadata,
    scope::{MatrixApiScopeToken, ScopeToken},
};
use matrix_sdk_base::{
    crypto::qr_login::{QrCodeData, QrCodeMode},
    SessionMeta,
};
use openidconnect::{
    core::{
        CoreAuthDisplay, CoreClaimName, CoreClaimType, CoreClient, CoreClientAuthMethod,
        CoreDeviceAuthorizationResponse, CoreGrantType, CoreJsonWebKey, CoreJsonWebKeyType,
        CoreJsonWebKeyUse, CoreJweContentEncryptionAlgorithm, CoreJweKeyManagementAlgorithm,
        CoreJwsSigningAlgorithm, CoreResponseMode, CoreResponseType, CoreSubjectIdentifierType,
    },
    reqwest::Proxy,
    AdditionalProviderMetadata, AuthType, ClientId, ClientSecret, DeviceAuthorizationUrl,
    EndpointMaybeSet, EndpointNotSet, EndpointSet, IssuerUrl, OAuth2TokenResponse,
    ProviderMetadata, Scope,
};
use ruma::{api::client::discovery::discover_homeserver::AuthenticationServerInfo, OwnedDeviceId};
use vodozemac::{secure_channel::CheckCode, Curve25519PublicKey, Curve25519SecretKey};

use crate::{
    authentication::qrcode::{
        messages::QrAuthMessage, secure_channel::EstablishedSecureChannel, Error,
    },
    oidc::OidcSessionTokens,
    Client,
};

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
    CoreJwsSigningAlgorithm,
    CoreJsonWebKeyType,
    CoreJsonWebKeyUse,
    CoreJsonWebKey,
    CoreResponseMode,
    CoreResponseType,
    CoreSubjectIdentifierType,
>;

type OidcClientInner<
    HasAuthUrl = EndpointSet,
    HasDeviceAuthUrl = EndpointSet,
    HasIntrospectionUrl = EndpointNotSet,
    HasRevocationUrl = EndpointNotSet,
    HasTokenUrl = EndpointMaybeSet,
    HasUserInfoUrl = EndpointMaybeSet,
> = openidconnect::Client<
    openidconnect::EmptyAdditionalClaims,
    CoreAuthDisplay,
    openidconnect::core::CoreGenderClaim,
    CoreJweContentEncryptionAlgorithm,
    CoreJwsSigningAlgorithm,
    CoreJsonWebKeyType,
    CoreJsonWebKeyUse,
    CoreJsonWebKey,
    openidconnect::core::CoreAuthPrompt,
    openidconnect::StandardErrorResponse<openidconnect::core::CoreErrorResponseType>,
    openidconnect::core::CoreTokenResponse,
    openidconnect::core::CoreTokenType,
    openidconnect::core::CoreTokenIntrospectionResponse,
    openidconnect::core::CoreRevocableToken,
    openidconnect::core::CoreRevocationErrorResponse,
    HasAuthUrl,
    HasDeviceAuthUrl,
    HasIntrospectionUrl,
    HasRevocationUrl,
    HasTokenUrl,
    HasUserInfoUrl,
>;

pub struct OidcClient {
    inner: OidcClientInner,
    http_client: openidconnect::reqwest::Client,
}

impl OidcClient {
    async fn request_device_authorization(
        &self,
        device_id: Curve25519PublicKey,
    ) -> Result<CoreDeviceAuthorizationResponse, Error> {
        let scopes = [
            ScopeToken::Openid,
            ScopeToken::MatrixApi(MatrixApiScopeToken::Full),
            ScopeToken::try_with_matrix_device(device_id.to_base64()).unwrap(),
        ]
        .into_iter()
        .map(|scope| Scope::new(scope.to_string()));

        let details: CoreDeviceAuthorizationResponse = self
            .inner
            .exchange_device_code()
            .add_scopes(scopes)
            .request_async(&self.http_client)
            .await
            .unwrap();

        Ok(details)
    }

    async fn wait_for_tokens(
        &self,
        details: &CoreDeviceAuthorizationResponse,
    ) -> Result<OidcSessionTokens, Error> {
        let response = self
            .inner
            .exchange_device_access_token(&details)
            .unwrap()
            .request_async(&self.http_client, tokio::time::sleep, None)
            .await
            .unwrap();

        let tokens = OidcSessionTokens {
            access_token: response.access_token().secret().to_owned(),
            refresh_token: response.refresh_token().map(|t| t.secret().to_owned()),
            // TODO: How do we convert this into the appropriate type?
            // latest_id_token: response.id_token(),
            latest_id_token: None,
        };

        Ok(tokens)
    }
}

#[derive(Clone, Debug, Default)]
pub enum LoginProgress {
    #[default]
    Starting,
    EstablishingSecureChannel {
        check_code: CheckCode,
    },
    WaitingForToken,
    Done,
}

/// Named future for the [`Backups::wait_for_steady_state()`] method.
#[derive(Debug)]
pub struct LoginWithQrCode<'a> {
    client: &'a Client,
    client_metadata: VerifiedClientMetadata,
    qr_code_data: &'a QrCodeData,
    state: SharedObservable<LoginProgress>,
}

impl<'a> LoginWithQrCode<'a> {
    /// Subscribe to the progress of the backup upload step while waiting for it
    /// to settle down.
    pub fn subscribe_to_progress(&self) -> impl Stream<Item = LoginProgress> {
        self.state.subscribe()
    }
}

impl<'a> IntoFuture for LoginWithQrCode<'a> {
    type Output = Result<(), Error>;
    type IntoFuture = Pin<Box<dyn Future<Output = Self::Output> + 'a>>;

    fn into_future(self) -> Self::IntoFuture {
        Box::pin(async move {
            let mut channel = self.establish_channel().await?;

            let check_code = channel.check_code().to_owned();
            self.state.set(LoginProgress::EstablishingSecureChannel { check_code });

            let oidc_client = self.register_client().await?;

            // TODO: Create a `vodozemac::Account` which we'll feed into a new `OlmMachine`
            // constructor.
            let secret_key = Curve25519SecretKey::new();
            let public_key = Curve25519PublicKey::from(&secret_key);
            let device_id = public_key;

            let auth_grant_response = oidc_client.request_device_authorization(device_id).await?;

            let message = QrAuthMessage::login_protocols((&auth_grant_response).into(), device_id);
            channel.send_json(&message).await.unwrap();

            let message = channel.receive_json().await.unwrap();
            let QrAuthMessage::LoginProtocolAccepted() = message else { todo!() };

            self.state.set(LoginProgress::WaitingForToken);

            let session_tokens = oidc_client.wait_for_tokens(&auth_grant_response).await?;
            self.client.oidc().set_session_tokens(session_tokens);
            let whoami_response = self.client.whoami().await.map_err(crate::Error::from).unwrap();

            self.client
                .set_session_meta(SessionMeta {
                    user_id: whoami_response.user_id,
                    device_id: OwnedDeviceId::from(device_id.to_base64()),
                })
                .await
                .unwrap();

            // Tell the existing device that we're logged in.
            let message = QrAuthMessage::LoginSuccess();
            channel.send_json(&message).await.unwrap();

            let message = channel.receive_json().await.unwrap();
            let QrAuthMessage::LoginSecrets(bundle) = message else {
                todo!();
            };

            // Upload the device keys and stuff.
            self.client.encryption().import_secrets_bundle(&bundle).await.unwrap();
            self.client.encryption().run_initialization_tasks(None).await.unwrap();

            self.state.set(LoginProgress::Done);

            Ok(())
        })
    }
}

impl<'a> LoginWithQrCode<'a> {
    pub(crate) fn new(
        client: &'a Client,
        client_metadata: VerifiedClientMetadata,
        qr_code_data: &'a QrCodeData,
    ) -> LoginWithQrCode<'a> {
        LoginWithQrCode { client, client_metadata, qr_code_data, state: Default::default() }
    }

    async fn establish_channel(&self) -> Result<EstablishedSecureChannel, Error> {
        let http_client = self.client.inner.http_client.inner.clone();

        let channel = EstablishedSecureChannel::from_qr_code(
            http_client,
            &self.qr_code_data,
            QrCodeMode::Login,
        )
        .await
        .unwrap();

        Ok(channel)
    }

    async fn register_client(&self) -> Result<OidcClient, Error> {
        // Let's figure out the OIDC issuer, this fetches the info from the homeserver.
        let issuer = self.client.oidc().fetch_authentication_issuer().await.unwrap();
        // TODO: How do I get the account management URL.
        let issuer_info = AuthenticationServerInfo::new(issuer, None);

        let registration_response = self
            .client
            .oidc()
            .register_client(&issuer_info.issuer, self.client_metadata.clone(), None)
            .await
            .unwrap();

        let client_secret = registration_response.client_secret.map(ClientSecret::new);
        let client_id = ClientId::new(registration_response.client_id);
        let issuer_url = IssuerUrl::new(issuer_info.issuer.clone()).unwrap();

        // Let's put the relevant data we got from the `register_client()` request into
        // the `Client`, why isn't `register_client()` doing this automagically?
        self.client.oidc().restore_registered_client(
            issuer_info,
            self.client_metadata.clone(),
            ClientCredentials::None { client_id: client_id.as_str().to_owned() },
        );

        // TODO: How do we use our own reqwest client here...
        // let http_client = self.client.inner.http_client.inner.clone();
        let http_client = openidconnect::reqwest::Client::builder()
            .proxy(Proxy::all("http://localhost:8011").unwrap())
            .danger_accept_invalid_certs(true)
            .build()
            .unwrap();

        let provider_metadata =
            DeviceProviderMetadata::discover_async(issuer_url, &http_client).await.unwrap();

        let device_authorization_endpoint =
            provider_metadata.additional_metadata().device_authorization_endpoint.clone();

        let oidc_client =
            CoreClient::from_provider_metadata(provider_metadata, client_id.clone(), client_secret)
                .set_device_authorization_url(device_authorization_endpoint)
                .set_auth_type(AuthType::RequestBody);

        Ok(OidcClient { inner: oidc_client, http_client })
    }
}
