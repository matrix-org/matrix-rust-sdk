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

#![allow(dead_code)]

use matrix_sdk_base::crypto::qr_login::{QrCodeData, QrCodeMode};
use openidconnect::{
    core::{
        CoreAuthDisplay, CoreClaimName, CoreClaimType, CoreClient, CoreClientAuthMethod,
        CoreDeviceAuthorizationResponse, CoreGrantType, CoreJsonWebKey, CoreJsonWebKeyType,
        CoreJsonWebKeyUse, CoreJweContentEncryptionAlgorithm, CoreJweKeyManagementAlgorithm,
        CoreJwsSigningAlgorithm, CoreResponseMode, CoreResponseType, CoreSubjectIdentifierType,
    },
    AdditionalProviderMetadata, AuthType, ClientId, DeviceAuthorizationUrl, HttpRequest,
    HttpResponse, IssuerUrl, ProviderMetadata, Scope,
};
use thiserror::Error;
use url::Url;
use vodozemac::secure_channel::SecureChannelError as EciesError;

use crate::{
    authentication::qrcode::messages::QrAuthMessage, http_client::HttpClient, oidc::OidcSession,
    Client, HttpError,
};

mod messages;
mod rendezvous_channel;
mod requests;
pub mod secure_channel;

#[derive(Debug, Error)]
pub enum Error {
    #[error(transparent)]
    Http(#[from] HttpError),
    #[error(transparent)]
    UrlParse(#[from] url::ParseError),
    #[error(transparent)]
    Ecies(#[from] EciesError),
}

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

pub struct AuthGrandDings {
    http_client: HttpClient,
    auth_issuer: Url,
    secure_channel: secure_channel::EstablishedSecureChannel,
}

impl AuthGrandDings {
    pub async fn from_qr_code(client: Client, data: QrCodeData) -> Result<Client, ()> {
        let http_client = client.inner.http_client.clone();

        let mut channel = secure_channel::EstablishedSecureChannel::from_qr_code(
            http_client.inner,
            &data,
            QrCodeMode::Login,
        )
        .await
        .unwrap();

        // TODO: Show the check code here.

        let foo = client.oidc().fetch_authentication_issuer().await.unwrap();

        if let Some(issuer) = foo {
            let url = IssuerUrl::from_url(issuer);
            let http_client = &client.inner.http_client.inner;

            let provider_metadata =
                DeviceProviderMetadata::discover_async(url, |request: HttpRequest| async {
                    let method = request.method;
                    let url = request.url;

                    let response = http_client.request(method, url).send().await.unwrap();
                    let status_code = response.status();
                    let headers = response.headers().clone();
                    let body = response.bytes().await.unwrap().to_vec();

                    Ok::<_, crate::Error>(HttpResponse { status_code, headers, body })
                })
                .await
                .unwrap();

            let client_id = ClientId::new("foobar".to_owned());

            let device_authorization_endpoint =
                provider_metadata.additional_metadata().device_authorization_endpoint.clone();

            // TODO: Which AuthType do we use? Do I set a client secret?
            let oidc_client =
                CoreClient::from_provider_metadata(provider_metadata, client_id, None)
                    .set_device_authorization_uri(device_authorization_endpoint)
                    .set_auth_type(AuthType::BasicAuth);

            // Do I need to add a scope here?
            let details: CoreDeviceAuthorizationResponse = oidc_client
                .exchange_device_code()
                .unwrap()
                // TODO: Is the scope correct or do i need to fish it out of the provider metadata.
                .add_scope(Scope::new("openid".to_owned()))
                .request_async(|request| async {
                    let method = request.method;
                    let url = request.url;

                    let response = http_client.request(method, url).send().await.unwrap();
                    let status_code = response.status();
                    let headers = response.headers().clone();
                    let body = response.bytes().await.unwrap().to_vec();

                    Ok::<_, crate::Error>(HttpResponse { status_code, headers, body })
                })
                .await
                .unwrap();

            // TODO: We need to ensure in the builder that this exists.
            let device_id = client.encryption().curve25519_key().await.unwrap();

            let message = messages::QrAuthMessage::LoginProtocol {
                device_id,
                device_authorization_grant: messages::AuthorizationGrant {
                    verification_uri: details.verification_uri().to_owned(),
                    verification_uri_complete: details
                        .verification_uri_complete()
                        .map(|u| u.to_owned()),
                },
                // TODO WHAT GOES HERE.
                protocol: 10,
            };

            let message = serde_json::to_string(&message).unwrap();

            channel.send(&message).await.unwrap();

            let message = channel.receive().await.unwrap();

            let message: QrAuthMessage = serde_json::from_str(&message).unwrap();

            let QrAuthMessage::LoginProtocolAccepted() = message else { todo!() };
            let token = oidc_client
                .exchange_device_access_token(&details)
                .request_async(
                    |request| async {
                        let method = request.method;
                        let url = request.url;

                        let response = http_client.request(method, url).send().await.unwrap();
                        let status_code = response.status();
                        let headers = response.headers().clone();
                        let body = response.bytes().await.unwrap().to_vec();

                        Ok::<_, crate::Error>(HttpResponse { status_code, headers, body })
                    },
                    tokio::time::sleep,
                    None,
                )
                .await
                .unwrap();

            // TODO: How do I construct whatever is needed to get the session restored from
            // the `token` above.
            let session: OidcSession = todo!();
            client.restore_session(session);

            // Tell the existing device that we're logged in.
            let message = QrAuthMessage::LoginSuccess();
            let message = serde_json::to_string(&message).unwrap();

            channel.send(&message).await.unwrap();

            let message = channel.receive().await.unwrap();
            let message = serde_json::from_str(&message).unwrap();

            let QrAuthMessage::LoginSecrets(bundle) = message else {
                todo!();
            };

            // Upload the device keys and stuff.
            client.encryption().import_secrets_bundle(&bundle).await.unwrap();
            client.encryption().run_initialization_tasks(None).await.unwrap();

            Ok(client)
        } else {
            todo!()
        }
    }
}
