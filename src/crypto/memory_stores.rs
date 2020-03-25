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

use super::olm::InboundGroupSession;
use std::collections::HashMap;

#[derive(Debug)]
pub struct GroupSessionStore {
    entries: HashMap<String, HashMap<String, HashMap<String, InboundGroupSession>>>,
}

impl GroupSessionStore {
    pub fn new() -> Self {
        GroupSessionStore {
            entries: HashMap::new(),
        }
    }

    pub fn add(&mut self, session: InboundGroupSession) -> bool {
        if !self.entries.contains_key(&session.room_id) {
            self.entries
                .insert(session.room_id.to_owned(), HashMap::new());
        }

        let mut room_map = self.entries.get_mut(&session.room_id).unwrap();

        if !room_map.contains_key(&session.sender_key) {
            room_map.insert(session.sender_key.to_owned(), HashMap::new());
        }

        let mut sender_map = room_map.get_mut(&session.sender_key).unwrap();
        let ret = sender_map.insert(session.session_id(), session);

        ret.is_some()
    }

    pub fn get(
        &self,
        room_id: &str,
        sender_key: &str,
        session_id: &str,
    ) -> Option<&InboundGroupSession> {
        self.entries
            .get(room_id)
            .and_then(|m| m.get(sender_key).and_then(|m| m.get(session_id)))
    }
}
