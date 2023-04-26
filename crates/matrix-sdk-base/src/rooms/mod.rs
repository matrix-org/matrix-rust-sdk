mod members;
mod normal;

use std::{collections::HashSet, fmt};

use bitflags::bitflags;
pub use members::RoomMember;
pub use normal::{Room, RoomInfo, RoomState};
use ruma::{
    assign,
    events::{
        room::{
            avatar::RoomAvatarEventContent, canonical_alias::RoomCanonicalAliasEventContent,
            create::RoomCreateEventContent, encryption::RoomEncryptionEventContent,
            guest_access::RoomGuestAccessEventContent,
            history_visibility::RoomHistoryVisibilityEventContent,
            join_rules::RoomJoinRulesEventContent, member::MembershipState,
            name::RoomNameEventContent, redaction::OriginalSyncRoomRedactionEvent,
            tombstone::RoomTombstoneEventContent, topic::RoomTopicEventContent,
        },
        AnyStrippedStateEvent, AnySyncStateEvent, RedactContent, RedactedStateEventContent,
        StaticStateEventContent, SyncStateEvent,
    },
    EventId, OwnedUserId, RoomVersionId,
};
use serde::{Deserialize, Serialize};

use crate::MinimalStateEvent;

/// The name of the room, either from the metadata or calculated
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
                write!(f, "{s}")
            }
            DisplayName::EmptyWas(s) => write!(f, "Empty Room (was {s})"),
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
    name: Option<MinimalStateEvent<RoomNameEventContent>>,
    /// The `m.room.tombstone` event content of this room.
    tombstone: Option<MinimalStateEvent<RoomTombstoneEventContent>>,
    /// The topic of this room.
    topic: Option<MinimalStateEvent<RoomTopicEventContent>>,
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
                self.name = Some(n.into());
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
                self.topic = Some(t.into());
            }
            AnySyncStateEvent::RoomTombstone(t) => {
                self.tombstone = Some(t.into());
            }
            AnySyncStateEvent::RoomPowerLevels(p) => {
                self.max_power_level = p.power_levels().max().into();
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
                if let Some(algorithm) = &encryption.content.algorithm {
                    let content = assign!(RoomEncryptionEventContent::new(algorithm.clone()), {
                        rotation_period_ms: encryption.content.rotation_period_ms,
                        rotation_period_msgs: encryption.content.rotation_period_msgs,
                    });
                    self.encryption = Some(content);
                }
                // If encryption event is redacted, we don't care much. When
                // entering the room, we will fetch the proper event before
                // sending any messages.
            }
            AnyStrippedStateEvent::RoomAvatar(a) => {
                self.avatar = Some(a.into());
            }
            AnyStrippedStateEvent::RoomName(n) => {
                self.name = Some(n.into());
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
                self.topic = Some(t.into());
            }
            AnyStrippedStateEvent::RoomTombstone(t) => {
                self.tombstone = Some(t.into());
            }
            AnyStrippedStateEvent::RoomPowerLevels(p) => {
                self.max_power_level = p.power_levels().max().into();
            }
            _ => return false,
        }

        true
    }

    pub fn handle_redaction(&mut self, event: &OriginalSyncRoomRedactionEvent) {
        let room_version = self
            .create
            .as_ref()
            .and_then(|ev| Some(ev.as_original()?.content.room_version.to_owned()))
            .unwrap_or(RoomVersionId::V1);

        // FIXME: Use let chains once available to get rid of unwrap()s
        if self.avatar.has_event_id(&event.redacts) {
            self.avatar.as_mut().unwrap().redact(&room_version);
        } else if self.canonical_alias.has_event_id(&event.redacts) {
            self.canonical_alias.as_mut().unwrap().redact(&room_version);
        } else if self.create.has_event_id(&event.redacts) {
            self.create.as_mut().unwrap().redact(&room_version);
        } else if self.guest_access.has_event_id(&event.redacts) {
            self.guest_access.as_mut().unwrap().redact(&room_version);
        } else if self.history_visibility.has_event_id(&event.redacts) {
            self.history_visibility.as_mut().unwrap().redact(&room_version);
        } else if self.join_rules.has_event_id(&event.redacts) {
            self.join_rules.as_mut().unwrap().redact(&room_version);
        } else if self.name.has_event_id(&event.redacts) {
            self.name.as_mut().unwrap().redact(&room_version);
        } else if self.tombstone.has_event_id(&event.redacts) {
            self.tombstone.as_mut().unwrap().redact(&room_version);
        } else if self.topic.has_event_id(&event.redacts) {
            self.topic.as_mut().unwrap().redact(&room_version);
        }
    }
}

trait OptionExt {
    fn has_event_id(&self, ev_id: &EventId) -> bool;
}

impl<C> OptionExt for Option<MinimalStateEvent<C>>
where
    C: StaticStateEventContent + RedactContent,
    C::Redacted: RedactedStateEventContent,
{
    fn has_event_id(&self, ev_id: &EventId) -> bool {
        self.as_ref().and_then(|ev| ev.event_id()).map_or(false, |id| id == ev_id)
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

bitflags! {
    /// Room membership filter as a bitset.
    ///
    /// Note that [`RoomMemberships::empty()`] doesn't filter the results and
    /// [`RoomMemberships::all()`] filters out unknown memberships.
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
    pub struct RoomMemberships: u16 {
        /// The member joined the room.
        const JOIN    = 0b00000001;
        /// The member was invited to the room.
        const INVITE  = 0b00000010;
        /// The member requested to join the room.
        const KNOCK   = 0b00000100;
        /// The member left the room.
        const LEAVE   = 0b00001000;
        /// The member was banned.
        const BAN     = 0b00010000;

        /// The member is active in the room (i.e. joined or invited).
        const ACTIVE = Self::JOIN.bits() | Self::INVITE.bits();
    }
}

impl RoomMemberships {
    /// Whether the given membership matches this `RoomMemberships`.
    pub fn matches(&self, membership: &MembershipState) -> bool {
        if self.is_empty() {
            return true;
        }

        let membership = match membership {
            MembershipState::Ban => Self::BAN,
            MembershipState::Invite => Self::INVITE,
            MembershipState::Join => Self::JOIN,
            MembershipState::Knock => Self::KNOCK,
            MembershipState::Leave => Self::LEAVE,
            _ => return false,
        };

        self.contains(membership)
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
