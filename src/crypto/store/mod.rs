use std::io::Result;
use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::Mutex;

use super::olm::Account;

#[cfg(feature = "sqlite-cryptostore")]
pub mod sqlite;

#[async_trait]
pub trait CryptoStore {
    async fn load_account(&self) -> Result<Account>;
    async fn save_account(&self, account: Arc<Mutex<Account>>) -> Result<()>;
}
