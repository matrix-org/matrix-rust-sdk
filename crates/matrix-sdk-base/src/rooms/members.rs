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

use std::sync::Arc;

use ruma::{
    events::{
        presence::PresenceEvent,
        room::{
            member::MembershipState,
            power_levels::{PowerLevelAction, RoomPowerLevels, SyncRoomPowerLevelsEvent},
        },
        MessageLikeEventType, StateEventType,
    },
    MxcUri, UserId,
};

use crate::{deserialized_responses::MemberEvent, MinimalRoomMemberEvent};

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
    pub(crate) power_levels: Arc<Option<SyncRoomPowerLevelsEvent>>,
    pub(crate) max_power_level: i64,
    pub(crate) is_room_creator: bool,
    pub(crate) display_name_ambiguous: bool,
    pub(crate) is_ignored: bool,
}

impl RoomMember {
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
        if let Some(d) = self.display_name() {
            d
        } else {
            self.user_id().localpart()
        }
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
    /// be found in a certain room, it's always in the range of 0-100.
    pub fn normalized_power_level(&self) -> i64 {
        if self.max_power_level > 0 {
            (self.power_level() * 100) / self.max_power_level
        } else {
            self.power_level()
        }
    }

    /// Get the power level of this member.
    pub fn power_level(&self) -> i64 {
        (*self.power_levels)
            .as_ref()
            .map(|e| e.power_levels().for_user(self.user_id()).into())
            .unwrap_or_else(|| if self.is_room_creator { 100 } else { 0 })
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

    /// Whether this user can redact events based on the power levels.
    ///
    /// Same as `member.can_do(PowerLevelAction::Redact)`.
    pub fn can_redact(&self) -> bool {
        self.can_do_impl(|pls| pls.user_can_redact(self.user_id()))
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

    fn can_do_impl(&self, f: impl FnOnce(RoomPowerLevels) -> bool) -> bool {
        match &*self.power_levels {
            Some(event) => f(event.power_levels()),
            None => self.is_room_creator,
        }
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
