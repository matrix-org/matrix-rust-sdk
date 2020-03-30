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
use serde_json::Error as SerdeError;
use thiserror::Error;
use tokio::sync::Mutex;

use super::olm::{Account, InboundGroupSession, Session};
use olm_rs::errors::{OlmAccountError, OlmSessionError};
use olm_rs::PicklingMode;

pub mod memorystore;
#[cfg(feature = "sqlite-cryptostore")]
pub mod sqlite;

#[cfg(feature = "sqlite-cryptostore")]
use sqlx::Error as SqlxError;

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
    #[error("error serializing data for the database")]
    Serialization(#[from] SerdeError),
    #[error("can't load session timestamps")]
    SessionTimestampError,
    #[error("can't save/load sessions or group sessions in the store before a account is stored")]
    AccountUnset,
    // TODO flatten the SqlxError to make it easier for other store
    // implementations.
    #[cfg(feature = "sqlite-cryptostore")]
    #[error("database error")]
    DatabaseError(#[from] SqlxError),
}

pub type Result<T> = std::result::Result<T, CryptoStoreError>;

#[async_trait]
pub trait CryptoStore: Debug + Send + Sync {
    async fn load_account(&mut self) -> Result<Option<Account>>;
    async fn save_account(&mut self, account: Arc<Mutex<Account>>) -> Result<()>;
    async fn save_session(&mut self, session: Arc<Mutex<Session>>) -> Result<()>;
    async fn add_and_save_session(&mut self, session: Session) -> Result<()>;
    async fn get_sessions(
        &mut self,
        sender_key: &str,
    ) -> Result<Option<Arc<Mutex<Vec<Arc<Mutex<Session>>>>>>>;
    async fn save_inbound_group_session(&mut self, session: InboundGroupSession) -> Result<()>;
    async fn get_inbound_group_session(
        &mut self,
        room_id: &str,
        sender_key: &str,
        session_id: &str,
    ) -> Result<Option<Arc<Mutex<InboundGroupSession>>>>;
}
