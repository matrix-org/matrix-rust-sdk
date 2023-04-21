#![allow(missing_docs)]

use matrix_sdk_crypto::{
    store::CryptoStoreError as InnerStoreError, KeyExportError, MegolmError, OlmError,
    SecretImportError as RustSecretImportError, SignatureError as InnerSignatureError,
};
use matrix_sdk_sqlite::OpenStoreError;
use ruma::{IdParseError, OwnedUserId};

#[derive(Debug, thiserror::Error)]
pub enum KeyImportError {
    #[error(transparent)]
    Export(#[from] KeyExportError),
    #[error(transparent)]
    CryptoStore(#[from] InnerStoreError),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

#[derive(Debug, thiserror::Error, uniffi::Error)]
#[uniffi(flat_error)]
pub enum SecretImportError {
    #[error(transparent)]
    CryptoStore(#[from] InnerStoreError),
    #[error(transparent)]
    Import(#[from] RustSecretImportError),
}

#[derive(Debug, thiserror::Error)]
pub enum SignatureError {
    #[error(transparent)]
    Signature(#[from] InnerSignatureError),
    #[error(transparent)]
    Identifier(#[from] IdParseError),
    #[error(transparent)]
    CryptoStore(#[from] InnerStoreError),
    #[error("Unknown device {0} {1}")]
    UnknownDevice(OwnedUserId, String),
    #[error("Unknown user identity {0}")]
    UnknownUserIdentity(String),
}

#[derive(Debug, thiserror::Error)]
pub enum CryptoStoreError {
    #[error("Failed to open the store")]
    OpenStore(#[from] OpenStoreError),
    #[error(transparent)]
    CryptoStore(#[from] InnerStoreError),
    #[error(transparent)]
    OlmError(#[from] OlmError),
    #[error(transparent)]
    Serialization(#[from] serde_json::Error),
    #[error("The given string is not a valid user ID: source {0}, error {1}")]
    InvalidUserId(String, IdParseError),
    #[error(transparent)]
    Identifier(#[from] IdParseError),
}

#[derive(Debug, uniffi::Error)]
pub enum DecryptionError {
    Serialization { error: String },
    Identifier { error: String },
    Megolm { error: String },
    MissingRoomKey { error: String, withheld_code: Option<String> },
    Store { error: String },
}

impl From<MegolmError> for DecryptionError {
    fn from(value: MegolmError) -> Self {
        match value {
            MegolmError::MissingRoomKey(withheld_code) => Self::MissingRoomKey {
                error: "Withheld Inbound group session".to_owned(),
                withheld_code: withheld_code.map(|w| w.as_str().to_owned()),
            },
            _ => Self::Megolm { error: value.to_string() },
        }
    }
}

impl From<serde_json::Error> for DecryptionError {
    fn from(err: serde_json::Error) -> Self {
        Self::Serialization { error: err.to_string() }
    }
}

impl From<IdParseError> for DecryptionError {
    fn from(err: IdParseError) -> Self {
        Self::Identifier { error: err.to_string() }
    }
}

impl From<InnerStoreError> for DecryptionError {
    fn from(err: InnerStoreError) -> Self {
        Self::Store { error: err.to_string() }
    }
}

#[cfg(test)]
mod tests {

    use assert_matches::assert_matches;

    use super::*;

    #[test]
    fn test_withheld_error_mapping() {
        use matrix_sdk_crypto::types::events::room_key_withheld::WithheldCode;

        let inner_error = MegolmError::MissingRoomKey(Some(WithheldCode::Unverified));

        let binding_error: DecryptionError = inner_error.into();

        let code = assert_matches!(
            binding_error,
            DecryptionError::MissingRoomKey { error: _, withheld_code: Some(withheld_code) } => withheld_code
        );
        assert_eq!("m.unverified", code)
    }
}
