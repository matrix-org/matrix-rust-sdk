// Copyright 2020 Damir Jelić
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

use std::path::Path;

pub mod state_store;
pub use state_store::JsonStore;

use serde::{Deserialize, Serialize};

use crate::base_client::Token;
use crate::events::push_rules::Ruleset;
use crate::identifiers::{RoomId, UserId};
use crate::models::Room;
use crate::session::Session;

#[derive(Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ClientState {
    /// The current client session containing our user id, device id and access
    /// token.
    pub session: Option<Session>,
    /// The current sync token that should be used for the next sync call.
    pub sync_token: Option<Token>,
    /// A list of ignored users.
    pub ignored_users: Vec<UserId>,
    /// The push ruleset for the logged in user.
    pub push_ruleset: Option<Ruleset>,
}

/// Abstraction around the data store to avoid unnecessary request on client initialization.
pub trait StateStore: Send + Sync {
    /// The type of store to create. The default `JsonStore` uses `ClientState` as the store
    /// to serialize and deserialize state to JSON files.
    type Store;

    /// The error type to return.
    type IoError;

    /// Set up connections or open files to load/save state.
    fn open(&self, path: &Path) -> Result<(), Self::IoError>;
    ///
    fn load_client_state(&self) -> Result<Self::Store, Self::IoError>;
    ///
    fn load_room_state(&self, room_id: &RoomId) -> Result<Room, Self::IoError>;
    ///
    fn store_client_state(&self, _: Self::Store) -> Result<(), Self::IoError>;
    ///
    fn store_room_state(&self, _: &Room) -> Result<(), Self::IoError>;
}

#[cfg(test)]
mod test {
    use super::*;

    use std::collections::HashMap;
    use std::convert::TryFrom;

    use crate::identifiers::{RoomId, UserId};

    #[test]
    fn serialize() {
        let id = RoomId::try_from("!roomid:example.com").unwrap();
        let user = UserId::try_from("@example:example.com").unwrap();

        let room = Room::new(&id, &user);

        let state = ClientState {
            session: None,
            sync_token: Some("hello".into()),
            ignored_users: vec![user],
            push_ruleset: None,
        };
        assert_eq!(
            r#"{"session":null,"sync_token":"hello","ignored_users":["@example:example.com"],"push_ruleset":null}"#,
            serde_json::to_string(&state).unwrap()
        );

        let mut joined_rooms = HashMap::new();
        joined_rooms.insert(id, room);
        assert_eq!(
            r#"{
  "!roomid:example.com": {
    "room_id": "!roomid:example.com",
    "room_name": {
      "name": null,
      "canonical_alias": null,
      "aliases": [],
      "heroes": [],
      "joined_member_count": null,
      "invited_member_count": null
    },
    "own_user_id": "@example:example.com",
    "creator": null,
    "members": {},
    "typing_users": [],
    "power_levels": null,
    "encrypted": false,
    "unread_highlight": null,
    "unread_notifications": null
  }
}"#,
            serde_json::to_string_pretty(&joined_rooms).unwrap()
        );
    }

    #[test]
    fn deserialize() {
        let id = RoomId::try_from("!roomid:example.com").unwrap();
        let user = UserId::try_from("@example:example.com").unwrap();

        let room = Room::new(&id, &user);

        let state = ClientState {
            session: None,
            sync_token: Some("hello".into()),
            ignored_users: vec![user],
            push_ruleset: None,
        };
        let json = serde_json::to_string(&state).unwrap();

        assert_eq!(state, serde_json::from_str(&json).unwrap());

        let mut joined_rooms = HashMap::new();
        joined_rooms.insert(id, room);
        let json = serde_json::to_string(&joined_rooms).unwrap();

        assert_eq!(joined_rooms, serde_json::from_str(&json).unwrap());
    }
}
