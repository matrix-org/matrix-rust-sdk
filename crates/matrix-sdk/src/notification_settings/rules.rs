//! Ruleset utility struct

use ruma::{
    push::{
        PredefinedContentRuleId, PredefinedOverrideRuleId, PredefinedUnderrideRuleId,
        PushCondition, RuleKind, Ruleset,
    },
    RoomId,
};

use super::{command::Command, rule_commands::RuleCommands, RoomNotificationMode};
use crate::error::NotificationSettingsError;

#[derive(Clone, Debug)]
pub(crate) struct Rules {
    pub ruleset: Ruleset,
}

impl Rules {
    pub(crate) fn new(ruleset: Ruleset) -> Self {
        Rules { ruleset }
    }

    /// Gets all user defined rules matching a given `room_id`.
    pub(crate) fn get_custom_rules_for_room(&self, room_id: &RoomId) -> Vec<(RuleKind, String)> {
        let mut custom_rules = vec![];

        // add any `Override` rules matching this `room_id`
        for rule in &self.ruleset.override_ {
            // if the rule_id is the room_id
            if &rule.rule_id == room_id || rule.conditions.iter().any(|x| matches!(
                x,
                PushCondition::EventMatch { key, pattern } if key == "room_id" && pattern == room_id
            )) {
                // the rule contains a condition matching this `room_id`
                custom_rules.push((RuleKind::Override, rule.rule_id.clone()));
            }
        }

        // add any `Room` rules matching this `room_id`
        if let Some(rule) = self.ruleset.room.iter().find(|x| x.rule_id == room_id) {
            custom_rules.push((RuleKind::Room, rule.rule_id.to_string()));
        }

        // add any `Underride` rules matching this `room_id`
        for rule in &self.ruleset.underride {
            // if the rule_id is the room_id
            if &rule.rule_id == room_id || rule.conditions.iter().any(|x| matches!(
                x,
                PushCondition::EventMatch { key, pattern } if key == "room_id" && pattern == room_id
            )) {
                // the rule contains a condition matching this `room_id`
                custom_rules.push((RuleKind::Underride, rule.rule_id.clone()));
            }
        }

        custom_rules
    }

    /// Gets the user defined notification mode for a room.
    pub(crate) fn get_user_defined_room_notification_mode(
        &self,
        room_id: &RoomId,
    ) -> Option<RoomNotificationMode> {
        // Search for an enabled `Override` rule
        if self.ruleset.override_.iter().any(|x| {
            // enabled
            x.enabled &&
            // with a condition of type `EventMatch` for this `room_id`
            // (checking on x.rule_id is not sufficient here as more than one override rule may have a condition matching on `room_id`)
            x.conditions.iter().any(|x| matches!(
                x,
                PushCondition::EventMatch { key, pattern } if key == "room_id" && pattern == room_id
            )) &&
            // and without a Notify action
            !x.actions.iter().any(|x| x.should_notify())
        }) {
            return Some(RoomNotificationMode::Mute);
        }

        // Search for an enabled `Room` rule where `rule_id` is the `room_id`
        if let Some(rule) = self.ruleset.room.iter().find(|x| x.enabled && x.rule_id == room_id) {
            // if this rule contains a `Notify` action
            if rule.actions.iter().any(|x| x.should_notify()) {
                return Some(RoomNotificationMode::AllMessages);
            }
            return Some(RoomNotificationMode::MentionsAndKeywordsOnly);
        }

        // There is no custom rule matching this `room_id`
        None
    }

    /// Gets the default notification mode for a room.
    ///
    /// # Arguments
    ///
    /// * `is_encrypted` - `true` if the room is encrypted
    /// * `members_count` - the room members count
    pub(crate) fn get_default_room_notification_mode(
        &self,
        is_encrypted: bool,
        members_count: u64,
    ) -> RoomNotificationMode {
        // get the correct default rule ID based on `is_encrypted` and `members_count`
        let predefined_rule_id = get_predefined_underride_room_rule_id(is_encrypted, members_count);
        let rule_id = predefined_rule_id.as_str();

        // If there is an `Underride` rule that should trigger a notification, the mode
        // is `AllMessages`
        if self.ruleset.underride.iter().any(|r| {
            r.enabled && r.rule_id == rule_id && r.actions.iter().any(|a| a.should_notify())
        }) {
            RoomNotificationMode::AllMessages
        } else {
            // Otherwise, the mode is `MentionsAndKeywordsOnly`
            RoomNotificationMode::MentionsAndKeywordsOnly
        }
    }

    /// Get whether the `IsUserMention` rule is enabled.
    fn is_user_mention_enabled(&self) -> bool {
        // Search for an `Override` rule `IsUserMention` (MSC3952).
        // This is a new push rule that may not yet be present.
        if let Some(rule) =
            self.ruleset.get(RuleKind::Override, PredefinedOverrideRuleId::IsUserMention)
        {
            return rule.enabled();
        }

        // Fallback to deprecated rules for compatibility.
        #[allow(deprecated)]
        if let Some(rule) =
            self.ruleset.get(RuleKind::Override, PredefinedOverrideRuleId::ContainsDisplayName)
        {
            if rule.enabled() && rule.actions().iter().any(|a| a.should_notify()) {
                return true;
            }
        }

        #[allow(deprecated)]
        if let Some(rule) =
            self.ruleset.get(RuleKind::Content, PredefinedContentRuleId::ContainsUserName)
        {
            if rule.enabled() && rule.actions().iter().any(|a| a.should_notify()) {
                return true;
            }
        }

        false
    }

    /// Get whether the `IsRoomMention` rule is enabled.
    fn is_room_mention_enabled(&self) -> bool {
        // Search for an `Override` rule `IsRoomMention` (MSC3952).
        // This is a new push rule that may not yet be present.
        if let Some(rule) =
            self.ruleset.get(RuleKind::Override, PredefinedOverrideRuleId::IsRoomMention)
        {
            return rule.enabled();
        }

        // Fallback to deprecated rule for compatibility
        #[allow(deprecated)]
        let room_notif_rule_id = PredefinedOverrideRuleId::RoomNotif.as_str();
        self.ruleset.override_.iter().any(|r| {
            r.enabled
                && r.rule_id == room_notif_rule_id
                && r.actions.iter().any(|a| a.should_notify())
        })
    }

    /// Get whether the given ruleset contains some enabled keywords rules.
    pub(crate) fn contains_keyword_rules(&self) -> bool {
        // Search for a user defined `Content` rule.
        self.ruleset.content.iter().any(|r| !r.default && r.enabled)
    }

    /// Get whether a rule is enabled.
    pub(crate) fn is_enabled(
        &self,
        kind: RuleKind,
        rule_id: &str,
    ) -> Result<bool, NotificationSettingsError> {
        if rule_id == PredefinedOverrideRuleId::IsRoomMention.as_str() {
            Ok(self.is_room_mention_enabled())
        } else if rule_id == PredefinedOverrideRuleId::IsUserMention.as_str() {
            Ok(self.is_user_mention_enabled())
        } else if let Some(rule) = self.ruleset.get(kind, rule_id) {
            Ok(rule.enabled())
        } else {
            Err(NotificationSettingsError::RuleNotFound)
        }
    }

    /// Apply a group of commands to the managed ruleset.
    ///
    /// The command may silently fail because the ruleset may have changed
    /// between the time the command was created and the time it is applied.
    pub(crate) fn apply(&mut self, commands: RuleCommands) {
        for command in commands.commands {
            match command {
                Command::DeletePushRule { scope: _, kind, rule_id } => {
                    _ = self.ruleset.remove(kind, rule_id);
                }
                Command::SetRoomPushRule { .. } | Command::SetOverridePushRule { .. } => {
                    if let Ok(push_rule) = command.to_push_rule() {
                        _ = self.ruleset.insert(push_rule, None, None);
                    }
                }
                Command::SetPushRuleEnabled { scope: _, kind, rule_id, enabled } => {
                    _ = self.ruleset.set_enabled(kind, rule_id, enabled);
                }
            }
        }
    }
}

/// Gets the `PredefinedUnderrideRuleId` corresponding to the given
/// criteria.
///
/// # Arguments
///
/// * `is_encrypted` - `true` if the room is encrypted
/// * `members_count` - the room members count
fn get_predefined_underride_room_rule_id(
    is_encrypted: bool,
    members_count: u64,
) -> PredefinedUnderrideRuleId {
    match (is_encrypted, members_count) {
        (true, 2) => PredefinedUnderrideRuleId::EncryptedRoomOneToOne,
        (false, 2) => PredefinedUnderrideRuleId::RoomOneToOne,
        (true, _) => PredefinedUnderrideRuleId::Encrypted,
        (false, _) => PredefinedUnderrideRuleId::Message,
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use matrix_sdk_test::{
        async_test,
        notification_settings::{build_ruleset, get_server_default_ruleset},
    };
    use ruma::{
        push::{
            Action, NewConditionalPushRule, NewPushRule, PredefinedContentRuleId,
            PredefinedOverrideRuleId, PredefinedUnderrideRuleId, PushCondition, RuleKind,
        },
        OwnedRoomId, RoomId,
    };

    use super::RuleCommands;
    use crate::{
        error::NotificationSettingsError,
        notification_settings::{
            rules::{self, Rules},
            RoomNotificationMode,
        },
    };

    fn get_test_room_id() -> OwnedRoomId {
        RoomId::parse("!AAAaAAAAAaaAAaaaaa:matrix.org").unwrap()
    }

    #[async_test]
    async fn test_get_custom_rules_for_room() {
        let room_id = get_test_room_id();

        let rules = Rules::new(get_server_default_ruleset());
        assert_eq!(rules.get_custom_rules_for_room(&room_id).len(), 0);

        // Initialize with one rule.
        let ruleset = build_ruleset(vec![(RuleKind::Override, &room_id, false)]);
        let rules = Rules::new(ruleset);
        assert_eq!(rules.get_custom_rules_for_room(&room_id).len(), 1);

        // Insert a Room rule
        let ruleset = build_ruleset(vec![
            (RuleKind::Override, &room_id, false),
            (RuleKind::Room, &room_id, false),
        ]);
        let rules = Rules::new(ruleset);
        assert_eq!(rules.get_custom_rules_for_room(&room_id).len(), 2);
    }

    #[async_test]
    async fn test_get_custom_rules_for_room_special_override_rule() {
        let room_id = get_test_room_id();
        let mut ruleset = get_server_default_ruleset();

        // Insert an Override rule where the rule ID doesn't match the room id,
        // but with a condition that matches
        let new_rule = NewConditionalPushRule::new(
            "custom_rule_id".to_owned(),
            vec![PushCondition::EventMatch { key: "room_id".into(), pattern: room_id.to_string() }],
            vec![Action::Notify],
        );
        ruleset.insert(NewPushRule::Override(new_rule), None, None).unwrap();

        let rules = Rules::new(ruleset);
        assert_eq!(rules.get_custom_rules_for_room(&room_id).len(), 1);
    }

    #[async_test]
    async fn test_get_user_defined_room_notification_mode() {
        let room_id = get_test_room_id();
        let rules = Rules::new(get_server_default_ruleset());
        assert_eq!(rules.get_user_defined_room_notification_mode(&room_id), None);

        // Initialize with an `Override` rule that doesn't notify
        let ruleset = build_ruleset(vec![(RuleKind::Override, &room_id, false)]);
        let rules = Rules::new(ruleset);
        assert_eq!(
            rules.get_user_defined_room_notification_mode(&room_id),
            Some(RoomNotificationMode::Mute)
        );

        // Initialize with a `Room` rule that doesn't notify
        let ruleset = build_ruleset(vec![(RuleKind::Room, &room_id, false)]);
        let rules = Rules::new(ruleset);
        assert_eq!(
            rules.get_user_defined_room_notification_mode(&room_id),
            Some(RoomNotificationMode::MentionsAndKeywordsOnly)
        );

        // Initialize with a `Room` rule that doesn't notify
        let ruleset = build_ruleset(vec![(RuleKind::Room, &room_id, true)]);
        let rules = Rules::new(ruleset);
        assert_eq!(
            rules.get_user_defined_room_notification_mode(&room_id),
            Some(RoomNotificationMode::AllMessages)
        );

        let room_id_a = RoomId::parse("!AAAaAAAAAaaAAaaaaa:matrix.org").unwrap();
        let room_id_b = RoomId::parse("!BBBbBBBBBbbBBbbbbb:matrix.org").unwrap();
        let ruleset = build_ruleset(vec![
            // A mute rule for room_id_a
            (RuleKind::Override, &room_id_a, false),
            // A notifying rule for room_id_b
            (RuleKind::Override, &room_id_b, true),
        ]);
        let rules = Rules::new(ruleset);
        let mode = rules.get_user_defined_room_notification_mode(&room_id_a);

        // The mode should be Mute as there is an Override rule that doesn't notify,
        // with a condition matching the room_id_a
        assert_eq!(mode, Some(RoomNotificationMode::Mute));
    }

    #[async_test]
    async fn test_get_predefined_underride_room_rule_id() {
        assert_eq!(
            rules::get_predefined_underride_room_rule_id(false, 3),
            PredefinedUnderrideRuleId::Message
        );
        assert_eq!(
            rules::get_predefined_underride_room_rule_id(false, 2),
            PredefinedUnderrideRuleId::RoomOneToOne
        );
        assert_eq!(
            rules::get_predefined_underride_room_rule_id(true, 3),
            PredefinedUnderrideRuleId::Encrypted
        );
        assert_eq!(
            rules::get_predefined_underride_room_rule_id(true, 2),
            PredefinedUnderrideRuleId::EncryptedRoomOneToOne
        );
    }

    #[async_test]
    async fn test_get_default_room_notification_mode_mentions_and_keywords() {
        let mut ruleset = get_server_default_ruleset();
        // If the corresponding underride rule is disabled
        ruleset
            .set_enabled(RuleKind::Underride, PredefinedUnderrideRuleId::RoomOneToOne, false)
            .unwrap();

        let rules = Rules::new(ruleset);
        let mode = rules.get_default_room_notification_mode(false, 2);
        // Then the mode should be `MentionsAndKeywordsOnly`
        assert_eq!(mode, RoomNotificationMode::MentionsAndKeywordsOnly);
    }

    #[async_test]
    async fn test_get_default_room_notification_mode_all_messages() {
        let mut ruleset = get_server_default_ruleset();
        // If the corresponding underride rule is enabled
        ruleset
            .set_enabled(RuleKind::Underride, PredefinedUnderrideRuleId::RoomOneToOne, true)
            .unwrap();

        let rules = Rules::new(ruleset);
        let mode = rules.get_default_room_notification_mode(false, 2);
        // Then the mode should be `AllMessages`
        assert_eq!(mode, RoomNotificationMode::AllMessages);
    }

    #[async_test]
    async fn test_is_user_mention_enabled() {
        // If `IsUserMention` is enable, then is_user_mention_enabled() should return
        // `true` even if the deprecated rules are disabled
        let mut ruleset = get_server_default_ruleset();
        ruleset
            .set_enabled(RuleKind::Override, PredefinedOverrideRuleId::IsUserMention, true)
            .unwrap();
        #[allow(deprecated)]
        ruleset
            .set_enabled(RuleKind::Override, PredefinedOverrideRuleId::ContainsDisplayName, false)
            .unwrap();
        #[allow(deprecated)]
        ruleset
            .set_enabled(RuleKind::Content, PredefinedContentRuleId::ContainsUserName, false)
            .unwrap();

        let rules = Rules::new(ruleset);
        assert!(rules.is_user_mention_enabled());
        // is_enabled() should also return `true` for
        // PredefinedOverrideRuleId::IsUserMention
        assert!(rules
            .is_enabled(RuleKind::Override, PredefinedOverrideRuleId::IsUserMention.as_str())
            .unwrap());

        // If `IsUserMention` is disabled, then is_user_mention_enabled() should return
        // `false` even if the deprecated rules are enabled
        let mut ruleset = get_server_default_ruleset();
        ruleset
            .set_enabled(RuleKind::Override, PredefinedOverrideRuleId::IsUserMention, false)
            .unwrap();
        #[allow(deprecated)]
        ruleset
            .set_actions(
                RuleKind::Override,
                PredefinedOverrideRuleId::ContainsDisplayName,
                vec![Action::Notify],
            )
            .unwrap();
        #[allow(deprecated)]
        ruleset
            .set_enabled(RuleKind::Override, PredefinedOverrideRuleId::ContainsDisplayName, true)
            .unwrap();
        #[allow(deprecated)]
        ruleset
            .set_enabled(RuleKind::Content, PredefinedContentRuleId::ContainsUserName, true)
            .unwrap();

        let rules = Rules::new(ruleset);
        assert!(!rules.is_user_mention_enabled());
        // is_enabled() should also return `false` for
        // PredefinedOverrideRuleId::IsUserMention
        assert!(!rules
            .is_enabled(RuleKind::Override, PredefinedOverrideRuleId::IsUserMention.as_str())
            .unwrap());
    }

    #[async_test]
    async fn test_is_room_mention_enabled() {
        // If `IsRoomMention` is present and enabled then is_room_mention_enabled()
        // should return `true` even if the deprecated rule is disabled
        let mut ruleset = get_server_default_ruleset();
        ruleset
            .set_enabled(RuleKind::Override, PredefinedOverrideRuleId::IsRoomMention, true)
            .unwrap();
        #[allow(deprecated)]
        ruleset
            .set_enabled(RuleKind::Override, PredefinedOverrideRuleId::RoomNotif, false)
            .unwrap();

        let rules = Rules::new(ruleset);
        assert!(rules.is_room_mention_enabled());
        // is_enabled() should also return `true` for
        // PredefinedOverrideRuleId::IsRoomMention
        assert!(rules
            .is_enabled(RuleKind::Override, PredefinedOverrideRuleId::IsRoomMention.as_str())
            .unwrap());

        // If `IsRoomMention` is present and disabled then is_room_mention_enabled()
        // should return `false` even if the deprecated rule is enabled
        let mut ruleset = get_server_default_ruleset();
        ruleset
            .set_enabled(RuleKind::Override, PredefinedOverrideRuleId::IsRoomMention, false)
            .unwrap();
        #[allow(deprecated)]
        ruleset.set_enabled(RuleKind::Override, PredefinedOverrideRuleId::RoomNotif, true).unwrap();

        let rules = Rules::new(ruleset);
        assert!(!rules.is_room_mention_enabled());
        // is_enabled() should also return `false` for
        // PredefinedOverrideRuleId::IsRoomMention
        assert!(!rules
            .is_enabled(RuleKind::Override, PredefinedOverrideRuleId::IsRoomMention.as_str())
            .unwrap());
    }

    #[async_test]
    async fn test_is_enabled_rule_not_found() {
        let rules = Rules::new(get_server_default_ruleset());

        assert_eq!(
            rules.is_enabled(RuleKind::Override, "unknown_rule_id"),
            Err(NotificationSettingsError::RuleNotFound)
        );
    }

    #[async_test]
    async fn test_apply_delete_command() {
        let room_id = get_test_room_id();
        // Initialize with a custom rule
        let ruleset = build_ruleset(vec![(RuleKind::Override, &room_id, false)]);
        let mut rules = Rules::new(ruleset);

        // Build a `RuleCommands` deleting this rule
        let mut rules_commands = RuleCommands::new(rules.ruleset.clone());
        rules_commands.delete_rule(RuleKind::Override, room_id.to_string()).unwrap();

        rules.apply(rules_commands);

        // The rule must have been removed from the updated rules
        assert!(rules.get_custom_rules_for_room(&room_id).is_empty());
    }

    #[async_test]
    async fn test_apply_set_command() {
        let room_id = get_test_room_id();
        let mut rules = Rules::new(get_server_default_ruleset());

        // Build a `RuleCommands` inserting a rule
        let mut rules_commands = RuleCommands::new(rules.ruleset.clone());
        rules_commands.insert_rule(RuleKind::Override, &room_id, false).unwrap();

        rules.apply(rules_commands);

        // The rule must have been removed from the updated rules
        assert_eq!(rules.get_custom_rules_for_room(&room_id).len(), 1);
    }

    #[async_test]
    async fn test_apply_set_enabled_command() {
        let mut rules = Rules::new(get_server_default_ruleset());

        rules
            .ruleset
            .set_enabled(RuleKind::Override, PredefinedOverrideRuleId::Reaction, true)
            .unwrap();

        // Build a `RuleCommands` disabling the rule
        let mut rules_commands = RuleCommands::new(rules.ruleset.clone());
        rules_commands
            .set_rule_enabled(
                RuleKind::Override,
                PredefinedOverrideRuleId::Reaction.as_str(),
                false,
            )
            .unwrap();

        rules.apply(rules_commands);

        // The rule must have been disabled in the updated rules
        assert!(!rules
            .is_enabled(RuleKind::Override, PredefinedOverrideRuleId::Reaction.as_str())
            .unwrap());
    }
}
