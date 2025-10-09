//! Ruleset utility struct

use imbl::HashSet;
use indexmap::IndexSet;
use ruma::{
    RoomId,
    push::{
        AnyPushRuleRef, PatternedPushRule, PredefinedContentRuleId, PredefinedOverrideRuleId,
        PredefinedUnderrideRuleId, PushCondition, RuleKind, Ruleset,
    },
};

use super::{RoomNotificationMode, command::Command, rule_commands::RuleCommands};
use crate::{
    error::NotificationSettingsError,
    notification_settings::{IsEncrypted, IsOneToOne},
};

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
        if let Some(rule) = self.ruleset.get(RuleKind::Room, room_id) {
            custom_rules.push((RuleKind::Room, rule.rule_id().to_owned()));
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
        if let Some(rule) = self.ruleset.get(RuleKind::Room, room_id) {
            // if this rule contains a `Notify` action
            if rule.triggers_notification() {
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
    /// * `is_encrypted` - `Yes` if the room is encrypted
    /// * `is_one_to_one` - `Yes` if the room is a direct chat involving two
    ///   people
    pub(crate) fn get_default_room_notification_mode(
        &self,
        is_encrypted: IsEncrypted,
        is_one_to_one: IsOneToOne,
    ) -> RoomNotificationMode {
        // get the correct default rule ID based on `is_encrypted` and `is_one_to_one`
        let predefined_rule_id = get_predefined_underride_room_rule_id(is_encrypted, is_one_to_one);
        let rule_id = predefined_rule_id.as_str();

        // If there is an `Underride` rule that should trigger a notification, the mode
        // is `AllMessages`
        if self
            .ruleset
            .get(RuleKind::Underride, rule_id)
            .is_some_and(|r| r.enabled() && r.triggers_notification())
        {
            RoomNotificationMode::AllMessages
        } else {
            // Otherwise, the mode is `MentionsAndKeywordsOnly`
            RoomNotificationMode::MentionsAndKeywordsOnly
        }
    }

    /// Get all room IDs for which a user-defined rule exists.
    pub(crate) fn get_rooms_with_user_defined_rules(&self, enabled: Option<bool>) -> Vec<String> {
        let test_if_enabled = enabled.is_some();
        let must_be_enabled = enabled.unwrap_or(false);

        let mut room_ids = HashSet::new();
        for rule in &self.ruleset {
            if rule.is_server_default() {
                continue;
            }
            if test_if_enabled && rule.enabled() != must_be_enabled {
                continue;
            }
            match rule {
                AnyPushRuleRef::Override(r) | AnyPushRuleRef::Underride(r) => {
                    for condition in &r.conditions {
                        if let PushCondition::EventMatch { key, pattern } = condition
                            && key == "room_id"
                        {
                            room_ids.insert(pattern.clone());
                            break;
                        }
                    }
                }
                AnyPushRuleRef::Room(r) => {
                    room_ids.insert(r.rule_id.to_string());
                }
                _ => {}
            }
        }
        Vec::from_iter(room_ids)
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
            && rule.enabled()
            && rule.triggers_notification()
        {
            return true;
        }

        #[allow(deprecated)]
        if let Some(rule) =
            self.ruleset.get(RuleKind::Content, PredefinedContentRuleId::ContainsUserName)
            && rule.enabled()
            && rule.triggers_notification()
        {
            return true;
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
        self.ruleset
            .get(RuleKind::Override, PredefinedOverrideRuleId::RoomNotif)
            .is_some_and(|r| r.enabled() && r.triggers_notification())
    }

    /// Get whether the given ruleset contains some enabled keywords rules.
    pub(crate) fn contains_keyword_rules(&self) -> bool {
        // Search for a user defined `Content` rule.
        self.ruleset.content.iter().any(|r| !r.default && r.enabled)
    }

    /// The keywords which have enabled rules.
    pub(crate) fn enabled_keywords(&self) -> IndexSet<String> {
        self.ruleset
            .content
            .iter()
            .filter(|r| !r.default && r.enabled)
            .map(|r| r.pattern.clone())
            .collect()
    }

    /// The rules for a keyword, if any.
    pub(crate) fn keyword_rules(&self, keyword: &str) -> Vec<&PatternedPushRule> {
        self.ruleset.content.iter().filter(|r| !r.default && r.pattern == keyword).collect()
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
            Err(NotificationSettingsError::RuleNotFound(rule_id.to_owned()))
        }
    }

    /// Apply a group of commands to the managed ruleset.
    ///
    /// The command may silently fail because the ruleset may have changed
    /// between the time the command was created and the time it is applied.
    pub(crate) fn apply(&mut self, commands: RuleCommands) {
        for command in commands.commands {
            match command {
                Command::DeletePushRule { kind, rule_id } => {
                    _ = self.ruleset.remove(kind, rule_id);
                }
                Command::SetRoomPushRule { .. }
                | Command::SetOverridePushRule { .. }
                | Command::SetKeywordPushRule { .. } => {
                    if let Ok(push_rule) = command.to_push_rule() {
                        _ = self.ruleset.insert(push_rule, None, None);
                    }
                }
                Command::SetPushRuleEnabled { kind, rule_id, enabled } => {
                    _ = self.ruleset.set_enabled(kind, rule_id, enabled);
                }
                Command::SetPushRuleActions { kind, rule_id, actions } => {
                    _ = self.ruleset.set_actions(kind, rule_id, actions);
                }
                Command::SetCustomPushRule { rule } => {
                    _ = self.ruleset.insert(rule, None, None);
                }
            }
        }
    }
}

/// Gets the `PredefinedUnderrideRuleId` for rooms corresponding to the given
/// criteria.
///
/// # Arguments
///
/// * `is_encrypted` - `Yes` if the room is encrypted
/// * `is_one_to_one` - `Yes` if the room is a direct chat involving two people
pub(crate) fn get_predefined_underride_room_rule_id(
    is_encrypted: IsEncrypted,
    is_one_to_one: IsOneToOne,
) -> PredefinedUnderrideRuleId {
    match (is_encrypted, is_one_to_one) {
        (IsEncrypted::Yes, IsOneToOne::Yes) => PredefinedUnderrideRuleId::EncryptedRoomOneToOne,
        (IsEncrypted::No, IsOneToOne::Yes) => PredefinedUnderrideRuleId::RoomOneToOne,
        (IsEncrypted::Yes, IsOneToOne::No) => PredefinedUnderrideRuleId::Encrypted,
        (IsEncrypted::No, IsOneToOne::No) => PredefinedUnderrideRuleId::Message,
    }
}

/// Gets the `PredefinedUnderrideRuleId` for poll start events corresponding to
/// the given criteria.
///
/// # Arguments
///
/// * `is_one_to_one` - `Yes` if the room is a direct chat involving two people
pub(crate) fn get_predefined_underride_poll_start_rule_id(
    is_one_to_one: IsOneToOne,
) -> PredefinedUnderrideRuleId {
    match is_one_to_one {
        IsOneToOne::Yes => PredefinedUnderrideRuleId::PollStartOneToOne,
        IsOneToOne::No => PredefinedUnderrideRuleId::PollStart,
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use imbl::HashSet;
    use matrix_sdk_test::{
        async_test,
        notification_settings::{
            build_ruleset, get_server_default_ruleset, server_default_ruleset_with_legacy_mentions,
        },
    };
    use ruma::{
        OwnedRoomId, RoomId,
        push::{
            Action, NewConditionalPushRule, NewPushRule, PredefinedContentRuleId,
            PredefinedOverrideRuleId, PredefinedUnderrideRuleId, PushCondition, RuleKind,
        },
    };

    use super::RuleCommands;
    use crate::{
        error::NotificationSettingsError,
        notification_settings::{
            IsEncrypted, IsOneToOne, RoomNotificationMode,
            rules::{self, Rules},
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
            rules::get_predefined_underride_room_rule_id(IsEncrypted::No, IsOneToOne::No),
            PredefinedUnderrideRuleId::Message
        );
        assert_eq!(
            rules::get_predefined_underride_room_rule_id(IsEncrypted::No, IsOneToOne::Yes),
            PredefinedUnderrideRuleId::RoomOneToOne
        );
        assert_eq!(
            rules::get_predefined_underride_room_rule_id(IsEncrypted::Yes, IsOneToOne::No),
            PredefinedUnderrideRuleId::Encrypted
        );
        assert_eq!(
            rules::get_predefined_underride_room_rule_id(IsEncrypted::Yes, IsOneToOne::Yes),
            PredefinedUnderrideRuleId::EncryptedRoomOneToOne
        );
    }

    #[async_test]
    async fn test_get_predefined_underride_poll_start_rule_id() {
        assert_eq!(
            rules::get_predefined_underride_poll_start_rule_id(IsOneToOne::No),
            PredefinedUnderrideRuleId::PollStart
        );
        assert_eq!(
            rules::get_predefined_underride_poll_start_rule_id(IsOneToOne::Yes),
            PredefinedUnderrideRuleId::PollStartOneToOne
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
        let mode = rules.get_default_room_notification_mode(IsEncrypted::No, IsOneToOne::Yes);
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
        let mode = rules.get_default_room_notification_mode(IsEncrypted::No, IsOneToOne::Yes);
        // Then the mode should be `AllMessages`
        assert_eq!(mode, RoomNotificationMode::AllMessages);
    }

    #[async_test]
    async fn test_is_user_mention_enabled() {
        // If `IsUserMention` is enable, then is_user_mention_enabled() should return
        // `true` even if the deprecated rules are disabled
        let mut ruleset = server_default_ruleset_with_legacy_mentions();
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
        assert!(
            rules
                .is_enabled(RuleKind::Override, PredefinedOverrideRuleId::IsUserMention.as_str())
                .unwrap()
        );

        // If `IsUserMention` is disabled, then is_user_mention_enabled() should return
        // `false` even if the deprecated rules are enabled
        let mut ruleset = server_default_ruleset_with_legacy_mentions();
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
        assert!(
            !rules
                .is_enabled(RuleKind::Override, PredefinedOverrideRuleId::IsUserMention.as_str())
                .unwrap()
        );
    }

    #[async_test]
    async fn test_is_room_mention_enabled() {
        // If `IsRoomMention` is present and enabled then is_room_mention_enabled()
        // should return `true` even if the deprecated rule is disabled
        let mut ruleset = server_default_ruleset_with_legacy_mentions();
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
        assert!(
            rules
                .is_enabled(RuleKind::Override, PredefinedOverrideRuleId::IsRoomMention.as_str())
                .unwrap()
        );

        // If `IsRoomMention` is present and disabled then is_room_mention_enabled()
        // should return `false` even if the deprecated rule is enabled
        let mut ruleset = server_default_ruleset_with_legacy_mentions();
        ruleset
            .set_enabled(RuleKind::Override, PredefinedOverrideRuleId::IsRoomMention, false)
            .unwrap();
        #[allow(deprecated)]
        ruleset.set_enabled(RuleKind::Override, PredefinedOverrideRuleId::RoomNotif, true).unwrap();

        let rules = Rules::new(ruleset);
        assert!(!rules.is_room_mention_enabled());
        // is_enabled() should also return `false` for
        // PredefinedOverrideRuleId::IsRoomMention
        assert!(
            !rules
                .is_enabled(RuleKind::Override, PredefinedOverrideRuleId::IsRoomMention.as_str())
                .unwrap()
        );
    }

    #[async_test]
    async fn test_is_enabled_rule_not_found() {
        let rules = Rules::new(get_server_default_ruleset());

        assert_eq!(
            rules.is_enabled(RuleKind::Override, "unknown_rule_id"),
            Err(NotificationSettingsError::RuleNotFound("unknown_rule_id".to_owned()))
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
        assert!(
            !rules
                .is_enabled(RuleKind::Override, PredefinedOverrideRuleId::Reaction.as_str())
                .unwrap()
        );
    }

    #[async_test]
    async fn test_get_rooms_with_user_defined_rules() {
        // Without user-defined rules
        let rules = Rules::new(get_server_default_ruleset());
        let room_ids = rules.get_rooms_with_user_defined_rules(None);
        assert!(room_ids.is_empty());

        // With one rule.
        let room_id = RoomId::parse("!room_a:matrix.org").unwrap();
        let ruleset = build_ruleset(vec![(RuleKind::Override, &room_id, false)]);
        let rules = Rules::new(ruleset);

        let room_ids = rules.get_rooms_with_user_defined_rules(None);
        assert_eq!(room_ids.len(), 1);

        // With duplicates
        let ruleset = build_ruleset(vec![
            (RuleKind::Override, &room_id, false),
            (RuleKind::Underride, &room_id, false),
            (RuleKind::Room, &room_id, false),
        ]);
        let rules = Rules::new(ruleset);

        let room_ids = rules.get_rooms_with_user_defined_rules(None);
        assert_eq!(room_ids.len(), 1);
        assert_eq!(room_ids[0], room_id.to_string());

        // With multiple rules
        let ruleset = build_ruleset(vec![
            (RuleKind::Room, &RoomId::parse("!room_a:matrix.org").unwrap(), false),
            (RuleKind::Room, &RoomId::parse("!room_b:matrix.org").unwrap(), false),
            (RuleKind::Room, &RoomId::parse("!room_c:matrix.org").unwrap(), false),
            (RuleKind::Override, &RoomId::parse("!room_d:matrix.org").unwrap(), false),
            (RuleKind::Underride, &RoomId::parse("!room_e:matrix.org").unwrap(), false),
        ]);
        let rules = Rules::new(ruleset);

        let room_ids = rules.get_rooms_with_user_defined_rules(None);
        assert_eq!(room_ids.len(), 5);
        let expected_set: HashSet<String> = vec![
            "!room_a:matrix.org",
            "!room_b:matrix.org",
            "!room_c:matrix.org",
            "!room_d:matrix.org",
            "!room_e:matrix.org",
        ]
        .into_iter()
        .collect();
        assert!(expected_set.symmetric_difference(HashSet::from(room_ids)).is_empty());

        // Only disabled rules
        let room_ids = rules.get_rooms_with_user_defined_rules(Some(false));
        assert_eq!(room_ids.len(), 0);

        // Only enabled rules
        let room_ids = rules.get_rooms_with_user_defined_rules(Some(true));
        assert_eq!(room_ids.len(), 5);

        let mut ruleset = build_ruleset(vec![
            (RuleKind::Room, &RoomId::parse("!room_a:matrix.org").unwrap(), false),
            (RuleKind::Room, &RoomId::parse("!room_b:matrix.org").unwrap(), false),
            (RuleKind::Override, &RoomId::parse("!room_c:matrix.org").unwrap(), false),
            (RuleKind::Underride, &RoomId::parse("!room_d:matrix.org").unwrap(), false),
        ]);
        ruleset.set_enabled(RuleKind::Room, "!room_b:matrix.org", false).unwrap();
        ruleset.set_enabled(RuleKind::Override, "!room_c:matrix.org", false).unwrap();
        let rules = Rules::new(ruleset);
        // Only room_a and room_d rules are enabled
        let room_ids = rules.get_rooms_with_user_defined_rules(Some(true));
        assert_eq!(room_ids.len(), 2);
        let expected_set: HashSet<String> =
            vec!["!room_a:matrix.org", "!room_d:matrix.org"].into_iter().collect();
        assert!(expected_set.symmetric_difference(HashSet::from(room_ids)).is_empty());
    }
}
