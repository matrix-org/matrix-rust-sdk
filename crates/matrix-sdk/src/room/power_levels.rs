//! Power level configuration types used in [the `room` module][super].

use std::collections::HashMap;

use ruma::{
    OwnedUserId,
    events::{
        StateEventType,
        room::power_levels::{
            PossiblyRedactedRoomPowerLevelsEventContent, RoomPowerLevels,
            RoomPowerLevelsEventContent,
        },
    },
};

use crate::Result;

/// A set of common power levels required for various operations within a room,
/// that can be applied as a single operation. When updating these
/// settings, any levels that are `None` will remain unchanged.
#[derive(Debug)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct RoomPowerLevelChanges {
    // Actions
    /// The level required to ban a user.
    #[cfg_attr(feature = "uniffi", uniffi(default = None))]
    pub ban: Option<i64>,
    /// The level required to invite a user.
    #[cfg_attr(feature = "uniffi", uniffi(default = None))]
    pub invite: Option<i64>,
    /// The level required to kick a user.
    #[cfg_attr(feature = "uniffi", uniffi(default = None))]
    pub kick: Option<i64>,
    /// The level required to redact an event.
    #[cfg_attr(feature = "uniffi", uniffi(default = None))]
    pub redact: Option<i64>,

    // Events
    /// The default level required to send message events.
    #[cfg_attr(feature = "uniffi", uniffi(default = None))]
    pub events_default: Option<i64>,
    /// The default level required to send state events.
    #[cfg_attr(feature = "uniffi", uniffi(default = None))]
    pub state_default: Option<i64>,
    /// The default power level for every user in the room.
    #[cfg_attr(feature = "uniffi", uniffi(default = None))]
    pub users_default: Option<i64>,
    /// The level required to change the room's name.
    #[cfg_attr(feature = "uniffi", uniffi(default = None))]
    pub room_name: Option<i64>,
    /// The level required to change the room's avatar.
    #[cfg_attr(feature = "uniffi", uniffi(default = None))]
    pub room_avatar: Option<i64>,
    /// The level required to change the room's topic.
    #[cfg_attr(feature = "uniffi", uniffi(default = None))]
    pub room_topic: Option<i64>,
    /// The level required to change the space's children.
    #[cfg_attr(feature = "uniffi", uniffi(default = None))]
    pub space_child: Option<i64>,
}

impl RoomPowerLevelChanges {
    /// Constructs an empty set of `RoomPowerLevelChanges`.
    pub fn new() -> Self {
        Self {
            ban: None,
            invite: None,
            kick: None,
            redact: None,
            events_default: None,
            state_default: None,
            users_default: None,
            room_name: None,
            room_avatar: None,
            room_topic: None,
            space_child: None,
        }
    }
}

impl Default for RoomPowerLevelChanges {
    fn default() -> Self {
        Self::new()
    }
}

impl From<RoomPowerLevels> for RoomPowerLevelChanges {
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
            space_child: value
                .events
                .get(&StateEventType::SpaceChild.into())
                .map(|v| (*v).into())
                .or(Some(value.state_default.into())),
        }
    }
}

pub(crate) trait RoomPowerLevelsExt {
    /// Applies the updated settings to the power levels. Any levels that are
    /// `None` will remain unchanged. Unlike with members, we don't remove the
    /// event if the new level matches the default as this could result in
    /// unintended privileges when updating the default power level in
    /// isolation of the others.
    fn apply(&mut self, settings: RoomPowerLevelChanges) -> Result<()>;
}

impl RoomPowerLevelsExt for RoomPowerLevels {
    fn apply(&mut self, settings: RoomPowerLevelChanges) -> Result<()> {
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
            self.events.insert(StateEventType::RoomName.into(), room_name.try_into()?);
        }
        if let Some(room_avatar) = settings.room_avatar {
            self.events.insert(StateEventType::RoomAvatar.into(), room_avatar.try_into()?);
        }
        if let Some(room_topic) = settings.room_topic {
            self.events.insert(StateEventType::RoomTopic.into(), room_topic.try_into()?);
        }
        if let Some(space_child) = settings.space_child {
            self.events.insert(StateEventType::SpaceChild.into(), space_child.try_into()?);
        }

        Ok(())
    }
}

impl From<js_int::TryFromIntError> for crate::error::Error {
    fn from(e: js_int::TryFromIntError) -> Self {
        crate::error::Error::UnknownError(Box::new(e))
    }
}

/// Checks for changes in the power levels of users in a room based on a new
/// event.
pub fn power_level_user_changes(
    content: &RoomPowerLevelsEventContent,
    prev_content: &Option<PossiblyRedactedRoomPowerLevelsEventContent>,
) -> HashMap<OwnedUserId, i64> {
    let Some(prev_content) = prev_content.as_ref() else {
        return Default::default();
    };

    let mut changes = HashMap::new();
    let mut prev_users = prev_content.users.clone();
    let new_users = content.users.clone();

    // If a user is in the new power levels, but not in the old ones, or if the
    // power level has changed, add them to the changes.
    for (user_id, power_level) in new_users {
        let prev_power_level = prev_users.remove(&user_id).unwrap_or(prev_content.users_default);
        if power_level != prev_power_level {
            changes.insert(user_id, power_level.into());
        }
    }

    // Any remaining users from the old power levels have had their power level set
    // back to default.
    for (user_id, power_level) in prev_users {
        if power_level != content.users_default {
            changes.insert(user_id, content.users_default.into());
        }
    }

    changes
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use ruma::{
        int, power_levels::NotificationPowerLevels, room_version_rules::AuthorizationRules,
    };

    use super::*;

    #[test]
    fn test_apply_actions() {
        // Given a set of power levels and some settings that only change the
        // actions.
        let mut power_levels = default_power_levels();

        let new_level = int!(100);
        let settings = RoomPowerLevelChanges {
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
            space_child: None,
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
        let settings = RoomPowerLevelChanges {
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
            space_child: Some(new_level.into()),
        };

        // When applying the settings to the power levels.
        let original_levels = power_levels.clone();
        power_levels.apply(settings).unwrap();

        // Then levels for the necessary state events should be added.
        assert_eq!(
            power_levels.events,
            BTreeMap::from_iter(vec![
                (StateEventType::RoomName.into(), new_level),
                (StateEventType::RoomAvatar.into(), new_level),
                (StateEventType::RoomTopic.into(), new_level),
                (StateEventType::SpaceChild.into(), new_level),
            ])
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
    fn test_apply_state_event_to_default() {
        // Given a set of power levels and some settings that change the room name level
        // back to the default level.
        let original_level = int!(100);
        let mut power_levels = default_power_levels();
        power_levels.events = BTreeMap::from_iter(vec![
            (StateEventType::RoomName.into(), original_level),
            (StateEventType::RoomAvatar.into(), original_level),
            (StateEventType::RoomTopic.into(), original_level),
            (StateEventType::SpaceChild.into(), original_level),
        ]);

        let settings = RoomPowerLevelChanges {
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
            space_child: None,
        };

        // When applying the settings to the power levels.
        let original_levels = power_levels.clone();
        power_levels.apply(settings).unwrap();

        // Then the room name level should be updated (but not removed) without
        // affecting any other state events.
        assert_eq!(
            power_levels.events,
            BTreeMap::from_iter(vec![
                (StateEventType::RoomName.into(), power_levels.state_default),
                (StateEventType::RoomAvatar.into(), original_level),
                (StateEventType::RoomTopic.into(), original_level),
                (StateEventType::SpaceChild.into(), original_level),
            ])
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
    fn test_user_power_level_changes_add_mod() {
        // Given a set of power levels and a new set of power levels that adds a new
        // moderator.
        let prev_content = default_power_levels_event_content();
        let mut content = prev_content.clone();
        content.users.insert(OwnedUserId::try_from("@charlie:example.com").unwrap(), int!(50));

        // When calculating the changes.
        let changes = power_level_user_changes(&content, &Some(prev_content));

        // Then the changes should reflect the new moderator.
        assert_eq!(changes.len(), 1);
        assert_eq!(changes.get(&OwnedUserId::try_from("@charlie:example.com").unwrap()), Some(&50));
    }

    #[test]
    fn test_user_power_level_changes_remove_mod() {
        // Given a set of power levels and a new set of power levels that removes a
        // moderator.
        let prev_content = default_power_levels_event_content();
        let mut content = prev_content.clone();
        content.users.remove(&OwnedUserId::try_from("@bob:example.com").unwrap());

        // When calculating the changes.
        let changes = power_level_user_changes(&content, &Some(prev_content));

        // Then the changes should reflect the removed moderator.
        assert_eq!(changes.len(), 1);
        assert_eq!(changes.get(&OwnedUserId::try_from("@bob:example.com").unwrap()), Some(&0));
    }

    #[test]
    fn test_user_power_level_changes_change_mod() {
        // Given a set of power levels and a new set of power levels that changes a
        // moderator to an admin.
        let prev_content = default_power_levels_event_content();
        let mut content = prev_content.clone();
        content.users.insert(OwnedUserId::try_from("@bob:example.com").unwrap(), int!(100));

        // When calculating the changes.
        let changes = power_level_user_changes(&content, &Some(prev_content));

        // Then the changes should reflect the new admin.
        assert_eq!(changes.len(), 1);
        assert_eq!(changes.get(&OwnedUserId::try_from("@bob:example.com").unwrap()), Some(&100));
    }

    #[test]
    fn test_user_power_level_changes_new_default() {
        // Given a set of power levels and a new set of power levels that changes the
        // default user power level to moderator and removes the only moderator.
        let prev_content = default_power_levels_event_content();
        let mut content = prev_content.clone();
        content.users_default = int!(50);
        content.users.remove(&OwnedUserId::try_from("@bob:example.com").unwrap());

        // When calculating the changes.
        let changes = power_level_user_changes(&content, &Some(prev_content));

        // Then there should be no changes.
        assert!(changes.is_empty());
    }

    #[test]
    fn test_user_power_level_changes_no_change() {
        // Given a set of power levels and a new set of power levels that's the same.
        let prev_content = default_power_levels_event_content();
        let content = prev_content.clone();

        // When calculating the changes.
        let changes = power_level_user_changes(&content, &Some(prev_content));

        // Then there should be no changes.
        assert!(changes.is_empty());
    }

    #[test]
    fn test_user_power_level_changes_other_properties() {
        // Given a set of power levels and a new set of power levels with changes that
        // don't include the user power levels.
        let prev_content = default_power_levels_event_content();
        let mut content = prev_content.clone();
        content.events_default = int!(100);

        // When calculating the changes.
        let changes = power_level_user_changes(&content, &Some(prev_content));

        // Then there should be no changes.
        assert!(changes.is_empty());
    }

    fn default_power_levels() -> RoomPowerLevels {
        RoomPowerLevels::new(
            default_power_levels_event_content().into(),
            &AuthorizationRules::V1,
            [],
        )
    }

    fn default_power_levels_event_content() -> RoomPowerLevelsEventContent {
        let mut content = RoomPowerLevelsEventContent::new(&AuthorizationRules::V1);
        content.ban = int!(50);
        content.invite = int!(50);
        content.kick = int!(50);
        content.redact = int!(50);
        content.events_default = int!(0);
        content.state_default = int!(50);
        content.users_default = int!(0);
        content.users = BTreeMap::from_iter(vec![
            (OwnedUserId::try_from("@alice:example.com").unwrap(), int!(100)),
            (OwnedUserId::try_from("@bob:example.com").unwrap(), int!(50)),
        ]);
        content.notifications = NotificationPowerLevels::default();
        content
    }
}
