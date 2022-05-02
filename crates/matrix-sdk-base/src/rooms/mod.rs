mod members;
mod normal;

use std::{cmp::max, collections::HashSet, fmt};

pub use members::RoomMember;
pub use normal::{Room, RoomInfo, RoomType};
use ruma::{
    events::{
        room::{
            avatar::RoomAvatarEventContent, canonical_alias::RoomCanonicalAliasEventContent,
            create::RoomCreateEventContent, encryption::RoomEncryptionEventContent,
            guest_access::RoomGuestAccessEventContent,
            history_visibility::RoomHistoryVisibilityEventContent,
            join_rules::RoomJoinRulesEventContent, tombstone::RoomTombstoneEventContent,
        },
        AnyStrippedStateEvent, AnySyncStateEvent, EmptyStateKey, RedactContent,
        RedactedEventContent, StateEventContent, StrippedStateEvent, SyncStateEvent,
    },
    OwnedEventId, OwnedUserId,
};
use serde::{de::DeserializeOwned, Deserialize, Serialize};

// #[serde(bound)] instead of DeserializeOwned in type where clause does not
// work, it can only be a single bound that replaces the default and if a helper
// trait is used, the compiler still complains about Deserialize not being
// implemented for C::Redacted.
//
// It is unclear why a Serialize bound on C::Redacted is not also required.
#[derive(Clone, Debug, Deserialize, Serialize)]
enum MinimalStateEvent<C: StateEventContent<StateKey = EmptyStateKey> + RedactContent>
where
    C::Redacted:
        StateEventContent<StateKey = EmptyStateKey> + RedactedEventContent + DeserializeOwned,
{
    Original(OriginalMinimalStateEvent<C>),
    Redacted(RedactedMinimalStateEvent<C::Redacted>),
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct OriginalMinimalStateEvent<C>
where
    C: StateEventContent<StateKey = EmptyStateKey>,
{
    content: C,
    event_id: Option<OwnedEventId>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct RedactedMinimalStateEvent<C>
where
    C: StateEventContent<StateKey = EmptyStateKey> + RedactedEventContent,
{
    content: C,
    event_id: Option<OwnedEventId>,
}

impl<C> MinimalStateEvent<C>
where
    C: StateEventContent<StateKey = EmptyStateKey> + RedactContent,
    C::Redacted:
        StateEventContent<StateKey = EmptyStateKey> + RedactedEventContent + DeserializeOwned,
{
    fn as_original(&self) -> Option<&OriginalMinimalStateEvent<C>> {
        match self {
            MinimalStateEvent::Original(ev) => Some(ev),
            MinimalStateEvent::Redacted(_) => None,
        }
    }
}

impl<C> From<&SyncStateEvent<C>> for MinimalStateEvent<C>
where
    C: Clone + StateEventContent<StateKey = EmptyStateKey> + RedactContent,
    C::Redacted: Clone
        + StateEventContent<StateKey = EmptyStateKey>
        + RedactedEventContent
        + DeserializeOwned,
{
    fn from(ev: &SyncStateEvent<C>) -> Self {
        match ev {
            SyncStateEvent::Original(ev) => Self::Original(OriginalMinimalStateEvent {
                content: ev.content.clone(),
                event_id: Some(ev.event_id.clone()),
            }),
            SyncStateEvent::Redacted(ev) => Self::Redacted(RedactedMinimalStateEvent {
                content: ev.content.clone(),
                event_id: Some(ev.event_id.clone()),
            }),
        }
    }
}

impl<C> From<&StrippedStateEvent<C>> for MinimalStateEvent<C>
where
    C: Clone + StateEventContent<StateKey = EmptyStateKey> + RedactContent,
    C::Redacted: Clone
        + StateEventContent<StateKey = EmptyStateKey>
        + RedactedEventContent
        + DeserializeOwned,
{
    fn from(ev: &StrippedStateEvent<C>) -> Self {
        Self::Original(OriginalMinimalStateEvent { content: ev.content.clone(), event_id: None })
    }
}

/// The name of the room, either from the metadata or calculaetd
/// according to [matrix specification](https://matrix.org/docs/spec/client_server/latest#calculating-the-display-name-for-a-room)
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum DisplayName {
    /// The room has been named explicitly as
    Named(String),
    /// The room has a canonical alias that should be used
    Aliased(String),
    /// The room has not given an explicit name but a name could be
    /// calculated
    Calculated(String),
    /// The room doesn't have a name right now, but used to have one
    /// e.g. because it was a DM and everyone has left the room
    EmptyWas(String),
    /// No useful name could be calculated or ever found
    Empty,
}

impl fmt::Display for DisplayName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DisplayName::Named(s) | DisplayName::Calculated(s) | DisplayName::Aliased(s) => {
                write!(f, "{}", s)
            }
            DisplayName::EmptyWas(s) => write!(f, "Empty Room (was {})", s),
            DisplayName::Empty => write!(f, "Empty Room"),
        }
    }
}

/// A base room info struct that is the backbone of normal as well as stripped
/// rooms. Holds all the state events that are important to present a room to
/// users.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BaseRoomInfo {
    /// The avatar URL of this room.
    avatar: Option<MinimalStateEvent<RoomAvatarEventContent>>,
    /// The canonical alias of this room.
    canonical_alias: Option<MinimalStateEvent<RoomCanonicalAliasEventContent>>,
    /// The `m.room.create` event content of this room.
    create: Option<MinimalStateEvent<RoomCreateEventContent>>,
    /// A list of user ids this room is considered as direct message, if this
    /// room is a DM.
    pub(crate) dm_targets: HashSet<OwnedUserId>,
    /// The `m.room.encryption` event content that enabled E2EE in this room.
    pub(crate) encryption: Option<RoomEncryptionEventContent>,
    /// The guest access policy of this room.
    guest_access: Option<MinimalStateEvent<RoomGuestAccessEventContent>>,
    /// The history visibility policy of this room.
    history_visibility: Option<MinimalStateEvent<RoomHistoryVisibilityEventContent>>,
    /// The join rule policy of this room.
    join_rules: Option<MinimalStateEvent<RoomJoinRulesEventContent>>,
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
    ) -> DisplayName {
        calculate_room_name(
            joined_member_count,
            invited_member_count,
            heroes.iter().take(3).map(|mem| mem.name()).collect::<Vec<&str>>(),
        )
    }

    /// Handle a state event for this room and update our info accordingly.
    ///
    /// Returns true if the event modified the info, false otherwise.
    pub fn handle_state_event(&mut self, ev: &AnySyncStateEvent) -> bool {
        match ev {
            // No redacted branch - enabling encryption cannot be undone.
            AnySyncStateEvent::RoomEncryption(SyncStateEvent::Original(encryption)) => {
                self.encryption = Some(encryption.content.clone());
            }
            AnySyncStateEvent::RoomAvatar(a) => {
                self.avatar = Some(a.into());
            }
            AnySyncStateEvent::RoomName(n) => {
                self.name =
                    n.as_original().and_then(|n| n.content.name.as_ref().map(|n| n.to_string()));
            }
            AnySyncStateEvent::RoomCreate(c) if self.create.is_none() => {
                self.create = Some(c.into());
            }
            AnySyncStateEvent::RoomHistoryVisibility(h) => {
                self.history_visibility = Some(h.into());
            }
            AnySyncStateEvent::RoomGuestAccess(g) => {
                self.guest_access = Some(g.into());
            }
            AnySyncStateEvent::RoomJoinRules(c) => {
                self.join_rules = Some(c.into());
            }
            AnySyncStateEvent::RoomCanonicalAlias(a) => {
                self.canonical_alias = Some(a.into());
            }
            AnySyncStateEvent::RoomTopic(t) => {
                self.topic = t.as_original().map(|t| t.content.topic.clone());
            }
            AnySyncStateEvent::RoomTombstone(t) => {
                self.tombstone = t.as_original().map(|t| t.content.clone());
            }
            AnySyncStateEvent::RoomPowerLevels(p) => {
                self.max_power_level = p
                    .power_levels()
                    .users
                    .values()
                    .fold(self.max_power_level, |acc, &p| max(acc, p.into()));
            }
            _ => return false,
        }

        true
    }

    /// Handle a stripped state event for this room and update our info
    /// accordingly.
    ///
    /// Returns true if the event modified the info, false otherwise.
    pub fn handle_stripped_state_event(&mut self, ev: &AnyStrippedStateEvent) -> bool {
        match ev {
            AnyStrippedStateEvent::RoomEncryption(encryption) => {
                self.encryption = Some(encryption.content.clone());
            }
            AnyStrippedStateEvent::RoomAvatar(a) => {
                self.avatar = Some(a.into());
            }
            AnyStrippedStateEvent::RoomName(n) => {
                self.name = n.content.name.as_ref().map(|n| n.to_string());
            }
            AnyStrippedStateEvent::RoomCreate(c) if self.create.is_none() => {
                self.create = Some(c.into());
            }
            AnyStrippedStateEvent::RoomHistoryVisibility(h) => {
                self.history_visibility = Some(h.into());
            }
            AnyStrippedStateEvent::RoomGuestAccess(g) => {
                self.guest_access = Some(g.into());
            }
            AnyStrippedStateEvent::RoomJoinRules(c) => {
                self.join_rules = Some(c.into());
            }
            AnyStrippedStateEvent::RoomCanonicalAlias(a) => {
                self.canonical_alias = Some(a.into());
            }
            AnyStrippedStateEvent::RoomTopic(t) => {
                self.topic = Some(t.content.topic.clone());
            }
            AnyStrippedStateEvent::RoomTombstone(t) => {
                self.tombstone = Some(t.content.clone());
            }
            AnyStrippedStateEvent::RoomPowerLevels(p) => {
                self.max_power_level = p
                    .content
                    .users
                    .values()
                    .fold(self.max_power_level, |acc, &p| max(acc, p.into()));
            }
            _ => return false,
        }

        true
    }
}

impl Default for BaseRoomInfo {
    fn default() -> Self {
        Self {
            avatar: None,
            canonical_alias: None,
            create: None,
            dm_targets: Default::default(),
            encryption: None,
            guest_access: None,
            history_visibility: None,
            join_rules: None,
            max_power_level: 100,
            name: None,
            tombstone: None,
            topic: None,
        }
    }
}

/// Calculate room name according to step 3 of the [naming algorithm.]
fn calculate_room_name(
    joined_member_count: u64,
    invited_member_count: u64,
    heroes: Vec<&str>,
) -> DisplayName {
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
            DisplayName::Empty
        } else {
            DisplayName::EmptyWas(names)
        }
    } else {
        DisplayName::Calculated(names)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]

    fn test_calculate_room_name() {
        let mut actual = calculate_room_name(2, 0, vec!["a"]);
        assert_eq!(DisplayName::Calculated("a".to_owned()), actual);

        actual = calculate_room_name(3, 0, vec!["a", "b"]);
        assert_eq!(DisplayName::Calculated("a, b".to_owned()), actual);

        actual = calculate_room_name(4, 0, vec!["a", "b", "c"]);
        assert_eq!(DisplayName::Calculated("a, b, c".to_owned()), actual);

        actual = calculate_room_name(5, 0, vec!["a", "b", "c"]);
        assert_eq!(DisplayName::Calculated("a, b, c, and 2 others".to_owned()), actual);

        actual = calculate_room_name(0, 0, vec![]);
        assert_eq!(DisplayName::Empty, actual);

        actual = calculate_room_name(1, 0, vec![]);
        assert_eq!(DisplayName::Empty, actual);

        actual = calculate_room_name(0, 1, vec![]);
        assert_eq!(DisplayName::Empty, actual);

        actual = calculate_room_name(1, 0, vec!["a"]);
        assert_eq!(DisplayName::EmptyWas("a".to_owned()), actual);

        actual = calculate_room_name(1, 0, vec!["a", "b"]);
        assert_eq!(DisplayName::EmptyWas("a, b".to_owned()), actual);

        actual = calculate_room_name(1, 0, vec!["a", "b", "c"]);
        assert_eq!(DisplayName::EmptyWas("a, b, c".to_owned()), actual);
    }
}
