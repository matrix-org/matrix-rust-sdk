use std::io::Error as IoError;
use std::sync::Arc;
use url::ParseError;

use async_trait::async_trait;
use thiserror::Error;
use tokio::sync::Mutex;

use super::olm::Account;
use olm_rs::errors::OlmAccountError;

#[cfg(feature = "sqlite-cryptostore")]
pub mod sqlite;

#[cfg(feature = "sqlite-cryptostore")]
use sqlx::Error as SqlxError;

#[derive(Error, Debug)]
pub enum CryptoStoreError {
    #[error("can't read or write from the store")]
    Io(#[from] IoError),
    #[error("can't finish Olm account operation {0}")]
    OlmAccountError(#[from] OlmAccountError),
    #[error("URL can't be parsed")]
    UrlParse(#[from] ParseError),
    // TODO flatten the SqlxError to make it easier for other store
    // implementations.
    #[cfg(feature = "sqlite-cryptostore")]
    #[error("database error")]
    DatabaseError(#[from] SqlxError),
}

pub type Result<T> = std::result::Result<T, CryptoStoreError>;

#[async_trait]
pub trait CryptoStore {
    async fn load_account(&self) -> Result<Option<Account>>;
    async fn save_account(&self, account: Arc<Mutex<Account>>) -> Result<()>;
}
