//! Olm types.

use wasm_bindgen::prelude::*;

use crate::impl_from_to_inner;

/// Struct representing the state of our private cross signing keys,
/// it shows which private cross signing keys we have locally stored.
#[wasm_bindgen]
#[derive(Debug)]
pub struct CrossSigningStatus {
    inner: matrix_sdk_crypto::olm::CrossSigningStatus,
}

impl_from_to_inner!(matrix_sdk_crypto::olm::CrossSigningStatus => CrossSigningStatus);

#[wasm_bindgen]
impl CrossSigningStatus {
    /// Do we have the master key?
    #[wasm_bindgen(getter, js_name = "hasMaster")]
    pub fn has_master(&self) -> bool {
        self.inner.has_master
    }

    /// Do we have the self signing key? This one is necessary to sign
    /// our own devices.
    #[wasm_bindgen(getter, js_name = "hasSelfSigning")]
    pub fn has_self_signing(&self) -> bool {
        self.inner.has_self_signing
    }

    /// Do we have the user signing key? This one is necessary to sign
    /// other users.
    #[wasm_bindgen(getter, js_name = "hasUserSigning")]
    pub fn has_user_signing(&self) -> bool {
        self.inner.has_user_signing
    }
}
