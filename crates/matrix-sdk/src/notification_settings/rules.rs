//! Ruleset utility struct

use ruma::{
    api::client::push::RuleScope,
    push::{
        Action, NewConditionalPushRule, NewPushRule, NewSimplePushRule, PredefinedContentRuleId,
        PredefinedOverrideRuleId, PredefinedUnderrideRuleId, PushCondition, RemovePushRuleError,
        RuleKind, Ruleset, Tweak,
    },
    RoomId,
};

use super::RoomNotificationMode;
use crate::error::NotificationSettingsError;

/// enum describing the commands required to modify the owner's account data.
#[derive(Clone, Debug)]
#[allow(dead_code)]
pub(crate) enum Command {
    /// Set a new push rule
    SetPushRule { scope: RuleScope, rule: NewPushRule },
    /// Set whether a push rule is enabled
    SetPushRuleEnabled { scope: RuleScope, kind: RuleKind, rule_id: String, enabled: bool },
    /// Delete a push rule
    DeletePushRule { scope: RuleScope, kind: RuleKind, rule_id: String },
}

pub(crate) struct Rules {
    pub ruleset: Ruleset,
}

#[allow(dead_code)]
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
    pub(crate) fn is_user_mention_enabled(&self) -> bool {
        // Search for an enabled `Override` rule `IsUserMention` (MSC3952).
        // This is a new push rule that may not yet be present.
        if let Some(rule) =
            self.ruleset.get(RuleKind::Override, PredefinedOverrideRuleId::IsUserMention)
        {
            if rule.enabled() {
                return true;
            }
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
    pub(crate) fn is_room_mention_enabled(&self) -> bool {
        // Search for an enabled `Override` rule `IsRoomMention` (MSC3952).
        // This is a new push rule that may not yet be present.
        if let Some(rule) =
            self.ruleset.get(RuleKind::Override, PredefinedOverrideRuleId::IsRoomMention)
        {
            if rule.enabled() {
                return true;
            }
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

    /// Insert a new `Room` push rule for a given `room_id` and return an
    /// optional `Command` describing the action to be performed on the
    /// user's account data.
    pub(crate) fn insert_room_rule(
        &mut self,
        kind: RuleKind,
        room_id: &RoomId,
        notify: bool,
    ) -> Result<Option<Command>, NotificationSettingsError> {
        let command;
        let actions = if notify {
            vec![Action::Notify, Action::SetTweak(Tweak::Sound("default".into()))]
        } else {
            vec![]
        };

        match kind {
            RuleKind::Override => {
                // Insert a new push rule matching this `room_id`
                let new_rule = NewConditionalPushRule::new(
                    room_id.to_string(),
                    vec![PushCondition::EventMatch {
                        key: "room_id".into(),
                        pattern: room_id.to_string(),
                    }],
                    actions,
                );
                let new_rule = NewPushRule::Override(new_rule);
                self.ruleset.insert(new_rule.clone(), None, None)?;
                command = Some(Command::SetPushRule { scope: RuleScope::Global, rule: new_rule });
            }
            RuleKind::Room => {
                // Insert a new `Room` push rule for this `room_id`
                let new_rule = NewSimplePushRule::new(room_id.to_owned(), actions);
                let new_rule = NewPushRule::Room(new_rule);
                self.ruleset.insert(new_rule.clone(), None, None)?;
                command = Some(Command::SetPushRule { scope: RuleScope::Global, rule: new_rule });
            }
            _ => {
                return Err(NotificationSettingsError::InvalidParameter(
                    "kind must be either Override or Room.".to_owned(),
                ))
            }
        }

        Ok(command)
    }

    /// Deletes a list of rules and returns a list of `Command` describing the
    /// actions to be performed on the user's account data.
    ///
    /// # Arguments
    ///
    /// * `rules` - A list of rules to delete
    /// * `exceptions` - A list of rules to ignore
    pub(crate) fn delete_rules(
        &mut self,
        rules: &[(RuleKind, String)],
        exceptions: &[(RuleKind, String)],
    ) -> Result<Vec<Command>, RemovePushRuleError> {
        let mut commands = vec![];

        for (rule_kind, rule_id) in rules {
            // Ignore rules present in exceptions
            if exceptions.iter().any(|e| e.0 == *rule_kind && e.1 == *rule_id) {
                continue;
            }

            self.ruleset.remove(rule_kind.clone(), rule_id)?;
            commands.push(Command::DeletePushRule {
                scope: RuleScope::Global,
                kind: rule_kind.clone(),
                rule_id: rule_id.clone(),
            })
        }
        Ok(commands)
    }

    /// Sets whether a rule is enabled and returns a list of `Command`
    /// describing the actions to be performed on the user's account data.
    pub(crate) fn set_enabled(
        &mut self,
        kind: RuleKind,
        rule_id: &str,
        enabled: bool,
    ) -> Result<Vec<Command>, NotificationSettingsError> {
        if rule_id == PredefinedOverrideRuleId::IsRoomMention.as_str() {
            // Handle specific case for `PredefinedOverrideRuleId::IsRoomMention`
            self.set_room_mention_enabled(enabled)
        } else if rule_id == PredefinedOverrideRuleId::IsUserMention.as_str() {
            // Handle specific case for `PredefinedOverrideRuleId::IsUserMention`
            self.set_user_mention_enabled(enabled)
        } else {
            let mut commands = vec![];
            self.set_rule_enabled(kind, rule_id, enabled, &mut commands)?;
            Ok(commands)
        }
    }

    /// Helper function to enable or disable a rule and update a list of
    /// commands to be performed on the user's account data to reflect the
    /// change.
    fn set_rule_enabled(
        &mut self,
        kind: RuleKind,
        rule_id: &str,
        enabled: bool,
        commands: &mut Vec<Command>,
    ) -> Result<(), NotificationSettingsError> {
        self.ruleset.set_enabled(kind.clone(), rule_id, enabled)?;
        commands.push(Command::SetPushRuleEnabled {
            scope: RuleScope::Global,
            kind,
            rule_id: rule_id.to_owned(),
            enabled,
        });

        Ok(())
    }

    /// Set whether the `IsUserMention` rule is enabled and returns a list of
    /// `Command` describing the actions to be performed on the user's account
    /// data.
    fn set_user_mention_enabled(
        &mut self,
        enabled: bool,
    ) -> Result<Vec<Command>, NotificationSettingsError> {
        let mut commands = vec![];

        // Sets the `IsUserMention` `Override` rule (MSC3952).
        // This is a new push rule that may not yet be present.
        self.set_rule_enabled(
            RuleKind::Override,
            PredefinedOverrideRuleId::IsUserMention.as_str(),
            enabled,
            &mut commands,
        )?;

        // For compatibility purpose, we still need to set `ContainsUserName` and
        // `ContainsDisplayName` (deprecated rules).
        #[allow(deprecated)]
        {
            // `ContainsUserName`
            self.set_rule_enabled(
                RuleKind::Content,
                PredefinedContentRuleId::ContainsUserName.as_str(),
                enabled,
                &mut commands,
            )?;

            // `ContainsDisplayName`
            self.set_rule_enabled(
                RuleKind::Override,
                PredefinedOverrideRuleId::ContainsDisplayName.as_str(),
                enabled,
                &mut commands,
            )?;
        }

        Ok(commands)
    }

    /// Set whether the `IsRoomMention` rule is enabled and returns a list of
    /// `Command` describing the actions to be performed on the user's account
    /// data.
    fn set_room_mention_enabled(
        &mut self,
        enabled: bool,
    ) -> Result<Vec<Command>, NotificationSettingsError> {
        let mut commands = vec![];

        // Sets the `IsRoomMention` `Override` rule (MSC3952).
        // This is a new push rule that may not yet be present.
        self.set_rule_enabled(
            RuleKind::Override,
            PredefinedOverrideRuleId::IsRoomMention.as_str(),
            enabled,
            &mut commands,
        )?;

        // For compatibility purpose, we still need to set `RoomNotif` (deprecated
        // rule).
        #[allow(deprecated)]
        self.set_rule_enabled(
            RuleKind::Override,
            PredefinedOverrideRuleId::RoomNotif.as_str(),
            enabled,
            &mut commands,
        )?;

        Ok(commands)
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
    use assert_matches::assert_matches;
    use matrix_sdk_test::async_test;
    use ruma::{
        api::client::push::RuleScope,
        push::{
            Action, AnyPushRule, NewConditionalPushRule, NewPushRule, NewSimplePushRule,
            PredefinedContentRuleId, PredefinedOverrideRuleId, PredefinedUnderrideRuleId,
            PushCondition, RuleKind, Ruleset,
        },
        OwnedRoomId, RoomId, UserId,
    };

    use super::Command;
    use crate::{
        error::NotificationSettingsError,
        notification_settings::{
            rules::{self, Rules},
            RoomNotificationMode,
        },
    };

    fn get_server_default_ruleset() -> Ruleset {
        let user_id = UserId::parse("@user:matrix.org").unwrap();
        Ruleset::server_default(&user_id)
    }

    fn get_test_room_id() -> OwnedRoomId {
        RoomId::parse("!AAAaAAAAAaaAAaaaaa:matrix.org").unwrap()
    }

    #[async_test]
    async fn test_get_custom_rules_for_room() {
        let room_id = get_test_room_id();

        let mut rules = Rules::new(get_server_default_ruleset());
        assert_eq!(rules.get_custom_rules_for_room(&room_id).len(), 0);

        // Insert an Override rule
        rules.insert_room_rule(ruma::push::RuleKind::Override, &room_id, false).unwrap();
        assert_eq!(rules.get_custom_rules_for_room(&room_id).len(), 1);

        // Insert a Room rule
        rules.insert_room_rule(ruma::push::RuleKind::Room, &room_id, false).unwrap();
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
    async fn test_get_user_defined_room_notification_mode_none() {
        let room_id = get_test_room_id();
        let rules = Rules::new(get_server_default_ruleset());
        assert_eq!(rules.get_user_defined_room_notification_mode(&room_id), None);
    }

    #[async_test]
    async fn test_get_user_defined_room_notification_mode_mute() {
        let room_id = get_test_room_id();
        let mut rules = Rules::new(get_server_default_ruleset());

        // Insert an Override rule that doesn't notify
        rules.insert_room_rule(ruma::push::RuleKind::Override, &room_id, false).unwrap();
        let mode = rules.get_user_defined_room_notification_mode(&room_id);
        assert_eq!(mode, Some(RoomNotificationMode::Mute));
    }

    #[async_test]
    async fn test_get_user_defined_room_notification_mode_mentions_and_keywords() {
        let room_id = get_test_room_id();
        let mut rules = Rules::new(get_server_default_ruleset());

        // Insert a Room rule that doesn't notify
        rules.insert_room_rule(ruma::push::RuleKind::Room, &room_id, false).unwrap();
        let mode = rules.get_user_defined_room_notification_mode(&room_id);
        assert_eq!(mode, Some(RoomNotificationMode::MentionsAndKeywordsOnly));
    }

    #[async_test]
    async fn test_get_user_defined_room_notification_mode_all_messages() {
        let room_id = get_test_room_id();
        let mut rules = Rules::new(get_server_default_ruleset());

        // Insert a Room rule that notifies
        rules.insert_room_rule(ruma::push::RuleKind::Room, &room_id, true).unwrap();
        let mode = rules.get_user_defined_room_notification_mode(&room_id);
        assert_eq!(mode, Some(RoomNotificationMode::AllMessages));
    }

    #[async_test]
    async fn test_get_user_defined_room_notification_mode_multiple_override_rules() {
        let room_id_a = RoomId::parse("!AAAaAAAAAaaAAaaaaa:matrix.org").unwrap();
        let room_id_b = RoomId::parse("!BBBbBBBBBbbBBbbbbb:matrix.org").unwrap();

        let mut rules = Rules::new(get_server_default_ruleset());

        // Insert the muting rule
        rules.insert_room_rule(ruma::push::RuleKind::Override, &room_id_a, false).unwrap();
        // Insert another muting rule for another room (it will be inserted before the
        // previous one)
        rules.insert_room_rule(ruma::push::RuleKind::Override, &room_id_b, true).unwrap();

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
        // `true`
        let mut ruleset = get_server_default_ruleset();
        ruleset
            .set_enabled(RuleKind::Override, PredefinedOverrideRuleId::IsUserMention, true)
            .unwrap();
        let rules = Rules::new(ruleset);
        assert!(rules.is_user_mention_enabled());
        // is_enabled() should also return `true` for
        // PredefinedOverrideRuleId::IsUserMention
        assert!(rules
            .is_enabled(RuleKind::Override, PredefinedOverrideRuleId::IsUserMention.as_str())
            .unwrap());

        // If `IsUserMention` is disabled, and one of the deprecated rules is enabled,
        // then is_user_mention_enabled() should return `true`
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
            .set_enabled(RuleKind::Content, PredefinedContentRuleId::ContainsUserName, false)
            .unwrap();

        let rules = Rules::new(ruleset);
        assert!(rules.is_user_mention_enabled());
        // is_enabled() should also return `true` for
        // PredefinedOverrideRuleId::IsUserMention
        assert!(rules
            .is_enabled(RuleKind::Override, PredefinedOverrideRuleId::IsUserMention.as_str())
            .unwrap());

        // If `IsUserMention` is disabled, and none of the deprecated rules is enabled,
        // then is_user_mention_enabled() should return `false`
        let mut ruleset = get_server_default_ruleset();
        ruleset
            .set_enabled(RuleKind::Override, PredefinedOverrideRuleId::IsUserMention, false)
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
        assert!(!rules.is_user_mention_enabled());
        // is_enabled() should also return `false` for
        // PredefinedOverrideRuleId::IsUserMention
        assert!(!rules
            .is_enabled(RuleKind::Override, PredefinedOverrideRuleId::IsUserMention.as_str())
            .unwrap());
    }

    #[async_test]
    async fn test_is_room_mention_enabled() {
        // If `IsRoomMention` is enable, then is_room_mention_enabled() should return
        // `true`
        let mut ruleset = get_server_default_ruleset();
        ruleset
            .set_enabled(RuleKind::Override, PredefinedOverrideRuleId::IsRoomMention, true)
            .unwrap();
        let rules = Rules::new(ruleset);
        assert!(rules.is_room_mention_enabled());
        // is_enabled() should also return `true` for
        // PredefinedOverrideRuleId::IsRoomMention
        assert!(rules
            .is_enabled(RuleKind::Override, PredefinedOverrideRuleId::IsRoomMention.as_str())
            .unwrap());

        // If `IsRoomMention` is not present, and the deprecated rules is enabled,
        // then is_room_mention_enabled() should return `true`
        let mut ruleset = get_server_default_ruleset();
        ruleset
            .set_enabled(RuleKind::Override, PredefinedOverrideRuleId::IsRoomMention, false)
            .unwrap();
        #[allow(deprecated)]
        ruleset.set_enabled(RuleKind::Override, PredefinedOverrideRuleId::RoomNotif, true).unwrap();
        #[allow(deprecated)]
        ruleset
            .set_actions(
                RuleKind::Override,
                PredefinedOverrideRuleId::RoomNotif,
                vec![Action::Notify],
            )
            .unwrap();
        let rules = Rules::new(ruleset);
        assert!(rules.is_room_mention_enabled());
        // is_enabled() should also return `true` for
        // PredefinedOverrideRuleId::IsRoomMention
        assert!(rules
            .is_enabled(RuleKind::Override, PredefinedOverrideRuleId::IsRoomMention.as_str())
            .unwrap());

        // If `IsRoomMention` is disabled, and the deprecated rules is disabled,
        // then is_room_mention_enabled() should return `false`
        let mut ruleset = get_server_default_ruleset();
        ruleset
            .set_enabled(RuleKind::Override, PredefinedOverrideRuleId::IsRoomMention, false)
            .unwrap();
        #[allow(deprecated)]
        ruleset
            .set_enabled(RuleKind::Override, PredefinedOverrideRuleId::RoomNotif, false)
            .unwrap();

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
    async fn test_insert_room_rule_override() {
        let room_id = get_test_room_id();
        let mut rules = Rules::new(get_server_default_ruleset());

        let command = rules.insert_room_rule(RuleKind::Override, &room_id, true).unwrap();

        // The ruleset should contains the new rule
        let new_rule = rules.ruleset.get(RuleKind::Override, &room_id).unwrap();

        assert_matches!(
            new_rule.to_owned(),
            AnyPushRule::Override(rule) => {
                assert_eq!(rule.rule_id, room_id);
                assert!(rule.conditions.iter().any(|x| matches!(
                    x,
                    PushCondition::EventMatch { key, pattern } if key == "room_id" && *pattern == room_id
                )))
            }
        );

        // The command list should contains only a SetPushRule command
        assert_matches!(
            command,
            Some(Command::SetPushRule { scope, rule }) => {
                assert_eq!(scope, RuleScope::Global);
                assert_matches!(
                    rule,
                    NewPushRule::Override(rule) => {
                        assert_eq!(rule.rule_id, room_id);
                        assert!(rule.conditions.iter().any(|x| matches!(
                            x,
                            PushCondition::EventMatch { key, pattern } if key == "room_id" && *pattern == room_id
                        )))
                    }
                )
            }
        );
    }

    #[async_test]
    async fn test_insert_room_rule_room() {
        let room_id = get_test_room_id();
        let mut rules = Rules::new(get_server_default_ruleset());

        let command = rules.insert_room_rule(RuleKind::Room, &room_id, true).unwrap();

        // The ruleset should contains the new rule
        let new_rule =
            rules.ruleset.get(RuleKind::Room, &room_id).expect("a new Room rule is expected.");

        assert_matches!(
            new_rule.to_owned(),
            AnyPushRule::Room(rule) => {
                assert_eq!(rule.rule_id, room_id);
            }
        );

        // The command list should contains only a SetPushRule command
        assert_matches!(
            command,
            Some(Command::SetPushRule { scope, rule }) => {
                assert_eq!(scope, RuleScope::Global);
                assert_matches!(
                    rule,
                    NewPushRule::Room(rule) => {
                        assert_eq!(rule.rule_id, room_id);
                    }
                )
            }
        );
    }

    #[async_test]
    async fn test_insert_room_rule_invalid_kind() {
        let room_id = get_test_room_id();
        let mut rules = Rules::new(get_server_default_ruleset());

        rules
            .insert_room_rule(RuleKind::Content, &room_id, true)
            .expect_err("An InvalidParameter error is expected");
    }

    #[async_test]
    async fn test_delete_rules() {
        let room_id = get_test_room_id();
        let mut rules = Rules::new(get_server_default_ruleset());

        rules.delete_rules(&[(RuleKind::Room, room_id.to_string())], &[]).expect_err(
            "A RemovePushRuleError is expected while trying to delete an unknown rule.",
        );

        let new_rule = NewSimplePushRule::new(room_id.to_owned(), vec![]);
        let new_rule = NewPushRule::Room(new_rule);
        rules.ruleset.insert(new_rule, None, None).unwrap();

        let commands = rules.delete_rules(&[(RuleKind::Room, room_id.to_string())], &[]).unwrap();
        // The command list should contains only a SetPushRule command
        assert_eq!(commands.len(), 1);

        assert_matches!(
            &commands[0],
            Command::DeletePushRule { scope, kind, rule_id } => {
                assert_eq!(scope, &RuleScope::Global);
                assert_eq!(kind, &RuleKind::Room);
                assert_eq!(rule_id, room_id.as_str());
            }
        );
    }

    #[async_test]
    async fn test_delete_rules_with_exceptions() {
        let room_id = get_test_room_id();
        let mut rules = Rules::new(get_server_default_ruleset());

        let new_rule = NewSimplePushRule::new(room_id.to_owned(), vec![]);
        let new_rule = NewPushRule::Room(new_rule);
        rules.ruleset.insert(new_rule, None, None).unwrap();

        let rules_to_delete = vec![(RuleKind::Room, room_id.to_string())];
        let commands = rules.delete_rules(&rules_to_delete, &rules_to_delete).unwrap();
        // The command list should be empty as the rule to delete is also in the
        // exceptions.
        assert_eq!(commands.len(), 0);
    }

    #[async_test]
    async fn test_set_enabled() {
        let mut rules = Rules::new(get_server_default_ruleset());

        // Initialize the PredefinedOverrideRuleId::Reaction rule to enabled
        rules
            .ruleset
            .set_enabled(RuleKind::Override, PredefinedOverrideRuleId::Reaction, true)
            .unwrap();
        // Ensure the initial state is `true`
        let initial_state = rules
            .ruleset
            .get(RuleKind::Override, PredefinedOverrideRuleId::Reaction)
            .unwrap()
            .enabled();
        assert!(initial_state);
        // Disable the PredefinedOverrideRuleId::Reaction rule
        let commands = rules
            .set_enabled(RuleKind::Override, PredefinedOverrideRuleId::Reaction.as_str(), false)
            .unwrap();
        let new_enabled_state = rules
            .ruleset
            .get(RuleKind::Override, PredefinedOverrideRuleId::Reaction)
            .unwrap()
            .enabled();
        // The new enabled state should be `false`
        assert!(!new_enabled_state);

        // The command list should contains only a SetPushRuleEnabled command
        assert_eq!(commands.len(), 1);

        assert_matches!(
            &commands[0],
            Command::SetPushRuleEnabled { scope, kind, rule_id, enabled } => {
                assert_eq!(scope, &RuleScope::Global);
                assert_eq!(kind, &RuleKind::Override);
                assert_eq!(rule_id, &PredefinedOverrideRuleId::Reaction.to_string());
                assert!(!enabled);
            }
        );
    }

    #[async_test]
    async fn test_set_is_room_mention_enabled() {
        let mut rules = Rules::new(get_server_default_ruleset());

        // Initialize PredefinedOverrideRuleId::IsRoomMention and
        // PredefinedOverrideRuleId::RoomNotif rules to disabled
        rules
            .ruleset
            .set_enabled(RuleKind::Override, PredefinedOverrideRuleId::IsRoomMention, false)
            .unwrap();
        #[allow(deprecated)]
        rules
            .ruleset
            .set_enabled(RuleKind::Override, PredefinedOverrideRuleId::RoomNotif, false)
            .unwrap();

        // Ensure the initial state is `false`
        let is_room_mention_enabled = rules
            .ruleset
            .get(RuleKind::Override, PredefinedOverrideRuleId::IsRoomMention)
            .unwrap()
            .enabled();
        assert!(!is_room_mention_enabled);
        #[allow(deprecated)]
        let room_notif_enabled = rules
            .ruleset
            .get(RuleKind::Override, PredefinedOverrideRuleId::RoomNotif)
            .unwrap()
            .enabled();
        assert!(!room_notif_enabled);

        // Enable the PredefinedOverrideRuleId::IsRoomMention rule
        let commands = rules
            .set_enabled(RuleKind::Override, PredefinedOverrideRuleId::IsRoomMention.as_str(), true)
            .unwrap();

        // Ensure the new state is `true` for both rules
        let is_room_mention_enabled = rules
            .ruleset
            .get(RuleKind::Override, PredefinedOverrideRuleId::IsRoomMention)
            .unwrap()
            .enabled();
        assert!(is_room_mention_enabled);
        #[allow(deprecated)]
        let room_notif_enabled = rules
            .ruleset
            .get(RuleKind::Override, PredefinedOverrideRuleId::RoomNotif)
            .unwrap()
            .enabled();
        assert!(room_notif_enabled);

        // The command list should contains two SetPushRuleEnabled
        assert_eq!(commands.len(), 2);
        assert_matches!(
            &commands[0],
            Command::SetPushRuleEnabled { scope, kind, rule_id, enabled } => {
                assert_eq!(scope, &RuleScope::Global);
                assert_eq!(kind, &RuleKind::Override);
                assert_eq!(rule_id, &PredefinedOverrideRuleId::IsRoomMention.to_string());
                assert!(enabled);
            }
        );

        assert_matches!(
            &commands[1],
            Command::SetPushRuleEnabled { scope, kind, rule_id, enabled } => {
                assert_eq!(scope, &RuleScope::Global);
                assert_eq!(kind, &RuleKind::Override);
                #[allow(deprecated)]
                let expected_rule_id = PredefinedOverrideRuleId::RoomNotif;
                assert_eq!(rule_id, &expected_rule_id.to_string());
                assert!(enabled);
            }
        );
    }

    #[async_test]
    async fn test_set_is_user_mention_enabled() {
        let mut rules = Rules::new(get_server_default_ruleset());

        // Initialize PredefinedOverrideRuleId::IsRoomMention and
        // PredefinedOverrideRuleId::RoomNotif rules to disabled
        rules
            .ruleset
            .set_enabled(RuleKind::Override, PredefinedOverrideRuleId::IsUserMention, false)
            .unwrap();
        #[allow(deprecated)]
        rules
            .ruleset
            .set_enabled(RuleKind::Override, PredefinedOverrideRuleId::ContainsDisplayName, false)
            .unwrap();
        #[allow(deprecated)]
        rules
            .ruleset
            .set_enabled(RuleKind::Content, PredefinedContentRuleId::ContainsUserName, false)
            .unwrap();

        // Ensure the initial state is `false`
        let is_user_mention_enabled = rules
            .ruleset
            .get(RuleKind::Override, PredefinedOverrideRuleId::IsUserMention)
            .unwrap()
            .enabled();
        assert!(!is_user_mention_enabled);
        #[allow(deprecated)]
        let contains_display_name_enabled = rules
            .ruleset
            .get(RuleKind::Override, PredefinedOverrideRuleId::ContainsDisplayName)
            .unwrap()
            .enabled();
        assert!(!contains_display_name_enabled);
        #[allow(deprecated)]
        let contains_user_name_enabled = rules
            .ruleset
            .get(RuleKind::Content, PredefinedContentRuleId::ContainsUserName)
            .unwrap()
            .enabled();
        assert!(!contains_user_name_enabled);

        // Enable the PredefinedOverrideRuleId::IsUserMention rule
        let commands = rules
            .set_enabled(RuleKind::Override, PredefinedOverrideRuleId::IsUserMention.as_str(), true)
            .unwrap();

        // Ensure the new state is `true` for all corresponding rules
        let is_user_mention_enabled = rules
            .ruleset
            .get(RuleKind::Override, PredefinedOverrideRuleId::IsUserMention)
            .unwrap()
            .enabled();
        assert!(is_user_mention_enabled);
        #[allow(deprecated)]
        let contains_display_name_enabled = rules
            .ruleset
            .get(RuleKind::Override, PredefinedOverrideRuleId::ContainsDisplayName)
            .unwrap()
            .enabled();
        assert!(contains_display_name_enabled);
        #[allow(deprecated)]
        let contains_user_name_enabled = rules
            .ruleset
            .get(RuleKind::Content, PredefinedContentRuleId::ContainsUserName)
            .unwrap()
            .enabled();
        assert!(contains_user_name_enabled);

        // The command list should contains 3 SetPushRuleEnabled
        assert_eq!(commands.len(), 3);
        assert_matches!(
            &commands[0],
            Command::SetPushRuleEnabled { scope, kind, rule_id, enabled } => {
                assert_eq!(scope, &RuleScope::Global);
                assert_eq!(kind, &RuleKind::Override);
                assert_eq!(rule_id, &PredefinedOverrideRuleId::IsUserMention.to_string());
                assert!(enabled);
            }
        );
        assert_matches!(
            &commands[1],
            Command::SetPushRuleEnabled { scope, kind, rule_id, enabled } => {
                assert_eq!(scope, &RuleScope::Global);
                assert_eq!(kind, &RuleKind::Content);
                #[allow(deprecated)]
                let expected_rule_id = PredefinedContentRuleId::ContainsUserName;
                assert_eq!(rule_id, &expected_rule_id.to_string());
                assert!(enabled);
            }
        );
        assert_matches!(
            &commands[2],
            Command::SetPushRuleEnabled { scope, kind, rule_id, enabled } => {
                assert_eq!(scope, &RuleScope::Global);
                assert_eq!(kind, &RuleKind::Override);
                #[allow(deprecated)]
                let expected_rule_id = PredefinedOverrideRuleId::ContainsDisplayName;
                assert_eq!(rule_id, expected_rule_id.as_str());
                assert!(enabled);
            }
        );
    }
}
