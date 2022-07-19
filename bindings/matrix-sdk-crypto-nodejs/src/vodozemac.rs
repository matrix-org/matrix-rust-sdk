use napi_derive::*;

use crate::into_err;

/// An Ed25519 public key, used to verify digital signatures.
#[napi]
#[derive(Clone)]
pub struct Ed25519PublicKey {
    inner: vodozemac::Ed25519PublicKey,
}

#[napi]
impl Ed25519PublicKey {
    /// The number of bytes an Ed25519 public key has.
    #[napi(getter)]
    pub fn length(&self) -> u32 {
        vodozemac::Ed25519PublicKey::LENGTH as u32
    }

    /// Serialize an Ed25519 public key to an unpadded base64
    /// representation.
    #[napi]
    pub fn to_base64(&self) -> String {
        self.inner.to_base64()
    }
}

/// An Ed25519 digital signature, can be used to verify the
/// authenticity of a message.
#[napi]
pub struct Ed25519Signature {
    pub(crate) inner: vodozemac::Ed25519Signature,
}

impl From<vodozemac::Ed25519Signature> for Ed25519Signature {
    fn from(inner: vodozemac::Ed25519Signature) -> Self {
        Self { inner }
    }
}

#[napi]
impl Ed25519Signature {
    /// Try to create an Ed25519 signature from an unpadded base64
    /// representation.
    #[napi(constructor, strict)]
    pub fn new(signature: String) -> napi::Result<Self> {
        Ok(Self {
            inner: vodozemac::Ed25519Signature::from_base64(signature.as_str())
                .map_err(into_err)?,
        })
    }

    /// Serialize a Ed25519 signature to an unpadded base64
    /// representation.
    #[napi]
    pub fn to_base64(&self) -> String {
        self.inner.to_base64()
    }
}

/// A Curve25519 public key.
#[napi]
#[derive(Clone)]
pub struct Curve25519PublicKey {
    inner: vodozemac::Curve25519PublicKey,
}

#[napi]
impl Curve25519PublicKey {
    /// The number of bytes a Curve25519 public key has.
    #[napi(getter)]
    pub fn length(&self) -> u32 {
        vodozemac::Curve25519PublicKey::LENGTH as u32
    }

    /// Serialize an Curve25519 public key to an unpadded base64
    /// representation.
    #[napi]
    pub fn to_base64(&self) -> String {
        self.inner.to_base64()
    }
}

/// Struct holding the two public identity keys of an account.
#[napi]
pub struct IdentityKeys {
    ed25519: Ed25519PublicKey,
    curve25519: Curve25519PublicKey,
}

#[napi]
impl IdentityKeys {
    /// The Ed25519 public key, used for signing.
    #[napi(getter)]
    pub fn ed25519(&self) -> Ed25519PublicKey {
        self.ed25519.clone()
    }

    /// The Curve25519 public key, used for establish shared secrets.
    #[napi(getter)]
    pub fn curve25519(&self) -> Curve25519PublicKey {
        self.curve25519.clone()
    }
}

impl From<matrix_sdk_crypto::olm::IdentityKeys> for IdentityKeys {
    fn from(value: matrix_sdk_crypto::olm::IdentityKeys) -> Self {
        Self {
            ed25519: Ed25519PublicKey { inner: value.ed25519 },
            curve25519: Curve25519PublicKey { inner: value.curve25519 },
        }
    }
}
