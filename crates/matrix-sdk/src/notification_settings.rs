//! High-level push notification settings API

use ruma::{
    events::push_rules::PushRulesEventContent,
    push::{
        Action, NewConditionalPushRule, NewPushRule, NewSimplePushRule, PredefinedContentRuleId,
        PredefinedOverrideRuleId, PredefinedUnderrideRuleId, PushCondition, RuleKind, Ruleset,
        Tweak,
    },
    RoomId,
};

use crate::{error::NotificationSettingsError, Client, Result};

/// Enum representing the push notification modes for a room
#[derive(Debug, Clone, PartialEq)]
pub enum RoomNotificationMode {
    /// All messages
    AllMessages,
    /// Mentions and keywords only
    MentionsAndKeywordsOnly,
    /// Mute
    Mute,
}

/// A high-level API to manage the client owner's push notification settings.
#[derive(Debug, Clone)]
pub struct NotificationSettings {
    /// The underlying HTTP client.
    client: Client,
}

impl NotificationSettings {
    pub(crate) fn new(client: Client) -> Self {
        Self { client }
    }

    /// Save a given ruleset to the client's owner account data
    async fn save_push_rules(&self, ruleset: &Ruleset) -> Result<(), NotificationSettingsError> {
        let content = PushRulesEventContent::new(ruleset.clone());
        self.client
            .account()
            .set_account_data::<PushRulesEventContent>(content)
            .await
            .map_err(|_| NotificationSettingsError::UnableToSavePushRules)?;
        Ok(())
    }

    /// Restore the default push rule for a given room
    ///
    /// The owner's account data will be updated.
    /// If an error occurs, the ruleset will not be modified.
    pub async fn restore_room_default_push_rule(
        &self,
        room_id: &String,
        ruleset: &mut Ruleset,
    ) -> Result<(), NotificationSettingsError> {
        // Only if we have a custom mode for this room
        if self.get_user_defined_room_notification_mode(room_id, ruleset).is_some() {
            let updated_ruleset = &mut ruleset.clone();
            // get a new ruleset without the custom rules
            self.delete_user_defined_room_notification_mode(room_id, updated_ruleset)?;
            // save the updated ruleset
            if self.save_push_rules(updated_ruleset).await.is_ok() {
                *ruleset = updated_ruleset.clone();
            } else {
                return Err(NotificationSettingsError::UnableToSavePushRules);
            }
        }
        Ok(())
    }

    /// Sets a notification mode for a given room
    ///
    /// The owner's account data will be updated.
    /// If an error occurs, the ruleset will not be modified.
    pub async fn set_room_notification_mode(
        &self,
        room_id: &String,
        mode: RoomNotificationMode,
        ruleset: &mut Ruleset,
    ) -> Result<(), NotificationSettingsError> {
        let updated_ruleset = &mut ruleset.clone();
        match mode {
            RoomNotificationMode::AllMessages => {
                self.insert_notify_room_rule(room_id, updated_ruleset)?;
            }
            RoomNotificationMode::MentionsAndKeywordsOnly => {
                self.insert_mention_and_keywords_room_rule(room_id, updated_ruleset)?;
            }
            RoomNotificationMode::Mute => {
                self.insert_mute_room_rule(room_id, updated_ruleset)?;
            }
        }
        // Save the updated ruleset
        if self.save_push_rules(updated_ruleset).await.is_ok() {
            *ruleset = updated_ruleset.clone();
            Ok(())
        } else {
            Err(NotificationSettingsError::UnableToSavePushRules)
        }
    }

    /// Gets the user defined push notification mode for a given room id
    pub fn get_user_defined_room_notification_mode(
        &self,
        room_id: &String,
        ruleset: &Ruleset,
    ) -> Option<RoomNotificationMode> {
        // Search for an enabled Override rule where rule_id is the room_id
        if let Some(rule) =
            ruleset.override_.iter().find(|x| x.enabled /* && x.rule_id == *room_id */)
        {
            // without a Notify action
            if !rule.actions.iter().any(|x| matches!(x, Action::Notify)) {
                // with a condition of type `EventMatch` for this room_id
                if rule.conditions.iter().any(|x| match x {
                    PushCondition::EventMatch { key, pattern } => {
                        key == "room_id" && *pattern == *room_id
                    }
                    _ => false,
                }) {
                    return Some(RoomNotificationMode::Mute);
                }
            }
        }

        // Search for an enabled Room rule where rule_id is the room_id
        if let Some(rule) = ruleset.room.iter().find(|x| x.enabled && x.rule_id == *room_id) {
            // if this rule contains a Notify action
            if rule.actions.iter().any(|x| matches!(x, Action::Notify)) {
                return Some(RoomNotificationMode::AllMessages);
            }
            // if this rule does not contain a Notify action
            if !rule.actions.iter().any(|x| matches!(x, Action::Notify)) {
                // check if mentions or keywords are enabled
                if self.is_user_mention_enabled(ruleset)
                    || self.is_room_mention_enabled(ruleset)
                    || self.contains_keyword_rules(ruleset)
                {
                    return Some(RoomNotificationMode::MentionsAndKeywordsOnly);
                }
            }
            return Some(RoomNotificationMode::Mute);
        }

        // There is no custom rule matching this room_id
        None
    }

    /// Delete any user defined rules for this room in the given ruleset
    fn delete_user_defined_room_notification_mode(
        &self,
        room_id: &String,
        ruleset: &mut Ruleset,
    ) -> Result<(), NotificationSettingsError> {
        // remove any Override rules matching this room_id
        for rule in ruleset.override_.clone().iter() {
            if rule.rule_id == *room_id {
                ruleset.remove(RuleKind::Override, &rule.rule_id)?;
                continue;
            }
            if rule.conditions.iter().any(|x| match x {
                PushCondition::EventMatch { key, pattern } => {
                    key == "room_id" && *pattern == *room_id
                }
                _ => false,
            }) {
                ruleset.remove(RuleKind::Override, &rule.rule_id)?;
                continue;
            }
        }

        // remove any Room rules matching this room_id
        if let Some(rule) = ruleset.room.clone().iter().find(|x| x.rule_id == *room_id) {
            ruleset.remove(RuleKind::Room, &rule.rule_id)?;
        }

        // remove any Underride rules matching this room_id
        for rule in ruleset.underride.clone().iter() {
            if rule.rule_id == *room_id {
                ruleset.remove(RuleKind::Underride, &rule.rule_id)?;
                continue;
            }
            if rule.conditions.iter().any(|x| match x {
                PushCondition::EventMatch { key, pattern } => {
                    key == "room_id" && *pattern == *room_id
                }
                _ => false,
            }) {
                ruleset.remove(RuleKind::Underride, &rule.rule_id)?;
                continue;
            }
        }

        Ok(())
    }

    /// Gets the default push notification mode for a given room id
    pub fn get_default_room_notification_mode(
        &self,
        is_encrypted: bool,
        members_count: u64,
        ruleset: &Ruleset,
    ) -> Result<RoomNotificationMode, NotificationSettingsError> {
        // get the correct default rule id based on is_encrypted and members_count
        let rule_id = match (is_encrypted, members_count) {
            (true, 2) => PredefinedUnderrideRuleId::EncryptedRoomOneToOne,
            (false, 2) => PredefinedUnderrideRuleId::RoomOneToOne,
            (true, _) => PredefinedUnderrideRuleId::Encrypted,
            (false, _) => PredefinedUnderrideRuleId::Message,
        };

        // If there is an Underride rule that should trigger a notification, the mode is
        // 'AllMessages'
        if ruleset.underride.iter().any(|r| {
            r.enabled
                && r.rule_id == rule_id.to_string()
                && r.actions.iter().any(|a| a.should_notify())
        }) {
            return Ok(RoomNotificationMode::AllMessages);
        }

        // If user mention is enabled, the mode is 'MentionsAndKeywordsOnly'
        if self.is_user_mention_enabled(ruleset)
            || self.contains_keyword_rules(ruleset)
            || self.is_room_mention_enabled(ruleset)
        {
            return Ok(RoomNotificationMode::MentionsAndKeywordsOnly);
        }

        Ok(RoomNotificationMode::Mute)
    }

    /// Get whether the IsUserMention rule is enabled.
    #[allow(deprecated)]
    pub fn is_user_mention_enabled(&self, ruleset: &Ruleset) -> bool {
        // Search for an enabled Override rule IsUserMention (MSC3952).
        // This is a new push rule that may not yet be present.
        // The rule_id will be replaced by `PredefinedOverrideRuleId::IsUserMention`
        // once available in ruma-common.
        if let Some(rule) = ruleset.get(RuleKind::Override, ".m.rule.is_user_mention") {
            if rule.enabled() {
                return true;
            }
        }

        // Fallback to deprecated rules for compatibility.
        let mentions_and_keywords_rules_id = vec![
            PredefinedOverrideRuleId::ContainsDisplayName.to_string(),
            PredefinedContentRuleId::ContainsUserName.to_string(),
        ];
        ruleset.content.iter().any(|r| {
            r.enabled
                && mentions_and_keywords_rules_id.contains(&r.rule_id)
                && r.actions.iter().any(|a| a.should_notify())
        })
    }

    /// Set whether the IsUserMention rule is enabled.
    #[allow(deprecated)]
    pub fn set_user_mention_enabled(&self, enabled: bool, ruleset: &mut Ruleset) {
        // Sets the IsUserMention Override rule (MSC3952).
        // This is a new push rule that may not yet be present.
        // The rule_id will be replaced by `PredefinedOverrideRuleId::IsUserMention`
        // once available in ruma-common.
        _ = ruleset.set_enabled(RuleKind::Override, ".m.rule.is_user_mention", enabled);

        // For compatibility purpose, we still need to set ContainsUserName and
        // ContainsDisplayName (deprecated rules).
        _ = ruleset.set_enabled(
            RuleKind::Content,
            PredefinedContentRuleId::ContainsUserName,
            enabled,
        );

        _ = ruleset.set_enabled(
            RuleKind::Content,
            PredefinedOverrideRuleId::ContainsDisplayName,
            enabled,
        );
    }

    /// Get whether the IsRoomMention rule is enabled.
    #[allow(deprecated)]
    pub fn is_room_mention_enabled(&self, ruleset: &Ruleset) -> bool {
        // Search for an enabled Override rule IsRoomMention (MSC3952).
        // This is a new push rule that may not yet be present.
        // The rule_id will be replaced by `PredefinedOverrideRuleId::IsRoomMention`
        // once available in ruma-common.
        if let Some(rule) = ruleset.get(RuleKind::Override, ".m.rule.is_room_mention") {
            if rule.enabled() {
                return true;
            }
        }

        // Fallback to deprecated rule for compatibility
        ruleset.content.iter().any(|r| {
            r.enabled
                && r.rule_id == PredefinedOverrideRuleId::RoomNotif.to_string()
                && r.actions.iter().any(|a| a.should_notify())
        })
    }

    /// Set whether the IsRoomMention rule is enabled.
    #[allow(deprecated)]
    pub fn set_room_mention_enabled(&self, enabled: bool, ruleset: &mut Ruleset) {
        // Sets the IsRoomMention Override rule (MSC3952).
        // This is a new push rule that may not yet be present.
        // The rule_id will be replaced by `PredefinedOverrideRuleId::IsRoomMention`
        // once available in ruma-common.
        _ = ruleset.set_enabled(RuleKind::Override, ".m.rule.is_room_mention", enabled);

        // For compatibility purpose, we still need to set RoomNotif (deprecated rule).
        _ = ruleset.set_enabled(RuleKind::Content, PredefinedOverrideRuleId::RoomNotif, enabled);
    }

    /// Get whether the given ruleset contains some keywords rules
    pub fn contains_keyword_rules(&self, ruleset: &Ruleset) -> bool {
        // Search for a user defined Content rule.
        ruleset.content.iter().any(|r| !r.default && r.enabled)
    }

    /// Insert a new rule to mute a given room in the given ruleset
    fn insert_mute_room_rule(
        &self,
        room_id: &String,
        ruleset: &mut Ruleset,
    ) -> Result<(), NotificationSettingsError> {
        // Try to remove any existing rule for this room
        _ = self.delete_user_defined_room_notification_mode(room_id, ruleset);

        // Insert a new Override push rule without any actions
        let new_rule = NewConditionalPushRule::new(
            room_id.clone(),
            vec![PushCondition::EventMatch { key: "room_id".into(), pattern: room_id.clone() }],
            vec![],
        );
        ruleset.insert(NewPushRule::Override(new_rule), None, None)?;

        Ok(())
    }

    /// Insert a mention and keywords rule for a given room in the given
    /// ruleset
    ///
    /// Returns an error if mention rules are disabled and no keyword rules
    /// exists.
    fn insert_mention_and_keywords_room_rule(
        &self,
        room_id: &String,
        ruleset: &mut Ruleset,
    ) -> Result<(), NotificationSettingsError> {
        // Try to remove any existing rule for this room
        _ = self.delete_user_defined_room_notification_mode(room_id, ruleset);

        // Check if mentions or keywords are enabled
        let mentions_or_keywords_enabled = self.is_user_mention_enabled(ruleset)
            || self.is_room_mention_enabled(ruleset)
            || self.contains_keyword_rules(ruleset);
        if !mentions_or_keywords_enabled {
            return Err(NotificationSettingsError::MentionsNotEnabled);
        }

        // Insert a new Room push rule
        let new_rule = NewSimplePushRule::new(RoomId::parse(room_id)?, vec![]);
        ruleset.insert(NewPushRule::Room(new_rule), None, None)?;
        Ok(())
    }

    /// Insert a notify rule for a given room in the given ruleset
    fn insert_notify_room_rule(
        &self,
        room_id: &String,
        ruleset: &mut Ruleset,
    ) -> Result<(), NotificationSettingsError> {
        // Try to remove any existing rule for this room
        _ = self.delete_user_defined_room_notification_mode(room_id, ruleset);

        // Insert a new Room push rule with a Notify action.
        let new_rule = NewSimplePushRule::new(
            RoomId::parse(room_id)?,
            vec![Action::Notify, Action::SetTweak(Tweak::Sound("default".into()))],
        );
        ruleset.insert(NewPushRule::Room(new_rule), None, None)?;

        Ok(())
    }
}

// The http mocking library is not supported for wasm32
#[cfg(all(test, not(target_arch = "wasm32")))]
pub(crate) mod tests {
    use matrix_sdk_test::async_test;
    #[cfg(target_arch = "wasm32")]
    wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_browser);

    use ruma::push::{
        Action, NewPatternedPushRule, NewPushRule, PredefinedContentRuleId,
        PredefinedUnderrideRuleId, RuleKind,
    };
    use wiremock::{matchers::method, Mock, MockServer, ResponseTemplate};

    use crate::{
        notification_settings::RoomNotificationMode, test_utils::logged_in_client,
        NotificationSettingsError,
    };

    #[async_test]
    async fn get_default_room_notification_mode_encrypted_one_to_one() {
        let server = MockServer::start().await;
        let client = logged_in_client(Some(server.uri())).await;

        let mut ruleset = client.account().push_rules().await.unwrap();
        let notification_settings = client.notification_settings();

        let encrypted: bool = true;
        let members_count = 2;

        if let Ok(mode) = notification_settings.get_default_room_notification_mode(
            encrypted,
            members_count,
            &ruleset,
        ) {
            assert_eq!(mode, RoomNotificationMode::AllMessages)
        } else {
            panic!("A default mode should be defined.")
        }

        let result = ruleset.set_enabled(
            RuleKind::Underride,
            PredefinedUnderrideRuleId::EncryptedRoomOneToOne,
            false,
        );
        assert!(result.is_ok());

        let result =
            ruleset.set_enabled(RuleKind::Content, PredefinedContentRuleId::ContainsUserName, true);
        assert!(result.is_ok());

        let result = ruleset.set_actions(
            RuleKind::Content,
            PredefinedContentRuleId::ContainsUserName,
            vec![Action::Notify],
        );
        assert!(result.is_ok());

        if let Ok(mode) = notification_settings.get_default_room_notification_mode(
            encrypted,
            members_count,
            &ruleset,
        ) {
            assert_eq!(mode, RoomNotificationMode::MentionsAndKeywordsOnly)
        } else {
            panic!("A default mode should be defined.")
        }
    }

    #[async_test]
    async fn get_default_room_notification_mode_one_to_one() {
        let server = MockServer::start().await;
        let client = logged_in_client(Some(server.uri())).await;

        let mut ruleset = client.account().push_rules().await.unwrap();
        let notification_settings = client.notification_settings();

        let encrypted = false;
        let members_count = 2;

        if let Ok(mode) = notification_settings.get_default_room_notification_mode(
            encrypted,
            members_count,
            &ruleset,
        ) {
            assert_eq!(mode, RoomNotificationMode::AllMessages)
        } else {
            panic!("A default mode should be defined.")
        }

        let result = ruleset.set_enabled(
            RuleKind::Underride,
            PredefinedUnderrideRuleId::RoomOneToOne,
            false,
        );
        assert!(result.is_ok());

        let result =
            ruleset.set_enabled(RuleKind::Content, PredefinedContentRuleId::ContainsUserName, true);
        assert!(result.is_ok());

        let result = ruleset.set_actions(
            RuleKind::Content,
            PredefinedContentRuleId::ContainsUserName,
            vec![Action::Notify],
        );
        assert!(result.is_ok());

        if let Ok(mode) = notification_settings.get_default_room_notification_mode(
            encrypted,
            members_count,
            &ruleset,
        ) {
            assert_eq!(mode, RoomNotificationMode::MentionsAndKeywordsOnly)
        } else {
            panic!("A default mode should be defined.")
        }
    }

    #[async_test]
    async fn get_default_room_notification_mode_encrypted_room() {
        let server = MockServer::start().await;
        let client = logged_in_client(Some(server.uri())).await;

        let mut ruleset = client.account().push_rules().await.unwrap();
        let notification_settings = client.notification_settings();

        let encrypted: bool = true;
        let members_count = 3;

        if let Ok(mode) = notification_settings.get_default_room_notification_mode(
            encrypted,
            members_count,
            &ruleset,
        ) {
            assert_eq!(mode, RoomNotificationMode::AllMessages)
        } else {
            panic!("A default mode should be defined.")
        }

        let result =
            ruleset.set_enabled(RuleKind::Underride, PredefinedUnderrideRuleId::Encrypted, false);
        assert!(result.is_ok());

        let result =
            ruleset.set_enabled(RuleKind::Content, PredefinedContentRuleId::ContainsUserName, true);
        assert!(result.is_ok());

        let result = ruleset.set_actions(
            RuleKind::Content,
            PredefinedContentRuleId::ContainsUserName,
            vec![Action::Notify],
        );
        assert!(result.is_ok());

        if let Ok(mode) = notification_settings.get_default_room_notification_mode(
            encrypted,
            members_count,
            &ruleset,
        ) {
            assert_eq!(mode, RoomNotificationMode::MentionsAndKeywordsOnly)
        } else {
            panic!("A default mode should be defined.")
        }
    }

    #[async_test]
    async fn get_default_room_notification_mode_unencrypted_room() {
        let server = MockServer::start().await;
        let client = logged_in_client(Some(server.uri())).await;

        let mut ruleset = client.account().push_rules().await.unwrap();
        let notification_settings = client.notification_settings();

        let encrypted: bool = false;
        let members_count = 3;

        if let Ok(mode) = notification_settings.get_default_room_notification_mode(
            encrypted,
            members_count,
            &ruleset,
        ) {
            assert_eq!(mode, RoomNotificationMode::AllMessages)
        } else {
            panic!("A default mode should be defined.")
        }

        let result =
            ruleset.set_enabled(RuleKind::Underride, PredefinedUnderrideRuleId::Message, false);
        assert!(result.is_ok());

        let result =
            ruleset.set_enabled(RuleKind::Content, PredefinedContentRuleId::ContainsUserName, true);
        assert!(result.is_ok());

        let result = ruleset.set_actions(
            RuleKind::Content,
            PredefinedContentRuleId::ContainsUserName,
            vec![Action::Notify],
        );
        assert!(result.is_ok());

        if let Ok(mode) = notification_settings.get_default_room_notification_mode(
            encrypted,
            members_count,
            &ruleset,
        ) {
            assert_eq!(mode, RoomNotificationMode::MentionsAndKeywordsOnly)
        } else {
            panic!("A default mode should be defined.")
        }
    }

    #[async_test]
    async fn insert_notify_room_rule() {
        let server = MockServer::start().await;
        let client = logged_in_client(Some(server.uri())).await;

        let mut ruleset = client.account().push_rules().await.unwrap();
        let notification_settings = client.notification_settings();
        let room_id = "!test_room:matrix.org".to_string();

        let mode =
            notification_settings.get_user_defined_room_notification_mode(&room_id, &ruleset);
        assert!(mode.is_none());

        let result = notification_settings.insert_notify_room_rule(&room_id, &mut ruleset);
        assert!(result.is_ok());

        let mode =
            notification_settings.get_user_defined_room_notification_mode(&room_id, &ruleset);
        assert_eq!(mode, Some(RoomNotificationMode::AllMessages));
    }

    #[async_test]
    async fn insert_notify_room_rule_invalid_room_id() {
        let server = MockServer::start().await;
        let client = logged_in_client(Some(server.uri())).await;

        let mut ruleset = client.account().push_rules().await.unwrap();
        let notification_settings = client.notification_settings();
        let room_id = "invalid_test_room_id".to_string();

        let mode =
            notification_settings.get_user_defined_room_notification_mode(&room_id, &ruleset);
        assert!(mode.is_none());

        let result = notification_settings.insert_notify_room_rule(&room_id, &mut ruleset);
        match result {
            Err(error) => {
                assert!(matches!(error, NotificationSettingsError::InvalidRoomId))
            }
            _ => {
                panic!("a 'NotificationSettingsError::InvalidRoomId' error is expected.")
            }
        }
    }

    #[async_test]
    async fn insert_mention_and_keywords_room_rule() {
        let server = MockServer::start().await;
        let client = logged_in_client(Some(server.uri())).await;

        let mut ruleset = client.account().push_rules().await.unwrap();
        let notification_settings = client.notification_settings();
        let room_id = "!test_room:matrix.org".to_string();

        let mode =
            notification_settings.get_user_defined_room_notification_mode(&room_id, &ruleset);
        assert!(mode.is_none());

        let result =
            notification_settings.insert_mention_and_keywords_room_rule(&room_id, &mut ruleset);
        assert!(result.is_ok());

        let mode =
            notification_settings.get_user_defined_room_notification_mode(&room_id, &ruleset);
        assert_eq!(
            mode,
            Some(RoomNotificationMode::MentionsAndKeywordsOnly),
            "wrong mode, should be 'RoomNotificationMode::MentionsAndKeywordsOnly'"
        );
    }

    #[async_test]
    async fn insert_mention_and_keywords_room_rule_with_mentions_disabled() {
        let server = MockServer::start().await;
        let client = logged_in_client(Some(server.uri())).await;

        let mut ruleset = client.account().push_rules().await.unwrap();
        let notification_settings = client.notification_settings();
        let room_id = "!test_room:matrix.org".to_string();

        let mode =
            notification_settings.get_user_defined_room_notification_mode(&room_id, &ruleset);
        assert!(mode.is_none());

        // Disable users mentions and room mentions
        notification_settings.set_user_mention_enabled(false, &mut ruleset);
        notification_settings.set_room_mention_enabled(false, &mut ruleset);

        // An error is expected
        let result =
            notification_settings.insert_mention_and_keywords_room_rule(&room_id, &mut ruleset);
        assert!(result.is_err(), "An error is expected.");

        // And the mode must remain at None
        let mode =
            notification_settings.get_user_defined_room_notification_mode(&room_id, &ruleset);
        assert!(
            mode.is_none(),
            "wrong mode, should be 'RoomNotificationMode::MentionsAndKeywordsOnly'"
        );
    }

    #[async_test]
    async fn insert_mute_room_rule() {
        let server = MockServer::start().await;
        let client = logged_in_client(Some(server.uri())).await;

        let mut ruleset = client.account().push_rules().await.unwrap();
        let notification_settings = client.notification_settings();
        let room_id = "!test_room:matrix.org".to_string();

        let mode =
            notification_settings.get_user_defined_room_notification_mode(&room_id, &ruleset);
        assert!(mode.is_none());

        let result = notification_settings.insert_mute_room_rule(&room_id, &mut ruleset);
        assert!(result.is_ok());

        let mode =
            notification_settings.get_user_defined_room_notification_mode(&room_id, &ruleset);
        assert_eq!(
            mode,
            Some(RoomNotificationMode::Mute),
            "wrong mode, should be 'RoomNotificationMode::Mute'"
        );
    }

    #[async_test]
    async fn delete_user_defined_room_notification_mode() {
        let server = MockServer::start().await;
        let client = logged_in_client(Some(server.uri())).await;

        let mut ruleset = client.account().push_rules().await.unwrap();
        let notification_settings = client.notification_settings();
        let room_id = "!test_room:matrix.org".to_string();

        let mode =
            notification_settings.get_user_defined_room_notification_mode(&room_id, &ruleset);
        assert!(mode.is_none());

        let result = notification_settings.insert_mute_room_rule(&room_id, &mut ruleset);
        assert!(result.is_ok());

        let mode =
            notification_settings.get_user_defined_room_notification_mode(&room_id, &ruleset);
        assert_eq!(
            mode,
            Some(RoomNotificationMode::Mute),
            "wrong mode, should be 'RoomNotificationMode::Mute'"
        );

        let result = notification_settings
            .delete_user_defined_room_notification_mode(&room_id, &mut ruleset);
        assert!(result.is_ok());

        let mode =
            notification_settings.get_user_defined_room_notification_mode(&room_id, &ruleset);
        assert!(mode.is_none());
    }

    #[async_test]
    async fn set_room_notification_mode() {
        let server = MockServer::start().await;
        let client = logged_in_client(Some(server.uri())).await;

        let mut ruleset = client.account().push_rules().await.unwrap();
        let notification_settings = client.notification_settings();
        let room_id = "!test_room:matrix.org".to_string();

        let mode =
            notification_settings.get_user_defined_room_notification_mode(&room_id, &ruleset);
        assert!(mode.is_none());

        // Calling set_room_notification_mode will perform an API call
        let mock = Mock::given(method("PUT")).respond_with(ResponseTemplate::new(200));
        server.register(mock).await;

        // Test to set each modes
        for expected_mode in [
            RoomNotificationMode::AllMessages,
            RoomNotificationMode::MentionsAndKeywordsOnly,
            RoomNotificationMode::Mute,
        ]
        .iter()
        {
            let result = notification_settings
                .set_room_notification_mode(&room_id, expected_mode.clone(), &mut ruleset)
                .await;
            assert!(result.is_ok());

            match notification_settings.get_user_defined_room_notification_mode(&room_id, &ruleset)
            {
                Some(new_mode) => {
                    assert_eq!(&new_mode, expected_mode)
                }
                None => {
                    panic!("mode {:?} is expected.", expected_mode)
                }
            }
        }
    }

    #[async_test]
    async fn set_room_notification_mode_unable_to_save() {
        let server = MockServer::start().await;
        let client = logged_in_client(Some(server.uri())).await;

        let mut ruleset = client.account().push_rules().await.unwrap();
        let notification_settings = client.notification_settings();
        let room_id = "!test_room:matrix.org".to_string();

        let mode =
            notification_settings.get_user_defined_room_notification_mode(&room_id, &ruleset);
        assert!(mode.is_none());

        // Calling set_room_notification_mode will perform an API call
        let mock = Mock::given(method("PUT")).respond_with(ResponseTemplate::new(500));
        server.register(mock).await;

        // Test to set each modes
        for expected_mode in [
            RoomNotificationMode::AllMessages,
            RoomNotificationMode::MentionsAndKeywordsOnly,
            RoomNotificationMode::Mute,
        ]
        .iter()
        {
            let result = notification_settings
                .set_room_notification_mode(&room_id, expected_mode.clone(), &mut ruleset)
                .await;
            assert!(result.is_err());

            assert!(
                notification_settings
                    .get_user_defined_room_notification_mode(&room_id, &ruleset)
                    .is_none(),
                "ruleset should not have been updated."
            );
        }
    }

    #[async_test]
    async fn restore_room_default_push_rule() {
        let server = MockServer::start().await;
        let client = logged_in_client(Some(server.uri())).await;

        let mut ruleset = client.account().push_rules().await.unwrap();
        let notification_settings = client.notification_settings();
        let room_id = "!test_room:matrix.org".to_string();

        let mode =
            notification_settings.get_user_defined_room_notification_mode(&room_id, &ruleset);
        assert!(mode.is_none());

        // Calling set_room_notification_mode will perform an API call
        let mock = Mock::given(method("PUT")).respond_with(ResponseTemplate::new(200));
        server.register(mock).await;

        // Mute the room
        let result = notification_settings
            .set_room_notification_mode(&room_id, RoomNotificationMode::Mute, &mut ruleset)
            .await;
        assert!(result.is_ok());

        // Restore to default mode
        let result =
            notification_settings.restore_room_default_push_rule(&room_id, &mut ruleset).await;
        assert!(result.is_ok());

        // All user defined rules should have be deleted
        assert!(notification_settings
            .get_user_defined_room_notification_mode(&room_id, &ruleset)
            .is_none());
    }

    #[async_test]
    async fn restore_room_default_push_rule_unable_to_save() {
        let server = MockServer::start().await;
        let client = logged_in_client(Some(server.uri())).await;

        let mut ruleset = client.account().push_rules().await.unwrap();
        let notification_settings = client.notification_settings();
        let room_id = "!test_room:matrix.org".to_string();

        let mode =
            notification_settings.get_user_defined_room_notification_mode(&room_id, &ruleset);
        assert!(mode.is_none());

        // Calling set_room_notification_mode will perform an API call
        let mock = Mock::given(method("PUT")).respond_with(ResponseTemplate::new(500));
        server.register(mock).await;

        // Mute the room
        let result = notification_settings.insert_mute_room_rule(&room_id, &mut ruleset);
        assert!(result.is_ok());

        // Restore to default mode
        let result =
            notification_settings.restore_room_default_push_rule(&room_id, &mut ruleset).await;
        assert!(result.is_err());

        // Ruleset should not have been modified
        assert_eq!(
            notification_settings.get_user_defined_room_notification_mode(&room_id, &ruleset),
            Some(RoomNotificationMode::Mute)
        );
    }

    #[async_test]
    async fn contains_keyword_rules() {
        let server = MockServer::start().await;
        let client = logged_in_client(Some(server.uri())).await;

        let mut ruleset = client.account().push_rules().await.unwrap();
        let notification_settings = client.notification_settings();

        assert!(!notification_settings.contains_keyword_rules(&ruleset));

        let rule =
            NewPatternedPushRule::new("keyword".into(), "keyword".into(), vec![Action::Notify]);

        _ = ruleset.insert(NewPushRule::Content(rule), None, None);
        assert!(notification_settings.contains_keyword_rules(&ruleset));
    }
}
