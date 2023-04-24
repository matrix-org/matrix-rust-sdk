use std::time::Duration;

use matrix_sdk_common::deserialized_responses::ShieldState as RustShieldState;
use napi::bindgen_prelude::{BigInt, FromNapiValue, ToNapiValue};
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
    pub rotation_period: BigInt,

    /// How many messages should be sent before changing the session.
    pub rotation_period_messages: BigInt,

    /// The history visibility of the room when the session was
    /// created.
    pub history_visibility: events::HistoryVisibility,

    /// Should untrusted devices receive the room key, or should they be
    /// excluded from the conversation.
    pub only_allow_trusted_devices: bool,
}

impl Default for EncryptionSettings {
    fn default() -> Self {
        let default = matrix_sdk_crypto::olm::EncryptionSettings::default();

        Self {
            algorithm: default.algorithm.into(),
            rotation_period: {
                let n: u64 = default.rotation_period.as_micros().try_into().unwrap();

                n.into()
            },
            rotation_period_messages: {
                let n = default.rotation_period_msgs;

                n.into()
            },
            history_visibility: default.history_visibility.into(),
            only_allow_trusted_devices: default.only_allow_trusted_devices,
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
            algorithm: value.algorithm.into(),
            rotation_period: Duration::from_micros(value.rotation_period.get_u64().1),
            rotation_period_msgs: value.rotation_period_messages.get_u64().1,
            history_visibility: value.history_visibility.into(),
            only_allow_trusted_devices: value.only_allow_trusted_devices,
        }
    }
}

/// Take a look at [`matrix_sdk_common::deserialized_responses::ShieldState`]
/// for more info.
#[napi]
pub enum ShieldColor {
    Red,
    Grey,
    None,
}

/// Take a look at [`matrix_sdk_common::deserialized_responses::ShieldState`]
/// for more info.
#[napi]
pub struct ShieldState {
    pub color: ShieldColor,
    pub message: Option<&'static str>,
}

impl From<RustShieldState> for ShieldState {
    fn from(value: RustShieldState) -> Self {
        match value {
            RustShieldState::Red { message } => {
                ShieldState { color: ShieldColor::Red, message: Some(message) }
            }
            RustShieldState::Grey { message } => {
                ShieldState { color: ShieldColor::Grey, message: Some(message) }
            }
            RustShieldState::None => ShieldState { color: ShieldColor::None, message: None },
        }
    }
}
