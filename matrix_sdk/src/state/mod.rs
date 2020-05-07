// Copyright 2020 Devin Ragotzy
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

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

pub mod state_store;
pub use state_store::JsonStore;

use crate::base_client::{Client as BaseClient, Token};
use crate::events::push_rules::Ruleset;
use crate::identifiers::{RoomId, UserId};
use crate::{Result, Room, Session};

/// `ClientState` holds all the information to restore a `BaseClient`
/// except the `access_token` as the default store is not secure.
///
/// When implementing `StateStore` for something other than the filesystem
/// implement `From<ClientState> for YourDbType` this allows for easy conversion
/// when needed in `StateStore::load/store_client_state`
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ClientState {
    /// The current sync token that should be used for the next sync call.
    pub sync_token: Option<Token>,
    /// A list of ignored users.
    pub ignored_users: Vec<UserId>,
    /// The push ruleset for the logged in user.
    pub push_ruleset: Option<Ruleset>,
}

impl PartialEq for ClientState {
    fn eq(&self, other: &Self) -> bool {
        self.sync_token == other.sync_token && self.ignored_users == other.ignored_users
    }
}

impl ClientState {
    pub async fn from_base_client(client: &BaseClient) -> ClientState {
        let BaseClient {
            sync_token,
            ignored_users,
            push_ruleset,
            ..
        } = client;
        Self {
            sync_token: sync_token.read().await.clone(),
            ignored_users: ignored_users.read().await.clone(),
            push_ruleset: push_ruleset.read().await.clone(),
        }
    }
}

/// Abstraction around the data store to avoid unnecessary request on client initialization.
#[async_trait::async_trait]
pub trait StateStore: Send + Sync {
    /// Loads the state of `BaseClient` through `ClientState` type.
    ///
    /// An `Option::None` should be returned only if the `StateStore` tries to
    /// load but no state has been stored.
    async fn load_client_state(&self, _: &Session) -> Result<Option<ClientState>>;
    /// Load the state of all `Room`s.
    ///
    /// This will be mapped over in the client in order to store `Room`s in an async safe way.
    async fn load_all_rooms(&self) -> Result<HashMap<RoomId, Room>>;
    /// Save the current state of the `BaseClient` using the `StateStore::Store` type.
    async fn store_client_state(&self, _: ClientState) -> Result<()>;
    /// Save the state a single `Room`.
    async fn store_room_state(&self, _: &Room) -> Result<()>;
}

#[cfg(test)]
mod test {
    use super::*;

    use std::collections::HashMap;
    use std::convert::TryFrom;

    #[test]
    fn serialize() {
        let id = RoomId::try_from("!roomid:example.com").unwrap();
        let user = UserId::try_from("@example:example.com").unwrap();

        let room = Room::new(&id, &user);

        let state = ClientState {
            sync_token: Some("hello".into()),
            ignored_users: vec![user],
            push_ruleset: None,
        };
        assert_eq!(
            r#"{"sync_token":"hello","ignored_users":["@example:example.com"],"push_ruleset":null}"#,
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
    "unread_notifications": null,
    "tombstone": null
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
