mod members;
mod normal;
mod stripped;

use matrix_sdk_common::{
    events::room::{
        create::CreateEventContent, guest_access::GuestAccess,
        history_visibility::HistoryVisibility, join_rules::JoinRule,
    },
    identifiers::UserId,
};
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

    pub fn invited(self) -> Option<InvitedRoom> {
        if let RoomState::Invited(r) = self {
            Some(r)
        } else {
            None
        }
    }

    pub fn left(self) -> Option<LeftRoom> {
        if let RoomState::Left(r) = self {
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
    pub avatar_url: Option<String>,
    pub canonical_alias: Option<RoomAliasId>,
    pub create: Option<CreateEventContent>,
    pub dm_target: Option<UserId>,
    pub encryption: Option<EncryptionEventContent>,
    pub guest_access: GuestAccess,
    pub history_visibility: HistoryVisibility,
    pub join_rule: JoinRule,
    pub max_power_level: i64,
    pub name: Option<String>,
    pub tombstone: Option<TombstoneEventContent>,
    pub topic: Option<String>,
}

impl BaseRoomInfo {
    pub fn new() -> Self {
        Self::default()
    }

    pub(crate) fn calculate_room_name(
        &self,
        joined_member_count: u64,
        invited_member_count: u64,
        heroes: Vec<RoomMember>,
    ) -> String {
        let heroes_count = heroes.len() as u64;
        let invited_joined = (invited_member_count + joined_member_count).saturating_sub(1);

        if heroes_count >= invited_joined {
            let mut names = heroes
                .iter()
                .take(3)
                .map(|mem| mem.name())
                .collect::<Vec<&str>>();
            // stabilize ordering
            names.sort_unstable();
            names.join(", ")
        } else if heroes_count < invited_joined && invited_joined > 1 {
            let mut names = heroes
                .iter()
                .take(3)
                .map(|mem| mem.name())
                .collect::<Vec<&str>>();
            names.sort_unstable();

            // TODO: What length does the spec want us to use here and in
            // the `else`?
            format!(
                "{}, and {} others",
                names.join(", "),
                (joined_member_count + invited_member_count)
            )
        } else {
            "Empty room".to_string()
        }
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
            AnyStateEventContent::RoomCreate(c) => {
                if self.create.is_none() {
                    self.create = Some(c.clone());
                    true
                } else {
                    false
                }
            }
            AnyStateEventContent::RoomHistoryVisibility(h) => {
                self.history_visibility = h.history_visibility.clone();
                true
            }
            AnyStateEventContent::RoomGuestAccess(g) => {
                self.guest_access = g.guest_access.clone();
                true
            }
            AnyStateEventContent::RoomJoinRules(c) => {
                self.join_rule = c.join_rule.clone();
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
            avatar_url: None,
            canonical_alias: None,
            create: None,
            dm_target: None,
            encryption: None,
            guest_access: GuestAccess::CanJoin,
            history_visibility: HistoryVisibility::WorldReadable,
            join_rule: JoinRule::Public,
            max_power_level: 100,
            name: None,
            tombstone: None,
            topic: None,
        }
    }
}
