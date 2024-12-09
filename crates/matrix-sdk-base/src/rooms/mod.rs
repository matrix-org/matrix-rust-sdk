#![allow(clippy::assign_op_pattern)] // Triggered by bitflags! usage
#![allow(unexpected_cfgs)] // Triggered by the `EventContent` macro usage

mod members;
pub(crate) mod normal;

use std::{
    collections::{BTreeMap, HashSet},
    fmt,
    hash::Hash,
};

use bitflags::bitflags;
pub use members::RoomMember;
pub use normal::{
    Room, RoomHero, RoomInfo, RoomInfoNotableUpdate, RoomInfoNotableUpdateReasons, RoomState,
    RoomStateFilter,
};
use regex::Regex;
use ruma::{
    assign,
    events::{
        beacon_info::BeaconInfoEventContent,
        call::member::{CallMemberEventContent, CallMemberStateKey},
        direct::OwnedDirectUserIdentifier,
        macros::EventContent,
        room::{
            avatar::RoomAvatarEventContent,
            canonical_alias::RoomCanonicalAliasEventContent,
            create::{PreviousRoom, RoomCreateEventContent},
            encryption::RoomEncryptionEventContent,
            guest_access::RoomGuestAccessEventContent,
            history_visibility::RoomHistoryVisibilityEventContent,
            join_rules::RoomJoinRulesEventContent,
            member::MembershipState,
            name::RoomNameEventContent,
            pinned_events::RoomPinnedEventsEventContent,
            tombstone::RoomTombstoneEventContent,
            topic::RoomTopicEventContent,
        },
        tag::{TagName, Tags},
        AnyStrippedStateEvent, AnySyncStateEvent, EmptyStateKey, RedactContent,
        RedactedStateEventContent, StaticStateEventContent, SyncStateEvent,
    },
    room::RoomType,
    EventId, OwnedUserId, RoomVersionId,
};
use serde::{Deserialize, Serialize};

use crate::MinimalStateEvent;

/// The name of the room, either from the metadata or calculated
/// according to [matrix specification](https://matrix.org/docs/spec/client_server/latest#calculating-the-display-name-for-a-room)
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum RoomDisplayName {
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

const WHITESPACE_REGEX: &str = r"\s+";
const INVALID_SYMBOLS_REGEX: &str = r"[#,:\{\}\\]+";

impl RoomDisplayName {
    /// Transforms the current display name into the name part of a
    /// `RoomAliasId`.
    pub fn to_room_alias_name(&self) -> String {
        let room_name = match self {
            Self::Named(name) => name,
            Self::Aliased(name) => name,
            Self::Calculated(name) => name,
            Self::EmptyWas(name) => name,
            Self::Empty => "",
        };

        let whitespace_regex =
            Regex::new(WHITESPACE_REGEX).expect("`WHITESPACE_REGEX` should be valid");
        let symbol_regex =
            Regex::new(INVALID_SYMBOLS_REGEX).expect("`INVALID_SYMBOLS_REGEX` should be valid");

        // Replace whitespaces with `-`
        let sanitised = whitespace_regex.replace_all(room_name, "-");
        // Remove non-ASCII characters and ASCII control characters
        let sanitised =
            String::from_iter(sanitised.chars().filter(|c| c.is_ascii() && !c.is_ascii_control()));
        // Remove other problematic ASCII symbols
        let sanitised = symbol_regex.replace_all(&sanitised, "");
        // Lowercased
        sanitised.to_lowercase()
    }
}

impl fmt::Display for RoomDisplayName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RoomDisplayName::Named(s)
            | RoomDisplayName::Calculated(s)
            | RoomDisplayName::Aliased(s) => {
                write!(f, "{s}")
            }
            RoomDisplayName::EmptyWas(s) => write!(f, "Empty Room (was {s})"),
            RoomDisplayName::Empty => write!(f, "Empty Room"),
        }
    }
}

/// A base room info struct that is the backbone of normal as well as stripped
/// rooms. Holds all the state events that are important to present a room to
/// users.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BaseRoomInfo {
    /// The avatar URL of this room.
    pub(crate) avatar: Option<MinimalStateEvent<RoomAvatarEventContent>>,
    /// All shared live location beacons of this room.
    #[serde(skip_serializing_if = "BTreeMap::is_empty", default)]
    pub(crate) beacons: BTreeMap<OwnedUserId, MinimalStateEvent<BeaconInfoEventContent>>,
    /// The canonical alias of this room.
    pub(crate) canonical_alias: Option<MinimalStateEvent<RoomCanonicalAliasEventContent>>,
    /// The `m.room.create` event content of this room.
    pub(crate) create: Option<MinimalStateEvent<RoomCreateWithCreatorEventContent>>,
    /// A list of user ids this room is considered as direct message, if this
    /// room is a DM.
    pub(crate) dm_targets: HashSet<OwnedDirectUserIdentifier>,
    /// The `m.room.encryption` event content that enabled E2EE in this room.
    pub(crate) encryption: Option<RoomEncryptionEventContent>,
    /// The guest access policy of this room.
    pub(crate) guest_access: Option<MinimalStateEvent<RoomGuestAccessEventContent>>,
    /// The history visibility policy of this room.
    pub(crate) history_visibility: Option<MinimalStateEvent<RoomHistoryVisibilityEventContent>>,
    /// The join rule policy of this room.
    pub(crate) join_rules: Option<MinimalStateEvent<RoomJoinRulesEventContent>>,
    /// The maximal power level that can be found in this room.
    pub(crate) max_power_level: i64,
    /// The `m.room.name` of this room.
    pub(crate) name: Option<MinimalStateEvent<RoomNameEventContent>>,
    /// The `m.room.tombstone` event content of this room.
    pub(crate) tombstone: Option<MinimalStateEvent<RoomTombstoneEventContent>>,
    /// The topic of this room.
    pub(crate) topic: Option<MinimalStateEvent<RoomTopicEventContent>>,
    /// All minimal state events that containing one or more running matrixRTC
    /// memberships.
    #[serde(skip_serializing_if = "BTreeMap::is_empty", default)]
    pub(crate) rtc_member_events:
        BTreeMap<CallMemberStateKey, MinimalStateEvent<CallMemberEventContent>>,
    /// Whether this room has been manually marked as unread.
    #[serde(default)]
    pub(crate) is_marked_unread: bool,
    /// Some notable tags.
    ///
    /// We are not interested by all the tags. Some tags are more important than
    /// others, and this field collects them.
    #[serde(skip_serializing_if = "RoomNotableTags::is_empty", default)]
    pub(crate) notable_tags: RoomNotableTags,
    /// The `m.room.pinned_events` of this room.
    pub(crate) pinned_events: Option<RoomPinnedEventsEventContent>,
}

impl BaseRoomInfo {
    /// Create a new, empty base room info.
    pub fn new() -> Self {
        Self::default()
    }

    /// Get the room version of this room.
    ///
    /// For room versions earlier than room version 11, if the event is
    /// redacted, this will return the default of [`RoomVersionId::V1`].
    pub fn room_version(&self) -> Option<&RoomVersionId> {
        match self.create.as_ref()? {
            MinimalStateEvent::Original(ev) => Some(&ev.content.room_version),
            MinimalStateEvent::Redacted(ev) => Some(&ev.content.room_version),
        }
    }

    /// Handle a state event for this room and update our info accordingly.
    ///
    /// Returns true if the event modified the info, false otherwise.
    pub fn handle_state_event(&mut self, ev: &AnySyncStateEvent) -> bool {
        match ev {
            AnySyncStateEvent::BeaconInfo(b) => {
                self.beacons.insert(b.state_key().clone(), b.into());
            }
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
            AnySyncStateEvent::CallMember(m) => {
                let Some(o_ev) = m.as_original() else {
                    return false;
                };

                // we modify the event so that `origin_sever_ts` gets copied into
                // `content.created_ts`
                let mut o_ev = o_ev.clone();
                o_ev.content.set_created_ts_if_none(o_ev.origin_server_ts);

                // Add the new event.
                self.rtc_member_events
                    .insert(m.state_key().clone(), SyncStateEvent::Original(o_ev).into());

                // Remove all events that don't contain any memberships anymore.
                self.rtc_member_events.retain(|_, ev| {
                    ev.as_original().is_some_and(|o| !o.content.active_memberships(None).is_empty())
                });
            }
            AnySyncStateEvent::RoomPinnedEvents(p) => {
                self.pinned_events = p.as_original().map(|p| p.content.clone());
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
            AnyStrippedStateEvent::CallMember(_) => {
                // Ignore stripped call state events. Rooms that are not in Joined or Left state
                // wont have call information.
                return false;
            }
            AnyStrippedStateEvent::RoomPinnedEvents(p) => {
                if let Some(pinned) = p.content.pinned.clone() {
                    self.pinned_events = Some(RoomPinnedEventsEventContent::new(pinned));
                }
            }
            _ => return false,
        }

        true
    }

    fn handle_redaction(&mut self, redacts: &EventId) {
        let room_version = self.room_version().unwrap_or(&RoomVersionId::V1).to_owned();

        // FIXME: Use let chains once available to get rid of unwrap()s
        if self.avatar.has_event_id(redacts) {
            self.avatar.as_mut().unwrap().redact(&room_version);
        } else if self.canonical_alias.has_event_id(redacts) {
            self.canonical_alias.as_mut().unwrap().redact(&room_version);
        } else if self.create.has_event_id(redacts) {
            self.create.as_mut().unwrap().redact(&room_version);
        } else if self.guest_access.has_event_id(redacts) {
            self.guest_access.as_mut().unwrap().redact(&room_version);
        } else if self.history_visibility.has_event_id(redacts) {
            self.history_visibility.as_mut().unwrap().redact(&room_version);
        } else if self.join_rules.has_event_id(redacts) {
            self.join_rules.as_mut().unwrap().redact(&room_version);
        } else if self.name.has_event_id(redacts) {
            self.name.as_mut().unwrap().redact(&room_version);
        } else if self.tombstone.has_event_id(redacts) {
            self.tombstone.as_mut().unwrap().redact(&room_version);
        } else if self.topic.has_event_id(redacts) {
            self.topic.as_mut().unwrap().redact(&room_version);
        } else {
            self.rtc_member_events
                .retain(|_, member_event| member_event.event_id() != Some(redacts));
        }
    }

    pub fn handle_notable_tags(&mut self, tags: &Tags) {
        let mut notable_tags = RoomNotableTags::empty();

        if tags.contains_key(&TagName::Favorite) {
            notable_tags.insert(RoomNotableTags::FAVOURITE);
        }

        if tags.contains_key(&TagName::LowPriority) {
            notable_tags.insert(RoomNotableTags::LOW_PRIORITY);
        }

        self.notable_tags = notable_tags;
    }
}

bitflags! {
    /// Notable tags, i.e. subset of tags that we are more interested by.
    ///
    /// We are not interested by all the tags. Some tags are more important than
    /// others, and this struct describes them.
    #[repr(transparent)]
    #[derive(Debug, Default, Clone, Copy, Deserialize, Serialize)]
    pub(crate) struct RoomNotableTags: u8 {
        /// The `m.favourite` tag.
        const FAVOURITE = 0b0000_0001;

        /// THe `m.lowpriority` tag.
        const LOW_PRIORITY = 0b0000_0010;
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
        self.as_ref().is_some_and(|ev| ev.event_id() == Some(ev_id))
    }
}

impl Default for BaseRoomInfo {
    fn default() -> Self {
        Self {
            avatar: None,
            beacons: BTreeMap::new(),
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
            rtc_member_events: BTreeMap::new(),
            is_marked_unread: false,
            notable_tags: RoomNotableTags::empty(),
            pinned_events: None,
        }
    }
}

/// The content of an `m.room.create` event, with a required `creator` field.
///
/// Starting with room version 11, the `creator` field should be removed and the
/// `sender` field of the event should be used instead. This is reflected on
/// [`RoomCreateEventContent`].
///
/// This type was created as an alternative for ease of use. When it is used in
/// the SDK, it is constructed by copying the `sender` of the original event as
/// the `creator`.
#[derive(Clone, Debug, Deserialize, Serialize, EventContent)]
#[ruma_event(type = "m.room.create", kind = State, state_key_type = EmptyStateKey, custom_redacted)]
pub struct RoomCreateWithCreatorEventContent {
    /// The `user_id` of the room creator.
    ///
    /// This is set by the homeserver.
    ///
    /// While this should be optional since room version 11, we copy the sender
    /// of the event so we can still access it.
    pub creator: OwnedUserId,

    /// Whether or not this room's data should be transferred to other
    /// homeservers.
    #[serde(
        rename = "m.federate",
        default = "ruma::serde::default_true",
        skip_serializing_if = "ruma::serde::is_true"
    )]
    pub federate: bool,

    /// The version of the room.
    ///
    /// Defaults to `RoomVersionId::V1`.
    #[serde(default = "default_create_room_version_id")]
    pub room_version: RoomVersionId,

    /// A reference to the room this room replaces, if the previous room was
    /// upgraded.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub predecessor: Option<PreviousRoom>,

    /// The room type.
    ///
    /// This is currently only used for spaces.
    #[serde(skip_serializing_if = "Option::is_none", rename = "type")]
    pub room_type: Option<RoomType>,
}

impl RoomCreateWithCreatorEventContent {
    /// Constructs a `RoomCreateWithCreatorEventContent` with the given original
    /// content and sender.
    pub fn from_event_content(content: RoomCreateEventContent, sender: OwnedUserId) -> Self {
        let RoomCreateEventContent { federate, room_version, predecessor, room_type, .. } = content;
        Self { creator: sender, federate, room_version, predecessor, room_type }
    }

    fn into_event_content(self) -> (RoomCreateEventContent, OwnedUserId) {
        let Self { creator, federate, room_version, predecessor, room_type } = self;

        #[allow(deprecated)]
        let content = assign!(RoomCreateEventContent::new_v11(), {
            creator: Some(creator.clone()),
            federate,
            room_version,
            predecessor,
            room_type,
        });

        (content, creator)
    }
}

/// Redacted form of [`RoomCreateWithCreatorEventContent`].
pub type RedactedRoomCreateWithCreatorEventContent = RoomCreateWithCreatorEventContent;

impl RedactedStateEventContent for RedactedRoomCreateWithCreatorEventContent {
    type StateKey = EmptyStateKey;
}

impl RedactContent for RoomCreateWithCreatorEventContent {
    type Redacted = RedactedRoomCreateWithCreatorEventContent;

    fn redact(self, version: &RoomVersionId) -> Self::Redacted {
        let (content, sender) = self.into_event_content();
        // Use Ruma's redaction algorithm.
        let content = content.redact(version);
        Self::from_event_content(content, sender)
    }
}

fn default_create_room_version_id() -> RoomVersionId {
    RoomVersionId::V1
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

    /// Get this `RoomMemberships` as a list of matching [`MembershipState`]s.
    pub fn as_vec(&self) -> Vec<MembershipState> {
        let mut memberships = Vec::new();

        if self.contains(Self::JOIN) {
            memberships.push(MembershipState::Join);
        }
        if self.contains(Self::INVITE) {
            memberships.push(MembershipState::Invite);
        }
        if self.contains(Self::KNOCK) {
            memberships.push(MembershipState::Knock);
        }
        if self.contains(Self::LEAVE) {
            memberships.push(MembershipState::Leave);
        }
        if self.contains(Self::BAN) {
            memberships.push(MembershipState::Ban);
        }

        memberships
    }
}

#[cfg(test)]
mod tests {
    use std::ops::Not;

    use ruma::events::tag::{TagInfo, TagName, Tags};

    use super::{BaseRoomInfo, RoomNotableTags};
    use crate::RoomDisplayName;

    #[test]
    fn test_handle_notable_tags_favourite() {
        let mut base_room_info = BaseRoomInfo::default();

        let mut tags = Tags::new();
        tags.insert(TagName::Favorite, TagInfo::default());

        assert!(base_room_info.notable_tags.contains(RoomNotableTags::FAVOURITE).not());
        base_room_info.handle_notable_tags(&tags);
        assert!(base_room_info.notable_tags.contains(RoomNotableTags::FAVOURITE));
        tags.clear();
        base_room_info.handle_notable_tags(&tags);
        assert!(base_room_info.notable_tags.contains(RoomNotableTags::FAVOURITE).not());
    }

    #[test]
    fn test_handle_notable_tags_low_priority() {
        let mut base_room_info = BaseRoomInfo::default();

        let mut tags = Tags::new();
        tags.insert(TagName::LowPriority, TagInfo::default());

        assert!(base_room_info.notable_tags.contains(RoomNotableTags::LOW_PRIORITY).not());
        base_room_info.handle_notable_tags(&tags);
        assert!(base_room_info.notable_tags.contains(RoomNotableTags::LOW_PRIORITY));
        tags.clear();
        base_room_info.handle_notable_tags(&tags);
        assert!(base_room_info.notable_tags.contains(RoomNotableTags::LOW_PRIORITY).not());
    }

    #[test]
    fn test_room_alias_from_room_display_name_lowercases() {
        assert_eq!(
            "roomalias",
            RoomDisplayName::Named("RoomAlias".to_owned()).to_room_alias_name()
        );
    }

    #[test]
    fn test_room_alias_from_room_display_name_removes_whitespace() {
        assert_eq!(
            "room-alias",
            RoomDisplayName::Named("Room Alias".to_owned()).to_room_alias_name()
        );
    }

    #[test]
    fn test_room_alias_from_room_display_name_removes_non_ascii_symbols() {
        assert_eq!(
            "roomalias",
            RoomDisplayName::Named("Room±Alias√".to_owned()).to_room_alias_name()
        );
    }

    #[test]
    fn test_room_alias_from_room_display_name_removes_invalid_ascii_symbols() {
        assert_eq!(
            "roomalias",
            RoomDisplayName::Named("#Room,{Alias}:".to_owned()).to_room_alias_name()
        );
    }
}
