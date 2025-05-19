// Copyright 2025 The Matrix.org Foundation C.I.C.
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

#![allow(clippy::assign_op_pattern)] // Triggered by bitflags! usage

mod members;
pub(crate) mod normal;
mod room_info;

use std::{fmt, hash::Hash};

use bitflags::bitflags;
pub use members::RoomMember;
pub use normal::{
    EncryptionState, Room, RoomHero, RoomInfoNotableUpdate, RoomInfoNotableUpdateReasons,
    RoomMembersUpdate, RoomState, RoomStateFilter,
};
use regex::Regex;
pub(crate) use room_info::SyncInfo;
pub use room_info::{apply_redaction, BaseRoomInfo, RoomInfo};
use ruma::{
    assign,
    events::{
        macros::EventContent,
        room::{
            create::{PreviousRoom, RoomCreateEventContent},
            member::MembershipState,
        },
        EmptyStateKey, RedactContent, RedactedStateEventContent,
    },
    room::RoomType,
    OwnedUserId, RoomVersionId,
};
use serde::{Deserialize, Serialize};

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

/// An internal representing whether a room display name is new or not when
/// computed.
pub(crate) enum UpdatedRoomDisplayName {
    New(RoomDisplayName),
    Same(RoomDisplayName),
}

impl UpdatedRoomDisplayName {
    /// Get the inner [`RoomDisplayName`].
    pub fn into_inner(self) -> RoomDisplayName {
        match self {
            UpdatedRoomDisplayName::New(room_display_name) => room_display_name,
            UpdatedRoomDisplayName::Same(room_display_name) => room_display_name,
        }
    }
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

/// The possible sources of an account data type.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum AccountDataSource {
    /// The source is account data with the stable prefix.
    Stable,

    /// The source is account data with the unstable prefix.
    #[default]
    Unstable,
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
