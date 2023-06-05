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
        match mode {
            RoomNotificationMode::AllMessages => {
                self.insert_notify_room_rule(room_id, ruleset).await
            }
            RoomNotificationMode::MentionsAndKeywordsOnly => {
                self.insert_mention_and_keywords_room_rule(room_id, ruleset).await
            }
            RoomNotificationMode::Mute => self.insert_mute_room_rule(room_id, ruleset).await,
        }
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

    /// Delete any user defined rules for a given room id
    pub async fn delete_user_defined_room_notification_mode(
        &self,
        room_id: &String,
        ruleset: &mut Ruleset,
    ) -> Result<(), NotificationSettingsError> {
        // remove any Override rules matching this room_id
        for rule in ruleset.override_.clone().iter() {
            // if the rule_id is the room_id
            if rule.rule_id == *room_id {
                let request = delete_pushrule::v3::Request::new(
                    RuleScope::Global,
                    RuleKind::Override,
                    rule.rule_id.clone(),
                );
                if self.client.send(request, None).await.is_err() {
                    return Err(NotificationSettingsError::UnableToRemovePushRule);
                }
                ruleset.remove(RuleKind::Override, &rule.rule_id)?;

                continue;
            }
            // if the rule contains a condition matching this room_id
            if rule.conditions.iter().any(|x| match x {
                PushCondition::EventMatch { key, pattern } => {
                    key == "room_id" && *pattern == *room_id
                }
                _ => false,
            }) {
                let request = delete_pushrule::v3::Request::new(
                    RuleScope::Global,
                    RuleKind::Override,
                    rule.rule_id.clone(),
                );
                if self.client.send(request, None).await.is_err() {
                    return Err(NotificationSettingsError::UnableToRemovePushRule);
                }
                ruleset.remove(RuleKind::Override, &rule.rule_id)?;

                continue;
            }
        }

        // remove any Room rules matching this room_id
        if let Some(rule) = ruleset.room.clone().iter().find(|x| x.rule_id == *room_id) {
            let request = delete_pushrule::v3::Request::new(
                RuleScope::Global,
                RuleKind::Room,
                rule.rule_id.to_string(),
            );
            if self.client.send(request, None).await.is_err() {
                return Err(NotificationSettingsError::UnableToRemovePushRule);
            }
            ruleset.remove(RuleKind::Room, &rule.rule_id)?;
        }

        // remove any Underride rules matching this room_id
        for rule in ruleset.underride.clone().iter() {
            // if the rule_id is the room_id
            if rule.rule_id == *room_id {
                let request = delete_pushrule::v3::Request::new(
                    RuleScope::Global,
                    RuleKind::Underride,
                    rule.rule_id.clone(),
                );
                if self.client.send(request, None).await.is_err() {
                    return Err(NotificationSettingsError::UnableToRemovePushRule);
                }
                ruleset.remove(RuleKind::Underride, &rule.rule_id)?;

                continue;
            }
            // if the rule contains a condition matching this room_id
            if rule.conditions.iter().any(|x| match x {
                PushCondition::EventMatch { key, pattern } => {
                    key == "room_id" && *pattern == *room_id
                }
                _ => false,
            }) {
                let request = delete_pushrule::v3::Request::new(
                    RuleScope::Global,
                    RuleKind::Underride,
                    rule.rule_id.clone(),
                );
                if self.client.send(request, None).await.is_err() {
                    return Err(NotificationSettingsError::UnableToRemovePushRule);
                }
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

    /// Insert a new rule to mute a given room in the given ruleset
    async fn insert_mute_room_rule(
        &self,
        room_id: &String,
        ruleset: &mut Ruleset,
    ) -> Result<(), NotificationSettingsError> {
        // Try to remove any existing rule for this room
        self.delete_user_defined_room_notification_mode(room_id, ruleset).await?;

        // Insert a new Override push rule without any actions
        let new_rule = NewConditionalPushRule::new(
            room_id.clone(),
            vec![PushCondition::EventMatch { key: "room_id".into(), pattern: room_id.clone() }],
            vec![],
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

    /// Insert a mention and keywords rule for a given room in the given
    /// ruleset
    async fn insert_mention_and_keywords_room_rule(
        &self,
        room_id: &String,
        ruleset: &mut Ruleset,
    ) -> Result<(), NotificationSettingsError> {
        // Try to remove any existing rule for this room
        self.delete_user_defined_room_notification_mode(room_id, ruleset).await?;

        // Insert a new Room push rule
        let new_rule = NewSimplePushRule::new(RoomId::parse(room_id)?, vec![]);
        let request =
            set_pushrule::v3::Request::new(RuleScope::Global, NewPushRule::Room(new_rule.clone()));
        if self.client.send(request, None).await.is_err() {
            return Err(NotificationSettingsError::UnableToAddPushRule);
        }
        ruleset.insert(NewPushRule::Room(new_rule), None, None)?;
        Ok(())
    }

    /// Insert a notify rule for a given room in the given ruleset
    async fn insert_notify_room_rule(
        &self,
        room_id: &String,
        ruleset: &mut Ruleset,
    ) -> Result<(), NotificationSettingsError> {
        // Try to remove any existing rule for this room
        self.delete_user_defined_room_notification_mode(room_id, ruleset).await?;

        // Insert a new Room push rule with a Notify action.
        let new_rule = NewSimplePushRule::new(
            RoomId::parse(room_id)?,
            vec![Action::Notify, Action::SetTweak(Tweak::Sound("default".into()))],
        );
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
    async fn insert_notify_room_rule() {
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

        let result = notification_settings.insert_notify_room_rule(&room_id, &mut ruleset).await;
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

        let result = notification_settings.insert_notify_room_rule(&room_id, &mut ruleset).await;
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
    async fn insert_mention_and_keywords_room_rule() {
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

        let result = notification_settings
            .insert_mention_and_keywords_room_rule(&room_id, &mut ruleset)
            .await;
        assert!(result.is_ok());

        let mode =
            notification_settings.get_user_defined_room_notification_mode(&room_id, &ruleset);
        assert_eq!(
            mode,
            Some(RoomNotificationMode::MentionsAndKeywordsOnly),
            "wrong mode, should be {:#?}",
            RoomNotificationMode::MentionsAndKeywordsOnly
        );
    }

    #[async_test]
    async fn insert_mute_room_rule() {
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

        let result = notification_settings.insert_mute_room_rule(&room_id, &mut ruleset).await;
        assert!(result.is_ok());

        let mode =
            notification_settings.get_user_defined_room_notification_mode(&room_id, &ruleset);
        assert_eq!(
            mode,
            Some(RoomNotificationMode::Mute),
            "wrong mode, should be {:#?}",
            RoomNotificationMode::Mute
        );
    }

    #[async_test]
    async fn delete_user_defined_room_notification_mode() {
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

        let result = notification_settings.insert_mute_room_rule(&room_id, &mut ruleset).await;
        assert!(result.is_ok());

        let mode =
            notification_settings.get_user_defined_room_notification_mode(&room_id, &ruleset);
        assert_eq!(
            mode,
            Some(RoomNotificationMode::Mute),
            "wrong mode, should be {:#?}",
            RoomNotificationMode::Mute
        );

        let result = notification_settings
            .delete_user_defined_room_notification_mode(&room_id, &mut ruleset)
            .await;
        assert!(result.is_ok());

        let mode =
            notification_settings.get_user_defined_room_notification_mode(&room_id, &ruleset);
        assert!(mode.is_none());
    }

    #[async_test]
    async fn set_room_notification_mode() {
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
