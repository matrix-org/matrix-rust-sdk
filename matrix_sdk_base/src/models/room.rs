// Copyright 2020 Damir Jelić
// Copyright 2020 The Matrix.org Foundation C.I.C.
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

use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap};
use std::convert::TryFrom;

#[cfg(feature = "messages")]
use super::message::{MessageQueue, MessageWrapper};
use super::RoomMember;

use crate::api::r0::sync::sync_events::{RoomSummary, UnreadNotificationsCount};
use crate::events::presence::PresenceEvent;
use crate::events::room::{
    aliases::AliasesEventContent,
    canonical_alias::CanonicalAliasEventContent,
    encryption::EncryptionEventContent,
    member::{MemberEventContent, MembershipChange},
    name::NameEventContent,
    power_levels::{NotificationPowerLevels, PowerLevelsEventContent},
    tombstone::TombstoneEventContent,
};

use crate::events::{
    Algorithm, AnyRoomEventStub, AnyStateEventStub, AnyStrippedStateEventStub, EventType,
    StateEventStub, StrippedStateEventStub,
};

#[cfg(feature = "messages")]
use crate::events::{room::redaction::RedactionEventStub, AnyMessageEventStub};

use crate::identifiers::{RoomAliasId, RoomId, UserId};

use crate::js_int::{Int, UInt};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
/// `RoomName` allows the calculation of a text room name.
pub struct RoomName {
    /// The displayed name of the room.
    name: Option<String>,
    /// The canonical alias of the room ex. `#room-name:example.com` and port number.
    canonical_alias: Option<RoomAliasId>,
    /// List of `RoomAliasId`s the room has been given.
    aliases: Vec<RoomAliasId>,
    /// Users which can be used to generate a room name if the room does not have
    /// one. Required if room name or canonical aliases are not set or empty.
    pub heroes: Vec<String>,
    /// Number of users whose membership status is `join`.
    /// Required if field has changed since last sync; otherwise, it may be
    /// omitted.
    pub joined_member_count: Option<UInt>,
    /// Number of users whose membership status is `invite`.
    /// Required if field has changed since last sync; otherwise, it may be
    /// omitted.
    pub invited_member_count: Option<UInt>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PowerLevels {
    /// The level required to ban a user.
    pub ban: Int,
    /// The level required to send specific event types.
    ///
    /// This is a mapping from event type to power level required.
    pub events: BTreeMap<EventType, Int>,
    /// The default level required to send message events.
    pub events_default: Int,
    /// The level required to invite a user.
    pub invite: Int,
    /// The level required to kick a user.
    pub kick: Int,
    /// The level required to redact an event.
    pub redact: Int,
    /// The default level required to send state events.
    pub state_default: Int,
    /// The default power level for every user in the room.
    pub users_default: Int,
    /// The power level requirements for specific notification types.
    ///
    /// This is a mapping from `key` to power level for that notifications key.
    pub notifications: Int,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
/// Encryption info of the room.
pub struct EncryptionInfo {
    /// The encryption algorithm that should be used to encrypt messages in the
    /// room.
    algorithm: Algorithm,
    /// How long should a session be used before it is rotated.
    rotation_period_ms: u64,
    /// The maximum amount of messages that should be encrypted using the same
    /// session.
    rotation_period_messages: u64,
}

impl EncryptionInfo {
    /// The encryption algorithm that should be used to encrypt messages in the
    /// room.
    pub fn algorithm(&self) -> &Algorithm {
        &self.algorithm
    }

    /// How long should a session be used before it is rotated.
    pub fn rotation_period(&self) -> u64 {
        self.rotation_period_ms
    }

    /// The maximum amount of messages that should be encrypted using the same
    /// session.
    pub fn rotation_period_messages(&self) -> u64 {
        self.rotation_period_messages
    }
}

impl From<&StateEventStub<EncryptionEventContent>> for EncryptionInfo {
    fn from(event: &StateEventStub<EncryptionEventContent>) -> Self {
        EncryptionInfo {
            algorithm: event.content.algorithm.clone(),
            rotation_period_ms: event
                .content
                .rotation_period_ms
                .map_or(604_800_000, Into::into),
            rotation_period_messages: event.content.rotation_period_msgs.map_or(100, Into::into),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tombstone {
    /// A server-defined message.
    body: String,
    /// The room that is now active.
    replacement: RoomId,
}

#[derive(Debug, PartialEq, Eq)]
enum MemberDirection {
    Entering,
    Exiting,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
/// A Matrix room.
pub struct Room {
    /// The unique id of the room.
    pub room_id: RoomId,
    /// The name of the room, clients use this to represent a room.
    pub room_name: RoomName,
    /// The mxid of our own user.
    pub own_user_id: UserId,
    /// The mxid of the room creator.
    pub creator: Option<UserId>,
    // TODO: Track banned members, e.g. for /unban support?
    /// The map of invited room members.
    pub invited_members: HashMap<UserId, RoomMember>,
    /// The map of joined room members.
    pub joined_members: HashMap<UserId, RoomMember>,
    /// A queue of messages, holds no more than 10 of the most recent messages.
    ///
    /// This is helpful when using a `StateStore` to avoid multiple requests
    /// to the server for messages.
    #[cfg(feature = "messages")]
    #[cfg_attr(docsrs, doc(cfg(feature = "messages")))]
    #[serde(with = "super::message::ser_deser")]
    pub messages: MessageQueue,
    /// A list of users that are currently typing.
    pub typing_users: Vec<UserId>,
    /// The power level requirements for specific actions in this room
    pub power_levels: Option<PowerLevels>,
    /// Optional encryption info, will be `Some` if the room is encrypted.
    pub encrypted: Option<EncryptionInfo>,
    /// Number of unread notifications with highlight flag set.
    pub unread_highlight: Option<UInt>,
    /// Number of unread notifications.
    pub unread_notifications: Option<UInt>,
    /// The tombstone state of this room.
    pub tombstone: Option<Tombstone>,
    /// The map of disambiguated display names for users who have the same display name
    disambiguated_display_names: HashMap<UserId, String>,
}

impl RoomName {
    pub fn push_alias(&mut self, alias: RoomAliasId) -> bool {
        self.aliases.push(alias);
        true
    }

    pub fn set_canonical(&mut self, alias: RoomAliasId) -> bool {
        self.canonical_alias = Some(alias);
        true
    }

    pub fn set_name(&mut self, name: &str) -> bool {
        self.name = Some(name.to_string());
        true
    }

    /// Calculate the canonical display name of a room, taking into account its name, aliases and
    /// members.
    ///
    /// The display name is calculated according to [this algorithm][spec].
    ///
    /// [spec]:
    /// <https://matrix.org/docs/spec/client_server/latest#calculating-the-display-name-for-a-room>
    pub fn calculate_name(
        &self,
        own_user_id: &UserId,
        invited_members: &HashMap<UserId, RoomMember>,
        joined_members: &HashMap<UserId, RoomMember>,
    ) -> String {
        if let Some(name) = &self.name {
            let name = name.trim();
            name.to_string()
        } else if let Some(alias) = &self.canonical_alias {
            let alias = alias.alias().trim();
            alias.to_string()
        } else if !self.aliases.is_empty() && !self.aliases[0].alias().is_empty() {
            self.aliases[0].alias().trim().to_string()
        } else {
            let joined = self.joined_member_count.unwrap_or(UInt::MIN);
            let invited = self.invited_member_count.unwrap_or(UInt::MIN);
            let heroes = UInt::new(self.heroes.len() as u64).unwrap();
            let one = UInt::new(1).unwrap();

            let invited_joined = if invited + joined == UInt::MIN {
                UInt::MIN
            } else {
                invited + joined - one
            };

            let members = joined_members.values().chain(invited_members.values());

            // TODO: This should use `self.heroes` but it is always empty??
            if heroes >= invited_joined {
                let mut names = members
                    .filter(|m| m.user_id != *own_user_id)
                    .take(3)
                    .map(|mem| {
                        mem.display_name
                            .clone()
                            .unwrap_or_else(|| mem.user_id.localpart().to_string())
                    })
                    .collect::<Vec<String>>();
                // stabilize ordering
                names.sort();
                names.join(", ")
            } else if heroes < invited_joined && invited + joined > one {
                let mut names = members
                    .filter(|m| m.user_id != *own_user_id)
                    .take(3)
                    .map(|mem| {
                        mem.display_name
                            .clone()
                            .unwrap_or_else(|| mem.user_id.localpart().to_string())
                    })
                    .collect::<Vec<String>>();
                names.sort();

                // TODO: What length does the spec want us to use here and in the `else`?
                format!("{}, and {} others", names.join(", "), (joined + invited))
            } else {
                "Empty room".to_string()
            }
        }
    }
}

impl Room {
    /// Create a new room.
    ///
    /// # Arguments
    ///
    /// * `room_id` - The unique id of the room.
    ///
    /// * `own_user_id` - The mxid of our own user.
    pub fn new(room_id: &RoomId, own_user_id: &UserId) -> Self {
        Room {
            room_id: room_id.clone(),
            room_name: RoomName::default(),
            own_user_id: own_user_id.clone(),
            creator: None,
            invited_members: HashMap::new(),
            joined_members: HashMap::new(),
            #[cfg(feature = "messages")]
            messages: MessageQueue::new(),
            typing_users: Vec::new(),
            power_levels: None,
            encrypted: None,
            unread_highlight: None,
            unread_notifications: None,
            tombstone: None,
            disambiguated_display_names: HashMap::new(),
        }
    }

    /// Return the display name of the room.
    pub fn display_name(&self) -> String {
        self.room_name.calculate_name(
            &self.own_user_id,
            &self.invited_members,
            &self.joined_members,
        )
    }

    /// Is the room a encrypted room.
    pub fn is_encrypted(&self) -> bool {
        self.encrypted.is_some()
    }

    /// Get the encryption info if any of the room.
    ///
    /// Returns None if the room is not encrypted.
    pub fn encryption_info(&self) -> Option<&EncryptionInfo> {
        self.encrypted.as_ref()
    }

    /// Get the disambiguated display name for a member of this room.
    ///
    /// If a member has no display name set, returns the MXID as a fallback. Additionally, we
    /// return the MXID even if there is no such member in the room.
    ///
    /// When displaying a room member's display name, clients *must* use this method to obtain the
    /// name instead of displaying the `RoomMember::display_name` directly. This is because
    /// multiple members can share the same display name in which case the display name has to be
    /// disambiguated.
    pub fn member_display_name<'a>(&'a self, id: &'a UserId) -> Cow<'a, str> {
        let disambiguated_name = self
            .disambiguated_display_names
            .get(id)
            .map(|s| s.as_str().into());

        if let Some(name) = disambiguated_name {
            // The display name of the member is non-unique so we return a disambiguated version.
            name
        } else if let Some(member) = self
            .joined_members
            .get(id)
            .or_else(|| self.invited_members.get(id))
        {
            // The display name of the member is unique so we can return it directly if it is set.
            // If not, we return his MXID.
            member.name().into()
        } else {
            // There is no member with the requested MXID in the room. We still return the MXID.
            id.as_str().into()
        }
    }

    fn add_member(&mut self, event: &StateEventStub<MemberEventContent>) -> bool {
        let new_member = RoomMember::new(event, &self.room_id);

        if self.joined_members.contains_key(&new_member.user_id)
            || self.invited_members.contains_key(&new_member.user_id)
        {
            return false;
        }

        match event.membership_change() {
            MembershipChange::Joined => self
                .joined_members
                .insert(new_member.user_id.clone(), new_member.clone()),
            MembershipChange::Invited => self
                .invited_members
                .insert(new_member.user_id.clone(), new_member.clone()),
            _ => {
                panic!("Room::add_member called on an event that is neither a join nor an invite.")
            }
        };

        // Perform display name disambiguations, if necessary.
        let disambiguations = self.disambiguation_updates(&new_member, MemberDirection::Entering);
        for (id, name) in disambiguations.into_iter() {
            match name {
                None => self.disambiguated_display_names.remove(&id),
                Some(name) => self.disambiguated_display_names.insert(id, name),
            };
        }

        true
    }

    /// Process the member event of a leaving user.
    ///
    /// Returns true if this made a change to the room's state, false otherwise.
    fn remove_member(&mut self, event: &StateEventStub<MemberEventContent>) -> bool {
        let leaving_member = RoomMember::new(event, &self.room_id);

        // Perform display name disambiguations, if necessary.
        let disambiguations =
            self.disambiguation_updates(&leaving_member, MemberDirection::Exiting);
        for (id, name) in disambiguations.into_iter() {
            match name {
                None => self.disambiguated_display_names.remove(&id),
                Some(name) => self.disambiguated_display_names.insert(id, name),
            };
        }

        if self.joined_members.contains_key(&leaving_member.user_id) {
            self.joined_members.remove(&leaving_member.user_id);
            true
        } else if self.invited_members.contains_key(&leaving_member.user_id) {
            self.invited_members.remove(&leaving_member.user_id);
            true
        } else {
            false
        }
    }

    /// Given a room `member`, return the list of members which have the same display name.
    ///
    /// The `inclusive` parameter controls whether the passed member should be included in the
    /// list or not.
    fn shares_displayname_with(&self, member: &RoomMember, inclusive: bool) -> Vec<UserId> {
        let members = self
            .invited_members
            .iter()
            .chain(self.joined_members.iter());

        // Find all other users that share the same display name as the joining user.
        members
            .filter(|(_, existing_member)| {
                member
                    .display_name
                    .as_ref()
                    .and_then(|new_member_name| {
                        existing_member
                            .display_name
                            .as_ref()
                            .map(|existing_member_name| new_member_name == existing_member_name)
                    })
                    .unwrap_or(false)
            })
            // If not an inclusive search, do not consider the member for which we are disambiguating.
            .filter(|(id, _)| inclusive || **id != member.user_id)
            .map(|(id, _)| id)
            .cloned()
            .collect()
    }

    /// Given a room member, generate a map of all display name disambiguations which are necessary
    /// in order to make that member's display name unique.
    ///
    /// The `inclusive` parameter controls whether or not the member for which we are
    /// disambiguating should be considered a current member of the room.
    ///
    /// Returns a map from MXID to disambiguated name.
    fn member_disambiguations(
        &self,
        member: &RoomMember,
        inclusive: bool,
    ) -> HashMap<UserId, String> {
        let users_with_same_name = self.shares_displayname_with(member, inclusive);
        let disambiguate_with = |members: Vec<UserId>, f: fn(&RoomMember) -> String| {
            members
                .into_iter()
                .filter_map(|id| {
                    self.joined_members
                        .get(&id)
                        .or_else(|| self.invited_members.get(&id))
                        .map(f)
                        .map(|m| (id, m))
                })
                .collect::<HashMap<UserId, String>>()
        };

        match users_with_same_name.len() {
            0 => HashMap::new(),
            1 => disambiguate_with(users_with_same_name, |m: &RoomMember| m.name()),
            _ => disambiguate_with(users_with_same_name, |m: &RoomMember| m.unique_name()),
        }
    }

    /// Calculate disambiguation updates needed when a room member either enters or exits.
    fn disambiguation_updates(
        &self,
        member: &RoomMember,
        when: MemberDirection,
    ) -> HashMap<UserId, Option<String>> {
        let before;
        let after;

        match when {
            MemberDirection::Entering => {
                before = self.member_disambiguations(member, false);
                after = self.member_disambiguations(member, true);
            }
            MemberDirection::Exiting => {
                before = self.member_disambiguations(member, true);
                after = self.member_disambiguations(member, false);
            }
        }

        let mut res = before;
        res.extend(after.clone());

        res.into_iter()
            .map(|(user_id, name)| {
                if !after.contains_key(&user_id) {
                    (user_id, None)
                } else {
                    (user_id, Some(name))
                }
            })
            .collect()
    }

    /// Add to the list of `RoomAliasId`s.
    fn push_room_alias(&mut self, alias: &RoomAliasId) -> bool {
        self.room_name.push_alias(alias.clone());
        true
    }

    /// RoomAliasId is `#alias:hostname` and `port`
    fn canonical_alias(&mut self, alias: &RoomAliasId) -> bool {
        self.room_name.set_canonical(alias.clone());
        true
    }

    fn set_room_name(&mut self, name: &str) -> bool {
        self.room_name.set_name(name);
        true
    }

    fn set_room_power_level(&mut self, event: &StateEventStub<PowerLevelsEventContent>) -> bool {
        let PowerLevelsEventContent {
            ban,
            events,
            events_default,
            invite,
            kick,
            redact,
            state_default,
            users_default,
            notifications: NotificationPowerLevels { room },
            ..
        } = &event.content;

        let power = PowerLevels {
            ban: *ban,
            events: events.clone(),
            events_default: *events_default,
            invite: *invite,
            kick: *kick,
            redact: *redact,
            state_default: *state_default,
            users_default: *users_default,
            notifications: *room,
        };
        self.power_levels = Some(power);
        true
    }

    pub(crate) fn set_room_summary(&mut self, summary: &RoomSummary) {
        let RoomSummary {
            heroes,
            joined_member_count,
            invited_member_count,
        } = summary;
        self.room_name.heroes = heroes.clone();
        self.room_name.invited_member_count = *invited_member_count;
        self.room_name.joined_member_count = *joined_member_count;
    }

    pub(crate) fn set_unread_notice_count(&mut self, notifications: &UnreadNotificationsCount) {
        self.unread_highlight = notifications.highlight_count;
        self.unread_notifications = notifications.notification_count;
    }

    /// Handle a room.member updating the room state if necessary.
    ///
    /// Returns true if the joined member list changed, false otherwise.
    pub fn handle_membership(&mut self, event: &StateEventStub<MemberEventContent>) -> bool {
        use MembershipChange::*;

        // TODO: This would not be handled correctly as all the MemberEvents have the `prev_content`
        // inside of `unsigned` field.
        match event.membership_change() {
            Invited | Joined => self.add_member(event),
            Kicked | Banned | KickedAndBanned | InvitationRejected | Left => {
                self.remove_member(event)
            }
            ProfileChanged { .. } => {
                let user_id = if let Ok(id) = UserId::try_from(event.state_key.as_str()) {
                    id
                } else {
                    return false;
                };

                if let Some(member) = self.joined_members.get_mut(&user_id) {
                    member.update_profile(event)
                } else {
                    false
                }
            }

            // Not interested in other events.
            _ => false,
        }
    }

    /// Handle a room.message event and update the `MessageQueue` if necessary.
    ///
    /// Returns true if `MessageQueue` was added to.
    #[cfg(feature = "messages")]
    #[cfg_attr(docsrs, doc(cfg(feature = "messages")))]
    pub fn handle_message(&mut self, event: &AnyMessageEventStub) -> bool {
        self.messages.push(event.clone())
    }

    /// Handle a room.redaction event and update the `MessageQueue` if necessary.
    ///
    /// Returns true if `MessageQueue` was updated.
    #[cfg(feature = "messages")]
    #[cfg_attr(docsrs, doc(cfg(feature = "messages")))]
    pub fn handle_redaction(&mut self, event: &RedactionEventStub) -> bool {
        if let Some(msg) = self
            .messages
            .iter_mut()
            .find(|msg| &event.redacts == msg.event_id())
        {
            *msg = MessageWrapper(AnyMessageEventStub::RoomRedaction(event.clone()));
            true
        } else {
            false
        }
    }

    /// Handle a room.aliases event, updating the room state if necessary.
    ///
    /// Returns true if the room name changed, false otherwise.
    pub fn handle_room_aliases(&mut self, event: &StateEventStub<AliasesEventContent>) -> bool {
        match event.content.aliases.as_slice() {
            [alias] => self.push_room_alias(alias),
            [alias, ..] => self.push_room_alias(alias),
            _ => false,
        }
    }

    /// Handle a room.canonical_alias event, updating the room state if necessary.
    ///
    /// Returns true if the room name changed, false otherwise.
    pub fn handle_canonical(&mut self, event: &StateEventStub<CanonicalAliasEventContent>) -> bool {
        match &event.content.alias {
            Some(name) => self.canonical_alias(&name),
            _ => false,
        }
    }

    /// Handle a room.name event, updating the room state if necessary.
    ///
    /// Returns true if the room name changed, false otherwise.
    pub fn handle_room_name(&mut self, event: &StateEventStub<NameEventContent>) -> bool {
        match event.content.name() {
            Some(name) => self.set_room_name(name),
            _ => false,
        }
    }

    /// Handle a room.name event, updating the room state if necessary.
    ///
    /// Returns true if the room name changed, false otherwise.
    pub fn handle_stripped_room_name(
        &mut self,
        event: &StrippedStateEventStub<NameEventContent>,
    ) -> bool {
        match event.content.name() {
            Some(name) => self.set_room_name(name),
            _ => false,
        }
    }

    /// Handle a room.power_levels event, updating the room state if necessary.
    ///
    /// Returns true if the room name changed, false otherwise.
    pub fn handle_power_level(&mut self, event: &StateEventStub<PowerLevelsEventContent>) -> bool {
        // NOTE: this is always true, we assume that if we get an event their is an update.
        let mut updated = self.set_room_power_level(event);

        let mut max_power = event.content.users_default;
        for power in event.content.users.values() {
            max_power = *power.max(&max_power);
        }

        for user in event.content.users.keys() {
            if let Some(member) = self.joined_members.get_mut(user) {
                if member.update_power(event, max_power) {
                    updated = true;
                }
            }
        }
        updated
    }

    fn handle_tombstone(&mut self, event: &StateEventStub<TombstoneEventContent>) -> bool {
        self.tombstone = Some(Tombstone {
            body: event.content.body.clone(),
            replacement: event.content.replacement_room.clone(),
        });
        true
    }

    fn handle_encryption_event(&mut self, event: &StateEventStub<EncryptionEventContent>) -> bool {
        self.encrypted = Some(event.into());
        true
    }

    /// Receive a timeline event for this room and update the room state.
    ///
    /// Returns true if the joined member list changed, false otherwise.
    ///
    /// # Arguments
    ///
    /// * `event` - The event of the room.
    pub fn receive_timeline_event(&mut self, event: &AnyRoomEventStub) -> bool {
        match &event {
            AnyRoomEventStub::State(event) => match &event {
                // update to the current members of the room
                AnyStateEventStub::RoomMember(event) => self.handle_membership(&event),
                // finds all events related to the name of the room for later use
                AnyStateEventStub::RoomName(event) => self.handle_room_name(&event),
                AnyStateEventStub::RoomCanonicalAlias(event) => self.handle_canonical(&event),
                AnyStateEventStub::RoomAliases(event) => self.handle_room_aliases(&event),
                // power levels of the room members
                AnyStateEventStub::RoomPowerLevels(event) => self.handle_power_level(&event),
                AnyStateEventStub::RoomTombstone(event) => self.handle_tombstone(&event),
                AnyStateEventStub::RoomEncryption(event) => self.handle_encryption_event(&event),
                _ => false,
            },
            AnyRoomEventStub::Message(event) => match &event {
                #[cfg(feature = "messages")]
                // We ignore this variants event because `handle_message` takes the enum
                // to store AnyMessageEventStub events in the `MessageQueue`.
                AnyMessageEventStub::RoomMessage(_) => self.handle_message(&event),
                #[cfg(feature = "messages")]
                AnyMessageEventStub::RoomRedaction(event) => self.handle_redaction(&event),
                _ => false,
            },
        }
    }

    /// Receive a state event for this room and update the room state.
    ///
    /// Returns true if the state of the `Room` has changed, false otherwise.
    ///
    /// # Arguments
    ///
    /// * `event` - The event of the room.
    pub fn receive_state_event(&mut self, event: &AnyStateEventStub) -> bool {
        match event {
            // update to the current members of the room
            AnyStateEventStub::RoomMember(member) => self.handle_membership(member),
            // finds all events related to the name of the room for later use
            AnyStateEventStub::RoomName(name) => self.handle_room_name(name),
            AnyStateEventStub::RoomCanonicalAlias(c_alias) => self.handle_canonical(c_alias),
            AnyStateEventStub::RoomAliases(alias) => self.handle_room_aliases(alias),
            // power levels of the room members
            AnyStateEventStub::RoomPowerLevels(power) => self.handle_power_level(power),
            AnyStateEventStub::RoomTombstone(tomb) => self.handle_tombstone(tomb),
            AnyStateEventStub::RoomEncryption(encrypt) => self.handle_encryption_event(encrypt),
            _ => false,
        }
    }

    /// Receive a stripped state event for this room and update the room state.
    ///
    /// Returns true if the state of the `Room` has changed, false otherwise.
    ///
    /// # Arguments
    ///
    /// * `event` - The `AnyStrippedStateEvent` sent by the server for invited but not
    /// joined rooms.
    pub fn receive_stripped_state_event(&mut self, event: &AnyStrippedStateEventStub) -> bool {
        match &event {
            AnyStrippedStateEventStub::RoomName(event) => self.handle_stripped_room_name(event),
            _ => false,
        }
    }

    /// Receive a presence event from an `IncomingResponse` and updates the client state.
    ///
    /// This will only update the user if found in the current room looped through
    /// by `Client::sync`.
    /// Returns true if the specific users presence has changed, false otherwise.
    ///
    /// # Arguments
    ///
    /// * `event` - The presence event for a specified room member.
    pub fn receive_presence_event(&mut self, event: &PresenceEvent) -> bool {
        if let Some(member) = self.joined_members.get_mut(&event.sender) {
            if member.did_update_presence(event) {
                false
            } else {
                member.update_presence(event);
                true
            }
        } else {
            // this is probably an error as we have a `PresenceEvent` for a user
            // we don't know about
            false
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::events::{room::encryption::EncryptionEventContent, UnsignedData};
    use crate::identifiers::{EventId, UserId};
    use crate::{BaseClient, Session};
    use matrix_sdk_test::{async_test, sync_response, EventBuilder, EventsJson, SyncResponseFile};

    use std::time::SystemTime;

    #[cfg(target_arch = "wasm32")]
    use wasm_bindgen_test::*;

    use std::convert::TryFrom;
    use std::ops::Deref;

    async fn get_client() -> BaseClient {
        let session = Session {
            access_token: "1234".to_owned(),
            user_id: UserId::try_from("@example:localhost").unwrap(),
            device_id: "DEVICEID".to_owned(),
        };
        let client = BaseClient::new().unwrap();
        client.restore_login(session).await.unwrap();
        client
    }

    fn get_room_id() -> RoomId {
        RoomId::try_from("!SVkFJHzfwvuaIEawgC:localhost").unwrap()
    }

    #[async_test]
    async fn user_presence() {
        let client = get_client().await;

        let mut response = sync_response(SyncResponseFile::Default);

        client.receive_sync_response(&mut response).await.unwrap();

        let rooms_lock = &client.joined_rooms();
        let rooms = rooms_lock.read().await;
        let room = &rooms
            .get(&RoomId::try_from("!SVkFJHzfwvuaIEawgC:localhost").unwrap())
            .unwrap()
            .read()
            .await;

        assert_eq!(1, room.joined_members.len());
        assert!(room.deref().power_levels.is_some())
    }

    #[async_test]
    async fn test_member_display_name() {
        // Initialize

        let client = get_client().await;
        let room_id = get_room_id();
        let user_id1 = UserId::try_from("@example:localhost").unwrap();
        let user_id2 = UserId::try_from("@example2:localhost").unwrap();
        let user_id3 = UserId::try_from("@example3:localhost").unwrap();

        let member2_join_event = serde_json::json!({
            "content": {
                "avatar_url": null,
                "displayname": "example",
                "membership": "join"
            },
            "event_id": "$16345217l517tabbz:localhost",
            "membership": "join",
            "origin_server_ts": 1455123234,
            "sender": format!("{}", user_id2),
            "state_key": format!("{}", user_id2),
            "type": "m.room.member",
            "prev_content": {
                "avatar_url": null,
                "displayname": "example",
                "membership": "invite"
            },
            "unsigned": {
                "age": 1989321234,
                "replaces_state": "$1622a2311315tkjoA:localhost"
            }
        });

        let member2_leave_event = serde_json::json!({
            "content": {
                "avatar_url": null,
                "displayname": "example",
                "membership": "leave"
            },
            "event_id": "$263452333l22bggbz:localhost",
            "membership": "leave",
            "origin_server_ts": 1455123228,
            "sender": format!("{}", user_id2),
            "state_key": format!("{}", user_id2),
            "type": "m.room.member",
            "prev_content": {
                "avatar_url": null,
                "displayname": "example",
                "membership": "join"
            },
            "unsigned": {
                "age": 1989321221,
                "replaces_state": "$16345217l517tabbz:localhost"
            }
        });

        let member3_join_event = serde_json::json!({
            "content": {
                "avatar_url": null,
                "displayname": "example",
                "membership": "join"
            },
            "event_id": "$16845287981ktggba:localhost",
            "membership": "join",
            "origin_server_ts": 1455123244,
            "sender": format!("{}", user_id3),
            "state_key": format!("{}", user_id3),
            "type": "m.room.member",
            "prev_content": {
                "avatar_url": null,
                "displayname": "example",
                "membership": "invite"
            },
            "unsigned": {
                "age": 1989321254,
                "replaces_state": "$1622l2323445kabrA:localhost"
            }
        });

        let member3_leave_event = serde_json::json!({
            "content": {
                "avatar_url": null,
                "displayname": "example",
                "membership": "leave"
            },
            "event_id": "$11121987981abfgr:localhost",
            "membership": "leave",
            "origin_server_ts": 1455123230,
            "sender": format!("{}", user_id3),
            "state_key": format!("{}", user_id3),
            "type": "m.room.member",
            "prev_content": {
                "avatar_url": null,
                "displayname": "example",
                "membership": "join"
            },
            "unsigned": {
                "age": 1989321244,
                "replaces_state": "$16845287981ktggba:localhost"
            }
        });

        let mut event_builder = EventBuilder::new();

        let mut member1_join_sync_response = event_builder
            .add_room_event(EventsJson::Member)
            .build_sync_response();

        let mut member2_join_sync_response = event_builder
            .add_custom_joined_event(&room_id, member2_join_event)
            .build_sync_response();

        let mut member3_join_sync_response = event_builder
            .add_custom_joined_event(&room_id, member3_join_event)
            .build_sync_response();

        let mut member2_leave_sync_response = event_builder
            .add_custom_joined_event(&room_id, member2_leave_event)
            .build_sync_response();

        let mut member3_leave_sync_response = event_builder
            .add_custom_joined_event(&room_id, member3_leave_event)
            .build_sync_response();

        // First member with display name "example" joins
        client
            .receive_sync_response(&mut member1_join_sync_response)
            .await
            .unwrap();

        // First member's disambiguated display name is "example"
        {
            let room = client.get_joined_room(&room_id).await.unwrap();
            let room = room.read().await;
            let display_name1 = room.member_display_name(&user_id1);

            assert_eq!("example", display_name1);
        }

        // Second and third member with display name "example" join
        client
            .receive_sync_response(&mut member2_join_sync_response)
            .await
            .unwrap();
        client
            .receive_sync_response(&mut member3_join_sync_response)
            .await
            .unwrap();

        // All of their display names are now disambiguated with MXIDs
        {
            let room = client.get_joined_room(&room_id).await.unwrap();
            let room = room.read().await;
            let display_name1 = room.member_display_name(&user_id1);
            let display_name2 = room.member_display_name(&user_id2);
            let display_name3 = room.member_display_name(&user_id3);

            assert_eq!(format!("example ({})", user_id1), display_name1);
            assert_eq!(format!("example ({})", user_id2), display_name2);
            assert_eq!(format!("example ({})", user_id3), display_name3);
        }

        // Second and third member leave. The first's display name is now just "example" again.
        client
            .receive_sync_response(&mut member2_leave_sync_response)
            .await
            .unwrap();
        client
            .receive_sync_response(&mut member3_leave_sync_response)
            .await
            .unwrap();

        {
            let room = client.get_joined_room(&room_id).await.unwrap();
            let room = room.read().await;

            let display_name1 = room.member_display_name(&user_id1);

            assert_eq!("example", display_name1);
        }
    }

    #[async_test]
    async fn room_events() {
        let client = get_client().await;
        let room_id = get_room_id();
        let user_id = UserId::try_from("@example:localhost").unwrap();

        let mut response = EventBuilder::default()
            .add_state_event(EventsJson::Member)
            .add_state_event(EventsJson::PowerLevels)
            .build_sync_response();

        client.receive_sync_response(&mut response).await.unwrap();

        let room = client.get_joined_room(&room_id).await.unwrap();
        let room = room.read().await;

        assert_eq!(room.joined_members.len(), 1);
        assert!(room.power_levels.is_some());
        assert_eq!(
            room.power_levels.as_ref().unwrap().kick,
            crate::js_int::Int::new(50).unwrap()
        );
        let admin = room.joined_members.get(&user_id).unwrap();
        assert_eq!(
            admin.power_level.unwrap(),
            crate::js_int::Int::new(100).unwrap()
        );
    }

    #[async_test]
    async fn calculate_aliases() {
        let client = get_client().await;

        let room_id = get_room_id();

        let mut response = EventBuilder::default()
            .add_state_event(EventsJson::Aliases)
            .build_sync_response();

        client.receive_sync_response(&mut response).await.unwrap();

        let room = client.get_joined_room(&room_id).await.unwrap();
        let room = room.read().await;

        assert_eq!("tutorial", room.display_name());
    }

    #[async_test]
    async fn calculate_alias() {
        let client = get_client().await;

        let room_id = get_room_id();

        let mut response = EventBuilder::default()
            .add_state_event(EventsJson::Alias)
            .build_sync_response();

        client.receive_sync_response(&mut response).await.unwrap();

        let room = client.get_joined_room(&room_id).await.unwrap();
        let room = room.read().await;

        assert_eq!("tutorial", room.display_name());
    }

    #[async_test]
    async fn calculate_name() {
        let client = get_client().await;

        let room_id = get_room_id();

        let mut response = EventBuilder::default()
            .add_state_event(EventsJson::Name)
            .build_sync_response();

        client.receive_sync_response(&mut response).await.unwrap();

        let room = client.get_joined_room(&room_id).await.unwrap();
        let room = room.read().await;

        assert_eq!("room name", room.display_name());
    }

    #[async_test]
    async fn calculate_room_names_from_summary() {
        let mut response = sync_response(SyncResponseFile::DefaultWithSummary);

        let session = Session {
            access_token: "1234".to_owned(),
            user_id: UserId::try_from("@example:localhost").unwrap(),
            device_id: "DEVICEID".to_owned(),
        };
        let client = BaseClient::new().unwrap();
        client.restore_login(session).await.unwrap();
        client.receive_sync_response(&mut response).await.unwrap();

        let mut room_names = vec![];
        for room in client.joined_rooms().read().await.values() {
            room_names.push(room.read().await.display_name())
        }

        assert_eq!(vec!["example2"], room_names);
    }

    #[async_test]
    #[cfg(not(target_arch = "wasm32"))]
    async fn encryption_info_test() {
        let room_id = get_room_id();

        let mut response = sync_response(SyncResponseFile::DefaultWithSummary);
        let user_id = UserId::try_from("@example:localhost").unwrap();

        let session = Session {
            access_token: "1234".to_owned(),
            user_id: user_id.clone(),
            device_id: "DEVICEID".to_owned(),
        };
        let client = BaseClient::new().unwrap();
        client.restore_login(session).await.unwrap();
        client.receive_sync_response(&mut response).await.unwrap();

        let event = StateEventStub {
            event_id: EventId::try_from("$h29iv0s8:example.com").unwrap(),
            origin_server_ts: SystemTime::now(),
            sender: user_id,
            state_key: "".into(),
            unsigned: UnsignedData::default(),
            content: EncryptionEventContent {
                algorithm: Algorithm::MegolmV1AesSha2,
                rotation_period_ms: Some(100_000u32.into()),
                rotation_period_msgs: Some(100u32.into()),
            },
            prev_content: None,
        };

        let room = client.get_joined_room(&room_id).await.unwrap();

        assert!(!room.read().await.is_encrypted());
        room.write().await.handle_encryption_event(&event);
        assert!(room.read().await.is_encrypted());

        let room_lock = room.read().await;
        let encryption_info = room_lock.encryption_info().unwrap();

        assert_eq!(encryption_info.algorithm(), &Algorithm::MegolmV1AesSha2);
        assert_eq!(encryption_info.rotation_period(), 100_000);
        assert_eq!(encryption_info.rotation_period_messages(), 100);
    }
}
