// Copyright 2022 The Matrix.org Foundation C.I.C.
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

use ruma::OwnedUserId;
use serde::{Deserialize, Serialize};
use vodozemac::Ed25519PublicKey;

use crate::types::{deserialize_ed25519_key, serialize_ed25519_key};

/// An `m.olm.v1.curve25519-aes-sha2` decrypted to-device event.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct DecryptedOlmV1Event {
    /// The sender of the encrypted to-device event.
    pub sender: OwnedUserId,
    /// The recipient of the encrypted to-device event.
    pub recipient: OwnedUserId,
    /// The sender's signing keys of the encrypted event.
    pub keys: OlmV1Keys,
    /// The recipient's signing keys of the encrypted event.
    pub recipient_keys: OlmV1Keys,
    /// The type of the event.
    #[serde(rename = "type")]
    pub event_type: String,
}

/// Public keys used for an m.olm.v1.curve25519-aes-sha2 event.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct OlmV1Keys {
    /// The Ed25519 public key of the `m.olm.v1.curve25519-aes-sha2` keys.
    #[serde(
        deserialize_with = "deserialize_ed25519_key",
        serialize_with = "serialize_ed25519_key"
    )]
    pub ed25519: Ed25519PublicKey,
}
