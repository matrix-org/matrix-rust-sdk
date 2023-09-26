// Copyright 2023 The Matrix.org Foundation C.I.C.
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

//! Data migration helpers for StateStore implementations.

use std::collections::HashSet;

#[cfg(feature = "experimental-sliding-sync")]
use matrix_sdk_common::deserialized_responses::SyncTimelineEvent;
use ruma::{
    events::{
        room::{
            avatar::RoomAvatarEventContent,
            canonical_alias::RoomCanonicalAliasEventContent,
            create::RoomCreateEventContent,
            encryption::RoomEncryptionEventContent,
            guest_access::RoomGuestAccessEventContent,
            history_visibility::RoomHistoryVisibilityEventContent,
            join_rules::RoomJoinRulesEventContent,
            name::{RedactedRoomNameEventContent, RoomNameEventContent},
            tombstone::RoomTombstoneEventContent,
            topic::RoomTopicEventContent,
        },
        EmptyStateKey, EventContent, RedactContent, StateEventContent, StateEventType,
    },
    OwnedRoomId, OwnedUserId, RoomId,
};
use serde::{Deserialize, Serialize};

use crate::{
    deserialized_responses::SyncOrStrippedState,
    rooms::{
        normal::{RoomSummary, SyncInfo},
        BaseRoomInfo,
    },
    sync::UnreadNotificationsCount,
    MinimalStateEvent, OriginalMinimalStateEvent, RoomInfo, RoomState,
};

/// [`RoomInfo`] version 1.
///
/// The `name` field in `RoomNameEventContent` was optional and has become
/// required. It means that sometimes the field has been serialized with the
/// value `null`.
///
/// For the migration:
///
/// 1. Deserialize the stored room info using this type,
/// 2. Get the `m.room.create` event for the room, if it is available,
/// 3. Convert this to [`RoomInfo`] with `.migrate(create_event)`,
/// 4. Replace the room info in the store.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RoomInfoV1 {
    room_id: OwnedRoomId,
    room_type: RoomState,
    notification_counts: UnreadNotificationsCount,
    summary: RoomSummary,
    members_synced: bool,
    last_prev_batch: Option<String>,
    #[serde(default = "sync_info_complete")] // see fn docs for why we use this default
    sync_info: SyncInfo,
    #[serde(default = "encryption_state_default")] // see fn docs for why we use this default
    encryption_state_synced: bool,
    #[cfg(feature = "experimental-sliding-sync")]
    latest_event: Option<SyncTimelineEvent>,
    base_info: BaseRoomInfoV1,
}

impl RoomInfoV1 {
    /// Get the room ID of this room.
    pub fn room_id(&self) -> &RoomId {
        &self.room_id
    }

    /// Returns the state this room is in.
    pub fn state(&self) -> RoomState {
        self.room_type
    }

    /// Migrate this to a [`RoomInfo`], using the given `m.room.create` event
    /// from the room state.
    pub fn migrate(self, create: Option<&SyncOrStrippedState<RoomCreateEventContent>>) -> RoomInfo {
        let RoomInfoV1 {
            room_id,
            room_type,
            notification_counts,
            summary,
            members_synced,
            last_prev_batch,
            sync_info,
            encryption_state_synced,
            #[cfg(feature = "experimental-sliding-sync")]
            latest_event,
            base_info,
        } = self;

        RoomInfo {
            room_id,
            room_state: room_type,
            notification_counts,
            summary,
            members_synced,
            last_prev_batch,
            sync_info,
            encryption_state_synced,
            #[cfg(feature = "experimental-sliding-sync")]
            latest_event,
            base_info: base_info.migrate(create),
        }
    }
}

// The sync_info field introduced a new field in the database schema, for
// backwards compatibility we assume that if the room is in the database, yet
// the field isn't, we have synced it before this field was introduced - which
// was a a full sync.
fn sync_info_complete() -> SyncInfo {
    SyncInfo::FullySynced
}

// The encryption_state_synced field introduced a new field in the database
// schema, for backwards compatibility we assume that if the room is in the
// database, yet the field isn't, we have synced it before this field was
// introduced - which was a a full sync.
fn encryption_state_default() -> bool {
    true
}

/// [`BaseRoomInfo`] version 1.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct BaseRoomInfoV1 {
    avatar: Option<MinimalStateEvent<RoomAvatarEventContent>>,
    canonical_alias: Option<MinimalStateEvent<RoomCanonicalAliasEventContent>>,
    dm_targets: HashSet<OwnedUserId>,
    encryption: Option<RoomEncryptionEventContent>,
    guest_access: Option<MinimalStateEvent<RoomGuestAccessEventContent>>,
    history_visibility: Option<MinimalStateEvent<RoomHistoryVisibilityEventContent>>,
    join_rules: Option<MinimalStateEvent<RoomJoinRulesEventContent>>,
    max_power_level: i64,
    name: Option<MinimalStateEvent<RoomNameEventContentV1>>,
    tombstone: Option<MinimalStateEvent<RoomTombstoneEventContent>>,
    topic: Option<MinimalStateEvent<RoomTopicEventContent>>,
}

impl BaseRoomInfoV1 {
    /// Migrate this to a [`BaseRoomInfo`].
    fn migrate(self, create: Option<&SyncOrStrippedState<RoomCreateEventContent>>) -> BaseRoomInfo {
        let BaseRoomInfoV1 {
            avatar,
            canonical_alias,
            dm_targets,
            encryption,
            guest_access,
            history_visibility,
            join_rules,
            max_power_level,
            name,
            tombstone,
            topic,
        } = self;

        let create = create.map(|ev| match ev {
            SyncOrStrippedState::Sync(e) => e.into(),
            SyncOrStrippedState::Stripped(e) => e.into(),
        });
        let name = name.map(|name| match name {
            MinimalStateEvent::Original(ev) => {
                MinimalStateEvent::Original(OriginalMinimalStateEvent {
                    content: ev.content.into(),
                    event_id: ev.event_id,
                })
            }
            MinimalStateEvent::Redacted(ev) => MinimalStateEvent::Redacted(ev),
        });

        BaseRoomInfo {
            avatar,
            canonical_alias,
            create,
            dm_targets,
            encryption,
            guest_access,
            history_visibility,
            join_rules,
            max_power_level,
            name,
            tombstone,
            topic,
        }
    }
}

/// [`RoomNameEventContent`] version 1, with an optional `name`.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct RoomNameEventContentV1 {
    name: Option<String>,
}

impl EventContent for RoomNameEventContentV1 {
    type EventType = StateEventType;

    fn event_type(&self) -> Self::EventType {
        StateEventType::RoomName
    }
}

impl StateEventContent for RoomNameEventContentV1 {
    type StateKey = EmptyStateKey;
}

impl RedactContent for RoomNameEventContentV1 {
    type Redacted = RedactedRoomNameEventContent;

    fn redact(self, _version: &ruma::RoomVersionId) -> Self::Redacted {
        RedactedRoomNameEventContent::new()
    }
}

impl From<RoomNameEventContentV1> for RoomNameEventContent {
    fn from(value: RoomNameEventContentV1) -> Self {
        RoomNameEventContent::new(value.name.unwrap_or_default())
    }
}
