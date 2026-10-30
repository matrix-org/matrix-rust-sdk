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
use ruma::serde::{JsonObject, StringEnum};
use serde::{Deserialize, Serialize};
use url::Url;
use vodozemac::Curve25519PublicKey;

#[cfg(doc)]
use super::QRCodeLoginError::SecureChannel;
use super::secure_channel::ChannelVariant;

/// Messages that will be exchanged over the [`SecureChannel`] to log in a new
/// device using a QR code.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum QrAuthMessage {
    /// Message declaring the available protocols for sign in. Sent by the
    /// existing device.
    #[serde(rename = "m.login.protocols")]
    LoginProtocols(LoginProtocolsMessage),

    /// Message declaring which protocols from the previous `m.login.protocols`
    /// message the new device has picked. Sent by the new device.
    #[serde(rename = "m.login.protocol")]
    LoginProtocol {
        /// The protocol the new device has picked, along with the data specific
        /// to that protocol.
        #[serde(flatten)]
        protocol: LoginProtocolData,
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
        /// The homeserver the new device should use, optionally sent by the
        /// existing device so the user doesn't have to type it in.
        ///
        /// The MSC defines this as a server name, but older implementations
        /// send a homeserver URL, so both are accepted as is. Either form can
        /// be passed to [`ClientBuilder::server_name_or_homeserver_url()`].
        ///
        /// [`ClientBuilder::server_name_or_homeserver_url()`]: crate::ClientBuilder::server_name_or_homeserver_url
        #[serde(default, skip_serializing_if = "Option::is_none")]
        homeserver: Option<String>,
    },

    /// Message containing end-to-end encryption related secrets, the new device
    /// can use these secrets to mark itself as verified, connect to a room key
    /// backup, and login other devices via a QR login. Sent by the existing
    /// device.
    #[serde(rename = "m.login.secrets")]
    LoginSecrets(SecretsBundle),
}

/// Message declaring the available protocols for sign in. Sent by the
/// existing device.
///
/// Supports both the MSC4108 variant of the m.login.protocols message as well
/// as the MSC4388 variant.
#[derive(Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum LoginProtocolsMessage {
    Msc4388 {
        /// The login protocols the existing device supports.
        protocols: Vec<LoginProtocolType>,
        /// The homeserver we're going to log in to.
        base_url: Url,
    },
    Msc4108 {
        /// The login protocols the existing device supports.
        protocols: Vec<LoginProtocolType>,
        /// The homeserver we're going to log in to.
        ///
        /// Note: this doesn't match the MSC which says that it is a server name
        /// not a full URL. This is an implementation mistake in the first
        /// version of the QR code login support.
        homeserver: Url,
    },
}

impl QrAuthMessage {
    /// Create a new [`QrAuthMessage::LoginProtocol`] message with the
    /// [`LoginProtocolType::DeviceAuthorizationGrant`] protocol type, using the
    /// layout of the given channel variant.
    pub(super) fn authorization_grant_login_protocol(
        device_authorization_grant: AuthorizationGrant,
        device_id: Curve25519PublicKey,
        channel_variant: ChannelVariant,
    ) -> QrAuthMessage {
        QrAuthMessage::LoginProtocol {
            device_id: device_id.to_base64(),
            protocol: LoginProtocolData::device_authorization_grant(
                device_authorization_grant,
                channel_variant,
            ),
        }
    }
}

/// The login protocol the new device has picked in a
/// [`QrAuthMessage::LoginProtocol`] message, along with the data specific to
/// that protocol.
///
/// On the wire, the protocol name is in the `protocol` field, and the protocol
/// specific fields sit next to it, like other Matrix structures keyed on a
/// discriminator field.
///
/// For the `device_authorization_grant` protocol, clients using the MSC4108
/// variant of the secure channel instead nest the data in a field named after
/// the protocol, i.e. `"protocol": "device_authorization_grant"` comes with a
/// `"device_authorization_grant": { ... }` field. Which layout is used depends
/// on the variant of the secure channel.
///
/// Like other known message types, a known protocol only keeps the fields it
/// knows about, while an [`UnknownLoginProtocol`] keeps all the fields, as we
/// don't know which ones are relevant.
#[derive(Debug, Clone, Deserialize)]
#[serde(try_from = "LoginProtocolDataHelper")]
#[non_exhaustive]
pub enum LoginProtocolData {
    /// The `device_authorization_grant` login protocol, as sent over the
    /// MSC4108 variant of the secure channel, with the data nested in a
    /// `device_authorization_grant` field.
    ///
    /// Contains the device authorization grant the OAuth 2.0 server has given
    /// to the new device, with the URL the existing device should use to
    /// confirm the log in.
    Msc4108DeviceAuthorizationGrant(AuthorizationGrant),
    /// The `device_authorization_grant` login protocol, as sent over the
    /// MSC4388 variant of the secure channel, with the data next to the
    /// `protocol` field.
    ///
    /// Contains the device authorization grant the OAuth 2.0 server has given
    /// to the new device, with the URL the existing device should use to
    /// confirm the log in.
    Msc4388DeviceAuthorizationGrant(AuthorizationGrant),
    /// An unknown and unsupported login protocol.
    Unknown(UnknownLoginProtocol),
}

impl LoginProtocolData {
    /// Create a new [`LoginProtocolData`] from the given login protocol and its
    /// data.
    ///
    /// The `data` contains the fields of the [`QrAuthMessage::LoginProtocol`]
    /// message other than `type`, `protocol` and `device_id`. If the protocol
    /// is a known one, the data is deserialized into the corresponding
    /// variant and only the fields that protocol knows about are kept.
    /// Otherwise a [`LoginProtocolData::Unknown`] is created, which keeps all
    /// the fields verbatim.
    ///
    /// For the [`LoginProtocolType::DeviceAuthorizationGrant`] protocol, the
    /// layout is detected from the data: if it contains a
    /// `device_authorization_grant` field, a
    /// [`LoginProtocolData::Msc4108DeviceAuthorizationGrant`] is created,
    /// otherwise a [`LoginProtocolData::Msc4388DeviceAuthorizationGrant`].
    ///
    /// Returns an error if the protocol is a known one and the data is not
    /// valid for it, or if the protocol is unknown and the data contains one
    /// of the `type`, `protocol` or `device_id` fields, as they would clash
    /// with the fields of the message when serialized.
    pub fn new(protocol: LoginProtocolType, data: JsonObject) -> Result<Self, serde_json::Error> {
        LoginProtocolDataHelper { protocol, other: data }.try_into()
    }

    /// Create a new [`LoginProtocolData`] for the
    /// [`LoginProtocolType::DeviceAuthorizationGrant`] protocol, using the
    /// layout of the given channel variant.
    pub(super) fn device_authorization_grant(
        device_authorization_grant: AuthorizationGrant,
        channel_variant: ChannelVariant,
    ) -> Self {
        match channel_variant {
            ChannelVariant::Msc4108 => {
                Self::Msc4108DeviceAuthorizationGrant(device_authorization_grant)
            }
            #[cfg(feature = "unstable-msc4388")]
            ChannelVariant::Msc4388 => {
                Self::Msc4388DeviceAuthorizationGrant(device_authorization_grant)
            }
        }
    }

    /// Get the login protocol.
    pub fn protocol(&self) -> LoginProtocolType {
        match self {
            Self::Msc4108DeviceAuthorizationGrant(_) | Self::Msc4388DeviceAuthorizationGrant(_) => {
                LoginProtocolType::DeviceAuthorizationGrant
            }
            Self::Unknown(c) => c.protocol.clone(),
        }
    }
}

/// An unknown and unsupported login protocol of a
/// [`QrAuthMessage::LoginProtocol`] message.
#[derive(Debug, Clone)]
pub struct UnknownLoginProtocol {
    /// The name of the unknown login protocol.
    protocol: LoginProtocolType,

    /// The other data of the unknown login protocol.
    other: JsonObject,
}

impl UnknownLoginProtocol {
    /// Get the name of the unknown login protocol.
    pub fn protocol(&self) -> &LoginProtocolType {
        &self.protocol
    }

    /// Get the data of the unknown login protocol, i.e. the fields of the
    /// [`QrAuthMessage::LoginProtocol`] message other than `type`, `protocol`
    /// and `device_id`.
    pub fn data(&self) -> &JsonObject {
        &self.other
    }
}

#[derive(Deserialize)]
struct LoginProtocolDataHelper {
    protocol: LoginProtocolType,
    #[serde(flatten)]
    other: JsonObject,
}

#[derive(Serialize)]
struct LoginProtocolDataSerHelper<'a, T: Serialize> {
    protocol: LoginProtocolType,
    #[serde(flatten)]
    data: &'a T,
}

#[derive(Serialize)]
struct Msc4108DeviceAuthorizationGrantSerHelper<'a> {
    device_authorization_grant: &'a AuthorizationGrant,
}

impl Serialize for LoginProtocolData {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let protocol = self.protocol();

        match self {
            Self::Msc4108DeviceAuthorizationGrant(device_authorization_grant) => {
                LoginProtocolDataSerHelper {
                    protocol,
                    data: &Msc4108DeviceAuthorizationGrantSerHelper { device_authorization_grant },
                }
                .serialize(serializer)
            }
            Self::Msc4388DeviceAuthorizationGrant(device_authorization_grant) => {
                LoginProtocolDataSerHelper { protocol, data: device_authorization_grant }
                    .serialize(serializer)
            }
            Self::Unknown(c) => {
                LoginProtocolDataSerHelper { protocol, data: &c.other }.serialize(serializer)
            }
        }
    }
}

impl TryFrom<LoginProtocolDataHelper> for LoginProtocolData {
    type Error = serde_json::Error;

    fn try_from(mut value: LoginProtocolDataHelper) -> Result<Self, Self::Error> {
        Ok(match value.protocol {
            LoginProtocolType::DeviceAuthorizationGrant => {
                let invalid_data = |error: serde_json::Error| {
                    serde::de::Error::custom(format_args!(
                        "invalid device_authorization_grant: {error}"
                    ))
                };

                // The MSC4108 variant nests the data in a field named after the
                // protocol, while the MSC4388 variant puts it next to the
                // `protocol` field.
                if let Some(device_authorization_grant) =
                    value.other.remove("device_authorization_grant")
                {
                    Self::Msc4108DeviceAuthorizationGrant(
                        serde_json::from_value(device_authorization_grant).map_err(invalid_data)?,
                    )
                } else {
                    Self::Msc4388DeviceAuthorizationGrant(
                        serde_json::from_value(value.other.into()).map_err(invalid_data)?,
                    )
                }
            }
            _ => {
                // The fields of an unknown protocol are serialized next to the
                // fields of the message, so they must not clash with them.
                const RESERVED_FIELDS: &[&str] = &["type", "protocol", "device_id"];

                if let Some(field) =
                    RESERVED_FIELDS.iter().find(|field| value.other.contains_key(**field))
                {
                    return Err(serde::de::Error::custom(format_args!(
                        "the `{field}` field is reserved and can't be part of the login \
                         protocol data"
                    )));
                }

                Self::Unknown(UnknownLoginProtocol { protocol: value.protocol, other: value.other })
            }
        })
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
    /// should open to log the new device in. If this URL is available, the user
    /// should be presented with it instead of the one in the
    /// [`AuthorizationGrant::verification_uri`] field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
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
    use matrix_sdk_base::crypto::types::BackupSecrets;
    use serde_json::json;
    use similar_asserts::assert_eq;
    use strass::assert_let;

    use super::*;

    #[test]
    fn test_protocols_serialization_msc_4108() {
        const HOMESERVER: &str = "https://matrix-client.matrix.org/";

        let json = json!({
            "type": "m.login.protocols",
            "protocols": ["device_authorization_grant"],
            "homeserver": HOMESERVER,

        });

        let message: QrAuthMessage = serde_json::from_value(json.clone()).unwrap();
        assert_let!(
            QrAuthMessage::LoginProtocols(LoginProtocolsMessage::Msc4108 {
                protocols,
                homeserver
            }) = &message
        );
        assert!(protocols.contains(&LoginProtocolType::DeviceAuthorizationGrant));
        assert_eq!(homeserver.as_str(), HOMESERVER);

        let serialized = serde_json::to_value(&message).unwrap();
        assert_eq!(json, serialized);
    }

    #[test]
    fn test_protocols_serialization_msc_4388() {
        const HOMESERVER: &str = "https://matrix-client.matrix.org/";

        let json = json!({
            "type": "m.login.protocols",
            "protocols": ["device_authorization_grant"],
            "base_url": HOMESERVER,

        });

        let message: QrAuthMessage = serde_json::from_value(json.clone()).unwrap();
        assert_let!(
            QrAuthMessage::LoginProtocols(LoginProtocolsMessage::Msc4388 { protocols, base_url }) =
                &message
        );
        assert!(protocols.contains(&LoginProtocolType::DeviceAuthorizationGrant));
        assert_eq!(base_url.as_str(), HOMESERVER);

        let serialized = serde_json::to_value(&message).unwrap();
        assert_eq!(json, serialized);
    }

    #[test]
    fn test_protocol_serialization_msc_4108() {
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
        assert_let!(
            QrAuthMessage::LoginProtocol {
                protocol: LoginProtocolData::Msc4108DeviceAuthorizationGrant(
                    device_authorization_grant
                ),
                device_id,
            } = &message
        );
        assert_eq!(
            device_authorization_grant.verification_uri.as_str(),
            "https://id.matrix.org/device/abcde?code=ABCDE"
        );
        assert_eq!(device_id, "wjLpTLRqbqBzLs63aYaEv2Boi6cFEbbM/sSRQ2oAKk4");
        let serialized = serde_json::to_value(&message).unwrap();
        assert_eq!(json, serialized);
    }

    #[test]
    fn test_protocol_serialization_msc_4388() {
        let json = json!({
            "type": "m.login.protocol",
            "protocol": "device_authorization_grant",
            "verification_uri_complete": "https://id.matrix.org/device/abcde",
            "verification_uri": "https://id.matrix.org/device/abcde?code=ABCDE",
            "device_id": "wjLpTLRqbqBzLs63aYaEv2Boi6cFEbbM/sSRQ2oAKk4"
        });

        let message: QrAuthMessage = serde_json::from_value(json.clone()).unwrap();
        assert_let!(
            QrAuthMessage::LoginProtocol {
                protocol: LoginProtocolData::Msc4388DeviceAuthorizationGrant(
                    device_authorization_grant
                ),
                device_id,
            } = &message
        );
        assert_eq!(
            device_authorization_grant.verification_uri.as_str(),
            "https://id.matrix.org/device/abcde?code=ABCDE"
        );
        assert_eq!(device_id, "wjLpTLRqbqBzLs63aYaEv2Boi6cFEbbM/sSRQ2oAKk4");
        let serialized = serde_json::to_value(&message).unwrap();
        assert_eq!(json, serialized);
    }

    #[test]
    fn test_protocol_serialization_unknown_protocol() {
        // A future protocol carries its own fields, and no
        // `verification_uri` or `device_authorization_grant` field.
        let json = json!({
            "type": "m.login.protocol",
            "protocol": "org.example.future_protocol",
            "org.example.field": {
                "some": "data"
            },
            "device_id": "ABCDEFGH"
        });

        let message: QrAuthMessage = serde_json::from_value(json.clone()).unwrap();
        assert_let!(
            QrAuthMessage::LoginProtocol {
                protocol: LoginProtocolData::Unknown(protocol),
                device_id
            } = &message
        );
        assert_eq!(protocol.protocol().as_str(), "org.example.future_protocol");
        assert_eq!(
            protocol.data(),
            &JsonObject::from_iter([("org.example.field".to_owned(), json!({ "some": "data" }))])
        );
        assert_eq!(device_id, "ABCDEFGH");

        // The data of the unknown protocol is kept, and no device
        // authorization grant fields are added.
        let serialized = serde_json::to_value(&message).unwrap();
        assert_eq!(json, serialized);
    }

    #[test]
    fn test_protocol_deserialization_missing_device_authorization_grant() {
        let json = json!({
            "type": "m.login.protocol",
            "protocol": "device_authorization_grant",
            "device_id": "ABCDEFGH"
        });

        let error = serde_json::from_value::<QrAuthMessage>(json).expect_err(
            "The device authorization grant protocol requires the device authorization grant",
        );
        let error = error.to_string();
        assert!(error.contains("device_authorization_grant"), "{error}");
        assert!(error.contains("verification_uri"), "{error}");
    }

    #[test]
    fn test_protocol_deserialization_invalid_device_authorization_grant() {
        let json = json!({
            "type": "m.login.protocol",
            "protocol": "device_authorization_grant",
            "device_authorization_grant": {},
            "device_id": "ABCDEFGH"
        });

        let error = serde_json::from_value::<QrAuthMessage>(json)
            .expect_err("The device authorization grant requires the verification_uri field");
        let error = error.to_string();
        assert!(error.contains("device_authorization_grant"), "{error}");
        assert!(error.contains("verification_uri"), "{error}");
    }

    #[test]
    fn test_protocol_serialization_drops_unknown_fields_of_known_protocol() {
        let json = json!({
            "type": "m.login.protocol",
            "protocol": "device_authorization_grant",
            "device_authorization_grant": {
                "verification_uri": "https://id.matrix.org/device/abcde"
            },
            "org.example.hint": "data",
            "device_id": "ABCDEFGH"
        });

        let message: QrAuthMessage = serde_json::from_value(json).unwrap();
        let serialized = serde_json::to_value(&message).unwrap();
        assert_eq!(
            serialized,
            json!({
                "type": "m.login.protocol",
                "protocol": "device_authorization_grant",
                "device_authorization_grant": {
                    "verification_uri": "https://id.matrix.org/device/abcde",
                },
                "device_id": "ABCDEFGH"
            })
        );

        let json = json!({
            "type": "m.login.protocol",
            "protocol": "device_authorization_grant",
            "verification_uri": "https://id.matrix.org/device/abcde",
            "org.example.hint": "data",
            "device_id": "ABCDEFGH"
        });

        let message: QrAuthMessage = serde_json::from_value(json).unwrap();
        let serialized = serde_json::to_value(&message).unwrap();
        assert_eq!(
            serialized,
            json!({
                "type": "m.login.protocol",
                "protocol": "device_authorization_grant",
                "verification_uri": "https://id.matrix.org/device/abcde",
                "device_id": "ABCDEFGH"
            })
        );
    }

    #[test]
    fn test_login_protocol_data_new() {
        let grant = json!({
            "verification_uri": "https://id.matrix.org/device/abcde",
        });
        let assert_grant = |device_authorization_grant: AuthorizationGrant| {
            assert_eq!(
                device_authorization_grant.verification_uri.as_str(),
                "https://id.matrix.org/device/abcde"
            );
        };

        // A known protocol with valid data nested in a field named after it
        // gets the MSC4108 variant.
        let data = LoginProtocolData::new(
            LoginProtocolType::DeviceAuthorizationGrant,
            JsonObject::from_iter([("device_authorization_grant".to_owned(), grant.clone())]),
        )
        .unwrap();
        assert_let!(
            LoginProtocolData::Msc4108DeviceAuthorizationGrant(device_authorization_grant) = data
        );
        assert_grant(device_authorization_grant);

        // A known protocol with valid data next to the protocol gets the
        // MSC4388 variant.
        let data = LoginProtocolData::new(
            LoginProtocolType::DeviceAuthorizationGrant,
            serde_json::from_value(grant.clone()).unwrap(),
        )
        .unwrap();
        assert_let!(
            LoginProtocolData::Msc4388DeviceAuthorizationGrant(device_authorization_grant) = data
        );
        assert_grant(device_authorization_grant);

        // A known protocol with missing data is an error.
        LoginProtocolData::new(LoginProtocolType::DeviceAuthorizationGrant, JsonObject::new())
            .expect_err("The device authorization grant protocol requires its data");

        // An unknown protocol keeps its data, even if it looks like the data of
        // a known protocol.
        let other =
            JsonObject::from_iter([("device_authorization_grant".to_owned(), grant.clone())]);
        let data =
            LoginProtocolData::new("org.example.future_protocol".into(), other.clone()).unwrap();
        assert_let!(LoginProtocolData::Unknown(protocol) = data);
        assert_eq!(protocol.protocol().as_str(), "org.example.future_protocol");
        assert_eq!(protocol.data(), &other);

        // The data of an unknown protocol can't contain the fields of the
        // message, as they would be duplicated when serialized.
        for field in ["type", "protocol", "device_id"] {
            let other = JsonObject::from_iter([(field.to_owned(), json!("foo"))]);
            let error = LoginProtocolData::new("org.example.future_protocol".into(), other)
                .expect_err("The reserved fields should be rejected");
            assert!(error.to_string().contains(field), "{error}");
        }

        // A known protocol only keeps the fields it knows about, so the
        // reserved fields can't clash and are dropped like any other field.
        let data = LoginProtocolData::new(
            LoginProtocolType::DeviceAuthorizationGrant,
            JsonObject::from_iter([
                ("device_authorization_grant".to_owned(), grant.clone()),
                ("type".to_owned(), json!("foo")),
                ("org.example.hint".to_owned(), json!(1)),
            ]),
        )
        .unwrap();
        assert_let!(LoginProtocolData::Msc4108DeviceAuthorizationGrant(_) = data);

        let data = LoginProtocolData::new(
            LoginProtocolType::DeviceAuthorizationGrant,
            JsonObject::from_iter([
                ("verification_uri".to_owned(), grant["verification_uri"].clone()),
                ("type".to_owned(), json!("foo")),
                ("org.example.hint".to_owned(), json!(1)),
            ]),
        )
        .unwrap();
        assert_let!(LoginProtocolData::Msc4388DeviceAuthorizationGrant(_) = data);
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
        assert_let!(QrAuthMessage::LoginFailure { reason, homeserver } = &message);
        assert_eq!(reason, &LoginFailureReason::UnsupportedProtocol);
        // Older implementations send a URL, which we keep as is.
        assert_eq!(homeserver.as_deref(), Some("https://matrix-client.matrix.org/"));
        let serialized = serde_json::to_value(&message).unwrap();
        assert_eq!(json, serialized);
    }

    #[test]
    fn test_login_failure_with_server_name() {
        // The MSC defines the homeserver as a server name, not a URL.
        let json = json!({
            "type": "m.login.failure",
            "reason": "unsupported_protocol",
            "homeserver": "matrix.org"
        });

        let message: QrAuthMessage = serde_json::from_value(json.clone()).unwrap();
        assert_let!(QrAuthMessage::LoginFailure { reason, homeserver } = &message);
        assert_eq!(reason, &LoginFailureReason::UnsupportedProtocol);
        assert_eq!(homeserver.as_deref(), Some("matrix.org"));
        let serialized = serde_json::to_value(&message).unwrap();
        assert_eq!(json, serialized);
    }

    #[test]
    fn test_login_failure_without_homeserver() {
        let json = json!({
            "type": "m.login.failure",
            "reason": "user_cancelled",
        });

        let message: QrAuthMessage = serde_json::from_value(json.clone()).unwrap();
        assert_let!(QrAuthMessage::LoginFailure { reason, homeserver } = &message);
        assert_eq!(reason, &LoginFailureReason::UserCancelled);
        assert!(homeserver.is_none());

        // A missing homeserver must not be serialized as `null`.
        let serialized = serde_json::to_value(&message).unwrap();
        assert_eq!(json, serialized);

        // But we accept an explicit `null` from other implementations.
        let message: QrAuthMessage = serde_json::from_value(json!({
            "type": "m.login.failure",
            "reason": "user_cancelled",
            "homeserver": null,
        }))
        .unwrap();
        assert_let!(QrAuthMessage::LoginFailure { homeserver: None, .. } = &message);
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
