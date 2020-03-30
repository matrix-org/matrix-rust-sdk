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

use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::Mutex;

use super::{Account, CryptoStore, CryptoStoreError, InboundGroupSession, Result, Session};
use crate::crypto::memory_stores::{GroupSessionStore, SessionStore};

#[derive(Debug)]
pub struct MemoryStore {
    sessions: SessionStore,
    inbound_group_sessions: GroupSessionStore,
}

impl MemoryStore {
    pub fn new() -> Self {
        MemoryStore {
            sessions: SessionStore::new(),
            inbound_group_sessions: GroupSessionStore::new(),
        }
    }
}

#[async_trait]
impl CryptoStore for MemoryStore {
    async fn load_account(&mut self) -> Result<Option<Account>> {
        Ok(None)
    }

    async fn save_account(&mut self, account: Arc<Mutex<Account>>) -> Result<()> {
        Ok(())
    }

    async fn save_session(&mut self, session: Arc<Mutex<Session>>) -> Result<()> {
        Ok(())
    }

    async fn add_and_save_session(&mut self, session: Session) -> Result<()> {
        self.sessions.add(session).await;
        Ok(())
    }

    async fn get_sessions(
        &mut self,
        sender_key: &str,
    ) -> Result<Option<Arc<Mutex<Vec<Arc<Mutex<Session>>>>>>> {
        Ok(self.sessions.get(sender_key))
    }

    async fn save_inbound_group_session(&mut self, session: InboundGroupSession) -> Result<()> {
        self.inbound_group_sessions.add(session);
        Ok(())
    }

    async fn get_inbound_group_session(
        &mut self,
        room_id: &str,
        sender_key: &str,
        session_id: &str,
    ) -> Result<Option<Arc<Mutex<InboundGroupSession>>>> {
        Ok(self
            .inbound_group_sessions
            .get(room_id, sender_key, session_id))
    }
}
