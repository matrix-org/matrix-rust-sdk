use matrix_sdk::{self, encryption::CryptoStoreError, HttpError, IdParseError, StoreError};

#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum ClientError {
    #[error("client error: {msg}")]
    Generic { msg: String },
}

impl From<anyhow::Error> for ClientError {
    fn from(e: anyhow::Error) -> ClientError {
        ClientError::Generic { msg: e.to_string() }
    }
}

impl From<matrix_sdk::Error> for ClientError {
    fn from(e: matrix_sdk::Error) -> Self {
        anyhow::Error::from(e).into()
    }
}

impl From<StoreError> for ClientError {
    fn from(e: StoreError) -> Self {
        anyhow::Error::from(e).into()
    }
}

impl From<CryptoStoreError> for ClientError {
    fn from(e: CryptoStoreError) -> Self {
        anyhow::Error::from(e).into()
    }
}

impl From<HttpError> for ClientError {
    fn from(e: HttpError) -> Self {
        anyhow::Error::from(e).into()
    }
}

impl From<IdParseError> for ClientError {
    fn from(e: IdParseError) -> Self {
        anyhow::Error::from(e).into()
    }
}

impl From<serde_json::Error> for ClientError {
    fn from(e: serde_json::Error) -> Self {
        anyhow::Error::from(e).into()
    }
}

impl From<url::ParseError> for ClientError {
    fn from(e: url::ParseError) -> Self {
        anyhow::Error::from(e).into()
    }
}

impl From<mime::FromStrError> for ClientError {
    fn from(e: mime::FromStrError) -> Self {
        anyhow::Error::from(e).into()
    }
}
