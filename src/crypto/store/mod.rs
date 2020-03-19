// Copyright 2020 The Matrix.org Foundation C.I.C.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use core::fmt::Debug;
use std::io::Error as IoError;
use std::sync::Arc;
use url::ParseError;

use async_trait::async_trait;
use thiserror::Error;
use tokio::sync::Mutex;

use super::olm::Account;
use olm_rs::errors::OlmAccountError;
use olm_rs::PicklingMode;

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
pub trait CryptoStore: Debug {
    async fn load_account(&mut self) -> Result<Option<Account>>;
    async fn save_account(&mut self, account: Arc<Mutex<Account>>) -> Result<()>;
}

#[derive(Debug)]
pub struct MemoryStore {
    pub(crate) account_info: Option<(String, bool)>,
}

impl MemoryStore {
    /// Create a new empty memory store.
    pub fn new() -> Self {
        MemoryStore { account_info: None }
    }
}

#[async_trait]
impl CryptoStore for MemoryStore {
    async fn load_account(&mut self) -> Result<Option<Account>> {
        let result = match &self.account_info {
            Some((pickle, shared)) => Some(Account::from_pickle(
                pickle.to_owned(),
                PicklingMode::Unencrypted,
                *shared,
            )?),
            None => None,
        };
        Ok(result)
    }

    async fn save_account(&mut self, account: Arc<Mutex<Account>>) -> Result<()> {
        let acc = account.lock().await;
        let pickle = acc.pickle(PicklingMode::Unencrypted);
        self.account_info = Some((pickle, acc.shared));
        Ok(())
    }
}
