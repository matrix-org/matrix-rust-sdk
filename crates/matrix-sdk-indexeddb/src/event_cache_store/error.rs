use matrix_sdk_base::{event_cache::store::EventCacheStoreError, StoreError};
use matrix_sdk_crypto::store::CryptoStoreError;
use matrix_sdk_store_encryption::Error as EncryptionError;

#[derive(Debug, thiserror::Error)]
pub enum IndexeddbEventCacheStoreError {
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Encryption(#[from] EncryptionError),
    #[error("DomException {name} ({code}): {message}")]
    DomException { name: String, message: String, code: u16 },
    #[error(transparent)]
    StoreError(#[from] StoreError),
    #[error("Can't migrate {name} from {old_version} to {new_version} without deleting data. See MigrationConflictStrategy for ways to configure.")]
    MigrationConflict { name: String, old_version: u32, new_version: u32 },
    #[error(transparent)]
    CryptoStoreError(#[from] CryptoStoreError),
}

impl From<web_sys::DomException> for IndexeddbEventCacheStoreError {
    fn from(frm: web_sys::DomException) -> IndexeddbEventCacheStoreError {
        IndexeddbEventCacheStoreError::DomException {
            name: frm.name(),
            message: frm.message(),
            code: frm.code(),
        }
    }
}

impl From<IndexeddbEventCacheStoreError> for StoreError {
    fn from(e: IndexeddbEventCacheStoreError) -> Self {
        match e {
            IndexeddbEventCacheStoreError::Json(e) => StoreError::Json(e),
            IndexeddbEventCacheStoreError::StoreError(e) => e,
            IndexeddbEventCacheStoreError::Encryption(e) => StoreError::Encryption(e),
            _ => StoreError::backend(e),
        }
    }
}

impl From<IndexeddbEventCacheStoreError> for EventCacheStoreError {
    fn from(e: IndexeddbEventCacheStoreError) -> Self {
        match e {
            IndexeddbEventCacheStoreError::Json(e) => EventCacheStoreError::Serialization(e),
            IndexeddbEventCacheStoreError::StoreError(e) => {
                EventCacheStoreError::Backend(Box::new(e))
            }
            IndexeddbEventCacheStoreError::Encryption(e) => EventCacheStoreError::Encryption(e),
            _ => EventCacheStoreError::backend(e),
        }
    }
}

impl From<IndexeddbEventCacheStoreError> for CryptoStoreError {
    fn from(frm: IndexeddbEventCacheStoreError) -> CryptoStoreError {
        match frm {
            IndexeddbEventCacheStoreError::Json(e) => CryptoStoreError::Serialization(e),
            IndexeddbEventCacheStoreError::CryptoStoreError(e) => e,
            _ => CryptoStoreError::backend(frm),
        }
    }
}

impl From<serde_wasm_bindgen::Error> for IndexeddbEventCacheStoreError {
    fn from(e: serde_wasm_bindgen::Error) -> Self {
        IndexeddbEventCacheStoreError::Json(serde::de::Error::custom(e.to_string()))
    }
}

impl From<rmp_serde::encode::Error> for IndexeddbEventCacheStoreError {
    fn from(e: rmp_serde::encode::Error) -> Self {
        IndexeddbEventCacheStoreError::Json(serde::ser::Error::custom(e.to_string()))
    }
}

impl From<rmp_serde::decode::Error> for IndexeddbEventCacheStoreError {
    fn from(e: rmp_serde::decode::Error) -> Self {
        IndexeddbEventCacheStoreError::Json(serde::de::Error::custom(e.to_string()))
    }
}
