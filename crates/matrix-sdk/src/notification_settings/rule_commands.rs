use ruma::{
    RoomId,
    push::{
        Action, NewPushRule, PredefinedContentRuleId, PredefinedOverrideRuleId,
        RemovePushRuleError, RuleKind, Ruleset,
    },
};

use super::command::Command;
use crate::NotificationSettingsError;

/// A `RuleCommand` allows to generate a list of `Command` needed to modify a
/// `Ruleset`
#[derive(Clone, Debug)]
pub(crate) struct RuleCommands {
    pub(crate) commands: Vec<Command>,
    pub(crate) rules: Ruleset,
}

impl RuleCommands {
    pub(crate) fn new(rules: Ruleset) -> Self {
        RuleCommands { commands: vec![], rules }
    }

    /// Insert a new rule
    pub(crate) fn insert_rule(
        &mut self,
        kind: RuleKind,
        room_id: &RoomId,
        notify: bool,
    ) -> Result<(), NotificationSettingsError> {
        let command = match kind {
            RuleKind::Room => Command::SetRoomPushRule { room_id: room_id.to_owned(), notify },
            RuleKind::Override => Command::SetOverridePushRule {
                rule_id: room_id.to_string(),
                room_id: room_id.to_owned(),
                notify,
            },
            _ => {
                return Err(NotificationSettingsError::InvalidParameter(
                    "cannot insert a rule for this kind.".to_owned(),
                ));
            }
        };

        self.rules.insert(command.to_push_rule()?, None, None)?;
        self.commands.push(command);

        Ok(())
    }

    /// Insert a new rule for a keyword.
    pub(crate) fn insert_keyword_rule(
        &mut self,
        keyword: String,
    ) -> Result<(), NotificationSettingsError> {
        let command = Command::SetKeywordPushRule { keyword };

        self.rules.insert(command.to_push_rule()?, None, None)?;
        self.commands.push(command);

        Ok(())
    }

    /// Insert a new custom rule
    pub(crate) fn insert_custom_rule(
        &mut self,
        rule: NewPushRule,
    ) -> Result<(), NotificationSettingsError> {
        let command = Command::SetCustomPushRule { rule: rule.clone() };

        self.rules.insert(rule, None, None)?;
        self.commands.push(command);

        Ok(())
    }

    /// Delete a rule
    pub(crate) fn delete_rule(
        &mut self,
        kind: RuleKind,
        rule_id: String,
    ) -> Result<(), RemovePushRuleError> {
        self.rules.remove(kind.clone(), &rule_id)?;
        self.commands.push(Command::DeletePushRule { kind, rule_id });

        Ok(())
    }

    /// Enable or disable the given rule.
    ///
    /// Will return an error if the rule does not exist.
    fn set_enabled_internal(
        &mut self,
        kind: RuleKind,
        rule_id: &str,
        enabled: bool,
    ) -> Result<(), NotificationSettingsError> {
        self.rules
            .set_enabled(kind.clone(), rule_id, enabled)
            .map_err(|_| NotificationSettingsError::RuleNotFound(rule_id.to_owned()))?;
        self.commands.push(Command::SetPushRuleEnabled {
            kind,
            rule_id: rule_id.to_owned(),
            enabled,
        });
        Ok(())
    }

    /// Set whether a rule is enabled
    pub(crate) fn set_rule_enabled(
        &mut self,
        kind: RuleKind,
        rule_id: &str,
        enabled: bool,
    ) -> Result<(), NotificationSettingsError> {
        if rule_id == PredefinedOverrideRuleId::IsRoomMention.as_str() {
            // Handle specific case for `PredefinedOverrideRuleId::IsRoomMention`
            self.set_room_mention_enabled(enabled)
        } else if rule_id == PredefinedOverrideRuleId::IsUserMention.as_str() {
            // Handle specific case for `PredefinedOverrideRuleId::IsUserMention`
            self.set_user_mention_enabled(enabled)
        } else {
            self.set_enabled_internal(kind, rule_id, enabled)
        }
    }

    /// Set whether `IsUserMention` is enabled
    fn set_user_mention_enabled(&mut self, enabled: bool) -> Result<(), NotificationSettingsError> {
        // Add a command for the `IsUserMention` `Override` rule (MSC3952).
        // This is a new push rule that may not yet be present.
        self.set_enabled_internal(
            RuleKind::Override,
            PredefinedOverrideRuleId::IsUserMention.as_str(),
            enabled,
        )?;

        // For compatibility purpose, we still need to add commands for
        // `ContainsUserName` and `ContainsDisplayName` (removed rules).
        #[allow(deprecated)]
        {
            // `ContainsUserName`
            if let Err(err) = self.set_enabled_internal(
                RuleKind::Content,
                PredefinedContentRuleId::ContainsUserName.as_str(),
                enabled,
            ) {
                // This rule has been removed from the spec, so it's fine if it wasn't found.
                if !err.is_rule_not_found() {
                    return Err(err);
                }
            }

            // `ContainsDisplayName`
            if let Err(err) = self.set_enabled_internal(
                RuleKind::Override,
                PredefinedOverrideRuleId::ContainsDisplayName.as_str(),
                enabled,
            ) {
                // This rule has been removed from the spec, so it's fine if it wasn't found.
                if !err.is_rule_not_found() {
                    return Err(err);
                }
            }
        }

        Ok(())
    }

    /// Set whether `IsRoomMention` is enabled
    fn set_room_mention_enabled(&mut self, enabled: bool) -> Result<(), NotificationSettingsError> {
        // Sets the `IsRoomMention` `Override` rule (MSC3952).
        // This is a new push rule that may not yet be present.
        self.set_enabled_internal(
            RuleKind::Override,
            PredefinedOverrideRuleId::IsRoomMention.as_str(),
            enabled,
        )?;

        // For compatibility purpose, we still need to set `RoomNotif` (removed
        // rule).
        #[allow(deprecated)]
        if let Err(err) = self.set_enabled_internal(
            RuleKind::Override,
            PredefinedOverrideRuleId::RoomNotif.as_str(),
            enabled,
        ) {
            // This rule has been removed from the spec, so it's fine if it wasn't found.
            if !err.is_rule_not_found() {
                return Err(err);
            }
        }

        Ok(())
    }

    /// Set the actions of the rule from the given kind and with the given
    /// `rule_id`
    pub(crate) fn set_rule_actions(
        &mut self,
        kind: RuleKind,
        rule_id: &str,
        actions: Vec<Action>,
    ) -> Result<(), NotificationSettingsError> {
        self.rules
            .set_actions(kind.clone(), rule_id, actions.clone())
            .map_err(|_| NotificationSettingsError::RuleNotFound(rule_id.to_owned()))?;
        self.commands.push(Command::SetPushRuleActions {
            kind,
            rule_id: rule_id.to_owned(),
            actions,
        });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use assert_matches::assert_matches;
    use matrix_sdk_test::{
        async_test,
        notification_settings::{
            get_server_default_ruleset, server_default_ruleset_with_legacy_mentions,
        },
    };
    use ruma::{
        OwnedRoomId, RoomId,
        push::{
            Action, NewPushRule, NewSimplePushRule, PredefinedContentRuleId,
            PredefinedOverrideRuleId, PredefinedUnderrideRuleId, RemovePushRuleError, RuleKind,
            Tweak,
        },
    };

    use super::RuleCommands;
    use crate::{error::NotificationSettingsError, notification_settings::command::Command};

    fn get_test_room_id() -> OwnedRoomId {
        RoomId::parse("!AAAaAAAAAaaAAaaaaa:matrix.org").unwrap()
    }

    #[async_test]
    async fn test_insert_rule_room() {
        let room_id = get_test_room_id();
        let mut rule_commands = RuleCommands::new(get_server_default_ruleset());
        rule_commands.insert_rule(RuleKind::Room, &room_id, true).unwrap();

        // A rule must have been inserted in the ruleset.
        assert!(rule_commands.rules.get(RuleKind::Room, &room_id).is_some());

        // Exactly one command must have been created.
        assert_eq!(rule_commands.commands.len(), 1);
        assert_matches!(&rule_commands.commands[0],
            Command::SetRoomPushRule { room_id: command_room_id, notify } => {
                assert_eq!(command_room_id, &room_id);
                assert!(notify);
            }
        );
    }

    #[async_test]
    async fn test_insert_rule_override() {
        let room_id = get_test_room_id();
        let mut rule_commands = RuleCommands::new(get_server_default_ruleset());
        rule_commands.insert_rule(RuleKind::Override, &room_id, true).unwrap();

        // A rule must have been inserted in the ruleset.
        assert!(rule_commands.rules.get(RuleKind::Override, &room_id).is_some());

        // Exactly one command must have been created.
        assert_eq!(rule_commands.commands.len(), 1);
        assert_matches!(&rule_commands.commands[0],
            Command::SetOverridePushRule {room_id: command_room_id, rule_id, notify } => {
                assert_eq!(command_room_id, &room_id);
                assert_eq!(rule_id, room_id.as_str());
                assert!(notify);
            }
        );
    }

    #[async_test]
    async fn test_insert_rule_unsupported() {
        let room_id = get_test_room_id();
        let mut rule_commands = RuleCommands::new(get_server_default_ruleset());

        assert_matches!(
            rule_commands.insert_rule(RuleKind::Underride, &room_id, true),
            Err(NotificationSettingsError::InvalidParameter(_)) => {}
        );

        assert_matches!(
            rule_commands.insert_rule(RuleKind::Content, &room_id, true),
            Err(NotificationSettingsError::InvalidParameter(_)) => {}
        );

        assert_matches!(
            rule_commands.insert_rule(RuleKind::Sender, &room_id, true),
            Err(NotificationSettingsError::InvalidParameter(_)) => {}
        );
    }

    #[async_test]
    async fn test_delete_rule() {
        let room_id = get_test_room_id();
        let mut ruleset = get_server_default_ruleset();

        let new_rule = NewSimplePushRule::new(room_id.to_owned(), vec![]);
        ruleset.insert(NewPushRule::Room(new_rule), None, None).unwrap();

        let mut rule_commands = RuleCommands::new(ruleset);

        // Delete must succeed.
        rule_commands.delete_rule(RuleKind::Room, room_id.to_string()).unwrap();

        // The ruleset must have been updated.
        assert!(rule_commands.rules.get(RuleKind::Room, &room_id).is_none());

        // Exactly one command must have been created.
        assert_eq!(rule_commands.commands.len(), 1);
        assert_matches!(&rule_commands.commands[0],
            Command::DeletePushRule { kind, rule_id } => {
                assert_eq!(kind, &RuleKind::Room);
                assert_eq!(rule_id, room_id.as_str());
            }
        );
    }

    #[async_test]
    async fn test_delete_rule_errors() {
        let room_id = get_test_room_id();
        let ruleset = get_server_default_ruleset();

        let mut rule_commands = RuleCommands::new(ruleset);

        // Deletion should fail if an attempt is made to delete a rule that does not
        // exist.
        assert_matches!(
            rule_commands.delete_rule(RuleKind::Room, room_id.to_string()),
            Err(RemovePushRuleError::NotFound) => {}
        );

        // Deletion should fail if an attempt is made to delete a default server rule.
        assert_matches!(
            rule_commands.delete_rule(RuleKind::Override, PredefinedOverrideRuleId::IsUserMention.to_string()),
            Err(RemovePushRuleError::ServerDefault) => {}
        );

        assert!(rule_commands.commands.is_empty());
    }

    #[async_test]
    async fn test_set_rule_enabled() {
        let mut ruleset = get_server_default_ruleset();

        // Initialize with `Reaction` rule disabled.
        ruleset.set_enabled(RuleKind::Override, PredefinedOverrideRuleId::Reaction, false).unwrap();

        let mut rule_commands = RuleCommands::new(ruleset);
        rule_commands
            .set_rule_enabled(RuleKind::Override, PredefinedOverrideRuleId::Reaction.as_str(), true)
            .unwrap();

        // The ruleset must have been updated
        let rule = rule_commands
            .rules
            .get(RuleKind::Override, PredefinedOverrideRuleId::Reaction.as_str())
            .unwrap();
        assert!(rule.enabled());

        // Exactly one command must have been created.
        assert_eq!(rule_commands.commands.len(), 1);
        assert_matches!(&rule_commands.commands[0],
            Command::SetPushRuleEnabled { kind, rule_id, enabled } => {
                assert_eq!(kind, &RuleKind::Override);
                assert_eq!(rule_id, PredefinedOverrideRuleId::Reaction.as_str());
                assert!(enabled);
            }
        );
    }

    #[async_test]
    async fn test_set_rule_enabled_not_found() {
        let ruleset = get_server_default_ruleset();
        let mut rule_commands = RuleCommands::new(ruleset);
        assert_eq!(
            rule_commands.set_rule_enabled(RuleKind::Room, "unknown_rule_id", true),
            Err(NotificationSettingsError::RuleNotFound("unknown_rule_id".to_owned()))
        );
    }

    #[async_test]
    async fn test_set_rule_enabled_user_mention() {
        let mut ruleset = server_default_ruleset_with_legacy_mentions();
        let mut rule_commands = RuleCommands::new(ruleset.clone());

        ruleset
            .set_enabled(RuleKind::Override, PredefinedOverrideRuleId::IsUserMention, false)
            .unwrap();

        #[allow(deprecated)]
        {
            ruleset
                .set_enabled(
                    RuleKind::Override,
                    PredefinedOverrideRuleId::ContainsDisplayName,
                    false,
                )
                .unwrap();
            ruleset
                .set_enabled(RuleKind::Content, PredefinedContentRuleId::ContainsUserName, false)
                .unwrap();
        }

        // Enable the user mention rule.
        rule_commands
            .set_rule_enabled(
                RuleKind::Override,
                PredefinedOverrideRuleId::IsUserMention.as_str(),
                true,
            )
            .unwrap();

        // The ruleset must have been updated.
        assert!(
            rule_commands
                .rules
                .get(RuleKind::Override, PredefinedOverrideRuleId::IsUserMention)
                .unwrap()
                .enabled()
        );
        #[allow(deprecated)]
        {
            assert!(
                rule_commands
                    .rules
                    .get(RuleKind::Override, PredefinedOverrideRuleId::ContainsDisplayName)
                    .unwrap()
                    .enabled()
            );
            assert!(
                rule_commands
                    .rules
                    .get(RuleKind::Content, PredefinedContentRuleId::ContainsUserName)
                    .unwrap()
                    .enabled()
            );
        }

        // Three commands are expected.
        assert_eq!(rule_commands.commands.len(), 3);

        assert_matches!(&rule_commands.commands[0],
            Command::SetPushRuleEnabled { kind, rule_id, enabled } => {
                assert_eq!(kind, &RuleKind::Override);
                assert_eq!(rule_id, PredefinedOverrideRuleId::IsUserMention.as_str());
                assert!(enabled);
            }
        );

        #[allow(deprecated)]
        {
            assert_matches!(&rule_commands.commands[1],
                Command::SetPushRuleEnabled { kind, rule_id, enabled } => {
                    assert_eq!(kind, &RuleKind::Content);
                    assert_eq!(rule_id, PredefinedContentRuleId::ContainsUserName.as_str());
                    assert!(enabled);
                }
            );

            assert_matches!(&rule_commands.commands[2],
                Command::SetPushRuleEnabled { kind, rule_id, enabled } => {
                    assert_eq!(kind, &RuleKind::Override);
                    assert_eq!(rule_id, PredefinedOverrideRuleId::ContainsDisplayName.as_str());
                    assert!(enabled);
                }
            );
        }
    }

    #[async_test]
    async fn test_set_rule_enabled_room_mention() {
        let mut ruleset = server_default_ruleset_with_legacy_mentions();
        let mut rule_commands = RuleCommands::new(ruleset.clone());

        ruleset
            .set_enabled(RuleKind::Override, PredefinedOverrideRuleId::IsRoomMention, false)
            .unwrap();

        #[allow(deprecated)]
        {
            ruleset
                .set_enabled(RuleKind::Override, PredefinedOverrideRuleId::RoomNotif, false)
                .unwrap();
        }

        rule_commands
            .set_rule_enabled(
                RuleKind::Override,
                PredefinedOverrideRuleId::IsRoomMention.as_str(),
                true,
            )
            .unwrap();

        // The ruleset must have been updated.
        assert!(
            rule_commands
                .rules
                .get(RuleKind::Override, PredefinedOverrideRuleId::IsRoomMention)
                .unwrap()
                .enabled()
        );
        #[allow(deprecated)]
        {
            assert!(
                rule_commands
                    .rules
                    .get(RuleKind::Override, PredefinedOverrideRuleId::RoomNotif)
                    .unwrap()
                    .enabled()
            );
        }

        // Two commands are expected.
        assert_eq!(rule_commands.commands.len(), 2);

        assert_matches!(&rule_commands.commands[0],
            Command::SetPushRuleEnabled {  kind, rule_id, enabled } => {
                assert_eq!(kind, &RuleKind::Override);
                assert_eq!(rule_id, PredefinedOverrideRuleId::IsRoomMention.as_str());
                assert!(enabled);
            }
        );

        #[allow(deprecated)]
        {
            assert_matches!(&rule_commands.commands[1],
                Command::SetPushRuleEnabled { kind, rule_id, enabled } => {
                    assert_eq!(kind, &RuleKind::Override);
                    assert_eq!(rule_id, PredefinedOverrideRuleId::RoomNotif.as_str());
                    assert!(enabled);
                }
            );
        }
    }

    #[async_test]
    async fn test_set_rule_actions() {
        let mut ruleset = get_server_default_ruleset();
        let mut rule_commands = RuleCommands::new(ruleset.clone());

        // Starting with an empty action list for `PredefinedUnderrideRuleId::Message`.
        ruleset
            .set_actions(RuleKind::Underride, PredefinedUnderrideRuleId::Message, vec![])
            .unwrap();

        // After setting a list of actions
        rule_commands
            .set_rule_actions(
                RuleKind::Underride,
                PredefinedUnderrideRuleId::Message.as_str(),
                vec![Action::Notify, Action::SetTweak(Tweak::Sound("default".into()))],
            )
            .unwrap();

        // The ruleset must have been updated
        let actions = rule_commands
            .rules
            .get(RuleKind::Underride, PredefinedUnderrideRuleId::Message)
            .unwrap()
            .actions();
        assert_eq!(actions.len(), 2);

        // and a `SetPushRuleActions` command must have been added
        assert_eq!(rule_commands.commands.len(), 1);
        assert_matches!(&rule_commands.commands[0],
            Command::SetPushRuleActions { kind, rule_id, actions } => {
                assert_eq!(kind, &RuleKind::Underride);
                assert_eq!(rule_id, PredefinedUnderrideRuleId::Message.as_str());
                assert_eq!(actions.len(), 2);
                assert_matches!(&actions[0], Action::Notify);
                assert_matches!(&actions[1], Action::SetTweak(Tweak::Sound(sound)) => {
                    assert_eq!(sound, "default");
                });
            }
        );
    }
}
