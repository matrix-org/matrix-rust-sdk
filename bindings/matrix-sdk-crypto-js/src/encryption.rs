//! Encryption types & siblings.

use std::time::Duration;

use wasm_bindgen::prelude::*;

use crate::events;

/// Settings for an encrypted room.
///
/// This determines the algorithm and rotation periods of a group
/// session.
#[wasm_bindgen(getter_with_clone)]
#[derive(Debug, Clone)]
pub struct EncryptionSettings {
    /// The encryption algorithm that should be used in the room.
    pub algorithm: EncryptionAlgorithm,

    /// How long the session should be used before changing it,
    /// expressed in microseconds.
    #[wasm_bindgen(js_name = "rotationPeriod")]
    pub rotation_period: u64,

    /// How many messages should be sent before changing the session.
    #[wasm_bindgen(js_name = "rotationPeriodMessages")]
    pub rotation_period_messages: u64,

    /// The history visibility of the room when the session was
    /// created.
    #[wasm_bindgen(js_name = "historyVisibility")]
    pub history_visibility: events::HistoryVisibility,

    /// Should untrusted devices receive the room key, or should they be
    /// excluded from the conversation.
    #[wasm_bindgen(js_name = "onlyAllowTrustedDevices")]
    pub only_allow_trusted_devices: bool,
}

impl Default for EncryptionSettings {
    fn default() -> Self {
        let default = matrix_sdk_crypto::olm::EncryptionSettings::default();

        Self {
            algorithm: default.algorithm.into(),
            rotation_period: default.rotation_period.as_micros().try_into().unwrap(),
            rotation_period_messages: default.rotation_period_msgs,
            history_visibility: default.history_visibility.into(),
            only_allow_trusted_devices: default.only_allow_trusted_devices,
        }
    }
}

#[wasm_bindgen]
impl EncryptionSettings {
    /// Create a new `EncryptionSettings` with default values.
    #[wasm_bindgen(constructor)]
    pub fn new() -> EncryptionSettings {
        Self::default()
    }
}

impl From<&EncryptionSettings> for matrix_sdk_crypto::olm::EncryptionSettings {
    fn from(value: &EncryptionSettings) -> Self {
        let algorithm = value.algorithm.clone().into();

        Self {
            algorithm,
            rotation_period: Duration::from_micros(value.rotation_period),
            rotation_period_msgs: value.rotation_period_messages,
            history_visibility: value.history_visibility.clone().into(),
            only_allow_trusted_devices: value.only_allow_trusted_devices,
        }
    }
}

/// An encryption algorithm to be used to encrypt messages sent to a
/// room.
#[wasm_bindgen]
#[derive(Debug, Clone)]
pub enum EncryptionAlgorithm {
    /// Olm version 1 using Curve25519, AES-256, and SHA-256.
    OlmV1Curve25519AesSha2,

    /// Megolm version 1 using AES-256 and SHA-256.
    MegolmV1AesSha2,
}

impl From<EncryptionAlgorithm> for matrix_sdk_crypto::types::EventEncryptionAlgorithm {
    fn from(value: EncryptionAlgorithm) -> Self {
        use EncryptionAlgorithm::*;

        match value {
            OlmV1Curve25519AesSha2 => Self::OlmV1Curve25519AesSha2,
            MegolmV1AesSha2 => Self::MegolmV1AesSha2,
        }
    }
}

impl From<matrix_sdk_crypto::types::EventEncryptionAlgorithm> for EncryptionAlgorithm {
    fn from(value: matrix_sdk_crypto::types::EventEncryptionAlgorithm) -> Self {
        use matrix_sdk_crypto::types::EventEncryptionAlgorithm::*;

        match value {
            OlmV1Curve25519AesSha2 => Self::OlmV1Curve25519AesSha2,
            MegolmV1AesSha2 => Self::MegolmV1AesSha2,
            _ => unreachable!("Unknown variant"),
        }
    }
}

/// The verification state of the device that sent an event to us.
#[wasm_bindgen]
#[derive(Debug)]
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
