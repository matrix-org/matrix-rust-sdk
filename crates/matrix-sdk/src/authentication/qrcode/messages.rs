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
use openidconnect::{EndUserVerificationUrl, VerificationUriComplete};
use ruma::OwnedDeviceId;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use url::Url;
use vodozemac::Curve25519PublicKey;

#[derive(Debug, Serialize, Deserialize)]
pub enum QrAuthMessage {
    #[serde(rename = "m.login.protocols")]
    LoginProtocols { protocols: Vec<u8>, homeserver: Url },
    #[serde(rename = "m.login.protocol")]
    LoginProtocol {
        device_authorization_grant: AuthorizationGrant,
        protocol: u8,
        #[serde(
            deserialize_with = "deserialize_curve_key",
            serialize_with = "serialize_curve_key"
        )]
        device_id: Curve25519PublicKey,
    },
    #[serde(rename = "m.login.protocol_accepted")]
    LoginProtocolAccepted(),
    #[serde(rename = "m.login.success")]
    LoginSuccess(),
    #[serde(rename = "m.login.declined")]
    LoginDeclined(),
    #[serde(rename = "m.login.secrets")]
    LoginSecrets(SecretsBundle),
}

#[derive(Debug, Serialize, Deserialize)]
pub struct AuthorizationGrant {
    pub verification_uri: EndUserVerificationUrl,
    pub verification_uri_complete: Option<VerificationUriComplete>,
}

// Vodozemac serializes Curve25519 keys directly as a byteslice, while Matrix
// likes to base64 encode all byte slices.
//
// This ensures that we serialize/deserialize in a Matrix-compatible way.
pub(crate) fn deserialize_curve_key<'de, D>(de: D) -> Result<Curve25519PublicKey, D::Error>
where
    D: Deserializer<'de>,
{
    let key: String = Deserialize::deserialize(de)?;
    Curve25519PublicKey::from_base64(&key).map_err(serde::de::Error::custom)
}

pub(crate) fn serialize_curve_key<S>(key: &Curve25519PublicKey, s: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    let key = key.to_base64();
    s.serialize_str(&key)
}
