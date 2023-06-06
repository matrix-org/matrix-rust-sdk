//! High-level push notification settings API

use ruma::{
    api::client::push::{delete_pushrule, set_pushrule, set_pushrule_enabled, RuleScope},
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

    /// Sets a notification mode for a given room
    pub async fn set_room_notification_mode(
        &self,
        room_id: &String,
        mode: RoomNotificationMode,
        ruleset: &mut Ruleset,
    ) -> Result<(), NotificationSettingsError> {
        // Get the current mode
        let current_mode = self.get_user_defined_room_notification_mode(room_id, ruleset);

        match (current_mode, mode) {
            // Default > AllMessages
            (None, RoomNotificationMode::AllMessages) => {
                self.insert_room_rule(room_id, true, ruleset).await?;
            }
            // Mentions&Keywords > AllMessages
            (
                Some(RoomNotificationMode::MentionsAndKeywordsOnly),
                RoomNotificationMode::AllMessages,
            ) => {
                // Insert the rule before deleting the other custom rules to obtain the correct
                // mode in the next sync response.
                let current_custom_rules = self.get_custom_rules_for_room(room_id, ruleset);
                self.insert_room_rule(room_id, true, ruleset).await?;
                self.delete_rules(
                    current_custom_rules,
                    vec![(RuleKind::Room, room_id.clone())],
                    ruleset,
                )
                .await?;
            }
            // Mute > AllMessages
            (Some(RoomNotificationMode::Mute), RoomNotificationMode::AllMessages) => {
                // Insert the rule before deleting the other custom rules to obtain the correct
                // mode in the next sync response.
                let current_custom_rules = self.get_custom_rules_for_room(room_id, ruleset);
                self.insert_room_rule(room_id, true, ruleset).await?;
                self.delete_rules(current_custom_rules, vec![], ruleset).await?;
            }
            // Default > Mentions&Keywords
            (None, RoomNotificationMode::MentionsAndKeywordsOnly) => {
                self.insert_room_rule(room_id, false, ruleset).await?;
            }
            // AllMessages > Mentions&Keywords
            (
                Some(RoomNotificationMode::AllMessages),
                RoomNotificationMode::MentionsAndKeywordsOnly,
            ) => {
                // Insert the rule before deleting the other custom rules to obtain the correct
                // mode in the next sync response.
                let current_custom_rules = self.get_custom_rules_for_room(room_id, ruleset);
                self.insert_room_rule(room_id, false, ruleset).await?;
                self.delete_rules(
                    current_custom_rules,
                    vec![(RuleKind::Room, room_id.clone())],
                    ruleset,
                )
                .await?;
            }
            // Mute > Mentions&Keywords
            (Some(RoomNotificationMode::Mute), RoomNotificationMode::MentionsAndKeywordsOnly) => {
                // Insert the rule before deleting the other custom rules to obtain the correct
                // mode in the next sync response.
                let current_custom_rules = self.get_custom_rules_for_room(room_id, ruleset);
                self.insert_room_rule(room_id, false, ruleset).await?;
                self.delete_rules(
                    current_custom_rules,
                    vec![(RuleKind::Room, room_id.clone())],
                    ruleset,
                )
                .await?;
            }
            // Mute > Mute
            (Some(RoomNotificationMode::Mute), RoomNotificationMode::Mute) => {}
            // _ > Mute
            (_, RoomNotificationMode::Mute) => {
                // Insert the rule before deleting the other custom rules to obtain the correct
                // mode in the next sync response.
                let current_custom_rules = self.get_custom_rules_for_room(room_id, ruleset);
                self.insert_override_room_rule(room_id, false, ruleset).await?;
                self.delete_rules(current_custom_rules, vec![], ruleset).await?;
            }
            // Other cases
            (_, _) => {}
        }

        Ok(())
    }

    /// Gets all user defined rules matching a given room_id
    fn get_custom_rules_for_room(
        &self,
        room_id: &String,
        ruleset: &Ruleset,
    ) -> Vec<(RuleKind, String)> {
        let mut custom_rules: Vec<(RuleKind, String)> = Vec::new();

        // add any Override rules matching this room_id
        for rule in ruleset.override_.clone().iter() {
            // if the rule_id is the room_id
            if rule.rule_id == *room_id {
                custom_rules.push((RuleKind::Override, rule.rule_id.clone()));
                continue;
            }
            // if the rule contains a condition matching this room_id
            if rule.conditions.iter().any(|x| match x {
                PushCondition::EventMatch { key, pattern } => {
                    key == "room_id" && *pattern == *room_id
                }
                _ => false,
            }) {
                custom_rules.push((RuleKind::Override, rule.rule_id.clone()));
                continue;
            }
        }

        // add any Room rules matching this room_id
        if let Some(rule) = ruleset.room.clone().iter().find(|x| x.rule_id == *room_id) {
            custom_rules.push((RuleKind::Room, rule.rule_id.to_string()));
        }

        // add any Underride rules matching this room_id
        for rule in ruleset.underride.clone().iter() {
            // if the rule_id is the room_id
            if rule.rule_id == *room_id {
                custom_rules.push((RuleKind::Underride, rule.rule_id.clone()));
                continue;
            }
            // if the rule contains a condition matching this room_id
            if rule.conditions.iter().any(|x| match x {
                PushCondition::EventMatch { key, pattern } => {
                    key == "room_id" && *pattern == *room_id
                }
                _ => false,
            }) {
                custom_rules.push((RuleKind::Underride, rule.rule_id.clone()));
                continue;
            }
        }

        custom_rules
    }

    /// Deletes a list of rules
    async fn delete_rules(
        &self,
        rules: Vec<(RuleKind, String)>,
        exception: Vec<(RuleKind, String)>,
        ruleset: &mut Ruleset,
    ) -> Result<(), NotificationSettingsError> {
        for (rule_kind, rule_id) in rules.iter() {
            if exception.contains(&(rule_kind.clone(), rule_id.clone())) {
                continue;
            }
            let request = delete_pushrule::v3::Request::new(
                RuleScope::Global,
                rule_kind.clone(),
                rule_id.clone(),
            );
            if self.client.send(request, None).await.is_err() {
                return Err(NotificationSettingsError::UnableToRemovePushRule);
            }
            ruleset.remove(rule_kind.clone(), rule_id)?;
        }
        Ok(())
    }

    /// Gets the user defined push notification mode for a given room id
    pub fn get_user_defined_room_notification_mode(
        &self,
        room_id: &String,
        ruleset: &Ruleset,
    ) -> Option<RoomNotificationMode> {
        // Search for an enabled Override
        if let Some(rule) = ruleset.override_.iter().find(|x| x.enabled) {
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
            } else {
                return Some(RoomNotificationMode::MentionsAndKeywordsOnly);
            }
        }

        // There is no custom rule matching this room_id
        None
    }

    /// Delete all user defined rules for a given room id
    pub async fn delete_user_defined_room_rules(
        &self,
        room_id: &String,
        ruleset: &mut Ruleset,
    ) -> Result<(), NotificationSettingsError> {
        let rules = self.get_custom_rules_for_room(room_id, ruleset);
        self.delete_rules(rules, vec![], ruleset).await
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
            Ok(RoomNotificationMode::AllMessages)
        } else {
            // Otherwise, the mode is 'MentionsAndKeywordsOnly'
            Ok(RoomNotificationMode::MentionsAndKeywordsOnly)
        }
    }

    /// Get whether the IsUserMention rule is enabled.
    #[allow(deprecated)]
    pub fn is_user_mention_enabled(&self, ruleset: &Ruleset) -> bool {
        // Search for an enabled Override rule IsUserMention (MSC3952).
        // This is a new push rule that may not yet be present.
        if let Some(rule) = ruleset.get(RuleKind::Override, PredefinedOverrideRuleId::IsUserMention)
        {
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
    pub async fn set_user_mention_enabled(
        &self,
        enabled: bool,
        ruleset: &mut Ruleset,
    ) -> Result<()> {
        // Sets the IsUserMention Override rule (MSC3952).
        // This is a new push rule that may not yet be present.
        let request = set_pushrule_enabled::v3::Request::new(
            RuleScope::Global,
            RuleKind::Override,
            PredefinedOverrideRuleId::IsUserMention.to_string(),
            enabled,
        );
        self.client.send(request, None).await?;
        _ = ruleset.set_enabled(
            RuleKind::Override,
            PredefinedOverrideRuleId::IsUserMention,
            enabled,
        );

        // For compatibility purpose, we still need to set ContainsUserName and
        // ContainsDisplayName (deprecated rules).
        let request = set_pushrule_enabled::v3::Request::new(
            RuleScope::Global,
            RuleKind::Content,
            PredefinedContentRuleId::ContainsUserName.to_string(),
            enabled,
        );
        self.client.send(request, None).await?;
        _ = ruleset.set_enabled(
            RuleKind::Content,
            PredefinedContentRuleId::ContainsUserName,
            enabled,
        );

        let request = set_pushrule_enabled::v3::Request::new(
            RuleScope::Global,
            RuleKind::Content,
            PredefinedOverrideRuleId::ContainsDisplayName.to_string(),
            enabled,
        );
        self.client.send(request, None).await?;
        _ = ruleset.set_enabled(
            RuleKind::Content,
            PredefinedOverrideRuleId::ContainsDisplayName,
            enabled,
        );

        Ok(())
    }

    /// Get whether the IsRoomMention rule is enabled.
    #[allow(deprecated)]
    pub fn is_room_mention_enabled(&self, ruleset: &Ruleset) -> bool {
        // Search for an enabled Override rule IsRoomMention (MSC3952).
        // This is a new push rule that may not yet be present.
        if let Some(rule) = ruleset.get(RuleKind::Override, PredefinedOverrideRuleId::IsRoomMention)
        {
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
    pub async fn set_room_mention_enabled(
        &self,
        enabled: bool,
        ruleset: &mut Ruleset,
    ) -> Result<()> {
        // Sets the IsRoomMention Override rule (MSC3952).
        // This is a new push rule that may not yet be present.
        let request = set_pushrule_enabled::v3::Request::new(
            RuleScope::Global,
            RuleKind::Override,
            PredefinedOverrideRuleId::IsRoomMention.to_string(),
            enabled,
        );
        self.client.send(request, None).await?;
        _ = ruleset.set_enabled(
            RuleKind::Override,
            PredefinedOverrideRuleId::IsRoomMention,
            enabled,
        );

        // For compatibility purpose, we still need to set RoomNotif (deprecated rule).
        let request = set_pushrule_enabled::v3::Request::new(
            RuleScope::Global,
            RuleKind::Content,
            PredefinedOverrideRuleId::RoomNotif.to_string(),
            enabled,
        );
        self.client.send(request, None).await?;
        _ = ruleset.set_enabled(RuleKind::Content, PredefinedOverrideRuleId::RoomNotif, enabled);

        Ok(())
    }

    /// Get whether the given ruleset contains some keywords rules
    pub fn contains_keyword_rules(&self, ruleset: &Ruleset) -> bool {
        // Search for a user defined Content rule.
        ruleset.content.iter().any(|r| !r.default && r.enabled)
    }

    /// Insert a new override rule for a given room_id
    async fn insert_override_room_rule(
        &self,
        room_id: &str,
        notify: bool,
        ruleset: &mut Ruleset,
    ) -> Result<(), NotificationSettingsError> {
        let actions = if notify {
            vec![Action::Notify, Action::SetTweak(Tweak::Sound("default".into()))]
        } else {
            vec![]
        };

        // Insert a new push rule without any actions
        let new_rule = NewConditionalPushRule::new(
            room_id.to_owned(),
            vec![PushCondition::EventMatch { key: "room_id".into(), pattern: room_id.to_owned() }],
            actions,
        );
        let request = set_pushrule::v3::Request::new(
            RuleScope::Global,
            NewPushRule::Override(new_rule.clone()),
        );
        if self.client.send(request, None).await.is_err() {
            return Err(NotificationSettingsError::UnableToAddPushRule);
        }
        ruleset.insert(NewPushRule::Override(new_rule), None, None)?;

        Ok(())
    }

    /// Insert a new room rule for a given room in the given ruleset
    async fn insert_room_rule(
        &self,
        room_id: &String,
        notify: bool,
        ruleset: &mut Ruleset,
    ) -> Result<(), NotificationSettingsError> {
        let actions = if notify {
            vec![Action::Notify, Action::SetTweak(Tweak::Sound("default".into()))]
        } else {
            vec![]
        };

        // Insert a new Room push rule with a Notify action.
        let new_rule = NewSimplePushRule::new(RoomId::parse(room_id)?, actions);
        let request =
            set_pushrule::v3::Request::new(RuleScope::Global, NewPushRule::Room(new_rule.clone()));
        if self.client.send(request, None).await.is_err() {
            return Err(NotificationSettingsError::UnableToAddPushRule);
        }
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
        Action, NewPatternedPushRule, NewPushRule, PredefinedOverrideRuleId,
        PredefinedUnderrideRuleId, RuleKind, Ruleset,
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
            ruleset.set_enabled(RuleKind::Override, PredefinedOverrideRuleId::IsUserMention, true);
        assert!(result.is_ok());

        let result = ruleset.set_actions(
            RuleKind::Override,
            PredefinedOverrideRuleId::IsUserMention,
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
            ruleset.set_enabled(RuleKind::Override, PredefinedOverrideRuleId::IsUserMention, true);
        assert!(result.is_ok());

        let result =
            ruleset.set_enabled(RuleKind::Override, PredefinedOverrideRuleId::IsUserMention, true);
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
        Mock::given(method("PUT")).respond_with(ResponseTemplate::new(200)).mount(&server).await;
        Mock::given(method("DELETE")).respond_with(ResponseTemplate::new(200)).mount(&server).await;

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
            ruleset.set_enabled(RuleKind::Override, PredefinedOverrideRuleId::IsUserMention, true);
        assert!(result.is_ok());

        let result = ruleset.set_actions(
            RuleKind::Override,
            PredefinedOverrideRuleId::IsUserMention,
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
            ruleset.set_enabled(RuleKind::Override, PredefinedOverrideRuleId::IsUserMention, true);
        assert!(result.is_ok());

        let result = ruleset.set_actions(
            RuleKind::Override,
            PredefinedOverrideRuleId::IsUserMention,
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
    async fn insert_notify_room_rule_invalid_room_id() {
        let server = MockServer::start().await;
        let client = logged_in_client(Some(server.uri())).await;

        let mut ruleset = client.account().push_rules().await.unwrap();
        let notification_settings = client.notification_settings();
        let room_id = "invalid_test_room_id".to_string();

        let mode =
            notification_settings.get_user_defined_room_notification_mode(&room_id, &ruleset);
        assert!(mode.is_none());

        let result = notification_settings.insert_room_rule(&room_id, true, &mut ruleset).await;
        match result {
            Err(error) => {
                assert!(matches!(error, NotificationSettingsError::InvalidRoomId))
            }
            _ => {
                panic!("a {:#?} error is expected.", NotificationSettingsError::InvalidRoomId)
            }
        }
    }

    #[async_test]
    async fn delete_user_defined_room_rules() {
        let server = MockServer::start().await;
        let client = logged_in_client(Some(server.uri())).await;
        Mock::given(method("PUT")).respond_with(ResponseTemplate::new(200)).mount(&server).await;
        Mock::given(method("DELETE")).respond_with(ResponseTemplate::new(200)).mount(&server).await;

        let mut ruleset = client.account().push_rules().await.unwrap();
        let notification_settings = client.notification_settings();
        let room_id = "!test_room:matrix.org".to_string();

        let mode =
            notification_settings.get_user_defined_room_notification_mode(&room_id, &ruleset);
        assert!(mode.is_none());

        // Add a Room rule
        let result = notification_settings.insert_room_rule(&room_id, false, &mut ruleset).await;
        assert!(result.is_ok());

        // Add a an Override rule
        let result =
            notification_settings.insert_override_room_rule(&room_id, false, &mut ruleset).await;
        assert!(result.is_ok());

        let custom_rules = notification_settings.get_custom_rules_for_room(&room_id, &ruleset);
        assert_eq!(custom_rules.len(), 2);

        // Delete the custom rules
        let result =
            notification_settings.delete_user_defined_room_rules(&room_id, &mut ruleset).await;
        assert!(result.is_ok());

        let custom_rules = notification_settings.get_custom_rules_for_room(&room_id, &ruleset);
        assert!(custom_rules.is_empty());
    }

    #[async_test]
    async fn set_room_notification_mode_default_to_all_messages() {
        let server = MockServer::start().await;
        let client = logged_in_client(Some(server.uri())).await;

        Mock::given(method("PUT")).respond_with(ResponseTemplate::new(200)).mount(&server).await;
        Mock::given(method("DELETE")).respond_with(ResponseTemplate::new(200)).mount(&server).await;

        let mut ruleset = client.account().push_rules().await.unwrap();
        let notification_settings = client.notification_settings();
        let room_id = "!test_room:matrix.org".to_string();

        // Default -> All
        let mode =
            notification_settings.get_user_defined_room_notification_mode(&room_id, &ruleset);
        assert!(mode.is_none());

        let result = notification_settings
            .set_room_notification_mode(&room_id, RoomNotificationMode::AllMessages, &mut ruleset)
            .await;
        assert!(result.is_ok());

        let expected_mode = &RoomNotificationMode::AllMessages;
        match notification_settings.get_user_defined_room_notification_mode(&room_id, &ruleset) {
            Some(new_mode) => {
                assert_eq!(
                    &new_mode, expected_mode,
                    "mode {:?} is expected (got {:?})",
                    expected_mode, &new_mode
                )
            }
            None => {
                panic!("mode {:?} is expected.", expected_mode)
            }
        }
    }

    #[async_test]
    async fn set_room_notification_mode_mentions_keywords_to_all_messages() {
        let server = MockServer::start().await;
        let client = logged_in_client(Some(server.uri())).await;

        Mock::given(method("PUT")).respond_with(ResponseTemplate::new(200)).mount(&server).await;
        Mock::given(method("DELETE")).respond_with(ResponseTemplate::new(200)).mount(&server).await;

        let mut ruleset = client.account().push_rules().await.unwrap();
        let notification_settings = client.notification_settings();
        let room_id = "!test_room:matrix.org".to_string();

        let result = notification_settings.insert_room_rule(&room_id, false, &mut ruleset).await;
        assert!(result.is_ok());

        let mode =
            notification_settings.get_user_defined_room_notification_mode(&room_id, &ruleset);
        assert_eq!(mode, Some(RoomNotificationMode::MentionsAndKeywordsOnly));

        // M&K -> All
        let result = notification_settings
            .set_room_notification_mode(&room_id, RoomNotificationMode::AllMessages, &mut ruleset)
            .await;
        assert!(result.is_ok());

        let expected_mode = &RoomNotificationMode::AllMessages;
        match notification_settings.get_user_defined_room_notification_mode(&room_id, &ruleset) {
            Some(new_mode) => {
                assert_eq!(
                    &new_mode, expected_mode,
                    "mode {:?} is expected (got {:?})",
                    expected_mode, &new_mode
                )
            }
            None => {
                panic!("mode {:?} is expected.", expected_mode)
            }
        }
    }

    #[async_test]
    async fn set_room_notification_mode_mute_to_all_messages() {
        let server = MockServer::start().await;
        let client = logged_in_client(Some(server.uri())).await;

        Mock::given(method("PUT")).respond_with(ResponseTemplate::new(200)).mount(&server).await;
        Mock::given(method("DELETE")).respond_with(ResponseTemplate::new(200)).mount(&server).await;

        let mut ruleset = client.account().push_rules().await.unwrap();
        let notification_settings = client.notification_settings();
        let room_id = "!test_room:matrix.org".to_string();

        let result =
            notification_settings.insert_override_room_rule(&room_id, false, &mut ruleset).await;
        assert!(result.is_ok());

        let mode =
            notification_settings.get_user_defined_room_notification_mode(&room_id, &ruleset);
        assert_eq!(mode, Some(RoomNotificationMode::Mute));

        // Mute -> All
        let result = notification_settings
            .set_room_notification_mode(&room_id, RoomNotificationMode::AllMessages, &mut ruleset)
            .await;
        assert!(result.is_ok());

        let expected_mode = &RoomNotificationMode::AllMessages;
        match notification_settings.get_user_defined_room_notification_mode(&room_id, &ruleset) {
            Some(new_mode) => {
                assert_eq!(
                    &new_mode, expected_mode,
                    "mode {:?} is expected (got {:?})",
                    expected_mode, &new_mode
                )
            }
            None => {
                panic!("mode {:?} is expected.", expected_mode)
            }
        }
    }

    #[async_test]
    async fn set_room_notification_mode_default_to_mentions_keywords() {
        let server = MockServer::start().await;
        let client = logged_in_client(Some(server.uri())).await;

        Mock::given(method("PUT")).respond_with(ResponseTemplate::new(200)).mount(&server).await;
        Mock::given(method("DELETE")).respond_with(ResponseTemplate::new(200)).mount(&server).await;

        let mut ruleset = client.account().push_rules().await.unwrap();
        let notification_settings = client.notification_settings();
        let room_id = "!test_room:matrix.org".to_string();

        let mode =
            notification_settings.get_user_defined_room_notification_mode(&room_id, &ruleset);
        assert!(mode.is_none());

        // Default -> M&K
        let result = notification_settings
            .set_room_notification_mode(
                &room_id,
                RoomNotificationMode::MentionsAndKeywordsOnly,
                &mut ruleset,
            )
            .await;
        assert!(result.is_ok());

        let expected_mode = &RoomNotificationMode::MentionsAndKeywordsOnly;
        match notification_settings.get_user_defined_room_notification_mode(&room_id, &ruleset) {
            Some(new_mode) => {
                assert_eq!(
                    &new_mode, expected_mode,
                    "mode {:?} is expected (got {:?})",
                    expected_mode, &new_mode
                )
            }
            None => {
                panic!("mode {:?} is expected.", expected_mode)
            }
        }
    }

    #[async_test]
    async fn set_room_notification_mode_all_messages_to_mentions_keywords() {
        let server = MockServer::start().await;
        let client = logged_in_client(Some(server.uri())).await;

        Mock::given(method("PUT")).respond_with(ResponseTemplate::new(200)).mount(&server).await;
        Mock::given(method("DELETE")).respond_with(ResponseTemplate::new(200)).mount(&server).await;

        let mut ruleset = client.account().push_rules().await.unwrap();
        let notification_settings = client.notification_settings();
        let room_id = "!test_room:matrix.org".to_string();

        let result = notification_settings.insert_room_rule(&room_id, true, &mut ruleset).await;
        assert!(result.is_ok());

        let mode =
            notification_settings.get_user_defined_room_notification_mode(&room_id, &ruleset);
        assert_eq!(mode, Some(RoomNotificationMode::AllMessages));

        // AllMessage -> M&K
        let result = notification_settings
            .set_room_notification_mode(
                &room_id,
                RoomNotificationMode::MentionsAndKeywordsOnly,
                &mut ruleset,
            )
            .await;
        assert!(result.is_ok());

        let expected_mode = &RoomNotificationMode::MentionsAndKeywordsOnly;
        match notification_settings.get_user_defined_room_notification_mode(&room_id, &ruleset) {
            Some(new_mode) => {
                assert_eq!(
                    &new_mode, expected_mode,
                    "mode {:?} is expected (got {:?})",
                    expected_mode, &new_mode
                )
            }
            None => {
                panic!("mode {:?} is expected.", expected_mode)
            }
        }
    }

    #[async_test]
    async fn set_room_notification_mode_mute_to_mentions_keywords() {
        let server = MockServer::start().await;
        let client = logged_in_client(Some(server.uri())).await;

        Mock::given(method("PUT")).respond_with(ResponseTemplate::new(200)).mount(&server).await;
        Mock::given(method("DELETE")).respond_with(ResponseTemplate::new(200)).mount(&server).await;

        let mut ruleset = client.account().push_rules().await.unwrap();
        let notification_settings = client.notification_settings();
        let room_id = "!test_room:matrix.org".to_string();

        let result =
            notification_settings.insert_override_room_rule(&room_id, false, &mut ruleset).await;
        assert!(result.is_ok());

        let mode =
            notification_settings.get_user_defined_room_notification_mode(&room_id, &ruleset);
        assert_eq!(mode, Some(RoomNotificationMode::Mute));

        // Mute -> M&K
        let result = notification_settings
            .set_room_notification_mode(
                &room_id,
                RoomNotificationMode::MentionsAndKeywordsOnly,
                &mut ruleset,
            )
            .await;
        assert!(result.is_ok());

        let expected_mode = &RoomNotificationMode::MentionsAndKeywordsOnly;
        match notification_settings.get_user_defined_room_notification_mode(&room_id, &ruleset) {
            Some(new_mode) => {
                assert_eq!(
                    &new_mode, expected_mode,
                    "mode {:?} is expected (got {:?})",
                    expected_mode, &new_mode
                )
            }
            None => {
                panic!("mode {:?} is expected.", expected_mode)
            }
        }
    }

    #[async_test]
    async fn set_room_notification_default_to_mute() {
        let server = MockServer::start().await;
        let client = logged_in_client(Some(server.uri())).await;

        Mock::given(method("PUT")).respond_with(ResponseTemplate::new(200)).mount(&server).await;
        Mock::given(method("DELETE")).respond_with(ResponseTemplate::new(200)).mount(&server).await;

        let mut ruleset = client.account().push_rules().await.unwrap();
        let notification_settings = client.notification_settings();
        let room_id = "!test_room:matrix.org".to_string();

        let mode =
            notification_settings.get_user_defined_room_notification_mode(&room_id, &ruleset);
        assert!(mode.is_none());

        // Default -> Mute
        let result = notification_settings
            .set_room_notification_mode(&room_id, RoomNotificationMode::Mute, &mut ruleset)
            .await;
        assert!(result.is_ok());

        let expected_mode = &RoomNotificationMode::Mute;
        match notification_settings.get_user_defined_room_notification_mode(&room_id, &ruleset) {
            Some(new_mode) => {
                assert_eq!(
                    &new_mode, expected_mode,
                    "mode {:?} is expected (got {:?})",
                    expected_mode, &new_mode
                )
            }
            None => {
                panic!("mode {:?} is expected.", expected_mode)
            }
        }
    }

    #[async_test]
    async fn set_room_notification_all_messages_to_mute() {
        let server = MockServer::start().await;
        let client = logged_in_client(Some(server.uri())).await;

        Mock::given(method("PUT")).respond_with(ResponseTemplate::new(200)).mount(&server).await;
        Mock::given(method("DELETE")).respond_with(ResponseTemplate::new(200)).mount(&server).await;

        let mut ruleset = client.account().push_rules().await.unwrap();
        let notification_settings = client.notification_settings();
        let room_id = "!test_room:matrix.org".to_string();

        let result = notification_settings.insert_room_rule(&room_id, true, &mut ruleset).await;
        assert!(result.is_ok());

        let mode =
            notification_settings.get_user_defined_room_notification_mode(&room_id, &ruleset);
        assert_eq!(mode, Some(RoomNotificationMode::AllMessages));

        // AllMessages -> Mute
        let result = notification_settings
            .set_room_notification_mode(&room_id, RoomNotificationMode::Mute, &mut ruleset)
            .await;
        assert!(result.is_ok());

        let expected_mode = &RoomNotificationMode::Mute;
        match notification_settings.get_user_defined_room_notification_mode(&room_id, &ruleset) {
            Some(new_mode) => {
                assert_eq!(
                    &new_mode, expected_mode,
                    "mode {:?} is expected (got {:?})",
                    expected_mode, &new_mode
                )
            }
            None => {
                panic!("mode {:?} is expected.", expected_mode)
            }
        }
    }

    #[async_test]
    async fn set_room_notification_mentions_keywords_to_mute() {
        let server = MockServer::start().await;
        let client = logged_in_client(Some(server.uri())).await;

        Mock::given(method("PUT")).respond_with(ResponseTemplate::new(200)).mount(&server).await;
        Mock::given(method("DELETE")).respond_with(ResponseTemplate::new(200)).mount(&server).await;

        let mut ruleset = client.account().push_rules().await.unwrap();
        let notification_settings = client.notification_settings();
        let room_id = "!test_room:matrix.org".to_string();

        let result = notification_settings.insert_room_rule(&room_id, false, &mut ruleset).await;
        assert!(result.is_ok());

        let mode =
            notification_settings.get_user_defined_room_notification_mode(&room_id, &ruleset);
        assert_eq!(mode, Some(RoomNotificationMode::MentionsAndKeywordsOnly));

        // M&K -> Mute
        let result = notification_settings
            .set_room_notification_mode(&room_id, RoomNotificationMode::Mute, &mut ruleset)
            .await;
        assert!(result.is_ok());

        let expected_mode = &RoomNotificationMode::Mute;
        match notification_settings.get_user_defined_room_notification_mode(&room_id, &ruleset) {
            Some(new_mode) => {
                assert_eq!(
                    &new_mode, expected_mode,
                    "mode {:?} is expected (got {:?})",
                    expected_mode, &new_mode
                )
            }
            None => {
                panic!("mode {:?} is expected.", expected_mode)
            }
        }
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

    #[async_test]
    async fn is_user_mention_enabled() {
        let server = MockServer::start().await;
        let client = logged_in_client(Some(server.uri())).await;

        let mut ruleset = client.account().push_rules().await.unwrap();
        let notification_settings = client.notification_settings();

        let result =
            ruleset.set_enabled(RuleKind::Override, PredefinedOverrideRuleId::IsUserMention, true);
        assert!(result.is_ok());
        assert!(notification_settings.is_user_mention_enabled(&ruleset));
    }

    #[async_test]
    async fn is_room_mention_enabled() {
        let server = MockServer::start().await;
        let client = logged_in_client(Some(server.uri())).await;

        let mut ruleset: Ruleset = client.account().push_rules().await.unwrap();
        let notification_settings = client.notification_settings();

        let result =
            ruleset.set_enabled(RuleKind::Override, PredefinedOverrideRuleId::IsRoomMention, true);
        assert!(result.is_ok());
        assert!(notification_settings.is_room_mention_enabled(&ruleset));
    }
}
