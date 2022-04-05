mod members;
mod normal;

use std::cmp::max;

pub use members::RoomMember;
pub use normal::{Room, RoomInfo, RoomType};
use ruma::{
    events::{
        room::{
            create::RoomCreateEventContent, encryption::RoomEncryptionEventContent,
            guest_access::GuestAccess, history_visibility::HistoryVisibility, join_rules::JoinRule,
            tombstone::RoomTombstoneEventContent,
        },
        AnyStateEventContent,
    },
    MxcUri, RoomAliasId, UserId,
};
use serde::{Deserialize, Serialize};

/// A base room info struct that is the backbone of normal as well as stripped
/// rooms. Holds all the state events that are important to present a room to
/// users.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BaseRoomInfo {
    /// The avatar URL of this room.
    pub(crate) avatar_url: Option<Box<MxcUri>>,
    /// The canonical alias of this room.
    pub(crate) canonical_alias: Option<Box<RoomAliasId>>,
    /// The `m.room.create` event content of this room.
    pub(crate) create: Option<RoomCreateEventContent>,
    /// The user id this room is sharing the direct message with, if the room is
    /// a direct message.
    pub(crate) dm_target: Option<Box<UserId>>,
    /// The `m.room.encryption` event content that enabled E2EE in this room.
    pub(crate) encryption: Option<RoomEncryptionEventContent>,
    /// The guest access policy of this room.
    pub(crate) guest_access: GuestAccess,
    /// The history visibility policy of this room.
    pub(crate) history_visibility: HistoryVisibility,
    /// The join rule policy of this room.
    pub(crate) join_rule: JoinRule,
    /// The maximal power level that can be found in this room.
    pub(crate) max_power_level: i64,
    /// The `m.room.name` of this room.
    pub(crate) name: Option<String>,
    /// The `m.room.tombstone` event content of this room.
    pub(crate) tombstone: Option<RoomTombstoneEventContent>,
    /// The topic of this room.
    pub(crate) topic: Option<String>,
}

impl BaseRoomInfo {
    /// Create a new, empty base room info.
    pub fn new() -> Self {
        Self::default()
    }

    pub(crate) fn calculate_room_name(
        &self,
        joined_member_count: u64,
        invited_member_count: u64,
        heroes: Vec<RoomMember>,
    ) -> String {
        calculate_room_name(
            joined_member_count,
            invited_member_count,
            heroes.iter().take(3).map(|mem| mem.name()).collect::<Vec<&str>>(),
        )
    }

    /// Handle a state event for this room and update our info accordingly.
    ///
    /// Returns true if the event modified the info, false otherwise.
    pub fn handle_state_event(&mut self, content: &AnyStateEventContent) -> bool {
        match content {
            AnyStateEventContent::RoomEncryption(encryption) => {
                self.encryption = Some(encryption.clone());
            }
            AnyStateEventContent::RoomAvatar(a) => {
                self.avatar_url = a.url.clone();
            }
            AnyStateEventContent::RoomName(n) => {
                self.name = n.name.as_ref().map(|n| n.to_string());
            }
            AnyStateEventContent::RoomCreate(c) if self.create.is_none() => {
                self.create = Some(c.clone());
            }
            AnyStateEventContent::RoomHistoryVisibility(h) => {
                self.history_visibility = h.history_visibility.clone();
            }
            AnyStateEventContent::RoomGuestAccess(g) => {
                self.guest_access = g.guest_access.clone();
            }
            AnyStateEventContent::RoomJoinRules(c) => {
                self.join_rule = c.join_rule.clone();
            }
            AnyStateEventContent::RoomCanonicalAlias(a) => {
                self.canonical_alias = a.alias.clone();
            }
            AnyStateEventContent::RoomTopic(t) => {
                self.topic = Some(t.topic.clone());
            }
            AnyStateEventContent::RoomTombstone(t) => {
                self.tombstone = Some(t.clone());
            }
            AnyStateEventContent::RoomPowerLevels(p) => {
                let max_power_level =
                    p.users.values().fold(self.max_power_level, |acc, &p| max(acc, p.into()));
                self.max_power_level = max_power_level;
            }
            _ => return false,
        }

        true
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

/// Calculate room name according to step 3 of the [naming algorithm.][spec]
///
/// [spec]: <https://matrix.org/docs/spec/client_server/latest#calculating-the-display-name-for-a-room>
fn calculate_room_name(
    joined_member_count: u64,
    invited_member_count: u64,
    heroes: Vec<&str>,
) -> String {
    let heroes_count = heroes.len() as u64;
    let invited_joined = invited_member_count + joined_member_count;
    let invited_joined_minus_one = invited_joined.saturating_sub(1);

    let names = if heroes_count >= invited_joined_minus_one {
        let mut names = heroes;
        // stabilize ordering
        names.sort_unstable();
        names.join(", ")
    } else if heroes_count < invited_joined_minus_one && invited_joined > 1 {
        let mut names = heroes;
        names.sort_unstable();

        // TODO: What length does the spec want us to use here and in
        // the `else`?
        format!("{}, and {} others", names.join(", "), (invited_joined - heroes_count))
    } else {
        "".to_owned()
    };

    // User is alone.
    if invited_joined <= 1 {
        if names.is_empty() {
            "Empty room".to_owned()
        } else {
            format!("Empty room (was {})", names)
        }
    } else {
        names
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]

    fn test_calculate_room_name() {
        let mut actual = calculate_room_name(2, 0, vec!["a"]);
        assert_eq!("a", actual);

        actual = calculate_room_name(3, 0, vec!["a", "b"]);
        assert_eq!("a, b", actual);

        actual = calculate_room_name(4, 0, vec!["a", "b", "c"]);
        assert_eq!("a, b, c", actual);

        actual = calculate_room_name(5, 0, vec!["a", "b", "c"]);
        assert_eq!("a, b, c, and 2 others", actual);

        actual = calculate_room_name(0, 0, vec![]);
        assert_eq!("Empty room", actual);

        actual = calculate_room_name(1, 0, vec![]);
        assert_eq!("Empty room", actual);

        actual = calculate_room_name(0, 1, vec![]);
        assert_eq!("Empty room", actual);

        actual = calculate_room_name(1, 0, vec!["a"]);
        assert_eq!("Empty room (was a)", actual);

        actual = calculate_room_name(1, 0, vec!["a", "b"]);
        assert_eq!("Empty room (was a, b)", actual);

        actual = calculate_room_name(1, 0, vec!["a", "b", "c"]);
        assert_eq!("Empty room (was a, b, c)", actual);
    }
}
