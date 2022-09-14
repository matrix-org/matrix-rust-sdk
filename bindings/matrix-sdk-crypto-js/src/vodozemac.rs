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

impl From<vodozemac::Ed25519PublicKey> for Ed25519PublicKey {
    fn from(inner: vodozemac::Ed25519PublicKey) -> Self {
        Self { inner }
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

impl From<vodozemac::Curve25519PublicKey> for Curve25519PublicKey {
    fn from(inner: vodozemac::Curve25519PublicKey) -> Self {
        Self { inner }
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

/// An enum over the different key types a device can have.
///
/// Currently devices have a curve25519 and ed25519 keypair. The keys
/// transport format is a base64 encoded string, any unknown key type
/// will be left as such a string.
#[wasm_bindgen]
#[derive(Debug)]
pub struct DeviceKey {
    inner: matrix_sdk_crypto::types::DeviceKey,
}

impl From<matrix_sdk_crypto::types::DeviceKey> for DeviceKey {
    fn from(inner: matrix_sdk_crypto::types::DeviceKey) -> Self {
        Self { inner }
    }
}

#[wasm_bindgen]
impl DeviceKey {
    /// Get the name of the device key.
    #[wasm_bindgen(getter)]
    pub fn name(&self) -> DeviceKeyName {
        (&self.inner).into()
    }

    /// Get the value associated to the `Curve25519` device key name.
    #[wasm_bindgen(getter)]
    pub fn curve25519(&self) -> Option<Curve25519PublicKey> {
        use matrix_sdk_crypto::types::DeviceKey::*;

        match &self.inner {
            Curve25519(key) => Some((*key).into()),
            _ => None,
        }
    }

    /// Get the value associated to the `Ed25519` device key name.
    #[wasm_bindgen(getter)]
    pub fn ed25519(&self) -> Option<Ed25519PublicKey> {
        use matrix_sdk_crypto::types::DeviceKey::*;

        match &self.inner {
            Ed25519(key) => Some((*key).into()),
            _ => None,
        }
    }

    /// Get the value associated to the `Unknown` device key name.
    #[wasm_bindgen(getter)]
    pub fn unknown(&self) -> Option<String> {
        use matrix_sdk_crypto::types::DeviceKey::*;

        match &self.inner {
            Unknown(key) => Some(key.clone()),
            _ => None,
        }
    }

    /// Convert the `DeviceKey` into a base64 encoded string.
    #[wasm_bindgen(js_name = "toBase64")]
    pub fn to_base64(&self) -> String {
        self.inner.to_base64()
    }
}

impl From<&matrix_sdk_crypto::types::DeviceKey> for DeviceKeyName {
    fn from(device_key: &matrix_sdk_crypto::types::DeviceKey) -> Self {
        use matrix_sdk_crypto::types::DeviceKey::*;

        match device_key {
            Curve25519(_) => Self::Curve25519,
            Ed25519(_) => Self::Ed25519,
            Unknown(_) => Self::Unknown,
        }
    }
}

/// An enum over the different key types a device can have.
///
/// Currently devices have a curve25519 and ed25519 keypair. The keys
/// transport format is a base64 encoded string, any unknown key type
/// will be left as such a string.
#[wasm_bindgen]
#[derive(Debug)]
pub enum DeviceKeyName {
    /// The curve25519 device key.
    Curve25519,

    /// The ed25519 device key.
    Ed25519,

    /// An unknown device key.
    Unknown,
}
