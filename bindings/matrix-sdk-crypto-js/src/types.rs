//! Extra types, like `Signatures`.

use std::collections::BTreeMap;

use js_sys::{Map, JSON};
use serde::{Serialize, Deserialize};
use tracing::trace;
use wasm_bindgen::prelude::*;

use crate::{
    identifiers::{DeviceKeyId, UserId},
    impl_from_to_inner,
    vodozemac::Ed25519Signature,
};

use matrix_sdk_crypto::{
    backups::SignatureVerification as InnerSignatureVerification,
    backups::SignatureState as InnerSignatureState,
};

/// A collection of `Signature`.
#[wasm_bindgen]
#[derive(Debug, Default)]
pub struct Signatures {
    inner: matrix_sdk_crypto::types::Signatures,
}

impl_from_to_inner!(matrix_sdk_crypto::types::Signatures => Signatures);

#[wasm_bindgen]
impl Signatures {
    /// Creates a new, empty, signatures collection.
    #[wasm_bindgen(constructor)]
    pub fn new() -> Self {
        matrix_sdk_crypto::types::Signatures::new().into()
    }

    /// Add the given signature from the given signer and the given key ID to
    /// the collection.
    #[wasm_bindgen(js_name = "addSignature")]
    pub fn add_signature(
        &mut self,
        signer: &UserId,
        key_id: &DeviceKeyId,
        signature: &Ed25519Signature,
    ) -> Option<MaybeSignature> {
        self.inner
            .add_signature(signer.inner.clone(), key_id.inner.clone(), signature.inner)
            .map(Into::into)
    }

    /// Try to find an Ed25519 signature from the given signer with
    /// the given key ID.
    #[wasm_bindgen(js_name = "getSignature")]
    pub fn get_signature(&self, signer: &UserId, key_id: &DeviceKeyId) -> Option<Ed25519Signature> {
        self.inner.get_signature(signer.inner.as_ref(), key_id.inner.as_ref()).map(Into::into)
    }

    /// Get the map of signatures that belong to the given user.
    pub fn get(&self, signer: &UserId) -> Option<Map> {
        let map = Map::new();

        for (device_key_id, maybe_signature) in
            self.inner.get(signer.inner.as_ref()).map(|map| {
                map.iter().map(|(device_key_id, maybe_signature)| {
                    (
                        device_key_id.as_str().to_owned(),
                        MaybeSignature::from(maybe_signature.clone()),
                    )
                })
            })?
        {
            map.set(&device_key_id.into(), &maybe_signature.into());
        }

        Some(map)
    }

    /// Remove all the signatures we currently hold.
    pub fn clear(&mut self) {
        self.inner.clear();
    }

    /// Do we hold any signatures or is our collection completely
    /// empty.
    #[wasm_bindgen(js_name = "isEmpty")]
    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    /// How many signatures do we currently hold.
    #[wasm_bindgen(getter)]
    pub fn count(&self) -> usize {
        self.inner.signature_count()
    }

    /// Get the json with all signatures
    #[wasm_bindgen(js_name="asJSON")]
    pub fn as_json(&self) -> JsValue {
        trace!(?self.inner, "The signature");
        // can't use directly serde_wasm_bindgen as there is an issue with BTreeMap
        // It's always returning an empty {} if I do:
        // serde_wasm_bindgen::to_value(&self.inner).unwrap()
        
        // Keep it like that for now as it's working
        let map : BTreeMap<String, BTreeMap<String,String>> = self.inner.clone().into_iter().map(|(u,sign)| {
            (
                u.as_str().to_owned(),
                sign.iter().map(|(device, maybe_sign)| {
                    (
                        device.as_str().to_owned(),
                        match maybe_sign {
                           Ok(s) => s.to_base64(),
                           Err(e) => e.source.to_owned()
                        }
                    )
                }).collect()
            )
        }).collect();

        let raw_string = serde_json::to_string(&map).unwrap();

        JSON::parse(&raw_string).unwrap()
    }

}

/// Represents a potentially decoded signature (but not a validated
/// one).
#[wasm_bindgen]
#[derive(Debug)]
pub struct Signature {
    inner: matrix_sdk_crypto::types::Signature,
}

impl_from_to_inner!(matrix_sdk_crypto::types::Signature => Signature);

#[wasm_bindgen]
impl Signature {
    /// Get the Ed25519 signature, if this is one.
    #[wasm_bindgen(getter)]
    pub fn ed25519(&self) -> Option<Ed25519Signature> {
        self.inner.ed25519().map(Into::into)
    }

    /// Convert the signature to a base64 encoded string.
    #[wasm_bindgen(js_name = "toBase64")]
    pub fn to_base64(&self) -> String {
        self.inner.to_base64()
    }
}

type MaybeSignatureInner =
    Result<matrix_sdk_crypto::types::Signature, matrix_sdk_crypto::types::InvalidSignature>;

/// Represents a signature that is either valid _or_ that could not be
/// decoded.
#[wasm_bindgen]
#[derive(Debug)]
pub struct MaybeSignature {
    inner: MaybeSignatureInner,
}

impl_from_to_inner!(MaybeSignatureInner => MaybeSignature);

#[wasm_bindgen]
impl MaybeSignature {
    /// Check whether the signature has been successfully decoded.
    #[wasm_bindgen(js_name = "isValid")]
    pub fn is_valid(&self) -> bool {
        self.inner.is_ok()
    }

    /// Check whether the signature could not be successfully decoded.
    #[wasm_bindgen(js_name = "isInvalid")]
    pub fn is_invalid(&self) -> bool {
        self.inner.is_err()
    }

    /// The signature, if successfully decoded.
    #[wasm_bindgen(getter)]
    pub fn signature(&self) -> Option<Signature> {
        self.inner.as_ref().cloned().map(Into::into).ok()
    }

    /// The base64 encoded string that is claimed to contain a
    /// signature but could not be decoded, if any.
    #[wasm_bindgen(getter, js_name = "invalidSignatureSource")]
    pub fn invalid_signature_source(&self) -> Option<String> {
        match &self.inner {
            Ok(_) => None,
            Err(signature) => Some(signature.source.clone()),
        }
    }
}


/// The result of a signature verification of a signed JSON object.
#[derive(Debug)]
#[wasm_bindgen]
pub struct SignatureVerification {
    pub(crate) inner: InnerSignatureVerification
}

/// The result of a signature check.
#[derive(Debug)]
#[wasm_bindgen]
pub enum SignatureState {
    /// The signature is missing.
    Missing = 0,
    /// The signature is invalid.
    Invalid = 1,
    /// The signature is valid but the device or user identity that created the
    /// signature is not trusted.
    ValidButNotTrusted = 2,
    /// The signature is valid and the device or user identity that created the
    /// signature is trusted.
    ValidAndTrusted = 3,
}

impl Into<SignatureState> for InnerSignatureState {
    fn into(self) -> SignatureState {
        match self {
            InnerSignatureState::Missing => SignatureState::Missing,
            InnerSignatureState::Invalid => SignatureState::Invalid,
            InnerSignatureState::ValidButNotTrusted => SignatureState::ValidButNotTrusted,
            InnerSignatureState::ValidAndTrusted => SignatureState::ValidAndTrusted,
        }
    }
}

#[wasm_bindgen]
impl SignatureVerification {

    /// Give the backup signature state from the current device. 
    /// See SignatureState for values
    #[wasm_bindgen(getter, js_name="deviceState")]
    pub fn device_state(&self) -> SignatureState {
        self.inner.device_signature.into()
    }

    /// Give the backup signature state from the current user identity. 
    /// See SignatureState for values
    #[wasm_bindgen(getter, js_name="userState")]
    pub fn user_state(&self) -> SignatureState {
        self.inner.user_identity_signature.into()
    }
}


/// Struct holding the number of room keys we have.
#[derive(Debug, Serialize, Deserialize)]
pub struct RoomKeyCounts {
    /// The total number of room keys.
    pub total: i64,
    /// The number of backed up room keys.
    #[serde(rename="backedUp")]
    pub backed_up: i64,
}