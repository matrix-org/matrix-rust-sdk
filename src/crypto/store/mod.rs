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
use std::collections::HashMap;
use std::io::Error as IoError;
use std::result::Result as StdResult;
use std::sync::Arc;
use url::ParseError;

use async_trait::async_trait;
use thiserror::Error;
use tokio::sync::Mutex;

use super::olm::{Account, Session};
use olm_rs::errors::{OlmAccountError, OlmSessionError};
use olm_rs::PicklingMode;

#[cfg(feature = "sqlite-cryptostore")]
pub mod sqlite;

#[cfg(feature = "sqlite-cryptostore")]
use sqlx::{sqlite::Sqlite, Error as SqlxError};

#[derive(Error, Debug)]
pub enum CryptoStoreError {
    #[error("can't read or write from the store")]
    Io(#[from] IoError),
    #[error("can't finish Olm Account operation {0}")]
    OlmAccountError(#[from] OlmAccountError),
    #[error("can't finish Olm Session operation {0}")]
    OlmSessionError(#[from] OlmSessionError),
    #[error("URL can't be parsed")]
    UrlParse(#[from] ParseError),
    // TODO flatten the SqlxError to make it easier for other store
    // implementations.
    #[cfg(feature = "sqlite-cryptostore")]
    #[error("database error")]
    DatabaseError(#[from] SqlxError<Sqlite>),
}

pub type Result<T> = std::result::Result<T, CryptoStoreError>;

#[async_trait]
pub trait CryptoStore: Debug {
    async fn load_account(&mut self) -> Result<Option<Account>>;
    async fn save_account(&mut self, account: Arc<Mutex<Account>>) -> Result<()>;
    async fn sessions_mut(&mut self, sender_key: &str) -> Result<Option<&mut Vec<Session>>>;
}

pub struct MemoryStore {
    pub(crate) account_info: Option<(String, bool)>,
    sessions: HashMap<String, Vec<Session>>,
}

impl MemoryStore {
    /// Create a new empty memory store.
    pub fn new() -> Self {
        MemoryStore {
            account_info: None,
            sessions: HashMap::new(),
        }
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

    async fn sessions_mut<'a>(
        &'a mut self,
        sender_key: &str,
    ) -> Result<Option<&'a mut Vec<Session>>> {
        Ok(self.sessions.get_mut(sender_key))
    }
}

impl std::fmt::Debug for MemoryStore {
    fn fmt(&self, fmt: &mut std::fmt::Formatter<'_>) -> StdResult<(), std::fmt::Error> {
        write!(
            fmt,
            "MemoryStore {{ account_stored: {}, account shared: {} }}",
            self.account_info.is_some(),
            self.account_info.as_ref().map_or(false, |a| a.1)
        )
    }
}
