use std::time::Duration;

use napi::bindgen_prelude::ToNapiValue;
use napi_derive::*;

use crate::events;

/// An encryption algorithm to be used to encrypt messages sent to a
/// room.
#[napi]
pub enum EncryptionAlgorithm {
    /// Olm version 1 using Curve25519, AES-256, and SHA-256.
    OlmV1Curve25519AesSha2,

    /// Megolm version 1 using AES-256 and SHA-256.
    MegolmV1AesSha2,
}

impl From<EncryptionAlgorithm> for ruma::EventEncryptionAlgorithm {
    fn from(value: EncryptionAlgorithm) -> Self {
        use EncryptionAlgorithm::*;

        match value {
            OlmV1Curve25519AesSha2 => Self::OlmV1Curve25519AesSha2,
            MegolmV1AesSha2 => Self::MegolmV1AesSha2,
        }
    }
}

impl From<ruma::EventEncryptionAlgorithm> for EncryptionAlgorithm {
    fn from(value: ruma::EventEncryptionAlgorithm) -> Self {
        use ruma::EventEncryptionAlgorithm::*;

        match value {
            OlmV1Curve25519AesSha2 => Self::OlmV1Curve25519AesSha2,
            MegolmV1AesSha2 => Self::MegolmV1AesSha2,
            _ => unreachable!("Unknown variant"),
        }
    }
}

/// Settings for an encrypted room.
///
/// This determines the algorithm and rotation periods of a group
/// session.
#[napi]
pub struct EncryptionSettings {
    /// The encryption algorithm that should be used in the room.
    pub algorithm: EncryptionAlgorithm,

    /// How long the session should be used before changing it,
    /// expressed in microseconds.
    pub rotation_period: u32,

    /// How many messages should be sent before changing the session.
    pub rotation_period_messages: u32,

    /// The history visibility of the room when the session was
    /// created.
    pub history_visibility: events::HistoryVisibility,
}

impl Default for EncryptionSettings {
    fn default() -> Self {
        let default = matrix_sdk_crypto::olm::EncryptionSettings::default();

        Self {
            algorithm: default.algorithm.into(),
            rotation_period: default.rotation_period.as_micros().try_into().unwrap(),
            rotation_period_messages: default.rotation_period_msgs.try_into().unwrap(),
            history_visibility: default.history_visibility.into(),
        }
    }
}

#[napi]
impl EncryptionSettings {
    /// Create a new `EncryptionSettings` with default values.
    #[napi(constructor)]
    pub fn new() -> EncryptionSettings {
        Self::default()
    }
}

impl From<&EncryptionSettings> for matrix_sdk_crypto::olm::EncryptionSettings {
    fn from(value: &EncryptionSettings) -> Self {
        Self {
            algorithm: value.algorithm.clone().into(),
            rotation_period: Duration::from_micros(value.rotation_period.into()),
            rotation_period_msgs: value.rotation_period_messages.into(),
            history_visibility: value.history_visibility.clone().into(),
        }
    }
}

#[napi]
pub enum VerificationState {
    /// The device is trusted.
    Trusted,

    /// The device is not trusted.
    Untrusted,

    /// The device is not known to us.
    UnknownDevice,
}

impl From<&matrix_sdk_common::deserialized_responses::VerificationState> for VerificationState {
    fn from(value: &matrix_sdk_common::deserialized_responses::VerificationState) -> Self {
        use matrix_sdk_common::deserialized_responses::VerificationState::*;

        match value {
            Trusted => Self::Trusted,
            Untrusted => Self::Untrusted,
            UnknownDevice => Self::UnknownDevice,
        }
    }
}
