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

mod display_name;
mod encryption;
mod members;
pub(crate) mod normal;
mod room_info;
mod tags;

pub use display_name::{RoomDisplayName, RoomHero};
pub(crate) use display_name::{RoomSummary, UpdatedRoomDisplayName};
pub use encryption::EncryptionState;
pub use members::{RoomMember, RoomMembersUpdate, RoomMemberships};
pub use normal::{Room, RoomState, RoomStateFilter};
pub(crate) use room_info::SyncInfo;
pub use room_info::{
    apply_redaction, BaseRoomInfo, RoomInfo, RoomInfoNotableUpdate, RoomInfoNotableUpdateReasons,
};
use ruma::{
    assign,
    events::{
        macros::EventContent,
        room::create::{PreviousRoom, RoomCreateEventContent},
        EmptyStateKey, RedactContent, RedactedStateEventContent,
    },
    room::RoomType,
    OwnedUserId, RoomVersionId,
};
use serde::{Deserialize, Serialize};
pub(crate) use tags::RoomNotableTags;

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
