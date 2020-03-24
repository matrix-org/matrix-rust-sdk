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

use olm_rs::inbound_group_session::OlmInboundGroupSession;
use std::collections::HashMap;

#[derive(Debug)]
pub struct GroupSessionStore {
    entries: HashMap<String, HashMap<String, HashMap<String, OlmInboundGroupSession>>>,
}

impl GroupSessionStore {
    pub fn new() -> Self {
        GroupSessionStore {
            entries: HashMap::new(),
        }
    }

    pub fn add(
        &mut self,
        room_id: &str,
        sender_key: &str,
        session: OlmInboundGroupSession,
    ) -> bool {
        if !self.entries.contains_key(room_id) {
            self.entries.insert(room_id.to_owned(), HashMap::new());
        }

        let mut room_map = self.entries.get_mut(room_id).unwrap();

        if !room_map.contains_key(sender_key) {
            room_map.insert(sender_key.to_owned(), HashMap::new());
        }

        let mut sender_map = room_map.get_mut(sender_key).unwrap();
        let ret = sender_map.insert(session.session_id(), session);

        ret.is_some()
    }

    pub fn get(
        &self,
        room_id: &str,
        sender_key: &str,
        session_id: &str,
    ) -> Option<&OlmInboundGroupSession> {
        self.entries
            .get(room_id)
            .and_then(|m| m.get(sender_key).and_then(|m| m.get(session_id)))
    }
}
