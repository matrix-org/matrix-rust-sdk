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

use std::{
    collections::{BTreeMap, HashSet},
    sync::{Arc, atomic::AtomicBool},
};

use as_variant::as_variant;
use bitflags::bitflags;
use eyeball::Subscriber;
use matrix_sdk_common::{ROOM_VERSION_FALLBACK, ROOM_VERSION_RULES_FALLBACK};
use ruma::{
    EventId, MilliSecondsSinceUnixEpoch, MxcUri, OwnedEventId, OwnedMxcUri, OwnedRoomAliasId,
    OwnedRoomId, OwnedUserId, RoomAliasId, RoomId, RoomVersionId,
    api::client::sync::sync_events::v3::RoomSummary as RumaSummary,
    assign,
    events::{
        AnyStrippedStateEvent, AnySyncStateEvent, AnySyncTimelineEvent, StateEventType,
        SyncStateEvent,
        call::member::{CallMemberEventContent, CallMemberStateKey, MembershipData},
        direct::OwnedDirectUserIdentifier,
        room::{
            avatar::{self, RoomAvatarEventContent},
            canonical_alias::RoomCanonicalAliasEventContent,
            encryption::RoomEncryptionEventContent,
            guest_access::{GuestAccess, RoomGuestAccessEventContent},
            history_visibility::{HistoryVisibility, RoomHistoryVisibilityEventContent},
            join_rules::{JoinRule, RoomJoinRulesEventContent},
            name::RoomNameEventContent,
            pinned_events::RoomPinnedEventsEventContent,
            redaction::SyncRoomRedactionEvent,
            tombstone::RoomTombstoneEventContent,
            topic::RoomTopicEventContent,
        },
        tag::{TagEventContent, TagName, Tags},
    },
    room::RoomType,
    room_version_rules::{AuthorizationRules, RedactionRules, RoomVersionRules},
    serde::Raw,
};
use serde::{Deserialize, Serialize};
use tracing::{error, field::debug, info, instrument, warn};

use super::{
    AccountDataSource, EncryptionState, Room, RoomCreateWithCreatorEventContent, RoomDisplayName,
    RoomHero, RoomNotableTags, RoomState, RoomSummary,
};
use crate::{
    MinimalStateEvent, OriginalMinimalStateEvent,
    deserialized_responses::RawSyncOrStrippedState,
    latest_event::LatestEventValue,
    notification_settings::RoomNotificationMode,
    read_receipts::RoomReadReceipts,
    store::{DynStateStore, StateStoreExt},
    sync::UnreadNotificationsCount,
    utils::RawSyncStateEventWithKeys,
};

/// The default value of the maximum power level.
const DEFAULT_MAX_POWER_LEVEL: i64 = 100;

/// A struct remembering details of an invite and if the invite has been
/// accepted on this particular client.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InviteAcceptanceDetails {
    /// A timestamp remembering when we observed the user accepting an invite
    /// using this client.
    pub invite_accepted_at: MilliSecondsSinceUnixEpoch,

    /// The user ID of the person that invited us.
    pub inviter: OwnedUserId,
}

impl Room {
    /// Subscribe to the inner `RoomInfo`.
    pub fn subscribe_info(&self) -> Subscriber<RoomInfo> {
        self.info.subscribe()
    }

    /// Clone the inner `RoomInfo`.
    pub fn clone_info(&self) -> RoomInfo {
        self.info.get()
    }

    /// Update the summary with given RoomInfo.
    pub fn set_room_info(
        &self,
        room_info: RoomInfo,
        room_info_notable_update_reasons: RoomInfoNotableUpdateReasons,
    ) {
        self.info.set(room_info);

        if !room_info_notable_update_reasons.is_empty() {
            // Ignore error if no receiver exists.
            let _ = self.room_info_notable_update_sender.send(RoomInfoNotableUpdate {
                room_id: self.room_id.clone(),
                reasons: room_info_notable_update_reasons,
            });
        } else {
            // TODO: remove this block!
            // Read `RoomInfoNotableUpdateReasons::NONE` to understand why it must be
            // removed.
            let _ = self.room_info_notable_update_sender.send(RoomInfoNotableUpdate {
                room_id: self.room_id.clone(),
                reasons: RoomInfoNotableUpdateReasons::NONE,
            });
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
    pub fn handle_state_event(&mut self, raw_event: &mut RawSyncStateEventWithKeys) -> bool {
        match (&raw_event.event_type, raw_event.state_key.as_str()) {
            (StateEventType::RoomEncryption, "") => {
                // No redacted or failed deserialization branch - enabling encryption cannot be
                // undone.
                if let Some(SyncStateEvent::Original(event)) =
                    raw_event.deserialize_as(|any_event| {
                        as_variant!(any_event, AnySyncStateEvent::RoomEncryption)
                    })
                {
                    self.encryption = Some(event.content.clone());
                    true
                } else {
                    false
                }
            }
            (StateEventType::RoomAvatar, "") => {
                if let Some(event) = raw_event.deserialize_as(|any_event| {
                    as_variant!(any_event, AnySyncStateEvent::RoomAvatar)
                }) {
                    self.avatar = Some(event.into());
                    true
                } else {
                    // Remove the previous content if the new content is unknown.
                    self.avatar.take().is_some()
                }
            }
            (StateEventType::RoomName, "") => {
                if let Some(event) = raw_event
                    .deserialize_as(|any_event| as_variant!(any_event, AnySyncStateEvent::RoomName))
                {
                    self.name = Some(event.into());
                    true
                } else {
                    // Remove the previous content if the new content is unknown.
                    self.name.take().is_some()
                }
            }
            // `m.room.create` CANNOT be overwritten.
            (StateEventType::RoomCreate, "") if self.create.is_none() => {
                if let Some(event) = raw_event.deserialize_as(|any_event| {
                    as_variant!(any_event, AnySyncStateEvent::RoomCreate)
                }) {
                    self.create = Some(event.into());
                    true
                } else {
                    false
                }
            }
            (StateEventType::RoomHistoryVisibility, "") => {
                if let Some(event) = raw_event.deserialize_as(|any_event| {
                    as_variant!(any_event, AnySyncStateEvent::RoomHistoryVisibility)
                }) {
                    self.history_visibility = Some(event.into());
                    true
                } else {
                    // Remove the previous content if the new content is unknown.
                    self.history_visibility.take().is_some()
                }
            }
            (StateEventType::RoomGuestAccess, "") => {
                if let Some(event) = raw_event.deserialize_as(|any_event| {
                    as_variant!(any_event, AnySyncStateEvent::RoomGuestAccess)
                }) {
                    self.guest_access = Some(event.into());
                    true
                } else {
                    // Remove the previous content if the new content is unknown.
                    self.guest_access.take().is_some()
                }
            }
            (StateEventType::RoomJoinRules, "") => {
                if let Some(event) = raw_event.deserialize_as(|any_event| {
                    as_variant!(any_event, AnySyncStateEvent::RoomJoinRules)
                }) {
                    match event.join_rule() {
                        JoinRule::Invite
                        | JoinRule::Knock
                        | JoinRule::Private
                        | JoinRule::Restricted(_)
                        | JoinRule::KnockRestricted(_)
                        | JoinRule::Public => {
                            self.join_rules = Some(event.into());
                            true
                        }
                        r => {
                            warn!(join_rule = ?r.as_str(), "Encountered a custom join rule, skipping");
                            // Remove the previous content if the new content is unsupported.
                            self.join_rules.take().is_some()
                        }
                    }
                } else {
                    // Remove the previous content if the new content is unknown.
                    self.join_rules.take().is_some()
                }
            }
            (StateEventType::RoomCanonicalAlias, "") => {
                if let Some(event) = raw_event.deserialize_as(|any_event| {
                    as_variant!(any_event, AnySyncStateEvent::RoomCanonicalAlias)
                }) {
                    self.canonical_alias = Some(event.into());
                    true
                } else {
                    // Remove the previous content if the new content is unknown.
                    self.canonical_alias.take().is_some()
                }
            }
            (StateEventType::RoomTopic, "") => {
                if let Some(event) = raw_event.deserialize_as(|any_event| {
                    as_variant!(any_event, AnySyncStateEvent::RoomTopic)
                }) {
                    self.topic = Some(event.into());
                    true
                } else {
                    // Remove the previous content if the new content is unknown.
                    self.topic.take().is_some()
                }
            }
            (StateEventType::RoomTombstone, "") => {
                if let Some(event) = raw_event.deserialize_as(|any_event| {
                    as_variant!(any_event, AnySyncStateEvent::RoomTombstone)
                }) {
                    self.tombstone = Some(event.into());
                    true
                } else {
                    // Remove the previous content if the new content is unknown.
                    self.tombstone.take().is_some()
                }
            }
            (StateEventType::RoomPowerLevels, "") => {
                if let Some(event) = raw_event.deserialize_as(|any_event| {
                    as_variant!(any_event, AnySyncStateEvent::RoomPowerLevels)
                }) {
                    // The rules and creators do not affect the max power level.
                    self.max_power_level =
                        event.power_levels(&AuthorizationRules::V1, vec![]).max().into();
                    true
                } else if self.max_power_level != DEFAULT_MAX_POWER_LEVEL {
                    // Reset the previous value if the new value is unknown.
                    self.max_power_level = DEFAULT_MAX_POWER_LEVEL;
                    true
                } else {
                    false
                }
            }
            (StateEventType::CallMember, _) => {
                if let Some(SyncStateEvent::Original(event)) =
                    raw_event.deserialize_as(|any_event| {
                        as_variant!(any_event, AnySyncStateEvent::CallMember)
                    })
                {
                    // we modify the event so that `origin_server_ts` gets copied into
                    // `content.created_ts`
                    let mut event = event.clone();
                    event.content.set_created_ts_if_none(event.origin_server_ts);

                    // Add the new event.
                    self.rtc_member_events
                        .insert(event.state_key.clone(), SyncStateEvent::Original(event).into());

                    // Remove all events that don't contain any memberships anymore.
                    self.rtc_member_events.retain(|_, ev| {
                        ev.as_original()
                            .is_some_and(|o| !o.content.active_memberships(None).is_empty())
                    });

                    true
                } else if let Ok(call_member_key) =
                    raw_event.state_key.parse::<CallMemberStateKey>()
                {
                    // Remove the previous content with the same state key if the new content is
                    // unknown.
                    self.rtc_member_events.remove(&call_member_key).is_some()
                } else {
                    false
                }
            }
            (StateEventType::RoomPinnedEvents, "") => {
                if let Some(SyncStateEvent::Original(event)) =
                    raw_event.deserialize_as(|any_event| {
                        as_variant!(any_event, AnySyncStateEvent::RoomPinnedEvents)
                    })
                {
                    self.pinned_events = Some(event.content.clone());
                    true
                } else {
                    // Remove the previous content if the new content is unknown.
                    self.pinned_events.take().is_some()
                }
            }
            _ => false,
        }
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
            AnyStrippedStateEvent::RoomJoinRules(c) => match &c.content.join_rule {
                JoinRule::Invite
                | JoinRule::Knock
                | JoinRule::Private
                | JoinRule::Restricted(_)
                | JoinRule::KnockRestricted(_)
                | JoinRule::Public => self.join_rules = Some(c.into()),
                r => warn!("Encountered a custom join rule {}, skipping", r.as_str()),
            },
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
                // The rules and creators do not affect the max power level.
                self.max_power_level = p.power_levels(&AuthorizationRules::V1, vec![]).max().into();
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
        let redaction_rules = self
            .room_version()
            .and_then(|room_version| room_version.rules())
            .unwrap_or(ROOM_VERSION_RULES_FALLBACK)
            .redaction;

        if let Some(ev) = &mut self.avatar
            && ev.event_id() == Some(redacts)
        {
            ev.redact(&redaction_rules);
        } else if let Some(ev) = &mut self.canonical_alias
            && ev.event_id() == Some(redacts)
        {
            ev.redact(&redaction_rules);
        } else if let Some(ev) = &mut self.create
            && ev.event_id() == Some(redacts)
        {
            ev.redact(&redaction_rules);
        } else if let Some(ev) = &mut self.guest_access
            && ev.event_id() == Some(redacts)
        {
            ev.redact(&redaction_rules);
        } else if let Some(ev) = &mut self.history_visibility
            && ev.event_id() == Some(redacts)
        {
            ev.redact(&redaction_rules);
        } else if let Some(ev) = &mut self.join_rules
            && ev.event_id() == Some(redacts)
        {
            ev.redact(&redaction_rules);
        } else if let Some(ev) = &mut self.name
            && ev.event_id() == Some(redacts)
        {
            ev.redact(&redaction_rules);
        } else if let Some(ev) = &mut self.tombstone
            && ev.event_id() == Some(redacts)
        {
            ev.redact(&redaction_rules);
        } else if let Some(ev) = &mut self.topic
            && ev.event_id() == Some(redacts)
        {
            ev.redact(&redaction_rules);
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
            canonical_alias: None,
            create: None,
            dm_targets: Default::default(),
            encryption: None,
            guest_access: None,
            history_visibility: None,
            join_rules: None,
            max_power_level: DEFAULT_MAX_POWER_LEVEL,
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

/// The underlying pure data structure for joined and left rooms.
///
/// Holds all the info needed to persist a room into the state store.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RoomInfo {
    /// The version of the room info type. It is used to migrate the `RoomInfo`
    /// serialization from one version to another.
    #[serde(default, alias = "version")]
    pub(crate) data_format_version: u8,

    /// The unique room id of the room.
    pub(crate) room_id: OwnedRoomId,

    /// The state of the room.
    pub(crate) room_state: RoomState,

    /// The unread notifications counts, as returned by the server.
    ///
    /// These might be incorrect for encrypted rooms, since the server doesn't
    /// have access to the content of the encrypted events.
    pub(crate) notification_counts: UnreadNotificationsCount,

    /// The summary of this room.
    pub(crate) summary: RoomSummary,

    /// Flag remembering if the room members are synced.
    pub(crate) members_synced: bool,

    /// The prev batch of this room we received during the last sync.
    pub(crate) last_prev_batch: Option<String>,

    /// How much we know about this room.
    pub(crate) sync_info: SyncInfo,

    /// Whether or not the encryption info was been synced.
    pub(crate) encryption_state_synced: bool,

    /// The latest event value of this room.
    #[serde(default)]
    pub(crate) latest_event_value: LatestEventValue,

    /// Information about read receipts for this room.
    #[serde(default)]
    pub(crate) read_receipts: RoomReadReceipts,

    /// Base room info which holds some basic event contents important for the
    /// room state.
    pub(crate) base_info: Box<BaseRoomInfo>,

    /// Whether we already warned about unknown room version rules in
    /// [`RoomInfo::room_version_rules_or_default`]. This is done to avoid
    /// spamming about unknown room versions rules in the log for the same room.
    #[serde(skip)]
    pub(crate) warned_about_unknown_room_version_rules: Arc<AtomicBool>,

    /// Cached display name, useful for sync access.
    ///
    /// Filled by calling [`Room::compute_display_name`]. It's automatically
    /// filled at start when creating a room, or on every successful sync.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) cached_display_name: Option<RoomDisplayName>,

    /// Cached user defined notification mode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) cached_user_defined_notification_mode: Option<RoomNotificationMode>,

    /// The recency stamp of this room.
    ///
    /// It's not to be confused with the `origin_server_ts` value of an event.
    /// Sliding Sync might “ignore” some events when computing the recency
    /// stamp of the room. The recency stamp must be considered as an opaque
    /// unsigned integer value.
    ///
    /// # Sorting rooms
    ///
    /// The recency stamp is designed to _sort_ rooms between them. The room
    /// with the highest stamp should be at the top of a room list. However, in
    /// some situation, it might be inaccurate (for example if the server and
    /// the client disagree on which events should increment the recency stamp).
    /// The [`LatestEventValue`] might be a useful alternative to sort rooms
    /// between them as it's all computed client-side. In this case, the recency
    /// stamp nicely acts as a default fallback.
    #[serde(default)]
    pub(crate) recency_stamp: Option<RoomRecencyStamp>,

    /// A timestamp remembering when we observed the user accepting an invite on
    /// this current device.
    ///
    /// This is useful to remember if the user accepted this join on this
    /// specific client.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) invite_acceptance_details: Option<InviteAcceptanceDetails>,
}

impl RoomInfo {
    #[doc(hidden)] // used by store tests, otherwise it would be pub(crate)
    pub fn new(room_id: &RoomId, room_state: RoomState) -> Self {
        Self {
            data_format_version: 1,
            room_id: room_id.into(),
            room_state,
            notification_counts: Default::default(),
            summary: Default::default(),
            members_synced: false,
            last_prev_batch: None,
            sync_info: SyncInfo::NoState,
            encryption_state_synced: false,
            latest_event_value: LatestEventValue::default(),
            read_receipts: Default::default(),
            base_info: Box::new(BaseRoomInfo::new()),
            warned_about_unknown_room_version_rules: Arc::new(false.into()),
            cached_display_name: None,
            cached_user_defined_notification_mode: None,
            recency_stamp: None,
            invite_acceptance_details: None,
        }
    }

    /// Mark this Room as joined.
    pub fn mark_as_joined(&mut self) {
        self.set_state(RoomState::Joined);
    }

    /// Mark this Room as left.
    pub fn mark_as_left(&mut self) {
        self.set_state(RoomState::Left);
    }

    /// Mark this Room as invited.
    pub fn mark_as_invited(&mut self) {
        self.set_state(RoomState::Invited);
    }

    /// Mark this Room as knocked.
    pub fn mark_as_knocked(&mut self) {
        self.set_state(RoomState::Knocked);
    }

    /// Mark this Room as banned.
    pub fn mark_as_banned(&mut self) {
        self.set_state(RoomState::Banned);
    }

    /// Set the membership RoomState of this Room
    pub fn set_state(&mut self, room_state: RoomState) {
        if self.state() != RoomState::Joined && self.invite_acceptance_details.is_some() {
            error!(room_id = %self.room_id, "The RoomInfo contains invite acceptance details but the room is not in the joined state");
        }
        // Changing our state removes the invite details since we can't know that they
        // are relevant anymore.
        self.invite_acceptance_details = None;
        self.room_state = room_state;
    }

    /// Mark this Room as having all the members synced.
    pub fn mark_members_synced(&mut self) {
        self.members_synced = true;
    }

    /// Mark this Room as still missing member information.
    pub fn mark_members_missing(&mut self) {
        self.members_synced = false;
    }

    /// Returns whether the room members are synced.
    pub fn are_members_synced(&self) -> bool {
        self.members_synced
    }

    /// Mark this Room as still missing some state information.
    pub fn mark_state_partially_synced(&mut self) {
        self.sync_info = SyncInfo::PartiallySynced;
    }

    /// Mark this Room as still having all state synced.
    pub fn mark_state_fully_synced(&mut self) {
        self.sync_info = SyncInfo::FullySynced;
    }

    /// Mark this Room as still having no state synced.
    pub fn mark_state_not_synced(&mut self) {
        self.sync_info = SyncInfo::NoState;
    }

    /// Mark this Room as having the encryption state synced.
    pub fn mark_encryption_state_synced(&mut self) {
        self.encryption_state_synced = true;
    }

    /// Mark this Room as still missing encryption state information.
    pub fn mark_encryption_state_missing(&mut self) {
        self.encryption_state_synced = false;
    }

    /// Set the `prev_batch`-token.
    /// Returns whether the token has differed and thus has been upgraded:
    /// `false` means no update was applied as the were the same
    pub fn set_prev_batch(&mut self, prev_batch: Option<&str>) -> bool {
        if self.last_prev_batch.as_deref() != prev_batch {
            self.last_prev_batch = prev_batch.map(|p| p.to_owned());
            true
        } else {
            false
        }
    }

    /// Returns the state this room is in.
    pub fn state(&self) -> RoomState {
        self.room_state
    }

    /// Returns the encryption state of this room.
    #[cfg(not(feature = "experimental-encrypted-state-events"))]
    pub fn encryption_state(&self) -> EncryptionState {
        if !self.encryption_state_synced {
            EncryptionState::Unknown
        } else if self.base_info.encryption.is_some() {
            EncryptionState::Encrypted
        } else {
            EncryptionState::NotEncrypted
        }
    }

    /// Returns the encryption state of this room.
    #[cfg(feature = "experimental-encrypted-state-events")]
    pub fn encryption_state(&self) -> EncryptionState {
        if !self.encryption_state_synced {
            EncryptionState::Unknown
        } else {
            self.base_info
                .encryption
                .as_ref()
                .map(|state| {
                    if state.encrypt_state_events {
                        EncryptionState::StateEncrypted
                    } else {
                        EncryptionState::Encrypted
                    }
                })
                .unwrap_or(EncryptionState::NotEncrypted)
        }
    }

    /// Set the encryption event content in this room.
    pub fn set_encryption_event(&mut self, event: Option<RoomEncryptionEventContent>) {
        self.base_info.encryption = event;
    }

    /// Handle the encryption state.
    pub fn handle_encryption_state(
        &mut self,
        requested_required_states: &[(StateEventType, String)],
    ) {
        if requested_required_states
            .iter()
            .any(|(state_event, _)| state_event == &StateEventType::RoomEncryption)
        {
            // The `m.room.encryption` event was requested during the sync. Whether we have
            // received a `m.room.encryption` event in return doesn't matter: we must mark
            // the encryption state as synced; if the event is present, it means the room
            // _is_ encrypted, otherwise it means the room _is not_ encrypted.

            self.mark_encryption_state_synced();
        }
    }

    /// Handle the given state event.
    ///
    /// Returns true if the event modified the info, false otherwise.
    pub fn handle_state_event(&mut self, raw_event: &mut RawSyncStateEventWithKeys) -> bool {
        // Store the state event in the `BaseRoomInfo` first.
        let base_info_has_been_modified = self.base_info.handle_state_event(raw_event);

        if raw_event.event_type == StateEventType::RoomEncryption && raw_event.state_key.is_empty()
        {
            // The `m.room.encryption` event was or wasn't explicitly requested, we don't
            // know here (see `Self::handle_encryption_state`) but we got one in
            // return! In this case, we can deduce the room _is_ encrypted, but we cannot
            // know if it _is not_ encrypted.

            self.mark_encryption_state_synced();
        }

        base_info_has_been_modified
    }

    /// Handle the given stripped state event.
    ///
    /// Returns true if the event modified the info, false otherwise.
    pub fn handle_stripped_state_event(&mut self, event: &AnyStrippedStateEvent) -> bool {
        self.base_info.handle_stripped_state_event(event)
    }

    /// Handle the given redaction.
    #[instrument(skip_all, fields(redacts))]
    pub fn handle_redaction(
        &mut self,
        event: &SyncRoomRedactionEvent,
        _raw: &Raw<SyncRoomRedactionEvent>,
    ) {
        let redaction_rules = self.room_version_rules_or_default().redaction;

        let Some(redacts) = event.redacts(&redaction_rules) else {
            info!("Can't apply redaction, redacts field is missing");
            return;
        };
        tracing::Span::current().record("redacts", debug(redacts));

        self.base_info.handle_redaction(redacts);
    }

    /// Returns the current room avatar.
    pub fn avatar_url(&self) -> Option<&MxcUri> {
        self.base_info
            .avatar
            .as_ref()
            .and_then(|e| e.as_original().and_then(|e| e.content.url.as_deref()))
    }

    /// Update the room avatar.
    pub fn update_avatar(&mut self, url: Option<OwnedMxcUri>) {
        self.base_info.avatar = url.map(|url| {
            let mut content = RoomAvatarEventContent::new();
            content.url = Some(url);

            MinimalStateEvent::Original(OriginalMinimalStateEvent { content, event_id: None })
        });
    }

    /// Returns information about the current room avatar.
    pub fn avatar_info(&self) -> Option<&avatar::ImageInfo> {
        self.base_info
            .avatar
            .as_ref()
            .and_then(|e| e.as_original().and_then(|e| e.content.info.as_deref()))
    }

    /// Update the notifications count.
    pub fn update_notification_count(&mut self, notification_counts: UnreadNotificationsCount) {
        self.notification_counts = notification_counts;
    }

    /// Update the RoomSummary from a Ruma `RoomSummary`.
    ///
    /// Returns true if any field has been updated, false otherwise.
    pub fn update_from_ruma_summary(&mut self, summary: &RumaSummary) -> bool {
        let mut changed = false;

        if !summary.is_empty() {
            if !summary.heroes.is_empty() {
                self.summary.room_heroes = summary
                    .heroes
                    .iter()
                    .map(|hero_id| RoomHero {
                        user_id: hero_id.to_owned(),
                        display_name: None,
                        avatar_url: None,
                    })
                    .collect();

                changed = true;
            }

            if let Some(joined) = summary.joined_member_count {
                self.summary.joined_member_count = joined.into();
                changed = true;
            }

            if let Some(invited) = summary.invited_member_count {
                self.summary.invited_member_count = invited.into();
                changed = true;
            }
        }

        changed
    }

    /// Updates the joined member count.
    pub(crate) fn update_joined_member_count(&mut self, count: u64) {
        self.summary.joined_member_count = count;
    }

    /// Updates the invited member count.
    pub(crate) fn update_invited_member_count(&mut self, count: u64) {
        self.summary.invited_member_count = count;
    }

    pub(crate) fn set_invite_acceptance_details(&mut self, details: InviteAcceptanceDetails) {
        self.invite_acceptance_details = Some(details);
    }

    /// Returns the timestamp when an invite to this room has been accepted by
    /// this specific client.
    ///
    /// # Returns
    /// - `Some` if the invite has been accepted by this specific client.
    /// - `None` if the invite has not been accepted
    pub fn invite_acceptance_details(&self) -> Option<InviteAcceptanceDetails> {
        self.invite_acceptance_details.clone()
    }

    /// Updates the room heroes.
    pub(crate) fn update_heroes(&mut self, heroes: Vec<RoomHero>) {
        self.summary.room_heroes = heroes;
    }

    /// The heroes for this room.
    pub fn heroes(&self) -> &[RoomHero] {
        &self.summary.room_heroes
    }

    /// The number of active members (invited + joined) in the room.
    ///
    /// The return value is saturated at `u64::MAX`.
    pub fn active_members_count(&self) -> u64 {
        self.summary.joined_member_count.saturating_add(self.summary.invited_member_count)
    }

    /// The number of invited members in the room
    pub fn invited_members_count(&self) -> u64 {
        self.summary.invited_member_count
    }

    /// The number of joined members in the room
    pub fn joined_members_count(&self) -> u64 {
        self.summary.joined_member_count
    }

    /// Get the canonical alias of this room.
    pub fn canonical_alias(&self) -> Option<&RoomAliasId> {
        self.base_info.canonical_alias.as_ref()?.as_original()?.content.alias.as_deref()
    }

    /// Get the alternative aliases of this room.
    pub fn alt_aliases(&self) -> &[OwnedRoomAliasId] {
        self.base_info
            .canonical_alias
            .as_ref()
            .and_then(|ev| ev.as_original())
            .map(|ev| ev.content.alt_aliases.as_ref())
            .unwrap_or_default()
    }

    /// Get the room ID of this room.
    pub fn room_id(&self) -> &RoomId {
        &self.room_id
    }

    /// Get the room version of this room.
    pub fn room_version(&self) -> Option<&RoomVersionId> {
        self.base_info.room_version()
    }

    /// Get the room version rules of this room, or a sensible default.
    ///
    /// Will warn (at most once) if the room create event is missing from this
    /// [`RoomInfo`] or if the room version is unsupported.
    pub fn room_version_rules_or_default(&self) -> RoomVersionRules {
        use std::sync::atomic::Ordering;

        self.base_info.room_version().and_then(|room_version| room_version.rules()).unwrap_or_else(
            || {
                if self
                    .warned_about_unknown_room_version_rules
                    .compare_exchange(false, true, Ordering::Relaxed, Ordering::Relaxed)
                    .is_ok()
                {
                    warn!("Unable to get the room version rules, defaulting to rules for room version {ROOM_VERSION_FALLBACK}");
                }

                ROOM_VERSION_RULES_FALLBACK
            },
        )
    }

    /// Get the room type of this room.
    pub fn room_type(&self) -> Option<&RoomType> {
        match self.base_info.create.as_ref()? {
            MinimalStateEvent::Original(ev) => ev.content.room_type.as_ref(),
            MinimalStateEvent::Redacted(ev) => ev.content.room_type.as_ref(),
        }
    }

    /// Get the creators of this room.
    pub fn creators(&self) -> Option<Vec<OwnedUserId>> {
        match self.base_info.create.as_ref()? {
            MinimalStateEvent::Original(ev) => Some(ev.content.creators()),
            MinimalStateEvent::Redacted(ev) => Some(ev.content.creators()),
        }
    }

    pub(super) fn guest_access(&self) -> &GuestAccess {
        match &self.base_info.guest_access {
            Some(MinimalStateEvent::Original(ev)) => &ev.content.guest_access,
            _ => &GuestAccess::Forbidden,
        }
    }

    /// Returns the history visibility for this room.
    ///
    /// Returns None if the event was never seen during sync.
    pub fn history_visibility(&self) -> Option<&HistoryVisibility> {
        match &self.base_info.history_visibility {
            Some(MinimalStateEvent::Original(ev)) => Some(&ev.content.history_visibility),
            _ => None,
        }
    }

    /// Returns the history visibility for this room, or a sensible default.
    ///
    /// Returns `Shared`, the default specified by the [spec], when the event is
    /// missing.
    ///
    /// [spec]: https://spec.matrix.org/latest/client-server-api/#server-behaviour-7
    pub fn history_visibility_or_default(&self) -> &HistoryVisibility {
        match &self.base_info.history_visibility {
            Some(MinimalStateEvent::Original(ev)) => &ev.content.history_visibility,
            _ => &HistoryVisibility::Shared,
        }
    }

    /// Return the join rule for this room, if the `m.room.join_rules` event is
    /// available.
    pub fn join_rule(&self) -> Option<&JoinRule> {
        match &self.base_info.join_rules {
            Some(MinimalStateEvent::Original(ev)) => Some(&ev.content.join_rule),
            _ => None,
        }
    }

    /// Get the name of this room.
    pub fn name(&self) -> Option<&str> {
        let name = &self.base_info.name.as_ref()?.as_original()?.content.name;
        (!name.is_empty()).then_some(name)
    }

    /// Get the content of the `m.room.create` event if any.
    pub fn create(&self) -> Option<&RoomCreateWithCreatorEventContent> {
        Some(&self.base_info.create.as_ref()?.as_original()?.content)
    }

    /// Get the content of the `m.room.tombstone` event if any.
    pub fn tombstone(&self) -> Option<&RoomTombstoneEventContent> {
        Some(&self.base_info.tombstone.as_ref()?.as_original()?.content)
    }

    /// Returns the topic for this room, if set.
    pub fn topic(&self) -> Option<&str> {
        Some(&self.base_info.topic.as_ref()?.as_original()?.content.topic)
    }

    /// Get a list of all the valid (non expired) matrixRTC memberships and
    /// associated UserId's in this room.
    ///
    /// The vector is ordered by oldest membership to newest.
    fn active_matrix_rtc_memberships(&self) -> Vec<(CallMemberStateKey, MembershipData<'_>)> {
        let mut v = self
            .base_info
            .rtc_member_events
            .iter()
            .filter_map(|(user_id, ev)| {
                ev.as_original().map(|ev| {
                    ev.content
                        .active_memberships(None)
                        .into_iter()
                        .map(move |m| (user_id.clone(), m))
                })
            })
            .flatten()
            .collect::<Vec<_>>();
        v.sort_by_key(|(_, m)| m.created_ts());
        v
    }

    /// Similar to
    /// [`matrix_rtc_memberships`](Self::active_matrix_rtc_memberships) but only
    /// returns Memberships with application "m.call" and scope "m.room".
    ///
    /// The vector is ordered by oldest membership user to newest.
    fn active_room_call_memberships(&self) -> Vec<(CallMemberStateKey, MembershipData<'_>)> {
        self.active_matrix_rtc_memberships()
            .into_iter()
            .filter(|(_user_id, m)| m.is_room_call())
            .collect()
    }

    /// Is there a non expired membership with application "m.call" and scope
    /// "m.room" in this room.
    pub fn has_active_room_call(&self) -> bool {
        !self.active_room_call_memberships().is_empty()
    }

    /// Returns a Vec of userId's that participate in the room call.
    ///
    /// matrix_rtc memberships with application "m.call" and scope "m.room" are
    /// considered. A user can occur twice if they join with two devices.
    /// convert to a set depending if the different users are required or the
    /// amount of sessions.
    ///
    /// The vector is ordered by oldest membership user to newest.
    pub fn active_room_call_participants(&self) -> Vec<OwnedUserId> {
        self.active_room_call_memberships()
            .iter()
            .map(|(call_member_state_key, _)| call_member_state_key.user_id().to_owned())
            .collect()
    }

    /// Sets the new [`LatestEventValue`].
    pub fn set_latest_event(&mut self, new_value: LatestEventValue) {
        self.latest_event_value = new_value;
    }

    /// Updates the recency stamp of this room.
    ///
    /// Please read `Self::recency_stamp` to learn more.
    pub fn update_recency_stamp(&mut self, stamp: RoomRecencyStamp) {
        self.recency_stamp = Some(stamp);
    }

    /// Returns the current pinned event ids for this room.
    pub fn pinned_event_ids(&self) -> Option<Vec<OwnedEventId>> {
        self.base_info.pinned_events.clone().map(|c| c.pinned)
    }

    /// Checks if an `EventId` is currently pinned.
    /// It avoids having to clone the whole list of event ids to check a single
    /// value.
    ///
    /// Returns `true` if the provided `event_id` is pinned, `false` otherwise.
    pub fn is_pinned_event(&self, event_id: &EventId) -> bool {
        self.base_info
            .pinned_events
            .as_ref()
            .map(|p| p.pinned.contains(&event_id.to_owned()))
            .unwrap_or_default()
    }

    /// Apply migrations to this `RoomInfo` if needed.
    ///
    /// This should be used to populate new fields with data from the state
    /// store.
    ///
    /// Returns `true` if migrations were applied and this `RoomInfo` needs to
    /// be persisted to the state store.
    #[instrument(skip_all, fields(room_id = ?self.room_id))]
    pub(crate) async fn apply_migrations(&mut self, store: Arc<DynStateStore>) -> bool {
        let mut migrated = false;

        if self.data_format_version < 1 {
            info!("Migrating room info to version 1");

            // notable_tags
            match store.get_room_account_data_event_static::<TagEventContent>(&self.room_id).await {
                // Pinned events are never in stripped state.
                Ok(Some(raw_event)) => match raw_event.deserialize() {
                    Ok(event) => {
                        self.base_info.handle_notable_tags(&event.content.tags);
                    }
                    Err(error) => {
                        warn!("Failed to deserialize room tags: {error}");
                    }
                },
                Ok(_) => {
                    // Nothing to do.
                }
                Err(error) => {
                    warn!("Failed to load room tags: {error}");
                }
            }

            // pinned_events
            match store.get_state_event_static::<RoomPinnedEventsEventContent>(&self.room_id).await
            {
                // Pinned events are never in stripped state.
                Ok(Some(RawSyncOrStrippedState::Sync(raw_event))) => {
                    if let Some(mut raw_event) =
                        RawSyncStateEventWithKeys::try_from_raw_state_event(raw_event.cast())
                    {
                        self.handle_state_event(&mut raw_event);
                    }
                }
                Ok(_) => {
                    // Nothing to do.
                }
                Err(error) => {
                    warn!("Failed to load room pinned events: {error}");
                }
            }

            self.data_format_version = 1;
            migrated = true;
        }

        migrated
    }
}

/// Type to represent a `RoomInfo::recency_stamp`.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq)]
#[serde(transparent)]
pub struct RoomRecencyStamp(u64);

impl From<u64> for RoomRecencyStamp {
    fn from(value: u64) -> Self {
        Self(value)
    }
}

impl From<RoomRecencyStamp> for u64 {
    fn from(value: RoomRecencyStamp) -> Self {
        value.0
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) enum SyncInfo {
    /// We only know the room exists and whether it is in invite / joined / left
    /// state.
    ///
    /// This is the case when we have a limited sync or only seen the room
    /// because of a request we've done, like a room creation event.
    NoState,

    /// Some states have been synced, but they might have been filtered or is
    /// stale, as it is from a room we've left.
    PartiallySynced,

    /// We have all the latest state events.
    FullySynced,
}

/// Apply a redaction to the given target `event`, given the raw redaction event
/// and the room version.
pub fn apply_redaction(
    event: &Raw<AnySyncTimelineEvent>,
    raw_redaction: &Raw<SyncRoomRedactionEvent>,
    rules: &RedactionRules,
) -> Option<Raw<AnySyncTimelineEvent>> {
    use ruma::canonical_json::{RedactedBecause, redact_in_place};

    let mut event_json = match event.deserialize_as() {
        Ok(json) => json,
        Err(e) => {
            warn!("Failed to deserialize latest event: {e}");
            return None;
        }
    };

    let redacted_because = match RedactedBecause::from_raw_event(raw_redaction) {
        Ok(rb) => rb,
        Err(e) => {
            warn!("Redaction event is not valid canonical JSON: {e}");
            return None;
        }
    };

    let redact_result = redact_in_place(&mut event_json, rules, Some(redacted_because));

    if let Err(e) = redact_result {
        warn!("Failed to redact event: {e}");
        return None;
    }

    let raw = Raw::new(&event_json).expect("CanonicalJsonObject must be serializable");
    Some(raw.cast_unchecked())
}

/// Indicates that a notable update of `RoomInfo` has been applied, and why.
///
/// A room info notable update is an update that can be interesting for other
/// parts of the code. This mechanism is used in coordination with
/// [`BaseClient::room_info_notable_update_receiver`][baseclient] (and
/// `Room::info` plus `Room::room_info_notable_update_sender`) where `RoomInfo`
/// can be observed and some of its updates can be spread to listeners.
///
/// [baseclient]: crate::BaseClient::room_info_notable_update_receiver
#[derive(Debug, Clone)]
pub struct RoomInfoNotableUpdate {
    /// The room which was updated.
    pub room_id: OwnedRoomId,

    /// The reason for this update.
    pub reasons: RoomInfoNotableUpdateReasons,
}

bitflags! {
    /// The reason why a [`RoomInfoNotableUpdate`] is emitted.
    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub struct RoomInfoNotableUpdateReasons: u8 {
        /// The recency stamp of the `Room` has changed.
        const RECENCY_STAMP = 0b0000_0001;

        /// The latest event of the `Room` has changed.
        const LATEST_EVENT = 0b0000_0010;

        /// A read receipt has changed.
        const READ_RECEIPT = 0b0000_0100;

        /// The user-controlled unread marker value has changed.
        const UNREAD_MARKER = 0b0000_1000;

        /// A membership change happened for the current user.
        const MEMBERSHIP = 0b0001_0000;

        /// The display name has changed.
        const DISPLAY_NAME = 0b0010_0000;

        /// This is a temporary hack.
        ///
        /// So here is the thing. Ideally, we DO NOT want to emit this reason. It does not
        /// makes sense. However, all notable update reasons are not clearly identified
        /// so far. Why is it a problem? The `matrix_sdk_ui::room_list_service::RoomList`
        /// is listening this stream of [`RoomInfoNotableUpdate`], and emits an update on a
        /// room item if it receives a notable reason. Because all reasons are not
        /// identified, we are likely to miss particular updates, and it can feel broken.
        /// Ultimately, we want to clearly identify all the notable update reasons, and
        /// remove this one.
        const NONE = 0b1000_0000;
    }
}

impl Default for RoomInfoNotableUpdateReasons {
    fn default() -> Self {
        Self::empty()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use assert_matches::assert_matches;
    use matrix_sdk_test::{
        async_test,
        test_json::{TAG, sync_events::PINNED_EVENTS},
    };
    use ruma::{
        assign, events::room::pinned_events::RoomPinnedEventsEventContent, owned_event_id,
        owned_mxc_uri, owned_user_id, room_id, serde::Raw,
    };
    use serde_json::json;
    use similar_asserts::assert_eq;

    use super::{BaseRoomInfo, LatestEventValue, RoomInfo, SyncInfo};
    use crate::{
        RoomDisplayName, RoomHero, RoomState, StateChanges,
        notification_settings::RoomNotificationMode,
        room::{RoomNotableTags, RoomSummary},
        store::{IntoStateStore, MemoryStore},
        sync::UnreadNotificationsCount,
    };

    #[test]
    fn test_room_info_serialization() {
        // This test exists to make sure we don't accidentally change the
        // serialized format for `RoomInfo`.

        let info = RoomInfo {
            data_format_version: 1,
            room_id: room_id!("!gda78o:server.tld").into(),
            room_state: RoomState::Invited,
            notification_counts: UnreadNotificationsCount {
                highlight_count: 1,
                notification_count: 2,
            },
            summary: RoomSummary {
                room_heroes: vec![RoomHero {
                    user_id: owned_user_id!("@somebody:example.org"),
                    display_name: None,
                    avatar_url: None,
                }],
                joined_member_count: 5,
                invited_member_count: 0,
            },
            members_synced: true,
            last_prev_batch: Some("pb".to_owned()),
            sync_info: SyncInfo::FullySynced,
            encryption_state_synced: true,
            latest_event_value: LatestEventValue::None,
            base_info: Box::new(
                assign!(BaseRoomInfo::new(), { pinned_events: Some(RoomPinnedEventsEventContent::new(vec![owned_event_id!("$a")])) }),
            ),
            read_receipts: Default::default(),
            warned_about_unknown_room_version_rules: Arc::new(false.into()),
            cached_display_name: None,
            cached_user_defined_notification_mode: None,
            recency_stamp: Some(42.into()),
            invite_acceptance_details: None,
        };

        let info_json = json!({
            "data_format_version": 1,
            "room_id": "!gda78o:server.tld",
            "room_state": "Invited",
            "notification_counts": {
                "highlight_count": 1,
                "notification_count": 2,
            },
            "summary": {
                "room_heroes": [{
                    "user_id": "@somebody:example.org",
                    "display_name": null,
                    "avatar_url": null
                }],
                "joined_member_count": 5,
                "invited_member_count": 0,
            },
            "members_synced": true,
            "last_prev_batch": "pb",
            "sync_info": "FullySynced",
            "encryption_state_synced": true,
            "latest_event_value": "None",
            "base_info": {
                "avatar": null,
                "canonical_alias": null,
                "create": null,
                "dm_targets": [],
                "encryption": null,
                "guest_access": null,
                "history_visibility": null,
                "is_marked_unread": false,
                "is_marked_unread_source": "Unstable",
                "join_rules": null,
                "max_power_level": 100,
                "name": null,
                "tombstone": null,
                "topic": null,
                "pinned_events": {
                    "pinned": ["$a"]
                },
            },
            "read_receipts": {
                "num_unread": 0,
                "num_mentions": 0,
                "num_notifications": 0,
                "latest_active": null,
                "pending": [],
            },
            "recency_stamp": 42,
        });

        assert_eq!(serde_json::to_value(info).unwrap(), info_json);
    }

    #[async_test]
    async fn test_room_info_migration_v1() {
        let store = MemoryStore::new().into_state_store();

        let room_info_json = json!({
            "room_id": "!gda78o:server.tld",
            "room_state": "Joined",
            "notification_counts": {
                "highlight_count": 1,
                "notification_count": 2,
            },
            "summary": {
                "room_heroes": [{
                    "user_id": "@somebody:example.org",
                    "display_name": null,
                    "avatar_url": null
                }],
                "joined_member_count": 5,
                "invited_member_count": 0,
            },
            "members_synced": true,
            "last_prev_batch": "pb",
            "sync_info": "FullySynced",
            "encryption_state_synced": true,
            "latest_event": {
                "event": {
                    "encryption_info": null,
                    "event": {
                        "sender": "@u:i.uk",
                    },
                },
            },
            "base_info": {
                "avatar": null,
                "canonical_alias": null,
                "create": null,
                "dm_targets": [],
                "encryption": null,
                "guest_access": null,
                "history_visibility": null,
                "join_rules": null,
                "max_power_level": 100,
                "name": null,
                "tombstone": null,
                "topic": null,
            },
            "read_receipts": {
                "num_unread": 0,
                "num_mentions": 0,
                "num_notifications": 0,
                "latest_active": null,
                "pending": []
            },
            "recency_stamp": 42,
        });
        let mut room_info: RoomInfo = serde_json::from_value(room_info_json).unwrap();

        assert_eq!(room_info.data_format_version, 0);
        assert!(room_info.base_info.notable_tags.is_empty());
        assert!(room_info.base_info.pinned_events.is_none());

        // Apply migrations with an empty store.
        assert!(room_info.apply_migrations(store.clone()).await);

        assert_eq!(room_info.data_format_version, 1);
        assert!(room_info.base_info.notable_tags.is_empty());
        assert!(room_info.base_info.pinned_events.is_none());

        // Applying migrations again has no effect.
        assert!(!room_info.apply_migrations(store.clone()).await);

        assert_eq!(room_info.data_format_version, 1);
        assert!(room_info.base_info.notable_tags.is_empty());
        assert!(room_info.base_info.pinned_events.is_none());

        // Add events to the store.
        let mut changes = StateChanges::default();

        let raw_tag_event = Raw::new(&*TAG).unwrap().cast_unchecked();
        let tag_event = raw_tag_event.deserialize().unwrap();
        changes.add_room_account_data(&room_info.room_id, tag_event, raw_tag_event);

        let raw_pinned_events_event = Raw::new(&*PINNED_EVENTS).unwrap().cast_unchecked();
        let pinned_events_event = raw_pinned_events_event.deserialize().unwrap();
        changes.add_state_event(&room_info.room_id, pinned_events_event, raw_pinned_events_event);

        store.save_changes(&changes).await.unwrap();

        // Reset to version 0 and reapply migrations.
        room_info.data_format_version = 0;
        assert!(room_info.apply_migrations(store.clone()).await);

        assert_eq!(room_info.data_format_version, 1);
        assert!(room_info.base_info.notable_tags.contains(RoomNotableTags::FAVOURITE));
        assert!(room_info.base_info.pinned_events.is_some());

        // Creating a new room info initializes it to version 1.
        let new_room_info = RoomInfo::new(room_id!("!new_room:localhost"), RoomState::Joined);
        assert_eq!(new_room_info.data_format_version, 1);
    }

    #[test]
    fn test_room_info_deserialization() {
        let info_json = json!({
            "room_id": "!gda78o:server.tld",
            "room_state": "Joined",
            "notification_counts": {
                "highlight_count": 1,
                "notification_count": 2,
            },
            "summary": {
                "room_heroes": [{
                    "user_id": "@somebody:example.org",
                    "display_name": "Somebody",
                    "avatar_url": "mxc://example.org/abc"
                }],
                "joined_member_count": 5,
                "invited_member_count": 0,
            },
            "members_synced": true,
            "last_prev_batch": "pb",
            "sync_info": "FullySynced",
            "encryption_state_synced": true,
            "base_info": {
                "avatar": null,
                "canonical_alias": null,
                "create": null,
                "dm_targets": [],
                "encryption": null,
                "guest_access": null,
                "history_visibility": null,
                "join_rules": null,
                "max_power_level": 100,
                "name": null,
                "tombstone": null,
                "topic": null,
            },
            "cached_display_name": { "Calculated": "lol" },
            "cached_user_defined_notification_mode": "Mute",
            "recency_stamp": 42,
        });

        let info: RoomInfo = serde_json::from_value(info_json).unwrap();

        assert_eq!(info.room_id, room_id!("!gda78o:server.tld"));
        assert_eq!(info.room_state, RoomState::Joined);
        assert_eq!(info.notification_counts.highlight_count, 1);
        assert_eq!(info.notification_counts.notification_count, 2);
        assert_eq!(
            info.summary.room_heroes,
            vec![RoomHero {
                user_id: owned_user_id!("@somebody:example.org"),
                display_name: Some("Somebody".to_owned()),
                avatar_url: Some(owned_mxc_uri!("mxc://example.org/abc")),
            }]
        );
        assert_eq!(info.summary.joined_member_count, 5);
        assert_eq!(info.summary.invited_member_count, 0);
        assert!(info.members_synced);
        assert_eq!(info.last_prev_batch, Some("pb".to_owned()));
        assert_eq!(info.sync_info, SyncInfo::FullySynced);
        assert!(info.encryption_state_synced);
        assert_matches!(info.latest_event_value, LatestEventValue::None);
        assert!(info.base_info.avatar.is_none());
        assert!(info.base_info.canonical_alias.is_none());
        assert!(info.base_info.create.is_none());
        assert_eq!(info.base_info.dm_targets.len(), 0);
        assert!(info.base_info.encryption.is_none());
        assert!(info.base_info.guest_access.is_none());
        assert!(info.base_info.history_visibility.is_none());
        assert!(info.base_info.join_rules.is_none());
        assert_eq!(info.base_info.max_power_level, 100);
        assert!(info.base_info.name.is_none());
        assert!(info.base_info.tombstone.is_none());
        assert!(info.base_info.topic.is_none());

        assert_eq!(
            info.cached_display_name.as_ref(),
            Some(&RoomDisplayName::Calculated("lol".to_owned())),
        );
        assert_eq!(
            info.cached_user_defined_notification_mode.as_ref(),
            Some(&RoomNotificationMode::Mute)
        );
        assert_eq!(info.recency_stamp.as_ref(), Some(&42.into()));
    }

    // Ensure we can still deserialize RoomInfos before we added things to its
    // schema
    //
    // In an ideal world, we must not change this test. Please see
    // [`test_room_info_serialization`] if you want to test a “recent” `RoomInfo`
    // deserialization.
    #[test]
    fn test_room_info_deserialization_without_optional_items() {
        // The following JSON should never change if we want to be able to read in old
        // cached state
        let info_json = json!({
            "room_id": "!gda78o:server.tld",
            "room_state": "Invited",
            "notification_counts": {
                "highlight_count": 1,
                "notification_count": 2,
            },
            "summary": {
                "room_heroes": [{
                    "user_id": "@somebody:example.org",
                    "display_name": "Somebody",
                    "avatar_url": "mxc://example.org/abc"
                }],
                "joined_member_count": 5,
                "invited_member_count": 0,
            },
            "members_synced": true,
            "last_prev_batch": "pb",
            "sync_info": "FullySynced",
            "encryption_state_synced": true,
            "base_info": {
                "avatar": null,
                "canonical_alias": null,
                "create": null,
                "dm_targets": [],
                "encryption": null,
                "guest_access": null,
                "history_visibility": null,
                "join_rules": null,
                "max_power_level": 100,
                "name": null,
                "tombstone": null,
                "topic": null,
            },
        });

        let info: RoomInfo = serde_json::from_value(info_json).unwrap();

        assert_eq!(info.room_id, room_id!("!gda78o:server.tld"));
        assert_eq!(info.room_state, RoomState::Invited);
        assert_eq!(info.notification_counts.highlight_count, 1);
        assert_eq!(info.notification_counts.notification_count, 2);
        assert_eq!(
            info.summary.room_heroes,
            vec![RoomHero {
                user_id: owned_user_id!("@somebody:example.org"),
                display_name: Some("Somebody".to_owned()),
                avatar_url: Some(owned_mxc_uri!("mxc://example.org/abc")),
            }]
        );
        assert_eq!(info.summary.joined_member_count, 5);
        assert_eq!(info.summary.invited_member_count, 0);
        assert!(info.members_synced);
        assert_eq!(info.last_prev_batch, Some("pb".to_owned()));
        assert_eq!(info.sync_info, SyncInfo::FullySynced);
        assert!(info.encryption_state_synced);
        assert!(info.base_info.avatar.is_none());
        assert!(info.base_info.canonical_alias.is_none());
        assert!(info.base_info.create.is_none());
        assert_eq!(info.base_info.dm_targets.len(), 0);
        assert!(info.base_info.encryption.is_none());
        assert!(info.base_info.guest_access.is_none());
        assert!(info.base_info.history_visibility.is_none());
        assert!(info.base_info.join_rules.is_none());
        assert_eq!(info.base_info.max_power_level, 100);
        assert!(info.base_info.name.is_none());
        assert!(info.base_info.tombstone.is_none());
        assert!(info.base_info.topic.is_none());
    }
}
