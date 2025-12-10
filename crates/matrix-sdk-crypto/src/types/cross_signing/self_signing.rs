use std::collections::btree_map::Iter;

use ruma::{OwnedDeviceKeyId, UserId, encryption::KeyUsage};
use serde::{Deserialize, Serialize};
use vodozemac::Ed25519PublicKey;

use super::{CrossSigningKey, SigningKey};
use crate::{
    DeviceData, SignatureError,
    olm::VerifyJson,
    types::{DeviceKeys, SigningKeys},
};

/// Wrapper for a cross signing key marking it as a self signing key.
///
/// Self signing keys are used to sign the user's own devices.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(try_from = "CrossSigningKey")]
pub struct SelfSigningPubkey(pub(super) CrossSigningKey);

impl SelfSigningPubkey {
    /// Get the user id of the self signing key's owner.
    pub fn user_id(&self) -> &UserId {
        &self.0.user_id
    }

    /// Get the keys map of containing the self signing keys.
    pub fn keys(&self) -> &SigningKeys<OwnedDeviceKeyId> {
        &self.0.keys
    }

    /// Get the list of `KeyUsage` that is set for this key.
    pub fn usage(&self) -> &[KeyUsage] {
        &self.0.usage
    }

    /// Get the first available self signing key.
    ///
    /// There's usually only a single key so this will usually fetch the
    /// only key.
    pub fn get_first_key(&self) -> Option<Ed25519PublicKey> {
        self.0.get_first_key_and_id().map(|(_, k)| k)
    }

    /// Verify that the [`DeviceKeys`] have a valid signature from this
    /// self-signing key.
    pub fn verify_device_keys(&self, device_keys: &DeviceKeys) -> Result<(), SignatureError> {
        if let Some((key_id, key)) = self.0.get_first_key_and_id() {
            key.verify_json(&self.0.user_id, key_id, device_keys)
        } else {
            Err(SignatureError::UnsupportedAlgorithm)
        }
    }

    /// Check if the given device is signed by this self signing key.
    ///
    /// # Arguments
    ///
    /// * `device` - The device that should be checked for a valid signature.
    ///
    /// Returns an empty result if the signature check succeeded, otherwise a
    /// SignatureError indicating why the check failed.
    pub(crate) fn verify_device(&self, device: &DeviceData) -> Result<(), SignatureError> {
        self.verify_device_keys(device.as_device_keys())
    }
}

impl<'a> IntoIterator for &'a SelfSigningPubkey {
    type Item = (&'a OwnedDeviceKeyId, &'a SigningKey);
    type IntoIter = Iter<'a, OwnedDeviceKeyId, SigningKey>;

    fn into_iter(self) -> Self::IntoIter {
        self.keys().iter()
    }
}

impl TryFrom<CrossSigningKey> for SelfSigningPubkey {
    type Error = serde_json::Error;

    fn try_from(key: CrossSigningKey) -> Result<Self, Self::Error> {
        if key.usage.contains(&KeyUsage::SelfSigning) && key.usage.len() == 1 {
            Ok(Self(key))
        } else {
            Err(serde::de::Error::custom(format!(
                "Expected cross signing key usage {} was not found",
                KeyUsage::SelfSigning
            )))
        }
    }
}

impl AsRef<CrossSigningKey> for SelfSigningPubkey {
    fn as_ref(&self) -> &CrossSigningKey {
        &self.0
    }
}

impl AsMut<CrossSigningKey> for SelfSigningPubkey {
    fn as_mut(&mut self) -> &mut CrossSigningKey {
        &mut self.0
    }
}
