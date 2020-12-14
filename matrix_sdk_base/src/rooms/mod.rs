mod members;
mod normal;
mod stripped;

pub use normal::{Room, RoomInfo, RoomType};
pub use stripped::{StrippedRoom, StrippedRoomInfo};

pub use members::RoomMember;

use serde::{Deserialize, Serialize};
use std::cmp::max;

use matrix_sdk_common::{
    events::{
        room::{encryption::EncryptionEventContent, tombstone::TombstoneEventContent},
        AnyStateEventContent,
    },
    identifiers::RoomAliasId,
};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BaseRoomInfo {
    pub name: Option<String>,
    pub canonical_alias: Option<RoomAliasId>,
    pub avatar_url: Option<String>,
    pub topic: Option<String>,
    pub encryption: Option<EncryptionEventContent>,
    pub tombstone: Option<TombstoneEventContent>,
    pub max_power_level: i64,
}

impl BaseRoomInfo {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn handle_state_event(&mut self, content: &AnyStateEventContent) -> bool {
        match content {
            AnyStateEventContent::RoomEncryption(encryption) => {
                self.encryption = Some(encryption.clone());
                true
            }
            AnyStateEventContent::RoomName(n) => {
                self.name = n.name().map(|n| n.to_string());
                true
            }
            AnyStateEventContent::RoomCanonicalAlias(a) => {
                self.canonical_alias = a.alias.clone();
                true
            }
            AnyStateEventContent::RoomTopic(t) => {
                self.topic = Some(t.topic.clone());
                true
            }
            AnyStateEventContent::RoomTombstone(t) => {
                self.tombstone = Some(t.clone());
                true
            }
            AnyStateEventContent::RoomPowerLevels(p) => {
                let max_power_level = p
                    .users
                    .values()
                    .fold(self.max_power_level, |acc, p| max(acc, (*p).into()));
                self.max_power_level = max_power_level;
                true
            }
            _ => false,
        }
    }
}

impl Default for BaseRoomInfo {
    fn default() -> Self {
        Self {
            name: None,
            canonical_alias: None,
            avatar_url: None,
            topic: None,
            encryption: None,
            tombstone: None,
            max_power_level: 100,
        }
    }
}
