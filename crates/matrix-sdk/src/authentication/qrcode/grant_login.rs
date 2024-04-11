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

use eyeball::SharedObservable;
use futures_core::Stream;
use matrix_sdk_base::crypto::{qr_login::QrCodeData, types::SecretsBundle};

use super::messages::AuthorizationGrant;
use crate::{
    authentication::qrcode::{
        messages::QrAuthMessage,
        secure_channel::{AlmostEstablishedSecureChannel, SecureChannel},
        Error,
    },
    Client,
};

#[derive(Default)]
pub enum Channel {
    #[default]
    InProgress,
    Insecure(SecureChannel),
    Almost(AlmostEstablishedSecureChannel),
}

pub struct ExistingAuthGrantDings {
    client: Client,
    secrets_bundle: SecretsBundle,
    channel: Channel,
    qr_code_data: QrCodeData,
    state: SharedObservable<State>,
}

#[derive(Debug, Default, Clone)]
pub enum State {
    #[default]
    Created,
    WaitingForCheckCode,
    WaitingForAuth {
        device_authorization_grant: AuthorizationGrant,
    },
    Done,
}

impl ExistingAuthGrantDings {
    /// Subscribe to the progress of the QR code login.
    pub fn subscribe_to_progress(&self) -> impl Stream<Item = State> {
        self.state.subscribe()
    }

    pub(crate) async fn new(client: Client) -> Result<Self, ()> {
        let homeserver_url = client.homeserver();
        let http_client = client.inner.http_client.clone();

        let secrets_bundle = client
            .olm_machine()
            .await
            .as_ref()
            .unwrap()
            .store()
            .export_secrets_bundle()
            .await
            .unwrap();

        let channel = SecureChannel::reciprocate(http_client, &homeserver_url).await.unwrap();
        let qr_code_data = channel.qr_code_data().clone();
        let channel = Channel::Insecure(channel);

        Ok(Self { client, channel, qr_code_data, secrets_bundle, state: Default::default() })
    }

    pub fn qr_code_data(&self) -> &QrCodeData {
        &self.qr_code_data
    }

    pub async fn wait_for_scan(&mut self) -> Result<(), Error> {
        let channel = std::mem::take(&mut self.channel);

        if let Channel::Insecure(channel) = channel {
            self.channel = Channel::Almost(channel.connect().await.unwrap());
        } else {
            todo!()
        }

        Ok(())
    }

    pub async fn confirm_check_code(&mut self, check_code: u8) -> Result<(), Error> {
        let channel = std::mem::take(&mut self.channel);

        if let Channel::Almost(channel) = channel {
            let mut channel = channel.confirm(check_code).unwrap();

            let message = channel.receive_json().await.unwrap();

            let QrAuthMessage::LoginProtocol { device_authorization_grant, protocol, device_id } =
                message
            else {
                todo!()
            };

            if protocol != "device_authorization_grant" {
                todo!()
            }

            // TODO: Check that the device id doesn't exist.
            let message = QrAuthMessage::LoginProtocolAccepted();
            let message = serde_json::to_string(&message).unwrap();
            channel.send(&message).await.unwrap();

            println!(
                "Please open the following URL to authenticate the new device {}",
                device_authorization_grant.verification_uri_complete.as_ref().unwrap().secret()
            );

            self.state.set(State::WaitingForAuth { device_authorization_grant });

            let message = channel.receive().await.unwrap();
            let message: QrAuthMessage = serde_json::from_str(&message).unwrap();

            let QrAuthMessage::LoginSuccess() = message else {
                todo!("Received invalid message {message:?}")
            };

            let message = QrAuthMessage::LoginSecrets(self.secrets_bundle.clone());
            let message = serde_json::to_string(&message).unwrap();
            channel.send(&message).await.unwrap();

            // TODO: Check that the device ID exists.

            self.state.set(State::Done);

            Ok(())
        } else {
            todo!()
        }
    }
}
