//! Power level configuration types used in [the `room` module][super].

use ruma::events::{room::power_levels::RoomPowerLevels, StateEventType};

/// The power level settings required for various operations within a room.
/// When updating these settings, any levels that are `None` will remain
/// unchanged.
#[derive(Debug)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct RoomPowerLevelSettings {
    // Actions
    /// The level required to ban a user.
    pub ban: Option<i64>,
    /// The level required to invite a user.
    pub invite: Option<i64>,
    /// The level required to kick a user.
    pub kick: Option<i64>,
    /// The level required to redact an event.
    pub redact: Option<i64>,

    // Events
    /// The default level required to send message events.
    pub events_default: Option<i64>,
    /// The default level required to send state events.
    pub state_default: Option<i64>,
    /// The default power level for every user in the room.
    pub users_default: Option<i64>,
    /// The level required to change the room's name.
    pub room_name: Option<i64>,
    /// The level required to change the room's avatar.
    pub room_avatar: Option<i64>,
    /// The level required to change the room's topic.
    pub room_topic: Option<i64>,
}

impl From<RoomPowerLevels> for RoomPowerLevelSettings {
    fn from(value: RoomPowerLevels) -> Self {
        Self {
            ban: Some(value.ban.into()),
            invite: Some(value.invite.into()),
            kick: Some(value.kick.into()),
            redact: Some(value.redact.into()),
            events_default: Some(value.events_default.into()),
            state_default: Some(value.state_default.into()),
            users_default: Some(value.users_default.into()),
            room_name: value
                .events
                .get(&StateEventType::RoomName.into())
                .map(|v| (*v).into())
                .or(Some(value.state_default.into())),
            room_avatar: value
                .events
                .get(&StateEventType::RoomAvatar.into())
                .map(|v| (*v).into())
                .or(Some(value.state_default.into())),
            room_topic: value
                .events
                .get(&StateEventType::RoomTopic.into())
                .map(|v| (*v).into())
                .or(Some(value.state_default.into())),
        }
    }
}

pub(crate) trait RoomPowerLevelsExt {
    /// Applies the updated settings to the power levels. Any levels that are
    /// `None` will remain unchanged.
    fn apply(&mut self, settings: RoomPowerLevelSettings) -> anyhow::Result<(), anyhow::Error>;
    /// Updates the power level for a specific state event. If the new level is
    /// the default level, then the event will be removed.
    fn update_state_event(
        &mut self,
        event_type: StateEventType,
        new_level: i64,
    ) -> anyhow::Result<(), anyhow::Error>;
}

impl RoomPowerLevelsExt for RoomPowerLevels {
    fn apply(&mut self, settings: RoomPowerLevelSettings) -> anyhow::Result<(), anyhow::Error> {
        if let Some(ban) = settings.ban {
            self.ban = ban.try_into()?;
        }
        if let Some(invite) = settings.invite {
            self.invite = invite.try_into()?;
        }
        if let Some(kick) = settings.kick {
            self.kick = kick.try_into()?;
        }
        if let Some(redact) = settings.redact {
            self.redact = redact.try_into()?;
        }
        if let Some(events_default) = settings.events_default {
            self.events_default = events_default.try_into()?;
        }
        if let Some(state_default) = settings.state_default {
            self.state_default = state_default.try_into()?;
        }
        if let Some(users_default) = settings.users_default {
            self.users_default = users_default.try_into()?;
        }
        if let Some(room_name) = settings.room_name {
            self.update_state_event(StateEventType::RoomName, room_name)?;
        }
        if let Some(room_avatar) = settings.room_avatar {
            self.update_state_event(StateEventType::RoomAvatar, room_avatar)?;
        }
        if let Some(room_topic) = settings.room_topic {
            self.update_state_event(StateEventType::RoomTopic, room_topic)?;
        }

        Ok(())
    }

    fn update_state_event(
        &mut self,
        event_type: StateEventType,
        new_level: i64,
    ) -> anyhow::Result<(), anyhow::Error> {
        let new_level = new_level.try_into()?;
        if new_level == self.state_default {
            self.events.remove(&event_type.into());
        } else {
            self.events.insert(event_type.into(), new_level);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use ruma::{
        events::room::power_levels::{RoomPowerLevels, RoomPowerLevelsEventContent},
        int,
        power_levels::NotificationPowerLevels,
    };

    use super::*;

    #[test]
    fn test_apply_actions() {
        // Given a set of power levels and some settings that only change the
        // actions.
        let mut power_levels = default_power_levels();

        let new_level = int!(100);
        let settings = RoomPowerLevelSettings {
            ban: Some(new_level.into()),
            invite: Some(new_level.into()),
            kick: Some(new_level.into()),
            redact: Some(new_level.into()),
            events_default: None,
            state_default: None,
            users_default: None,
            room_name: None,
            room_avatar: None,
            room_topic: None,
        };

        // When applying the settings to the power levels.
        let original_levels = power_levels.clone();
        power_levels.apply(settings).unwrap();

        // Then the levels for the actions should be updated.
        assert_eq!(power_levels.ban, new_level);
        assert_eq!(power_levels.invite, new_level);
        assert_eq!(power_levels.kick, new_level);
        assert_eq!(power_levels.redact, new_level);
        // And the rest should remain unchanged.
        assert_eq!(power_levels.events_default, original_levels.events_default);
        assert_eq!(power_levels.state_default, original_levels.state_default);
        assert_eq!(power_levels.users_default, original_levels.users_default);
        assert_eq!(power_levels.events, original_levels.events);
    }

    #[test]
    fn test_apply_room_settings() {
        // Given a set of power levels and some settings that only change the specific
        // state event levels.
        let mut power_levels = default_power_levels();

        let new_level = int!(100);
        let settings = RoomPowerLevelSettings {
            ban: None,
            invite: None,
            kick: None,
            redact: None,
            events_default: None,
            state_default: None,
            users_default: None,
            room_name: Some(new_level.into()),
            room_avatar: Some(new_level.into()),
            room_topic: Some(new_level.into()),
        };

        // When applying the settings to the power levels.
        let original_levels = power_levels.clone();
        power_levels.apply(settings).unwrap();

        // Then levels for the necessary state events should be added.
        assert_eq!(
            power_levels.events,
            BTreeMap::from_iter(
                vec![
                    (StateEventType::RoomName.into(), new_level),
                    (StateEventType::RoomAvatar.into(), new_level),
                    (StateEventType::RoomTopic.into(), new_level),
                ]
                .into_iter()
            )
        );
        // And the rest should remain unchanged.
        assert_eq!(power_levels.ban, original_levels.ban);
        assert_eq!(power_levels.invite, original_levels.invite);
        assert_eq!(power_levels.kick, original_levels.kick);
        assert_eq!(power_levels.redact, original_levels.redact);
        assert_eq!(power_levels.events_default, original_levels.events_default);
        assert_eq!(power_levels.state_default, original_levels.state_default);
        assert_eq!(power_levels.users_default, original_levels.users_default);
    }

    #[test]
    fn test_apply_remove_single_room_setting() {
        // Given a set of power levels and some settings that change the room name level
        // back to the default level.
        let original_level = int!(100);
        let mut power_levels = default_power_levels();
        power_levels.events = BTreeMap::from_iter(
            vec![
                (StateEventType::RoomName.into(), original_level),
                (StateEventType::RoomAvatar.into(), original_level),
                (StateEventType::RoomTopic.into(), original_level),
            ]
            .into_iter(),
        );

        let settings = RoomPowerLevelSettings {
            ban: None,
            invite: None,
            kick: None,
            redact: None,
            events_default: None,
            state_default: None,
            users_default: None,
            room_name: Some(power_levels.state_default.into()),
            room_avatar: None,
            room_topic: None,
        };

        // When applying the settings to the power levels.
        let original_levels = power_levels.clone();
        power_levels.apply(settings).unwrap();

        // Then the room name level should be removed without updating any other state
        // events.
        assert_eq!(
            power_levels.events,
            BTreeMap::from_iter(
                vec![
                    (StateEventType::RoomAvatar.into(), original_level),
                    (StateEventType::RoomTopic.into(), original_level),
                ]
                .into_iter()
            )
        );
        // And the rest should remain unchanged.
        assert_eq!(power_levels.ban, original_levels.ban);
        assert_eq!(power_levels.invite, original_levels.invite);
        assert_eq!(power_levels.kick, original_levels.kick);
        assert_eq!(power_levels.redact, original_levels.redact);
        assert_eq!(power_levels.events_default, original_levels.events_default);
        assert_eq!(power_levels.state_default, original_levels.state_default);
        assert_eq!(power_levels.users_default, original_levels.users_default);
    }

    fn default_power_levels() -> RoomPowerLevels {
        let mut content = RoomPowerLevelsEventContent::new();
        content.ban = int!(50);
        content.invite = int!(50);
        content.kick = int!(50);
        content.redact = int!(50);
        content.events_default = int!(0);
        content.state_default = int!(50);
        content.users_default = int!(0);
        content.notifications = NotificationPowerLevels::default();
        content.into()
    }
}
