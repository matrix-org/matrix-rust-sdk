//! Store types.

use wasm_bindgen::prelude::*;

use crate::{
    encryption::EncryptionAlgorithm, identifiers::RoomId, impl_from_to_inner,
    vodozemac::Curve25519PublicKey,
};

/// A struct containing private cross signing keys that can be backed
/// up or uploaded to the secret store.
#[wasm_bindgen]
#[derive(Debug)]
pub struct CrossSigningKeyExport {
    pub(crate) inner: matrix_sdk_crypto::store::CrossSigningKeyExport,
}

impl_from_to_inner!(matrix_sdk_crypto::store::CrossSigningKeyExport => CrossSigningKeyExport);

#[wasm_bindgen]
impl CrossSigningKeyExport {
    /// The seed of the master key encoded as unpadded base64.
    #[wasm_bindgen(getter, js_name = "masterKey")]
    pub fn master_key(&self) -> Option<String> {
        self.inner.master_key.clone()
    }

    /// The seed of the self signing key encoded as unpadded base64.
    #[wasm_bindgen(getter, js_name = "self_signing_key")]
    pub fn self_signing_key(&self) -> Option<String> {
        self.inner.self_signing_key.clone()
    }

    /// The seed of the user signing key encoded as unpadded base64.
    #[wasm_bindgen(getter, js_name = "userSigningKey")]
    pub fn user_signing_key(&self) -> Option<String> {
        self.inner.user_signing_key.clone()
    }
}

/// Information on a room key that has been received or imported.
#[wasm_bindgen]
#[derive(Debug)]
pub struct RoomKeyInfo {
    pub(crate) inner: matrix_sdk_crypto::store::RoomKeyInfo,
}

impl_from_to_inner!(matrix_sdk_crypto::store::RoomKeyInfo => RoomKeyInfo);

#[wasm_bindgen]
impl RoomKeyInfo {
    /// The {@link EncryptionAlgorithm} that this key is used for. Will be one
    /// of the `m.megolm.*` algorithms.
    #[wasm_bindgen(getter)]
    pub fn algorithm(&self) -> EncryptionAlgorithm {
        self.inner.algorithm.clone().into()
    }

    /// The room where the key is used.
    #[wasm_bindgen(getter, js_name = "roomId")]
    pub fn room_id(&self) -> RoomId {
        self.inner.room_id.clone().into()
    }

    /// The Curve25519 key of the device which initiated the session originally.
    #[wasm_bindgen(getter, js_name = "senderKey")]
    pub fn sender_key(&self) -> Curve25519PublicKey {
        self.inner.sender_key.into()
    }

    /// The ID of the session that the key is for.
    #[wasm_bindgen(getter, js_name = "sessionId")]
    pub fn session_id(&self) -> String {
        self.inner.session_id.clone()
    }
}
