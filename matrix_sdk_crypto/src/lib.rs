// Copyright 2020 The Matrix.org Foundation C.I.C.
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

//! This is the encryption part of the matrix-sdk. It contains a state machine
//! that will aid in adding encryption support to a client library.

#![deny(
    missing_debug_implementations,
    dead_code,
    missing_docs,
    trivial_casts,
    trivial_numeric_casts,
    unused_extern_crates,
    unused_import_braces,
    unused_qualifications
)]

mod device;
mod error;
mod machine;
mod memory_stores;
mod olm;
mod store;

pub use device::{Device, TrustState};
pub use error::{MegolmError, OlmError};
pub use machine::{OlmMachine, OneTimeKeys};
pub use memory_stores::{DeviceStore, GroupSessionStore, SessionStore, UserDevices};
pub use olm::{Account, IdentityKeys, InboundGroupSession, OutboundGroupSession, Session};
#[cfg(feature = "sqlite-cryptostore")]
pub use store::sqlite::SqliteStore;
pub use store::{CryptoStore, CryptoStoreError};

use error::SignatureError;
use matrix_sdk_common::{
    api::r0::keys::{AlgorithmAndDeviceId, KeyAlgorithm},
    identifiers::UserId,
};
use olm_rs::utility::OlmUtility;
use serde_json::Value;

/// Verify a signed JSON object.
///
/// The object must have a signatures key associated  with an object of the
/// form `user_id: {key_id: signature}`.
///
/// Returns Ok if the signature was successfully verified, otherwise an
/// SignatureError.
///
/// # Arguments
///
/// * `user_id` - The user who signed the JSON object.
///
/// * `key_id` - The id of the key that signed the JSON object.
///
/// * `signing_key` - The public ed25519 key which was used to sign the JSON
///     object.
///
/// * `json` - The JSON object that should be verified.
pub(crate) fn verify_json(
    user_id: &UserId,
    key_id: &str,
    signing_key: &str,
    json: &mut Value,
) -> Result<(), SignatureError> {
    let json_object = json.as_object_mut().ok_or(SignatureError::NotAnObject)?;
    let unsigned = json_object.remove("unsigned");
    let signatures = json_object.remove("signatures");

    let canonical_json = cjson::to_string(json_object)?;

    if let Some(u) = unsigned {
        json_object.insert("unsigned".to_string(), u);
    }

    let key_id = AlgorithmAndDeviceId(KeyAlgorithm::Ed25519, key_id.into());

    let signatures = signatures.ok_or(SignatureError::NoSignatureFound)?;
    let signature_object = signatures
        .as_object()
        .ok_or(SignatureError::NoSignatureFound)?;
    let signature = signature_object
        .get(user_id.as_str())
        .ok_or(SignatureError::NoSignatureFound)?;
    let signature = signature
        .get(key_id.to_string())
        .ok_or(SignatureError::NoSignatureFound)?;
    let signature = signature.as_str().ok_or(SignatureError::NoSignatureFound)?;

    let utility = OlmUtility::new();

    let ret = if utility
        .ed25519_verify(signing_key, &canonical_json, signature)
        .is_ok()
    {
        Ok(())
    } else {
        Err(SignatureError::VerificationError)
    };

    json_object.insert("signatures".to_string(), signatures);

    ret
}
