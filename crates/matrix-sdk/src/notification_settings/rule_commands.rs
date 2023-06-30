use ruma::{
    api::client::push::RuleScope,
    push::{
        PredefinedContentRuleId, PredefinedOverrideRuleId, RemovePushRuleError, RuleKind, Ruleset,
    },
    RoomId,
};

use super::command::Command;
use crate::NotificationSettingsError;

#[derive(Clone, Debug)]
pub(crate) struct RuleCommands {
    pub(crate) commands: Vec<Command>,
    pub(crate) rules: Ruleset,
}

impl RuleCommands {
    pub(crate) fn new(rules: &Ruleset) -> Self {
        RuleCommands { commands: vec![], rules: rules.clone() }
    }

    pub(crate) fn insert_rule(
        &mut self,
        kind: RuleKind,
        room_id: &RoomId,
        notify: bool,
    ) -> Result<(), NotificationSettingsError> {
        let command;
        match kind {
            RuleKind::Room => {
                command = Command::SetRoomPushRule {
                    scope: RuleScope::Global,
                    room_id: room_id.to_owned(),
                    notify,
                };
            }
            RuleKind::Override => {
                command = Command::SetOverridePushRule {
                    scope: RuleScope::Global,
                    rule_id: room_id.to_string(),
                    room_id: room_id.to_owned(),
                    notify,
                };
            }
            _ => {
                return Err(NotificationSettingsError::InvalidParameter(
                    "cannot insert a rule for this kind.".to_owned(),
                ))
            }
        }
        self.rules.insert(command.to_push_rule()?, None, None)?;
        self.commands.push(command);

        Ok(())
    }

    /// Build a list of commands needed to delete rules
    pub(crate) fn delete_rule(
        &mut self,
        kind: RuleKind,
        rule_id: String,
    ) -> Result<(), RemovePushRuleError> {
        self.rules.remove(kind.clone(), &rule_id)?;
        self.commands.push(Command::DeletePushRule { scope: RuleScope::Global, kind, rule_id });

        Ok(())
    }

    /// Build a list of commands needed to set whether a rule is enabled
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
            self.set_enabled(kind, rule_id, enabled)
        }
    }

    /// Build a command needed to set whether a rule is enabled
    fn set_enabled(
        &mut self,
        kind: RuleKind,
        rule_id: &str,
        enabled: bool,
    ) -> Result<(), NotificationSettingsError> {
        self.rules.set_enabled(kind.clone(), rule_id, enabled)?;
        self.commands.push(Command::SetPushRuleEnabled {
            scope: RuleScope::Global,
            kind,
            rule_id: rule_id.to_owned(),
            enabled,
        });

        Ok(())
    }

    /// Build a list of commands needed to set whether a `IsUserMention` is
    /// enabled
    fn set_user_mention_enabled(&mut self, enabled: bool) -> Result<(), NotificationSettingsError> {
        // Add a command for the `IsUserMention` `Override` rule (MSC3952).
        // This is a new push rule that may not yet be present.
        self.set_enabled(
            RuleKind::Override,
            PredefinedOverrideRuleId::IsUserMention.as_str(),
            enabled,
        )?;

        // For compatibility purpose, we still need to add commands for
        // `ContainsUserName` and `ContainsDisplayName` (deprecated rules).
        #[allow(deprecated)]
        {
            // `ContainsUserName`
            self.set_enabled(
                RuleKind::Content,
                PredefinedContentRuleId::ContainsUserName.as_str(),
                enabled,
            )?;

            // `ContainsDisplayName`
            self.set_enabled(
                RuleKind::Override,
                PredefinedOverrideRuleId::ContainsDisplayName.as_str(),
                enabled,
            )?;
        }

        Ok(())
    }

    /// Build a list of commands needed to set whether a `IsRoomMention` is
    /// enabled
    fn set_room_mention_enabled(&mut self, enabled: bool) -> Result<(), NotificationSettingsError> {
        // Sets the `IsRoomMention` `Override` rule (MSC3952).
        // This is a new push rule that may not yet be present.
        self.set_enabled(
            RuleKind::Override,
            PredefinedOverrideRuleId::IsRoomMention.as_str(),
            enabled,
        )?;

        // For compatibility purpose, we still need to set `RoomNotif` (deprecated
        // rule).
        #[allow(deprecated)]
        self.set_enabled(
            RuleKind::Override,
            PredefinedOverrideRuleId::RoomNotif.as_str(),
            enabled,
        )?;

        Ok(())
    }
}
