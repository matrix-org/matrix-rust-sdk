//! Store types.

use wasm_bindgen::prelude::*;

/// A struct containing private cross signing keys that can be backed
/// up or uploaded to the secret store.
#[wasm_bindgen]
#[derive(Debug)]
pub struct CrossSigningKeyExport {
    pub(crate) inner: matrix_sdk_crypto::store::CrossSigningKeyExport,
}

impl From<matrix_sdk_crypto::store::CrossSigningKeyExport> for CrossSigningKeyExport {
    fn from(inner: matrix_sdk_crypto::store::CrossSigningKeyExport) -> Self {
        Self { inner }
    }
}

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
