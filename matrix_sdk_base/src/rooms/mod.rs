mod members;
mod normal;
mod stripped;

pub use normal::{Room, RoomInfo, RoomType};
pub use stripped::{StrippedRoom, StrippedRoomInfo};

pub use members::RoomMember;

use serde::{Deserialize, Serialize};
use std::{cmp::max, ops::Deref};

use matrix_sdk_common::{
    events::{
        room::{encryption::EncryptionEventContent, tombstone::TombstoneEventContent},
        AnyStateEventContent,
    },
    identifiers::RoomAliasId,
};

/// An enum that represents the state of the given `Room`.
///
/// If the event came from the `join`, `invite` or `leave` rooms map from the server
/// the variant that holds the corresponding room is used. `RoomState` is generic
/// so it can be used to represent a `Room` or an `Arc<RwLock<Room>>`
#[derive(Debug, Clone)]
pub enum RoomState {
    /// A room from the `join` section of a sync response.
    Joined(JoinedRoom),
    /// A room from the `leave` section of a sync response.
    Left(LeftRoom),
    /// A room from the `invite` section of a sync response.
    Invited(InvitedRoom),
}

impl RoomState {
    pub fn joined(self) -> Option<JoinedRoom> {
        if let RoomState::Joined(r) = self {
            Some(r)
        } else {
            None
        }
    }

    pub fn is_encrypted(&self) -> bool {
        match self {
            RoomState::Joined(r) => r.inner.is_encrypted(),
            RoomState::Left(r) => r.inner.is_encrypted(),
            RoomState::Invited(r) => r.inner.is_encrypted(),
        }
    }

    pub fn are_members_synced(&self) -> bool {
        if let RoomState::Joined(r) = self {
            r.inner.are_members_synced()
        } else {
            true
        }
    }
}

#[derive(Debug, Clone)]
pub struct JoinedRoom {
    pub(crate) inner: Room,
}

// TODO do we wan't to deref here or have separate implementations.
impl Deref for JoinedRoom {
    type Target = Room;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

#[derive(Debug, Clone)]
pub struct LeftRoom {
    pub(crate) inner: Room,
}

impl Deref for LeftRoom {
    type Target = Room;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

#[derive(Debug, Clone)]
pub struct InvitedRoom {
    pub(crate) inner: StrippedRoom,
}

impl Deref for InvitedRoom {
    type Target = StrippedRoom;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

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
