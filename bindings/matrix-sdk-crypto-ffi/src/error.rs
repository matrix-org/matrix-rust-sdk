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

#[derive(Debug, thiserror::Error)]
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

#[derive(Debug, thiserror::Error)]
pub enum DecryptionError {
    #[error(transparent)]
    Serialization(#[from] serde_json::Error),
    #[error(transparent)]
    Identifier(#[from] IdParseError),
    #[error("Megolm decryption error: {error_message}")]
    Megolm { error_message: String },
    #[error("Can't find the room key to decrypt the event")]
    MissingRoomKey,
    #[error(transparent)]
    Store(#[from] InnerStoreError),
}

impl From<MegolmError> for DecryptionError {
    fn from(value: MegolmError) -> Self {
        match value {
            MegolmError::MissingRoomKey => Self::MissingRoomKey,
            _ => Self::Megolm { error_message: value.to_string() },
        }
    }
}
