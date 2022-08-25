//! Vodozemac types.

use wasm_bindgen::prelude::*;

/// An Ed25519 public key, used to verify digital signatures.
#[wasm_bindgen]
#[derive(Debug, Clone)]
pub struct Ed25519PublicKey {
    inner: vodozemac::Ed25519PublicKey,
}

#[wasm_bindgen]
impl Ed25519PublicKey {
    /// The number of bytes an Ed25519 public key has.
    #[wasm_bindgen(getter)]
    pub fn length(&self) -> usize {
        vodozemac::Ed25519PublicKey::LENGTH
    }

    /// Serialize an Ed25519 public key to an unpadded base64
    /// representation.
    #[wasm_bindgen(js_name = "toBase64")]
    pub fn to_base64(&self) -> String {
        self.inner.to_base64()
    }
}

/// An Ed25519 digital signature, can be used to verify the
/// authenticity of a message.
#[wasm_bindgen]
#[derive(Debug)]
pub struct Ed25519Signature {
    pub(crate) inner: vodozemac::Ed25519Signature,
}

impl From<vodozemac::Ed25519Signature> for Ed25519Signature {
    fn from(inner: vodozemac::Ed25519Signature) -> Self {
        Self { inner }
    }
}

#[wasm_bindgen]
impl Ed25519Signature {
    /// Try to create an Ed25519 signature from an unpadded base64
    /// representation.
    #[wasm_bindgen(constructor)]
    pub fn new(signature: String) -> Result<Ed25519Signature, JsError> {
        Ok(Self { inner: vodozemac::Ed25519Signature::from_base64(signature.as_str())? })
    }

    /// Serialize a Ed25519 signature to an unpadded base64
    /// representation.
    #[wasm_bindgen(js_name = "toBase64")]
    pub fn to_base64(&self) -> String {
        self.inner.to_base64()
    }
}

/// A Curve25519 public key.
#[wasm_bindgen]
#[derive(Debug, Clone)]
pub struct Curve25519PublicKey {
    inner: vodozemac::Curve25519PublicKey,
}

#[wasm_bindgen]
impl Curve25519PublicKey {
    /// The number of bytes a Curve25519 public key has.
    #[wasm_bindgen(getter)]
    pub fn length(&self) -> usize {
        vodozemac::Curve25519PublicKey::LENGTH
    }

    /// Serialize an Curve25519 public key to an unpadded base64
    /// representation.
    #[wasm_bindgen(js_name = "toBase64")]
    pub fn to_base64(&self) -> String {
        self.inner.to_base64()
    }
}

/// Struct holding the two public identity keys of an account.
#[wasm_bindgen(getter_with_clone)]
#[derive(Debug)]
pub struct IdentityKeys {
    /// The Ed25519 public key, used for signing.
    pub ed25519: Ed25519PublicKey,

    /// The Curve25519 public key, used for establish shared secrets.
    pub curve25519: Curve25519PublicKey,
}

impl From<matrix_sdk_crypto::olm::IdentityKeys> for IdentityKeys {
    fn from(value: matrix_sdk_crypto::olm::IdentityKeys) -> Self {
        Self {
            ed25519: Ed25519PublicKey { inner: value.ed25519 },
            curve25519: Curve25519PublicKey { inner: value.curve25519 },
        }
    }
}
