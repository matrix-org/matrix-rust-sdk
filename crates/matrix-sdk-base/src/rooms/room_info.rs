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

use ruma::{
    assign,
    events::{
        beacon_info::BeaconInfoEventContent,
        call::member::{CallMemberEventContent, CallMemberStateKey},
        direct::OwnedDirectUserIdentifier,
        room::{
            avatar::RoomAvatarEventContent, canonical_alias::RoomCanonicalAliasEventContent,
            encryption::RoomEncryptionEventContent, guest_access::RoomGuestAccessEventContent,
            history_visibility::RoomHistoryVisibilityEventContent,
            join_rules::RoomJoinRulesEventContent, name::RoomNameEventContent,
            pinned_events::RoomPinnedEventsEventContent, tombstone::RoomTombstoneEventContent,
            topic::RoomTopicEventContent,
        },
        tag::{TagName, Tags},
        AnyStrippedStateEvent, AnySyncStateEvent, RedactContent, RedactedStateEventContent,
        StaticStateEventContent, SyncStateEvent,
    },
    EventId, OwnedUserId, RoomVersionId,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};

use crate::MinimalStateEvent;

use super::{AccountDataSource, RoomCreateWithCreatorEventContent, RoomNotableTags};

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
    /// The source of is_marked_unread.
    #[serde(default)]
    pub(crate) is_marked_unread_source: AccountDataSource,
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

    pub(super) fn handle_redaction(&mut self, redacts: &EventId) {
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
            is_marked_unread_source: AccountDataSource::Unstable,
            notable_tags: RoomNotableTags::empty(),
            pinned_events: None,
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
        self.as_ref().is_some_and(|ev| ev.event_id() == Some(ev_id))
    }
}
