use std::collections::HashMap;

use anyhow::Result;
use ruma::{
    events::{room::power_levels::RoomPowerLevels as RumaPowerLevels, TimelineEventType},
    OwnedUserId, UserId,
};

use crate::{
    error::ClientError,
    event::{MessageLikeEventType, StateEventType},
};

#[derive(uniffi::Object)]
pub struct RoomPowerLevels {
    inner: RumaPowerLevels,
    own_user_id: OwnedUserId,
}

impl RoomPowerLevels {
    pub fn new(value: RumaPowerLevels, own_user_id: OwnedUserId) -> Self {
        Self { inner: value, own_user_id }
    }
}

#[matrix_sdk_ffi_macros::export]
impl RoomPowerLevels {
    fn values(&self) -> RoomPowerLevelsValues {
        self.inner.clone().into()
    }

    fn events(&self) -> HashMap<crate::event::TimelineEventType, i64> {
        self.inner.events.iter().map(|(key, value)| (key.clone().into(), (*value).into())).collect()
    }

    /// Gets a map with the `UserId` of users with power levels other than `0`
    /// and their power level.
    pub fn user_power_levels(&self) -> HashMap<String, i64> {
        let mut user_power_levels = HashMap::<String, i64>::new();

        for (id, level) in self.inner.users.iter() {
            user_power_levels.insert(id.to_string(), (*level).into());
        }

        user_power_levels
    }

    /// Returns true if the current user is able to ban in the room.
    pub fn can_own_user_ban(&self) -> bool {
        self.inner.user_can_ban(&self.own_user_id)
    }

    /// Returns true if the user with the given user_id is able to ban in the
    /// room.
    ///
    /// The call may fail if there is an error in getting the power levels.
    pub fn can_user_ban(&self, user_id: String) -> Result<bool, ClientError> {
        let user_id = UserId::parse(&user_id)?;
        Ok(self.inner.user_can_ban(&user_id))
    }

    /// Returns true if the current user is able to redact their own messages in
    /// the room.
    pub fn can_own_user_redact_own(&self) -> bool {
        self.inner.user_can_redact_own_event(&self.own_user_id)
    }

    /// Returns true if the user with the given user_id is able to redact
    /// their own messages in the room.
    ///
    /// The call may fail if there is an error in getting the power levels.
    pub fn can_user_redact_own(&self, user_id: String) -> Result<bool, ClientError> {
        let user_id = UserId::parse(&user_id)?;
        Ok(self.inner.user_can_redact_own_event(&user_id))
    }

    /// Returns true if the current user user is able to redact messages of
    /// other users in the room.
    pub fn can_own_user_redact_other(&self) -> bool {
        self.inner.user_can_redact_event_of_other(&self.own_user_id)
    }

    /// Returns true if the user with the given user_id is able to redact
    /// messages of other users in the room.
    ///
    /// The call may fail if there is an error in getting the power levels.
    pub fn can_user_redact_other(&self, user_id: String) -> Result<bool, ClientError> {
        let user_id = UserId::parse(&user_id)?;
        Ok(self.inner.user_can_redact_event_of_other(&user_id))
    }

    /// Returns true if the current user is able to invite in the room.
    pub fn can_own_user_invite(&self) -> bool {
        self.inner.user_can_invite(&self.own_user_id)
    }

    /// Returns true if the user with the given user_id is able to invite in the
    /// room.
    ///
    /// The call may fail if there is an error in getting the power levels.
    pub fn can_user_invite(&self, user_id: String) -> Result<bool, ClientError> {
        let user_id = UserId::parse(&user_id)?;
        Ok(self.inner.user_can_invite(&user_id))
    }

    /// Returns true if the current user is able to kick in the room.
    pub fn can_own_user_kick(&self) -> bool {
        self.inner.user_can_kick(&self.own_user_id)
    }

    /// Returns true if the user with the given user_id is able to kick in the
    /// room.
    ///
    /// The call may fail if there is an error in getting the power levels.
    pub fn can_user_kick(&self, user_id: String) -> Result<bool, ClientError> {
        let user_id = UserId::parse(&user_id)?;
        Ok(self.inner.user_can_kick(&user_id))
    }

    /// Returns true if the current user is able to send a specific state event
    /// type in the room.
    pub fn can_own_user_send_state(&self, state_event: StateEventType) -> bool {
        self.inner.user_can_send_state(&self.own_user_id, state_event.into())
    }

    /// Returns true if the user with the given user_id is able to send a
    /// specific state event type in the room.
    ///
    /// The call may fail if there is an error in getting the power levels.
    pub fn can_user_send_state(
        &self,
        user_id: String,
        state_event: StateEventType,
    ) -> Result<bool, ClientError> {
        let user_id = UserId::parse(&user_id)?;
        Ok(self.inner.user_can_send_state(&user_id, state_event.into()))
    }

    /// Returns true if the current user is able to send a specific message type
    /// in the room.
    pub fn can_own_user_send_message(&self, message: MessageLikeEventType) -> bool {
        self.inner.user_can_send_message(&self.own_user_id, message.into())
    }

    /// Returns true if the user with the given user_id is able to send a
    /// specific message type in the room.
    ///
    /// The call may fail if there is an error in getting the power levels.
    pub fn can_user_send_message(
        &self,
        user_id: String,
        message: MessageLikeEventType,
    ) -> Result<bool, ClientError> {
        let user_id = UserId::parse(&user_id)?;
        Ok(self.inner.user_can_send_message(&user_id, message.into()))
    }

    /// Returns true if the current user is able to pin or unpin events in the
    /// room.
    pub fn can_own_user_pin_unpin(&self) -> bool {
        self.inner.user_can_send_state(&self.own_user_id, StateEventType::RoomPinnedEvents.into())
    }

    /// Returns true if the user with the given user_id is able to pin or unpin
    /// events in the room.
    ///
    /// The call may fail if there is an error in getting the power levels.
    pub fn can_user_pin_unpin(&self, user_id: String) -> Result<bool, ClientError> {
        let user_id = UserId::parse(&user_id)?;
        Ok(self.inner.user_can_send_state(&user_id, StateEventType::RoomPinnedEvents.into()))
    }

    /// Returns true if the current user is able to trigger a notification in
    /// the room.
    pub fn can_own_user_trigger_room_notification(&self) -> bool {
        self.inner.user_can_trigger_room_notification(&self.own_user_id)
    }

    /// Returns true if the user with the given user_id is able to trigger a
    /// notification in the room.
    ///
    /// The call may fail if there is an error in getting the power levels.
    pub fn can_user_trigger_room_notification(&self, user_id: String) -> Result<bool, ClientError> {
        let user_id = UserId::parse(&user_id)?;
        Ok(self.inner.user_can_trigger_room_notification(&user_id))
    }
}

/// This intermediary struct is used to expose the power levels values through
/// FFI and work around it not exposing public exported object fields.
#[derive(uniffi::Record)]
pub struct RoomPowerLevelsValues {
    /// The level required to ban a user.
    pub ban: i64,
    /// The level required to invite a user.
    pub invite: i64,
    /// The level required to kick a user.
    pub kick: i64,
    /// The level required to redact an event.
    pub redact: i64,
    /// The default level required to send message events.
    pub events_default: i64,
    /// The default level required to send state events.
    pub state_default: i64,
    /// The default power level for every user in the room.
    pub users_default: i64,
    /// The level required to change the room's name.
    pub room_name: i64,
    /// The level required to change the room's avatar.
    pub room_avatar: i64,
    /// The level required to change the room's topic.
    pub room_topic: i64,
    /// The level required to change the space's children.
    pub space_child: i64,
}

impl From<RumaPowerLevels> for RoomPowerLevelsValues {
    fn from(value: RumaPowerLevels) -> Self {
        fn state_event_level_for(
            power_levels: &RumaPowerLevels,
            event_type: &TimelineEventType,
        ) -> i64 {
            let default_state: i64 = power_levels.state_default.into();
            power_levels.events.get(event_type).map_or(default_state, |&level| level.into())
        }
        Self {
            ban: value.ban.into(),
            invite: value.invite.into(),
            kick: value.kick.into(),
            redact: value.redact.into(),
            events_default: value.events_default.into(),
            state_default: value.state_default.into(),
            users_default: value.users_default.into(),
            room_name: state_event_level_for(&value, &TimelineEventType::RoomName),
            room_avatar: state_event_level_for(&value, &TimelineEventType::RoomAvatar),
            room_topic: state_event_level_for(&value, &TimelineEventType::RoomTopic),
            space_child: state_event_level_for(&value, &TimelineEventType::SpaceChild),
        }
    }
}
