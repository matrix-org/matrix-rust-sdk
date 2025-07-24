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

use matrix_sdk_common::ROOM_VERSION_RULES_FALLBACK;
use ruma::{
    OwnedUserId, RoomVersionId, assign,
    events::{
        EmptyStateKey, RedactContent, RedactedStateEventContent, StateEventType,
        macros::EventContent,
        room::create::{PreviousRoom, RoomCreateEventContent},
    },
    room::RoomType,
    room_version_rules::RedactionRules,
};
use serde::{Deserialize, Serialize};

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

    /// Additional room creators, considered to have "infinite" power level, in
    /// room versions 12 onwards.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub additional_creators: Vec<OwnedUserId>,
}

impl RoomCreateWithCreatorEventContent {
    /// Constructs a `RoomCreateWithCreatorEventContent` with the given original
    /// content and sender.
    pub fn from_event_content(content: RoomCreateEventContent, sender: OwnedUserId) -> Self {
        let RoomCreateEventContent {
            federate,
            room_version,
            predecessor,
            room_type,
            additional_creators,
            ..
        } = content;
        Self {
            creator: sender,
            federate,
            room_version,
            predecessor,
            room_type,
            additional_creators,
        }
    }

    fn into_event_content(self) -> (RoomCreateEventContent, OwnedUserId) {
        let Self { creator, federate, room_version, predecessor, room_type, additional_creators } =
            self;

        #[allow(deprecated)]
        let content = assign!(RoomCreateEventContent::new_v11(), {
            creator: Some(creator.clone()),
            federate,
            room_version,
            predecessor,
            room_type,
            additional_creators,
        });

        (content, creator)
    }

    /// Get the creators of the room from this content, according to the room
    /// version.
    pub(crate) fn creators(&self) -> Vec<OwnedUserId> {
        let rules = self.room_version.rules().unwrap_or(ROOM_VERSION_RULES_FALLBACK);

        if rules.authorization.explicitly_privilege_room_creators {
            std::iter::once(self.creator.clone())
                .chain(self.additional_creators.iter().cloned())
                .collect()
        } else {
            vec![self.creator.clone()]
        }
    }
}

/// Redacted form of [`RoomCreateWithCreatorEventContent`].
pub type RedactedRoomCreateWithCreatorEventContent = RoomCreateWithCreatorEventContent;

impl RedactedStateEventContent for RedactedRoomCreateWithCreatorEventContent {
    type StateKey = EmptyStateKey;

    fn event_type(&self) -> StateEventType {
        StateEventType::RoomCreate
    }
}

impl RedactContent for RoomCreateWithCreatorEventContent {
    type Redacted = RedactedRoomCreateWithCreatorEventContent;

    fn redact(self, rules: &RedactionRules) -> Self::Redacted {
        let (content, sender) = self.into_event_content();
        // Use Ruma's redaction algorithm.
        let content = content.redact(rules);
        Self::from_event_content(content, sender)
    }
}

fn default_create_room_version_id() -> RoomVersionId {
    RoomVersionId::V1
}
