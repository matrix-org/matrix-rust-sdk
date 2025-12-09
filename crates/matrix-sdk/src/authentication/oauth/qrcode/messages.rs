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

use matrix_sdk_base::crypto::types::SecretsBundle;
use matrix_sdk_common::deserialized_responses::PrivOwnedStr;
use oauth2::{
    EndUserVerificationUrl, StandardDeviceAuthorizationResponse, VerificationUriComplete,
};
use ruma::serde::StringEnum;
use serde::{Deserialize, Serialize};
use url::Url;
use vodozemac::Curve25519PublicKey;

#[cfg(doc)]
use super::QRCodeLoginError::SecureChannel;

/// Messages that will be exchanged over the [`SecureChannel`] to log in a new
/// device using a QR code.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum QrAuthMessage {
    /// Message declaring the available protocols for sign in. Sent by the
    /// existing device.
    #[serde(rename = "m.login.protocols")]
    LoginProtocols {
        /// The login protocols the existing device supports.
        protocols: Vec<LoginProtocolType>,
        /// The homeserver we're going to log in to.
        homeserver: Url,
    },

    /// Message declaring which protocols from the previous `m.login.protocols`
    /// message the new device has picked. Sent by the new device.
    #[serde(rename = "m.login.protocol")]
    LoginProtocol {
        /// The device authorization grant the OAuth 2.0 server has given to the
        /// new device, contains the URL the existing device should use to
        /// confirm the log in.
        device_authorization_grant: AuthorizationGrant,
        /// The protocol the new device has picked.
        protocol: LoginProtocolType,
        /// The device ID the new device will be using.
        device_id: String,
    },

    /// Message declaring that the protocol in the previous `m.login.protocol`
    /// message was accepted. Sent by the existing device.
    #[serde(rename = "m.login.protocol_accepted")]
    LoginProtocolAccepted,

    /// Message that informs the existing device that it successfully obtained
    /// an access token from the OAuth 2.0 server. Sent by the new device.
    #[serde(rename = "m.login.success")]
    LoginSuccess,

    /// Message that informs the existing device that the OAuth 2.0 server has
    /// declined to give us an access token, i.e. because the user declined the
    /// log in. Sent by the new device.
    #[serde(rename = "m.login.declined")]
    LoginDeclined,

    /// Message signaling that a failure happened during the login. Can be sent
    /// by either device.
    #[serde(rename = "m.login.failure")]
    LoginFailure {
        /// The claimed reason for the login failure.
        reason: LoginFailureReason,
        /// The homeserver that we attempted to log in to.
        homeserver: Option<Url>,
    },

    /// Message containing end-to-end encryption related secrets, the new device
    /// can use these secrets to mark itself as verified, connect to a room
    /// key backup, and login other devices via a QR login. Sent by the
    /// existing device.
    #[serde(rename = "m.login.secrets")]
    LoginSecrets(SecretsBundle),
}

impl QrAuthMessage {
    /// Create a new [`QrAuthMessage::LoginProtocol`] message with the
    /// [`LoginProtocolType::DeviceAuthorizationGrant`] protocol type.
    pub fn authorization_grant_login_protocol(
        device_authorization_grant: AuthorizationGrant,
        device_id: Curve25519PublicKey,
    ) -> QrAuthMessage {
        QrAuthMessage::LoginProtocol {
            device_id: device_id.to_base64(),
            device_authorization_grant,
            protocol: LoginProtocolType::DeviceAuthorizationGrant,
        }
    }
}

impl From<&StandardDeviceAuthorizationResponse> for AuthorizationGrant {
    fn from(value: &StandardDeviceAuthorizationResponse) -> Self {
        Self {
            verification_uri: value.verification_uri().clone(),
            verification_uri_complete: value.verification_uri_complete().cloned(),
        }
    }
}

/// Data for the device authorization grant login protocol.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthorizationGrant {
    /// The verification URL the user should open to log the new device in.
    pub verification_uri: EndUserVerificationUrl,

    /// The verification URL, with the user code pre-filled, which the user
    /// should open to log the new device in. If this URL is available, the
    /// user should be presented with it instead of the one in the
    /// [`AuthorizationGrant::verification_uri`] field.
    pub verification_uri_complete: Option<VerificationUriComplete>,
}

/// Reasons why the login might have failed.
#[derive(Clone, StringEnum)]
#[ruma_enum(rename_all = "snake_case")]
pub enum LoginFailureReason {
    /// The Device Authorization Grant expired.
    AuthorizationExpired,
    /// The device ID specified by the new device already exists in the
    /// homeserver provided device list.
    DeviceAlreadyExists,
    /// The new device is not present in the device list as returned by the
    /// homeserver.
    DeviceNotFound,
    /// Sent by either device to indicate that they received a message of a type
    /// that they weren't expecting.
    UnexpectedMessageReceived,
    /// Sent by a device where no suitable protocol is available or the
    /// requested protocol requested is not supported.
    UnsupportedProtocol,
    /// Sent by either new or existing device to indicate that the user has
    /// cancelled the login.
    UserCancelled,
    #[doc(hidden)]
    _Custom(PrivOwnedStr),
}

/// Enum containing known login protocol types.
#[derive(Clone, StringEnum)]
#[ruma_enum(rename_all = "snake_case")]
pub enum LoginProtocolType {
    /// The `device_authorization_grant` login protocol type.
    DeviceAuthorizationGrant,
    #[doc(hidden)]
    _Custom(PrivOwnedStr),
}

#[cfg(test)]
mod test {
    use assert_matches2::assert_let;
    use matrix_sdk_base::crypto::types::BackupSecrets;
    use serde_json::json;
    use similar_asserts::assert_eq;

    use super::*;

    #[test]
    fn test_protocols_serialization() {
        let json = json!({
            "type": "m.login.protocols",
            "protocols": ["device_authorization_grant"],
            "homeserver": "https://matrix-client.matrix.org/"

        });

        let message: QrAuthMessage = serde_json::from_value(json.clone()).unwrap();
        assert_let!(QrAuthMessage::LoginProtocols { protocols, .. } = &message);
        assert!(protocols.contains(&LoginProtocolType::DeviceAuthorizationGrant));

        let serialized = serde_json::to_value(&message).unwrap();
        assert_eq!(json, serialized);
    }

    #[test]
    fn test_protocol_serialization() {
        let json = json!({
            "type": "m.login.protocol",
            "protocol": "device_authorization_grant",
            "device_authorization_grant": {
                "verification_uri_complete": "https://id.matrix.org/device/abcde",
                "verification_uri": "https://id.matrix.org/device/abcde?code=ABCDE"
            },
            "device_id": "wjLpTLRqbqBzLs63aYaEv2Boi6cFEbbM/sSRQ2oAKk4"
        });

        let message: QrAuthMessage = serde_json::from_value(json.clone()).unwrap();
        assert_let!(QrAuthMessage::LoginProtocol { protocol, device_id, .. } = &message);
        assert_eq!(protocol, &LoginProtocolType::DeviceAuthorizationGrant);
        assert_eq!(device_id, "wjLpTLRqbqBzLs63aYaEv2Boi6cFEbbM/sSRQ2oAKk4");
        let serialized = serde_json::to_value(&message).unwrap();
        assert_eq!(json, serialized);
    }

    #[test]
    fn test_protocol_accepted_serialization() {
        let json = json!({
            "type": "m.login.protocol_accepted",
        });

        let message: QrAuthMessage = serde_json::from_value(json.clone()).unwrap();
        assert_let!(QrAuthMessage::LoginProtocolAccepted = &message);
        let serialized = serde_json::to_value(&message).unwrap();
        assert_eq!(json, serialized);
    }

    #[test]
    fn test_login_success() {
        let json = json!({
            "type": "m.login.success",
        });

        let message: QrAuthMessage = serde_json::from_value(json.clone()).unwrap();
        assert_let!(QrAuthMessage::LoginSuccess = &message);
        let serialized = serde_json::to_value(&message).unwrap();
        assert_eq!(json, serialized);
    }

    #[test]
    fn test_login_declined() {
        let json = json!({
            "type": "m.login.declined",
        });

        let message: QrAuthMessage = serde_json::from_value(json.clone()).unwrap();
        assert_let!(QrAuthMessage::LoginDeclined = &message);
        let serialized = serde_json::to_value(&message).unwrap();
        assert_eq!(json, serialized);
    }

    #[test]
    fn test_login_failure() {
        let json = json!({
            "type": "m.login.failure",
            "reason": "unsupported_protocol",
            "homeserver": "https://matrix-client.matrix.org/"
        });

        let message: QrAuthMessage = serde_json::from_value(json.clone()).unwrap();
        assert_let!(QrAuthMessage::LoginFailure { reason, .. } = &message);
        assert_eq!(reason, &LoginFailureReason::UnsupportedProtocol);
        let serialized = serde_json::to_value(&message).unwrap();
        assert_eq!(json, serialized);
    }

    #[test]
    fn test_login_secrets() {
        let json = json!({
            "type": "m.login.secrets",
            "cross_signing": {
                "master_key": "rTtSv67XGS6k/rg6/yTG/m573cyFTPFRqluFhQY+hSw",
                "self_signing_key": "4jbPt7jh5D2iyM4U+3IDa+WthgJB87IQN1ATdkau+xk",
                "user_signing_key": "YkFKtkjcsTxF6UAzIIG/l6Nog/G2RigCRfWj3cjNWeM",
            },
            "backup": {
                "algorithm": "m.megolm_backup.v1.curve25519-aes-sha2",
                "backup_version": "2",
                "key": "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
            },
        });

        let message: QrAuthMessage = serde_json::from_value(json.clone()).unwrap();
        assert_let!(
            QrAuthMessage::LoginSecrets(SecretsBundle { cross_signing, backup }) = &message
        );
        assert_eq!(cross_signing.master_key, "rTtSv67XGS6k/rg6/yTG/m573cyFTPFRqluFhQY+hSw");
        assert_eq!(cross_signing.self_signing_key, "4jbPt7jh5D2iyM4U+3IDa+WthgJB87IQN1ATdkau+xk");
        assert_eq!(cross_signing.user_signing_key, "YkFKtkjcsTxF6UAzIIG/l6Nog/G2RigCRfWj3cjNWeM");

        assert_let!(Some(BackupSecrets::MegolmBackupV1Curve25519AesSha2(backup)) = backup);
        assert_eq!(backup.backup_version, "2");
        assert_eq!(&backup.key.to_base64(), "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA");

        let serialized = serde_json::to_value(&message).unwrap();
        assert_eq!(json, serialized);
    }
}
