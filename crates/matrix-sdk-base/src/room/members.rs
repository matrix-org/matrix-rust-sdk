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

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    mem,
    sync::Arc,
};

use bitflags::bitflags;
use futures_util::future;
use ruma::{
    Int, MxcUri, OwnedUserId, UserId,
    events::{
        MessageLikeEventType, StateEventType,
        ignored_user_list::IgnoredUserListEventContent,
        presence::PresenceEvent,
        room::{
            member::{MembershipState, RoomMemberEventContent},
            power_levels::{PowerLevelAction, RoomPowerLevels, UserPowerLevel},
        },
    },
};
use tracing::debug;

use super::Room;
use crate::{
    MinimalRoomMemberEvent, StoreError,
    deserialized_responses::{DisplayName, MemberEvent},
    store::{Result as StoreResult, StateStoreExt, ambiguity_map::is_display_name_ambiguous},
};

impl Room {
    /// Check if the room has its members fully synced.
    ///
    /// Members might be missing if lazy member loading was enabled for the
    /// sync.
    ///
    /// Returns true if no members are missing, false otherwise.
    pub fn are_members_synced(&self) -> bool {
        self.info.read().members_synced
    }

    /// Mark this Room as holding all member information.
    ///
    /// Useful in tests if we want to persuade the Room not to sync when asked
    /// about its members.
    #[cfg(feature = "testing")]
    pub fn mark_members_synced(&self) {
        self.info.update(|info| {
            info.members_synced = true;
        });
    }

    /// Mark this Room as still missing member information.
    pub fn mark_members_missing(&self) {
        self.info.update_if(|info| {
            // notify observable subscribers only if the previous value was false
            mem::replace(&mut info.members_synced, false)
        })
    }

    /// Get the `RoomMember`s of this room that are known to the store, with the
    /// given memberships.
    pub async fn members(&self, memberships: RoomMemberships) -> StoreResult<Vec<RoomMember>> {
        let user_ids = self.store.get_user_ids(self.room_id(), memberships).await?;

        if user_ids.is_empty() {
            return Ok(Vec::new());
        }

        let member_events = self
            .store
            .get_state_events_for_keys_static::<RoomMemberEventContent, _, _>(
                self.room_id(),
                &user_ids,
            )
            .await?
            .into_iter()
            .map(|raw_event| raw_event.deserialize())
            .collect::<Result<Vec<_>, _>>()?;

        let mut profiles = self.store.get_profiles(self.room_id(), &user_ids).await?;

        let mut presences = self
            .store
            .get_presence_events(&user_ids)
            .await?
            .into_iter()
            .filter_map(|e| {
                e.deserialize().ok().map(|presence| (presence.sender.clone(), presence))
            })
            .collect::<BTreeMap<_, _>>();

        let display_names = member_events.iter().map(|e| e.display_name()).collect::<Vec<_>>();
        let room_info = self.member_room_info(&display_names).await?;

        let mut members = Vec::new();

        for event in member_events {
            let profile = profiles.remove(event.user_id());
            let presence = presences.remove(event.user_id());
            members.push(RoomMember::from_parts(event, profile, presence, &room_info))
        }

        Ok(members)
    }

    /// Returns the number of members who have joined or been invited to the
    /// room.
    pub fn active_members_count(&self) -> u64 {
        self.info.read().active_members_count()
    }

    /// Returns the number of members who have been invited to the room.
    pub fn invited_members_count(&self) -> u64 {
        self.info.read().invited_members_count()
    }

    /// Returns the number of members who have joined the room.
    pub fn joined_members_count(&self) -> u64 {
        self.info.read().joined_members_count()
    }

    /// Get the `RoomMember` with the given `user_id`.
    ///
    /// Returns `None` if the member was never part of this room, otherwise
    /// return a `RoomMember` that can be in a joined, RoomState::Invited, left,
    /// banned state.
    ///
    /// Async because it can read from storage.
    pub async fn get_member(&self, user_id: &UserId) -> StoreResult<Option<RoomMember>> {
        let event = async {
            let Some(raw_event) = self.store.get_member_event(self.room_id(), user_id).await?
            else {
                debug!(%user_id, "Member event not found in state store");
                return Ok(None);
            };

            Ok(Some(raw_event.deserialize()?))
        };
        let presence = async {
            let raw_event = self.store.get_presence_event(user_id).await?;
            Ok::<Option<PresenceEvent>, StoreError>(raw_event.and_then(|e| e.deserialize().ok()))
        };

        let profile = async { self.store.get_profile(self.room_id(), user_id).await };

        let (Some(event), presence, profile) = future::try_join3(event, presence, profile).await?
        else {
            return Ok(None);
        };

        let display_names = [event.display_name()];
        let room_info = self.member_room_info(&display_names).await?;

        Ok(Some(RoomMember::from_parts(event, profile, presence, &room_info)))
    }

    /// The current `MemberRoomInfo` for this room.
    ///
    /// Async because it can read from storage.
    async fn member_room_info<'a>(
        &self,
        display_names: &'a [DisplayName],
    ) -> StoreResult<MemberRoomInfo<'a>> {
        let max_power_level = self.max_power_level();
        let power_levels = async { Ok(self.power_levels_or_default().await) };

        let users_display_names =
            self.store.get_users_with_display_names(self.room_id(), display_names);

        let ignored_users = async {
            Ok(self
                .store
                .get_account_data_event_static::<IgnoredUserListEventContent>()
                .await?
                .map(|c| c.deserialize())
                .transpose()?
                .map(|e| e.content.ignored_users.into_keys().collect()))
        };

        let (power_levels, users_display_names, ignored_users) =
            future::try_join3(power_levels, users_display_names, ignored_users).await?;

        Ok(MemberRoomInfo {
            power_levels: power_levels.into(),
            max_power_level,
            users_display_names,
            ignored_users,
        })
    }
}

/// A member of a room.
#[derive(Clone, Debug)]
pub struct RoomMember {
    pub(crate) event: Arc<MemberEvent>,
    // The latest member event sent by the member themselves.
    // Stored in addition to the latest member event overall to get displayname
    // and avatar from, which should be ignored on events sent by others.
    pub(crate) profile: Arc<Option<MinimalRoomMemberEvent>>,
    #[allow(dead_code)]
    pub(crate) presence: Arc<Option<PresenceEvent>>,
    pub(crate) power_levels: Arc<RoomPowerLevels>,
    pub(crate) max_power_level: i64,
    pub(crate) display_name_ambiguous: bool,
    pub(crate) is_ignored: bool,
}

impl RoomMember {
    pub(crate) fn from_parts(
        event: MemberEvent,
        profile: Option<MinimalRoomMemberEvent>,
        presence: Option<PresenceEvent>,
        room_info: &MemberRoomInfo<'_>,
    ) -> Self {
        let MemberRoomInfo { power_levels, max_power_level, users_display_names, ignored_users } =
            room_info;

        let display_name = event.display_name();
        let display_name_ambiguous = users_display_names
            .get(&display_name)
            .is_some_and(|s| is_display_name_ambiguous(&display_name, s));
        let is_ignored = ignored_users.as_ref().is_some_and(|s| s.contains(event.user_id()));

        Self {
            event: event.into(),
            profile: profile.into(),
            presence: presence.into(),
            power_levels: power_levels.clone(),
            max_power_level: *max_power_level,
            display_name_ambiguous,
            is_ignored,
        }
    }

    /// Get the unique user id of this member.
    pub fn user_id(&self) -> &UserId {
        self.event.user_id()
    }

    /// Get the original member event
    pub fn event(&self) -> &Arc<MemberEvent> {
        &self.event
    }

    /// Get the display name of the member if there is one.
    pub fn display_name(&self) -> Option<&str> {
        if let Some(p) = self.profile.as_ref() {
            p.as_original().and_then(|e| e.content.displayname.as_deref())
        } else {
            self.event.original_content()?.displayname.as_deref()
        }
    }

    /// Get the name of the member.
    ///
    /// This returns either the display name or the local part of the user id if
    /// the member didn't set a display name.
    pub fn name(&self) -> &str {
        if let Some(d) = self.display_name() { d } else { self.user_id().localpart() }
    }

    /// Get the avatar url of the member, if there is one.
    pub fn avatar_url(&self) -> Option<&MxcUri> {
        if let Some(p) = self.profile.as_ref() {
            p.as_original().and_then(|e| e.content.avatar_url.as_deref())
        } else {
            self.event.original_content()?.avatar_url.as_deref()
        }
    }

    /// Get the normalized power level of this member.
    ///
    /// The normalized power level depends on the maximum power level that can
    /// be found in a certain room, positive values that are not `Infinite` are
    /// always in the range of 0-100.
    pub fn normalized_power_level(&self) -> UserPowerLevel {
        let UserPowerLevel::Int(power_level) = self.power_level() else {
            return UserPowerLevel::Infinite;
        };

        let normalized_power_level = if self.max_power_level > 0 {
            normalize_power_level(power_level, self.max_power_level)
        } else {
            power_level
        };

        UserPowerLevel::Int(normalized_power_level)
    }

    /// Get the power level of this member.
    pub fn power_level(&self) -> UserPowerLevel {
        self.power_levels.for_user(self.user_id())
    }

    /// Whether this user can ban other users based on the power levels.
    ///
    /// Same as `member.can_do(PowerLevelAction::Ban)`.
    pub fn can_ban(&self) -> bool {
        self.can_do_impl(|pls| pls.user_can_ban(self.user_id()))
    }

    /// Whether this user can invite other users based on the power levels.
    ///
    /// Same as `member.can_do(PowerLevelAction::Invite)`.
    pub fn can_invite(&self) -> bool {
        self.can_do_impl(|pls| pls.user_can_invite(self.user_id()))
    }

    /// Whether this user can kick other users based on the power levels.
    ///
    /// Same as `member.can_do(PowerLevelAction::Kick)`.
    pub fn can_kick(&self) -> bool {
        self.can_do_impl(|pls| pls.user_can_kick(self.user_id()))
    }

    /// Whether this user can redact their own events based on the power levels.
    ///
    /// Same as `member.can_do(PowerLevelAction::RedactOwn)`.
    pub fn can_redact_own(&self) -> bool {
        self.can_do_impl(|pls| pls.user_can_redact_own_event(self.user_id()))
    }

    /// Whether this user can redact events of other users based on the power
    /// levels.
    ///
    /// Same as `member.can_do(PowerLevelAction::RedactOther)`.
    pub fn can_redact_other(&self) -> bool {
        self.can_do_impl(|pls| pls.user_can_redact_event_of_other(self.user_id()))
    }

    /// Whether this user can send message events based on the power levels.
    ///
    /// Same as `member.can_do(PowerLevelAction::SendMessage(msg_type))`.
    pub fn can_send_message(&self, msg_type: MessageLikeEventType) -> bool {
        self.can_do_impl(|pls| pls.user_can_send_message(self.user_id(), msg_type))
    }

    /// Whether this user can send state events based on the power levels.
    ///
    /// Same as `member.can_do(PowerLevelAction::SendState(state_type))`.
    pub fn can_send_state(&self, state_type: StateEventType) -> bool {
        self.can_do_impl(|pls| pls.user_can_send_state(self.user_id(), state_type))
    }

    /// Whether this user can pin or unpin events based on the power levels.
    pub fn can_pin_or_unpin_event(&self) -> bool {
        self.can_send_state(StateEventType::RoomPinnedEvents)
    }

    /// Whether this user can notify everybody in the room by writing `@room` in
    /// a message.
    ///
    /// Same as `member.
    /// can_do(PowerLevelAction::TriggerNotification(NotificationPowerLevelType::Room))`.
    pub fn can_trigger_room_notification(&self) -> bool {
        self.can_do_impl(|pls| pls.user_can_trigger_room_notification(self.user_id()))
    }

    /// Whether this user can do the given action based on the power
    /// levels.
    pub fn can_do(&self, action: PowerLevelAction) -> bool {
        self.can_do_impl(|pls| pls.user_can_do(self.user_id(), action))
    }

    fn can_do_impl(&self, f: impl FnOnce(&RoomPowerLevels) -> bool) -> bool {
        f(&self.power_levels)
    }

    /// Is the name that the member uses ambiguous in the room.
    ///
    /// A name is considered to be ambiguous if at least one other member shares
    /// the same name.
    pub fn name_ambiguous(&self) -> bool {
        self.display_name_ambiguous
    }

    /// Get the membership state of this member.
    pub fn membership(&self) -> &MembershipState {
        self.event.membership()
    }

    /// Is the room member ignored by the current account user
    pub fn is_ignored(&self) -> bool {
        self.is_ignored
    }
}

// Information about the room a member is in.
pub(crate) struct MemberRoomInfo<'a> {
    pub(crate) power_levels: Arc<RoomPowerLevels>,
    pub(crate) max_power_level: i64,
    pub(crate) users_display_names: HashMap<&'a DisplayName, BTreeSet<OwnedUserId>>,
    pub(crate) ignored_users: Option<BTreeSet<OwnedUserId>>,
}

/// The kind of room member updates that just happened.
#[derive(Debug, Clone)]
pub enum RoomMembersUpdate {
    /// The whole list room members was reloaded.
    FullReload,
    /// A few members were updated, their user ids are included.
    Partial(BTreeSet<OwnedUserId>),
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

/// Scale the given `power_level` to a range between 0-100.
pub fn normalize_power_level(power_level: Int, max_power_level: i64) -> Int {
    let mut power_level = i64::from(power_level);
    power_level = (power_level * 100) / max_power_level;

    Int::try_from(power_level.clamp(0, 100))
        .expect("We clamped the normalized power level so they must fit into the Int")
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    prop_compose! {
        fn arb_int()(id in any::<i64>()) -> Int {
            id.try_into().unwrap_or_default()
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(10_000))]
        #[test]
        fn test_power_level_normalization_with_min_max_level(power_level in arb_int()) {
            let normalized = normalize_power_level(power_level, 1);
            let normalized = i64::from(normalized);

            assert!(normalized >= 0);
            assert!(normalized <= 100);
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(10_000))]
        #[test]
        fn test_power_level_normalization(power_level in arb_int(), max_level in 1i64..) {
            let normalized = normalize_power_level(power_level, max_level);
            let normalized = i64::from(normalized);

            assert!(normalized >= 0);
            assert!(normalized <= 100);
        }
    }

    #[test]
    fn test_power_level_normalization_limits() {
        let level = Int::MIN;
        let normalized = normalize_power_level(level, 1);
        let normalized = i64::from(normalized);
        assert!(normalized >= 0);
        assert!(normalized <= 100);

        let level = Int::MAX;
        let normalized = normalize_power_level(level, 1);
        let normalized = i64::from(normalized);
        assert!(normalized >= 0);
        assert!(normalized <= 100);
    }
}
