//! High-level push notification settings API

use std::sync::Arc;

use indexmap::IndexSet;
use ruma::{
    api::client::push::{
        delete_pushrule, set_pushrule, set_pushrule_actions, set_pushrule_enabled,
    },
    events::push_rules::PushRulesEvent,
    push::{Action, PredefinedUnderrideRuleId, RuleKind, Ruleset, Tweak},
    RoomId,
};
use tokio::sync::{
    broadcast::{self, Receiver},
    RwLock,
};
use tracing::{debug, error};

use self::{command::Command, rule_commands::RuleCommands, rules::Rules};

mod command;
mod rule_commands;
mod rules;

use crate::{
    config::RequestConfig, error::NotificationSettingsError, event_handler::EventHandlerDropGuard,
    Client, Result,
};

/// Enum representing the push notification modes for a room.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoomNotificationMode {
    /// Receive notifications for all messages.
    AllMessages,
    /// Receive notifications for mentions and keywords only.
    MentionsAndKeywordsOnly,
    /// Do not receive any notifications.
    Mute,
}

/// Whether or not a room is encrypted
#[derive(Debug, Clone, Copy)]
pub enum IsEncrypted {
    /// The room is encrypted
    Yes,
    /// The room is not encrypted
    No,
}

impl From<bool> for IsEncrypted {
    fn from(value: bool) -> Self {
        if value {
            Self::Yes
        } else {
            Self::No
        }
    }
}

/// Whether or not a room is a `one-to-one`
#[derive(Debug, Clone, Copy)]
pub enum IsOneToOne {
    /// A room is a `one-to-one` room if it has exactly two members.
    Yes,
    /// The room doesn't have exactly two members.
    No,
}

impl From<bool> for IsOneToOne {
    fn from(value: bool) -> Self {
        if value {
            Self::Yes
        } else {
            Self::No
        }
    }
}

/// A high-level API to manage the client owner's push notification settings.
#[derive(Debug, Clone)]
pub struct NotificationSettings {
    /// The underlying HTTP client.
    client: Client,
    /// Owner's account push rules. They will be updated on sync.
    rules: Arc<RwLock<Rules>>,
    /// Drop guard of event handler for push rules event.
    _push_rules_event_handler_guard: Arc<EventHandlerDropGuard>,
    changes_sender: broadcast::Sender<()>,
}

impl NotificationSettings {
    /// Build a new `NotificationSettings``
    ///
    /// # Arguments
    ///
    /// * `client` - A `Client` used to perform API calls
    /// * `ruleset` - A `Ruleset` containing account's owner push rules
    pub(crate) fn new(client: Client, ruleset: Ruleset) -> Self {
        let changes_sender = broadcast::Sender::new(100);
        let rules = Arc::new(RwLock::new(Rules::new(ruleset)));

        // Listen for PushRulesEvent
        let push_rules_event_handler_handle = client.add_event_handler({
            let changes_sender = changes_sender.clone();
            let rules = Arc::clone(&rules);
            move |ev: PushRulesEvent| async move {
                *rules.write().await = Rules::new(ev.content.global);
                let _ = changes_sender.send(());
            }
        });
        let _push_rules_event_handler_guard =
            client.event_handler_drop_guard(push_rules_event_handler_handle).into();

        Self { client, rules, _push_rules_event_handler_guard, changes_sender }
    }

    /// Subscribe to changes in the `NotificationSettings`.
    ///
    /// Changes can happen due to local changes or changes in another session.
    pub fn subscribe_to_changes(&self) -> Receiver<()> {
        self.changes_sender.subscribe()
    }

    /// Get the user defined notification mode for a room.
    pub async fn get_user_defined_room_notification_mode(
        &self,
        room_id: &RoomId,
    ) -> Option<RoomNotificationMode> {
        self.rules.read().await.get_user_defined_room_notification_mode(room_id)
    }

    /// Get the default notification mode for a room.
    ///
    /// # Arguments
    ///
    /// * `is_encrypted` - `Yes` if the room is encrypted
    /// * `is_one_to_one` - `Yes` if the room is a direct chat involving two
    ///   people
    pub async fn get_default_room_notification_mode(
        &self,
        is_encrypted: IsEncrypted,
        is_one_to_one: IsOneToOne,
    ) -> RoomNotificationMode {
        self.rules.read().await.get_default_room_notification_mode(is_encrypted, is_one_to_one)
    }

    /// Get all room IDs for which a user-defined rule exists.
    pub async fn get_rooms_with_user_defined_rules(&self, enabled: Option<bool>) -> Vec<String> {
        self.rules.read().await.get_rooms_with_user_defined_rules(enabled)
    }

    /// Get whether the given ruleset contains some enabled keywords rules.
    pub async fn contains_keyword_rules(&self) -> bool {
        self.rules.read().await.contains_keyword_rules()
    }

    /// Get whether a push rule is enabled.
    pub async fn is_push_rule_enabled(
        &self,
        kind: RuleKind,
        rule_id: impl AsRef<str>,
    ) -> Result<bool, NotificationSettingsError> {
        self.rules.read().await.is_enabled(kind, rule_id.as_ref())
    }

    /// Set whether a push rule is enabled.
    pub async fn set_push_rule_enabled(
        &self,
        kind: RuleKind,
        rule_id: impl AsRef<str>,
        enabled: bool,
    ) -> Result<(), NotificationSettingsError> {
        let rules = self.rules.read().await.clone();

        let mut rule_commands = RuleCommands::new(rules.ruleset);
        rule_commands.set_rule_enabled(kind, rule_id.as_ref(), enabled)?;

        self.run_server_commands(&rule_commands).await?;

        let rules = &mut *self.rules.write().await;
        rules.apply(rule_commands);

        Ok(())
    }

    /// Set the default notification mode for a type of room.
    ///
    /// # Arguments
    ///
    /// * `is_encrypted` - `Yes` if the mode is for encrypted rooms
    /// * `is_one_to_one` - `Yes` if the mode if for `one-to-one` rooms (rooms
    ///   with exactly two members)
    /// * `mode` - the new default mode
    pub async fn set_default_room_notification_mode(
        &self,
        is_encrypted: IsEncrypted,
        is_one_to_one: IsOneToOne,
        mode: RoomNotificationMode,
    ) -> Result<(), NotificationSettingsError> {
        let actions = match mode {
            RoomNotificationMode::AllMessages => {
                vec![Action::Notify, Action::SetTweak(Tweak::Sound("default".into()))]
            }
            _ => {
                vec![]
            }
        };

        let room_rule_id =
            rules::get_predefined_underride_room_rule_id(is_encrypted, is_one_to_one);
        self.set_underride_push_rule_actions(room_rule_id, actions.clone()).await?;

        let poll_start_rule_id = rules::get_predefined_underride_poll_start_rule_id(is_one_to_one);
        if let Err(error) =
            self.set_underride_push_rule_actions(poll_start_rule_id, actions.clone()).await
        {
            // The poll start event rules are currently unstable so they might not be found
            // on every homeserver. Let's ignore this error for the moment.
            if let NotificationSettingsError::RuleNotFound(rule_id) = &error {
                debug!("Unable to update poll start push rule: rule `{rule_id}` not found");
            } else {
                return Err(error);
            }
        }

        Ok(())
    }

    /// Sets the push rule actions for a given underride push rule. It also
    /// enables the push rule if it is disabled. [Underride rules] are the
    /// lowest priority push rules
    ///
    /// # Arguments
    ///
    /// * `rule_id` - the identifier of the push rule
    /// * `actions` - the actions to set for the push rule
    ///
    /// [Underride rules]: https://spec.matrix.org/v1.8/client-server-api/#push-rules
    pub async fn set_underride_push_rule_actions(
        &self,
        rule_id: PredefinedUnderrideRuleId,
        actions: Vec<Action>,
    ) -> Result<(), NotificationSettingsError> {
        let rules = self.rules.read().await.clone();
        let rule_kind = RuleKind::Underride;
        let mut rule_commands = RuleCommands::new(rules.clone().ruleset);

        rule_commands.set_rule_actions(rule_kind.clone(), rule_id.as_str(), actions)?;

        if !rules.is_enabled(rule_kind.clone(), rule_id.as_str())? {
            rule_commands.set_rule_enabled(rule_kind, rule_id.as_str(), true)?
        }

        self.run_server_commands(&rule_commands).await?;

        let rules = &mut *self.rules.write().await;
        rules.apply(rule_commands);

        Ok(())
    }

    /// Set the notification mode for a room.
    pub async fn set_room_notification_mode(
        &self,
        room_id: &RoomId,
        mode: RoomNotificationMode,
    ) -> Result<(), NotificationSettingsError> {
        let rules = self.rules.read().await.clone();

        // Check that the current mode is not already the target mode.
        if rules.get_user_defined_room_notification_mode(room_id) == Some(mode) {
            return Ok(());
        }

        // Build the command list to set the new mode
        let (new_rule_kind, notify) = match mode {
            RoomNotificationMode::AllMessages => {
                // insert a `Room` rule which notifies
                (RuleKind::Room, true)
            }
            RoomNotificationMode::MentionsAndKeywordsOnly => {
                // insert a `Room` rule which doesn't notify
                (RuleKind::Room, false)
            }
            RoomNotificationMode::Mute => {
                // insert an `Override` rule which doesn't notify
                (RuleKind::Override, false)
            }
        };

        // Extract all the custom rules except the one we just created.
        let new_rule_id = room_id.as_str();
        let custom_rules: Vec<(RuleKind, String)> = rules
            .get_custom_rules_for_room(room_id)
            .into_iter()
            .filter(|(kind, rule_id)| kind != &new_rule_kind || rule_id != new_rule_id)
            .collect();

        // Build the command list to delete all other custom rules, with the exception
        // of the newly inserted rule.
        let mut rule_commands = RuleCommands::new(rules.ruleset);
        rule_commands.insert_rule(new_rule_kind.clone(), room_id, notify)?;
        for (kind, rule_id) in custom_rules {
            rule_commands.delete_rule(kind, rule_id)?;
        }

        self.run_server_commands(&rule_commands).await?;

        let rules = &mut *self.rules.write().await;
        rules.apply(rule_commands);

        Ok(())
    }

    /// Delete all user defined rules for a room.
    pub async fn delete_user_defined_room_rules(
        &self,
        room_id: &RoomId,
    ) -> Result<(), NotificationSettingsError> {
        let rules = self.rules.read().await.clone();

        let custom_rules = rules.get_custom_rules_for_room(room_id);
        if custom_rules.is_empty() {
            return Ok(());
        }

        let mut rule_commands = RuleCommands::new(rules.ruleset);
        for (kind, rule_id) in custom_rules {
            rule_commands.delete_rule(kind, rule_id)?;
        }

        self.run_server_commands(&rule_commands).await?;

        let rules = &mut *self.rules.write().await;
        rules.apply(rule_commands);

        Ok(())
    }

    /// Unmute a room.
    pub async fn unmute_room(
        &self,
        room_id: &RoomId,
        is_encrypted: IsEncrypted,
        is_one_to_one: IsOneToOne,
    ) -> Result<(), NotificationSettingsError> {
        let rules = self.rules.read().await.clone();

        // Check if there is a user defined mode
        if let Some(room_mode) = rules.get_user_defined_room_notification_mode(room_id) {
            if room_mode != RoomNotificationMode::Mute {
                // Already unmuted
                return Ok(());
            }

            // Get default mode for this room
            let default_mode =
                rules.get_default_room_notification_mode(is_encrypted, is_one_to_one);

            // If the default mode is `Mute`, set it to `AllMessages`
            if default_mode == RoomNotificationMode::Mute {
                self.set_room_notification_mode(room_id, RoomNotificationMode::AllMessages).await
            } else {
                // Otherwise, delete user defined rules to use the default mode
                self.delete_user_defined_room_rules(room_id).await
            }
        } else {
            // This is the default mode, create a custom rule to unmute this room by setting
            // the mode to `AllMessages`
            self.set_room_notification_mode(room_id, RoomNotificationMode::AllMessages).await
        }
    }

    /// Get the keywords which have enabled rules.
    pub async fn enabled_keywords(&self) -> IndexSet<String> {
        self.rules.read().await.enabled_keywords()
    }

    /// Add or enable a rule for the given keyword.
    ///
    /// # Arguments
    ///
    /// * `keyword` - The keyword to match.
    pub async fn add_keyword(&self, keyword: String) -> Result<(), NotificationSettingsError> {
        let rules = self.rules.read().await.clone();

        let mut rule_commands = RuleCommands::new(rules.clone().ruleset);

        let existing_rules = rules.keyword_rules(&keyword);

        if existing_rules.is_empty() {
            // Create a rule.
            rule_commands.insert_keyword_rule(keyword)?;
        } else {
            if existing_rules.iter().any(|r| r.enabled) {
                // Nothing to do.
                return Ok(());
            }

            // Enable one of the rules.
            rule_commands.set_rule_enabled(RuleKind::Content, &existing_rules[0].rule_id, true)?;
        }

        self.run_server_commands(&rule_commands).await?;

        let rules = &mut *self.rules.write().await;
        rules.apply(rule_commands);

        Ok(())
    }

    /// Remove the rules for the given keyword.
    ///
    /// # Arguments
    ///
    /// * `keyword` - The keyword to unmatch.
    pub async fn remove_keyword(&self, keyword: &str) -> Result<(), NotificationSettingsError> {
        let rules = self.rules.read().await.clone();

        let mut rule_commands = RuleCommands::new(rules.clone().ruleset);

        let existing_rules = rules.keyword_rules(keyword);

        if existing_rules.is_empty() {
            return Ok(());
        }

        for rule in existing_rules {
            rule_commands.delete_rule(RuleKind::Content, rule.rule_id.clone())?;
        }

        self.run_server_commands(&rule_commands).await?;

        let rules = &mut *self.rules.write().await;
        rules.apply(rule_commands);

        Ok(())
    }

    /// Convert commands into requests to the server, and run them.
    async fn run_server_commands(
        &self,
        rule_commands: &RuleCommands,
    ) -> Result<(), NotificationSettingsError> {
        let request_config = Some(RequestConfig::short_retry());
        for command in &rule_commands.commands {
            match command {
                Command::DeletePushRule { scope, kind, rule_id } => {
                    let request = delete_pushrule::v3::Request::new(
                        scope.clone(),
                        kind.clone(),
                        rule_id.clone(),
                    );
                    self.client.send(request, request_config).await.map_err(|error| {
                        error!("Unable to delete {kind} push rule `{rule_id}`: {error}");
                        NotificationSettingsError::UnableToRemovePushRule
                    })?;
                }
                Command::SetRoomPushRule { scope, room_id, notify: _ } => {
                    let push_rule = command.to_push_rule()?;
                    let request = set_pushrule::v3::Request::new(scope.clone(), push_rule);
                    self.client.send(request, request_config).await.map_err(|error| {
                        error!("Unable to set room push rule `{room_id}`: {error}");
                        NotificationSettingsError::UnableToAddPushRule
                    })?;
                }
                Command::SetOverridePushRule { scope, rule_id, room_id: _, notify: _ } => {
                    let push_rule = command.to_push_rule()?;
                    let request = set_pushrule::v3::Request::new(scope.clone(), push_rule);
                    self.client.send(request, request_config).await.map_err(|error| {
                        error!("Unable to set override push rule `{rule_id}`: {error}");
                        NotificationSettingsError::UnableToAddPushRule
                    })?;
                }
                Command::SetKeywordPushRule { scope, keyword: _ } => {
                    let push_rule = command.to_push_rule()?;
                    let request = set_pushrule::v3::Request::new(scope.clone(), push_rule);
                    self.client
                        .send(request, request_config)
                        .await
                        .map_err(|_| NotificationSettingsError::UnableToAddPushRule)?;
                }
                Command::SetPushRuleEnabled { scope, kind, rule_id, enabled } => {
                    let request = set_pushrule_enabled::v3::Request::new(
                        scope.clone(),
                        kind.clone(),
                        rule_id.clone(),
                        *enabled,
                    );
                    self.client.send(request, request_config).await.map_err(|error| {
                        error!("Unable to set {kind} push rule `{rule_id}` enabled: {error}");
                        NotificationSettingsError::UnableToUpdatePushRule
                    })?;
                }
                Command::SetPushRuleActions { scope, kind, rule_id, actions } => {
                    let request = set_pushrule_actions::v3::Request::new(
                        scope.clone(),
                        kind.clone(),
                        rule_id.clone(),
                        actions.clone(),
                    );
                    self.client.send(request, request_config).await.map_err(|error| {
                        error!("Unable to set {kind} push rule `{rule_id}` actions: {error}");
                        NotificationSettingsError::UnableToUpdatePushRule
                    })?;
                }
            }
        }
        Ok(())
    }
}

// The http mocking library is not supported for wasm32
#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    };

    use assert_matches::assert_matches;
    use matrix_sdk_test::{
        async_test,
        notification_settings::{build_ruleset, get_server_default_ruleset},
        test_json,
    };
    use ruma::{
        push::{
            Action, AnyPushRuleRef, NewPatternedPushRule, NewPushRule, PredefinedOverrideRuleId,
            PredefinedUnderrideRuleId, RuleKind,
        },
        OwnedRoomId, RoomId,
    };
    use serde_json::json;
    use stream_assert::{assert_next_eq, assert_pending};
    use tokio_stream::wrappers::BroadcastStream;
    use wiremock::{
        matchers::{header, method, path, path_regex},
        Mock, MockServer, ResponseTemplate,
    };

    use crate::{
        config::SyncSettings,
        error::NotificationSettingsError,
        notification_settings::{
            IsEncrypted, IsOneToOne, NotificationSettings, RoomNotificationMode,
        },
        test_utils::logged_in_client,
        Client,
    };

    fn get_test_room_id() -> OwnedRoomId {
        RoomId::parse("!AAAaAAAAAaaAAaaaaa:matrix.org").unwrap()
    }

    fn from_insert_rules(
        client: &Client,
        rules: Vec<(RuleKind, &RoomId, bool)>,
    ) -> NotificationSettings {
        let ruleset = build_ruleset(rules);
        NotificationSettings::new(client.to_owned(), ruleset)
    }

    async fn get_custom_rules_for_room(
        settings: &NotificationSettings,
        room_id: &RoomId,
    ) -> Vec<(RuleKind, String)> {
        settings.rules.read().await.get_custom_rules_for_room(room_id)
    }

    #[async_test]
    async fn subscribe_to_changes() {
        let server = MockServer::start().await;
        let client = logged_in_client(Some(server.uri())).await;
        let settings = client.notification_settings().await;

        Mock::given(method("GET"))
            .and(path("/_matrix/client/r0/sync"))
            .and(header("authorization", "Bearer 1234"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "next_batch": "1234",
                "account_data": {
                    "events": [*test_json::PUSH_RULES]
                }
            })))
            .expect(1)
            .mount(&server)
            .await;

        let subscriber = settings.subscribe_to_changes();
        let mut stream = BroadcastStream::new(subscriber);

        assert_pending!(stream);

        client.sync_once(SyncSettings::default()).await.unwrap();

        assert_next_eq!(stream, Ok(()));
        assert_pending!(stream);
    }

    #[async_test]
    async fn test_get_custom_rules_for_room() {
        let server = MockServer::start().await;
        let client = logged_in_client(Some(server.uri())).await;
        let room_id = get_test_room_id();

        let settings = from_insert_rules(&client, vec![(RuleKind::Room, &room_id, true)]);

        let custom_rules = get_custom_rules_for_room(&settings, &room_id).await;
        assert_eq!(custom_rules.len(), 1);
        assert_eq!(custom_rules[0], (RuleKind::Room, room_id.to_string()));

        let settings = from_insert_rules(
            &client,
            vec![(RuleKind::Room, &room_id, true), (RuleKind::Override, &room_id, true)],
        );
        let custom_rules = get_custom_rules_for_room(&settings, &room_id).await;
        assert_eq!(custom_rules.len(), 2);
        assert_eq!(custom_rules[0], (RuleKind::Override, room_id.to_string()));
        assert_eq!(custom_rules[1], (RuleKind::Room, room_id.to_string()));
    }

    #[async_test]
    async fn test_get_user_defined_room_notification_mode_none() {
        let server = MockServer::start().await;
        let client = logged_in_client(Some(server.uri())).await;
        let room_id = get_test_room_id();

        let settings = client.notification_settings().await;
        assert!(settings.get_user_defined_room_notification_mode(&room_id).await.is_none());
    }

    #[async_test]
    async fn test_get_user_defined_room_notification_mode_all_messages() {
        let server = MockServer::start().await;
        let client = logged_in_client(Some(server.uri())).await;
        let room_id = get_test_room_id();

        // Initialize with a notifying `Room` rule to be in `AllMessages`
        let settings = from_insert_rules(&client, vec![(RuleKind::Room, &room_id, true)]);

        assert_eq!(
            settings.get_user_defined_room_notification_mode(&room_id).await.unwrap(),
            RoomNotificationMode::AllMessages
        );
    }

    #[async_test]
    async fn test_get_user_defined_room_notification_mode_mentions_and_keywords() {
        let server = MockServer::start().await;
        let client = logged_in_client(Some(server.uri())).await;
        let room_id = get_test_room_id();

        // Initialize with a muted `Room` rule to be in `MentionsAndKeywordsOnly`
        let settings = from_insert_rules(&client, vec![(RuleKind::Room, &room_id, false)]);
        assert_eq!(
            settings.get_user_defined_room_notification_mode(&room_id).await.unwrap(),
            RoomNotificationMode::MentionsAndKeywordsOnly
        );
    }

    #[async_test]
    async fn test_get_user_defined_room_notification_mode_mute() {
        let server = MockServer::start().await;
        let client = logged_in_client(Some(server.uri())).await;
        let room_id = get_test_room_id();

        // Initialize with a muted `Override` rule to be in `Mute`
        let settings = from_insert_rules(&client, vec![(RuleKind::Override, &room_id, false)]);
        assert_eq!(
            settings.get_user_defined_room_notification_mode(&room_id).await.unwrap(),
            RoomNotificationMode::Mute
        );
    }

    #[async_test]
    async fn test_get_default_room_notification_mode_all_messages() {
        let server = MockServer::start().await;
        let client = logged_in_client(Some(server.uri())).await;

        let mut ruleset = get_server_default_ruleset();
        ruleset
            .set_actions(
                RuleKind::Underride,
                PredefinedUnderrideRuleId::RoomOneToOne,
                vec![Action::Notify],
            )
            .unwrap();

        let settings = NotificationSettings::new(client, ruleset);
        assert_eq!(
            settings.get_default_room_notification_mode(IsEncrypted::No, IsOneToOne::Yes).await,
            RoomNotificationMode::AllMessages
        );
    }

    #[async_test]
    async fn test_get_default_room_notification_mode_mentions_and_keywords() {
        let server = MockServer::start().await;
        let client = logged_in_client(Some(server.uri())).await;

        // The default mode must be `MentionsAndKeywords` if the corresponding Underride
        // rule doesn't notify
        let mut ruleset = get_server_default_ruleset();
        ruleset
            .set_actions(RuleKind::Underride, PredefinedUnderrideRuleId::RoomOneToOne, vec![])
            .unwrap();

        let settings = NotificationSettings::new(client.to_owned(), ruleset.to_owned());
        assert_eq!(
            settings.get_default_room_notification_mode(IsEncrypted::No, IsOneToOne::Yes).await,
            RoomNotificationMode::MentionsAndKeywordsOnly
        );

        // The default mode must be `MentionsAndKeywords` if the corresponding Underride
        // rule is disabled
        ruleset
            .set_enabled(RuleKind::Underride, PredefinedUnderrideRuleId::RoomOneToOne, false)
            .unwrap();

        let settings = NotificationSettings::new(client, ruleset);
        assert_eq!(
            settings.get_default_room_notification_mode(IsEncrypted::No, IsOneToOne::Yes).await,
            RoomNotificationMode::MentionsAndKeywordsOnly
        );
    }

    #[async_test]
    async fn test_contains_keyword_rules() {
        let server = MockServer::start().await;
        let client = logged_in_client(Some(server.uri())).await;

        let mut ruleset = get_server_default_ruleset();
        let settings = NotificationSettings::new(client.to_owned(), ruleset.to_owned());

        // By default, no keywords rules should be present
        let contains_keywords_rules = settings.contains_keyword_rules().await;
        assert!(!contains_keywords_rules);

        // Initialize with a keyword rule
        let rule = NewPatternedPushRule::new(
            "keyword_rule_id".into(),
            "keyword".into(),
            vec![Action::Notify],
        );
        ruleset.insert(NewPushRule::Content(rule), None, None).unwrap();

        let settings = NotificationSettings::new(client, ruleset);
        let contains_keywords_rules = settings.contains_keyword_rules().await;
        assert!(contains_keywords_rules);
    }

    #[async_test]
    async fn test_is_push_rule_enabled() {
        let server = MockServer::start().await;
        let client = logged_in_client(Some(server.uri())).await;

        // Initial state: Reaction disabled
        let mut ruleset = get_server_default_ruleset();
        ruleset.set_enabled(RuleKind::Override, PredefinedOverrideRuleId::Reaction, false).unwrap();

        let settings = NotificationSettings::new(client.clone(), ruleset);

        let enabled = settings
            .is_push_rule_enabled(RuleKind::Override, PredefinedOverrideRuleId::Reaction)
            .await
            .unwrap();

        assert!(!enabled);

        // Initial state: Reaction enabled
        let mut ruleset = get_server_default_ruleset();
        ruleset.set_enabled(RuleKind::Override, PredefinedOverrideRuleId::Reaction, true).unwrap();

        let settings = NotificationSettings::new(client, ruleset);

        let enabled = settings
            .is_push_rule_enabled(RuleKind::Override, PredefinedOverrideRuleId::Reaction)
            .await
            .unwrap();

        assert!(enabled);
    }

    #[async_test]
    async fn test_set_push_rule_enabled() {
        let server = MockServer::start().await;
        let client = logged_in_client(Some(server.uri())).await;
        let mut ruleset = client.account().push_rules().await.unwrap();
        // Initial state
        ruleset.set_enabled(RuleKind::Override, PredefinedOverrideRuleId::Reaction, false).unwrap();

        let settings = NotificationSettings::new(client, ruleset);

        Mock::given(method("PUT"))
            .and(path("/_matrix/client/r0/pushrules/global/override/.m.rule.reaction/enabled"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        settings
            .set_push_rule_enabled(RuleKind::Override, PredefinedOverrideRuleId::Reaction, true)
            .await
            .unwrap();

        // The ruleset must have been updated
        let rules = settings.rules.read().await;
        let rule =
            rules.ruleset.get(RuleKind::Override, PredefinedOverrideRuleId::Reaction).unwrap();
        assert!(rule.enabled());

        server.verify().await
    }

    #[async_test]
    async fn test_set_push_rule_enabled_api_error() {
        let server = MockServer::start().await;
        let client = logged_in_client(Some(server.uri())).await;
        let mut ruleset = client.account().push_rules().await.unwrap();
        // Initial state
        ruleset
            .set_enabled(RuleKind::Override, PredefinedOverrideRuleId::IsUserMention, false)
            .unwrap();

        let settings = NotificationSettings::new(client, ruleset);

        // If the server returns an error
        Mock::given(method("PUT")).respond_with(ResponseTemplate::new(500)).mount(&server).await;

        // When enabling the push rule
        assert_eq!(
            settings
                .set_push_rule_enabled(
                    RuleKind::Override,
                    PredefinedOverrideRuleId::IsUserMention,
                    true,
                )
                .await,
            Err(NotificationSettingsError::UnableToUpdatePushRule)
        );

        // The ruleset must not have been updated
        let rules = settings.rules.read().await;
        let rule =
            rules.ruleset.get(RuleKind::Override, PredefinedOverrideRuleId::IsUserMention).unwrap();
        assert!(!rule.enabled());
    }

    #[async_test]
    async fn test_set_room_notification_mode() {
        let server = MockServer::start().await;
        let client = logged_in_client(Some(server.uri())).await;

        Mock::given(method("PUT")).respond_with(ResponseTemplate::new(200)).mount(&server).await;
        Mock::given(method("DELETE")).respond_with(ResponseTemplate::new(200)).mount(&server).await;

        let settings = client.notification_settings().await;
        let room_id = get_test_room_id();

        let mode = settings.get_user_defined_room_notification_mode(&room_id).await;
        assert!(mode.is_none());

        let new_modes = [
            RoomNotificationMode::AllMessages,
            RoomNotificationMode::MentionsAndKeywordsOnly,
            RoomNotificationMode::Mute,
        ];
        for new_mode in new_modes {
            settings.set_room_notification_mode(&room_id, new_mode).await.unwrap();

            assert_eq!(
                new_mode,
                settings.get_user_defined_room_notification_mode(&room_id).await.unwrap()
            );
        }
    }

    #[async_test]
    async fn test_set_room_notification_mode_requests_order() {
        let server = MockServer::start().await;
        let client = logged_in_client(Some(server.uri())).await;

        let put_was_called = Arc::new(AtomicBool::default());

        Mock::given(method("PUT"))
            .and(path_regex(r"_matrix/client/r0/pushrules/global/override/.*"))
            .and({
                let put_was_called = put_was_called.clone();
                move |_: &wiremock::Request| {
                    put_was_called.store(true, Ordering::SeqCst);

                    true
                }
            })
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        Mock::given(method("DELETE"))
            .and(path_regex(r"_matrix/client/r0/pushrules/global/room/.*"))
            .and(move |_: &wiremock::Request| {
                // Make sure that the PUT is executed before the DELETE, so that the following
                // sync results will give the following transitions:
                // `AllMessages` -> `AllMessages` -> `Mute` by sending the
                // DELETE before the PUT, we would have `AllMessages` ->
                // `Default` -> `Mute`

                let put_was_called = put_was_called.load(Ordering::SeqCst);
                assert!(
                    put_was_called,
                    "The PUT /pushrules/global/override/ method should have been called before the \
                     DELETE method"
                );

                true
            })
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let room_id = get_test_room_id();

        // Set the initial state to `AllMessages` by setting a `Room` rule that notifies
        let settings = from_insert_rules(&client, vec![(RuleKind::Room, &room_id, true)]);

        // Set the new mode to `Mute`, this will add a new `Override` rule without
        // action and remove the `Room` rule.
        settings.set_room_notification_mode(&room_id, RoomNotificationMode::Mute).await.unwrap();

        assert_eq!(
            RoomNotificationMode::Mute,
            settings.get_user_defined_room_notification_mode(&room_id).await.unwrap()
        );

        server.verify().await
    }

    #[async_test]
    async fn test_set_room_notification_mode_put_api_error() {
        let server = MockServer::start().await;
        let client = logged_in_client(Some(server.uri())).await;

        // If the server returns an error
        Mock::given(method("PUT")).respond_with(ResponseTemplate::new(500)).mount(&server).await;
        Mock::given(method("DELETE")).respond_with(ResponseTemplate::new(200)).mount(&server).await;

        let room_id = get_test_room_id();

        // Set the initial state to `AllMessages` by setting a `Room` rule that notifies
        let settings = from_insert_rules(&client, vec![(RuleKind::Room, &room_id, true)]);

        assert_eq!(
            settings.get_user_defined_room_notification_mode(&room_id).await.unwrap(),
            RoomNotificationMode::AllMessages
        );

        // Setting the new mode should fail
        assert_eq!(
            settings.set_room_notification_mode(&room_id, RoomNotificationMode::Mute).await,
            Err(NotificationSettingsError::UnableToAddPushRule)
        );

        // The ruleset must not have been updated
        assert_eq!(
            settings.get_user_defined_room_notification_mode(&room_id).await.unwrap(),
            RoomNotificationMode::AllMessages
        );
    }

    #[async_test]
    async fn test_set_room_notification_mode_delete_api_error() {
        let server = MockServer::start().await;
        let client = logged_in_client(Some(server.uri())).await;

        // If the server returns an error
        Mock::given(method("PUT")).respond_with(ResponseTemplate::new(200)).mount(&server).await;
        Mock::given(method("DELETE")).respond_with(ResponseTemplate::new(500)).mount(&server).await;

        let room_id = get_test_room_id();

        // Set the initial state to `AllMessages` by setting a `Room` rule that notifies
        let settings = from_insert_rules(&client, vec![(RuleKind::Room, &room_id, true)]);

        assert_eq!(
            settings.get_user_defined_room_notification_mode(&room_id).await.unwrap(),
            RoomNotificationMode::AllMessages
        );

        // Setting the new mode should fail
        assert_eq!(
            settings.set_room_notification_mode(&room_id, RoomNotificationMode::Mute).await,
            Err(NotificationSettingsError::UnableToRemovePushRule)
        );

        // The ruleset must not have been updated
        assert_eq!(
            settings.get_user_defined_room_notification_mode(&room_id).await.unwrap(),
            RoomNotificationMode::AllMessages
        );
    }

    #[async_test]
    async fn test_delete_user_defined_room_rules() {
        let server = MockServer::start().await;
        let client = logged_in_client(Some(server.uri())).await;
        let room_id_a = RoomId::parse("!AAAaAAAAAaaAAaaaaa:matrix.org").unwrap();
        let room_id_b = RoomId::parse("!BBBbBBBBBbbBBbbbbb:matrix.org").unwrap();

        Mock::given(method("DELETE")).respond_with(ResponseTemplate::new(200)).mount(&server).await;

        // Initialize with some of custom rules
        let settings = from_insert_rules(
            &client,
            vec![
                (RuleKind::Room, &room_id_a, true),
                (RuleKind::Room, &room_id_b, true),
                (RuleKind::Override, &room_id_b, true),
            ],
        );

        // Delete all user defined rules for room_id_a
        settings.delete_user_defined_room_rules(&room_id_a).await.unwrap();

        // Only the rules for room_id_b should remain
        let updated_rules = settings.rules.read().await;
        assert_eq!(updated_rules.get_custom_rules_for_room(&room_id_b).len(), 2);
        assert!(updated_rules.get_custom_rules_for_room(&room_id_a).is_empty());
    }

    #[async_test]
    async fn test_unmute_room_not_muted() {
        let server = MockServer::start().await;
        let client = logged_in_client(Some(server.uri())).await;
        let room_id = get_test_room_id();

        // Initialize with a `MentionsAndKeywordsOnly` mode
        let settings = from_insert_rules(&client, vec![(RuleKind::Room, &room_id, false)]);
        assert_eq!(
            settings.get_user_defined_room_notification_mode(&room_id).await.unwrap(),
            RoomNotificationMode::MentionsAndKeywordsOnly
        );

        // Unmute the room
        settings.unmute_room(&room_id, IsEncrypted::Yes, IsOneToOne::Yes).await.unwrap();

        // The ruleset must not be modified
        assert_eq!(
            settings.get_user_defined_room_notification_mode(&room_id).await.unwrap(),
            RoomNotificationMode::MentionsAndKeywordsOnly
        );

        let room_rules = get_custom_rules_for_room(&settings, &room_id).await;
        assert_eq!(room_rules.len(), 1);
        assert_matches!(settings.rules.read().await.ruleset.get(RuleKind::Room, &room_id),
            Some(AnyPushRuleRef::Room(rule)) => {
                assert_eq!(rule.rule_id, room_id);
                assert!(rule.actions.is_empty());
            }
        );
    }

    #[async_test]
    async fn test_unmute_room() {
        let server = MockServer::start().await;
        Mock::given(method("PUT")).respond_with(ResponseTemplate::new(200)).mount(&server).await;
        Mock::given(method("DELETE")).respond_with(ResponseTemplate::new(200)).mount(&server).await;
        let client = logged_in_client(Some(server.uri())).await;
        let room_id = get_test_room_id();

        // Start with the room muted
        let settings = from_insert_rules(&client, vec![(RuleKind::Override, &room_id, false)]);
        assert_eq!(
            settings.get_user_defined_room_notification_mode(&room_id).await,
            Some(RoomNotificationMode::Mute)
        );

        // Unmute the room
        settings.unmute_room(&room_id, IsEncrypted::No, IsOneToOne::Yes).await.unwrap();

        // The user defined mode must have been removed
        assert!(settings.get_user_defined_room_notification_mode(&room_id).await.is_none());
    }

    #[async_test]
    async fn test_unmute_room_default_mode() {
        let server = MockServer::start().await;
        Mock::given(method("PUT")).respond_with(ResponseTemplate::new(200)).mount(&server).await;
        let client = logged_in_client(Some(server.uri())).await;
        let room_id = get_test_room_id();
        let settings = client.notification_settings().await;

        // Unmute the room
        settings.unmute_room(&room_id, IsEncrypted::No, IsOneToOne::Yes).await.unwrap();

        // The new mode must be `AllMessages`
        assert_eq!(
            Some(RoomNotificationMode::AllMessages),
            settings.get_user_defined_room_notification_mode(&room_id).await
        );

        let room_rules = get_custom_rules_for_room(&settings, &room_id).await;
        assert_eq!(room_rules.len(), 1);
        assert_matches!(settings.rules.read().await.ruleset.get(RuleKind::Room, &room_id),
            Some(AnyPushRuleRef::Room(rule)) => {
                assert_eq!(rule.rule_id, room_id);
                assert!(!rule.actions.is_empty());
            }
        );
    }

    #[async_test]
    async fn test_set_default_room_notification_mode() {
        let server = MockServer::start().await;
        Mock::given(method("PUT")).respond_with(ResponseTemplate::new(200)).mount(&server).await;
        let client = logged_in_client(Some(server.uri())).await;

        // If the initial mode is `AllMessages`
        let mut ruleset = get_server_default_ruleset();
        ruleset
            .set_actions(
                RuleKind::Underride,
                PredefinedUnderrideRuleId::Message,
                vec![Action::Notify],
            )
            .unwrap();

        ruleset
            .set_actions(
                RuleKind::Underride,
                PredefinedUnderrideRuleId::PollStart,
                vec![Action::Notify],
            )
            .unwrap();

        let settings = NotificationSettings::new(client, ruleset);
        assert_eq!(
            settings.get_default_room_notification_mode(IsEncrypted::No, IsOneToOne::No).await,
            RoomNotificationMode::AllMessages
        );

        // After setting the default mode to `MentionsAndKeywordsOnly`
        settings
            .set_default_room_notification_mode(
                IsEncrypted::No,
                IsOneToOne::No,
                RoomNotificationMode::MentionsAndKeywordsOnly,
            )
            .await
            .unwrap();

        // The list of actions for this rule must be empty
        assert_matches!(settings.rules.read().await.ruleset.get(RuleKind::Underride, PredefinedUnderrideRuleId::Message),
            Some(AnyPushRuleRef::Underride(rule)) => {
                assert!(rule.actions.is_empty());
            }
        );

        assert_matches!(settings.rules.read().await.ruleset.get(RuleKind::Underride, PredefinedUnderrideRuleId::PollStart),
            Some(AnyPushRuleRef::Underride(rule)) => {
                assert!(rule.actions.is_empty());
            }
        );

        // and the new mode returned by `get_default_room_notification_mode()` should
        // reflect the change.
        assert_matches!(
            settings.get_default_room_notification_mode(IsEncrypted::No, IsOneToOne::No).await,
            RoomNotificationMode::MentionsAndKeywordsOnly
        );
    }

    #[async_test]
    async fn test_set_default_room_notification_mode_one_to_one() {
        let server = MockServer::start().await;
        Mock::given(method("PUT")).respond_with(ResponseTemplate::new(200)).mount(&server).await;
        let client = logged_in_client(Some(server.uri())).await;

        // If the initial mode is `AllMessages`
        let mut ruleset = get_server_default_ruleset();
        ruleset
            .set_actions(
                RuleKind::Underride,
                PredefinedUnderrideRuleId::RoomOneToOne,
                vec![Action::Notify],
            )
            .unwrap();

        ruleset
            .set_actions(
                RuleKind::Underride,
                PredefinedUnderrideRuleId::PollStartOneToOne,
                vec![Action::Notify],
            )
            .unwrap();

        let settings = NotificationSettings::new(client, ruleset);
        assert_eq!(
            settings.get_default_room_notification_mode(IsEncrypted::No, IsOneToOne::Yes).await,
            RoomNotificationMode::AllMessages
        );

        // After setting the default mode to `MentionsAndKeywordsOnly`
        settings
            .set_default_room_notification_mode(
                IsEncrypted::No,
                IsOneToOne::Yes,
                RoomNotificationMode::MentionsAndKeywordsOnly,
            )
            .await
            .unwrap();

        // The list of actions for this rule must be empty
        assert_matches!(settings.rules.read().await.ruleset.get(RuleKind::Underride, PredefinedUnderrideRuleId::RoomOneToOne),
            Some(AnyPushRuleRef::Underride(rule)) => {
                assert!(rule.actions.is_empty());
            }
        );

        assert_matches!(settings.rules.read().await.ruleset.get(RuleKind::Underride, PredefinedUnderrideRuleId::PollStartOneToOne),
            Some(AnyPushRuleRef::Underride(rule)) => {
                assert!(rule.actions.is_empty());
            }
        );

        // and the new mode returned by `get_default_room_notification_mode()` should
        // reflect the change.
        assert_matches!(
            settings.get_default_room_notification_mode(IsEncrypted::No, IsOneToOne::Yes).await,
            RoomNotificationMode::MentionsAndKeywordsOnly
        );
    }

    #[async_test]
    async fn test_set_default_room_notification_mode_enables_rules() {
        let server = MockServer::start().await;
        Mock::given(method("PUT")).respond_with(ResponseTemplate::new(200)).mount(&server).await;
        let client = logged_in_client(Some(server.uri())).await;

        // If the initial mode is `MentionsAndKeywordsOnly`
        let mut ruleset = get_server_default_ruleset();
        ruleset
            .set_actions(RuleKind::Underride, PredefinedUnderrideRuleId::RoomOneToOne, vec![])
            .unwrap();

        ruleset
            .set_actions(RuleKind::Underride, PredefinedUnderrideRuleId::PollStartOneToOne, vec![])
            .unwrap();

        // Disable one of the rules that will be updated
        ruleset
            .set_enabled(RuleKind::Underride, PredefinedUnderrideRuleId::RoomOneToOne, false)
            .unwrap();

        let settings = NotificationSettings::new(client, ruleset);

        // After setting the default mode to `AllMessages`
        settings
            .set_default_room_notification_mode(
                IsEncrypted::No,
                IsOneToOne::Yes,
                RoomNotificationMode::AllMessages,
            )
            .await
            .unwrap();

        // The new mode returned should be `AllMessages` which means that the disabled
        // rule (`RoomOneToOne`) has been enabled.
        assert_matches!(
            settings.get_default_room_notification_mode(IsEncrypted::No, IsOneToOne::Yes).await,
            RoomNotificationMode::AllMessages
        );
    }

    #[async_test]
    async fn list_keywords() {
        let server = MockServer::start().await;
        let client = logged_in_client(Some(server.uri())).await;

        // Initial state: No keywords
        let ruleset = get_server_default_ruleset();
        let settings = NotificationSettings::new(client.clone(), ruleset);

        let keywords = settings.enabled_keywords().await;

        assert!(keywords.is_empty());

        // Initial state: 3 rules, 2 keywords
        let mut ruleset = get_server_default_ruleset();
        ruleset
            .insert(
                NewPushRule::Content(NewPatternedPushRule::new(
                    "a".to_owned(),
                    "a".to_owned(),
                    vec![],
                )),
                None,
                None,
            )
            .unwrap();
        // Test deduplication.
        ruleset
            .insert(
                NewPushRule::Content(NewPatternedPushRule::new(
                    "a_bis".to_owned(),
                    "a".to_owned(),
                    vec![],
                )),
                None,
                None,
            )
            .unwrap();
        ruleset
            .insert(
                NewPushRule::Content(NewPatternedPushRule::new(
                    "b".to_owned(),
                    "b".to_owned(),
                    vec![],
                )),
                None,
                None,
            )
            .unwrap();

        let settings = NotificationSettings::new(client, ruleset);

        let keywords = settings.enabled_keywords().await;
        assert_eq!(keywords.len(), 2);
        assert!(keywords.get("a").is_some());
        assert!(keywords.get("b").is_some())
    }

    #[async_test]
    async fn add_keyword_missing() {
        let server = MockServer::start().await;
        let client = logged_in_client(Some(server.uri())).await;
        let settings = client.notification_settings().await;

        Mock::given(method("PUT"))
            .and(path("/_matrix/client/r0/pushrules/global/content/banana"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        settings.add_keyword("banana".to_owned()).await.unwrap();

        // The ruleset must have been updated.
        let keywords = settings.enabled_keywords().await;
        assert_eq!(keywords.len(), 1);
        assert!(keywords.get("banana").is_some());

        // Rule exists.
        let rule_enabled =
            settings.is_push_rule_enabled(RuleKind::Content, "banana").await.unwrap();
        assert!(rule_enabled)
    }

    #[async_test]
    async fn add_keyword_disabled() {
        let server = MockServer::start().await;
        let client = logged_in_client(Some(server.uri())).await;

        let mut ruleset = get_server_default_ruleset();
        ruleset
            .insert(
                NewPushRule::Content(NewPatternedPushRule::new(
                    "banana_two".to_owned(),
                    "banana".to_owned(),
                    vec![],
                )),
                None,
                None,
            )
            .unwrap();
        ruleset.set_enabled(RuleKind::Content, "banana_two", false).unwrap();
        ruleset
            .insert(
                NewPushRule::Content(NewPatternedPushRule::new(
                    "banana_one".to_owned(),
                    "banana".to_owned(),
                    vec![],
                )),
                None,
                None,
            )
            .unwrap();
        ruleset.set_enabled(RuleKind::Content, "banana_one", false).unwrap();

        let settings = NotificationSettings::new(client, ruleset);
        Mock::given(method("PUT"))
            .and(path("/_matrix/client/r0/pushrules/global/content/banana_one/enabled"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        settings.add_keyword("banana".to_owned()).await.unwrap();

        // The ruleset must have been updated.
        let keywords = settings.enabled_keywords().await;

        assert_eq!(keywords.len(), 1);
        assert!(keywords.get("banana").is_some());

        // The first rule was enabled.
        let first_rule_enabled =
            settings.is_push_rule_enabled(RuleKind::Content, "banana_one").await.unwrap();
        assert!(first_rule_enabled);
        let second_rule_enabled =
            settings.is_push_rule_enabled(RuleKind::Content, "banana_two").await.unwrap();
        assert!(!second_rule_enabled);
    }

    #[async_test]
    async fn add_keyword_noop() {
        let server = MockServer::start().await;
        let client = logged_in_client(Some(server.uri())).await;

        let mut ruleset = get_server_default_ruleset();
        ruleset
            .insert(
                NewPushRule::Content(NewPatternedPushRule::new(
                    "banana_two".to_owned(),
                    "banana".to_owned(),
                    vec![],
                )),
                None,
                None,
            )
            .unwrap();
        ruleset
            .insert(
                NewPushRule::Content(NewPatternedPushRule::new(
                    "banana_one".to_owned(),
                    "banana".to_owned(),
                    vec![],
                )),
                None,
                None,
            )
            .unwrap();
        ruleset.set_enabled(RuleKind::Content, "banana_one", false).unwrap();

        let settings = NotificationSettings::new(client, ruleset);
        settings.add_keyword("banana".to_owned()).await.unwrap();

        // Nothing changed.
        let keywords = settings.enabled_keywords().await;

        assert_eq!(keywords.len(), 1);
        assert!(keywords.get("banana").is_some());

        let first_rule_enabled =
            settings.is_push_rule_enabled(RuleKind::Content, "banana_one").await.unwrap();
        assert!(!first_rule_enabled);
        let second_rule_enabled =
            settings.is_push_rule_enabled(RuleKind::Content, "banana_two").await.unwrap();
        assert!(second_rule_enabled);
    }

    #[async_test]
    async fn remove_keyword_all() {
        let server = MockServer::start().await;
        let client = logged_in_client(Some(server.uri())).await;

        let mut ruleset = get_server_default_ruleset();
        ruleset
            .insert(
                NewPushRule::Content(NewPatternedPushRule::new(
                    "banana_two".to_owned(),
                    "banana".to_owned(),
                    vec![],
                )),
                None,
                None,
            )
            .unwrap();
        ruleset
            .insert(
                NewPushRule::Content(NewPatternedPushRule::new(
                    "banana_one".to_owned(),
                    "banana".to_owned(),
                    vec![],
                )),
                None,
                None,
            )
            .unwrap();
        ruleset.set_enabled(RuleKind::Content, "banana_one", false).unwrap();

        let settings = NotificationSettings::new(client, ruleset);

        Mock::given(method("DELETE"))
            .and(path("/_matrix/client/r0/pushrules/global/content/banana_one"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("DELETE"))
            .and(path("/_matrix/client/r0/pushrules/global/content/banana_two"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        settings.remove_keyword("banana").await.unwrap();

        // The ruleset must have been updated.
        let keywords = settings.enabled_keywords().await;
        assert!(keywords.is_empty());

        // Rules we removed.
        let first_rule_error =
            settings.is_push_rule_enabled(RuleKind::Content, "banana_one").await.unwrap_err();
        assert_matches!(first_rule_error, NotificationSettingsError::RuleNotFound(_));
        let second_rule_error =
            settings.is_push_rule_enabled(RuleKind::Content, "banana_two").await.unwrap_err();
        assert_matches!(second_rule_error, NotificationSettingsError::RuleNotFound(_));
    }

    #[async_test]
    async fn remove_keyword_noop() {
        let server = MockServer::start().await;
        let client = logged_in_client(Some(server.uri())).await;
        let settings = client.notification_settings().await;

        settings.remove_keyword("banana").await.unwrap();
    }

    #[async_test]
    async fn test_set_default_room_notification_mode_missing_poll_start() {
        let server = MockServer::start().await;
        Mock::given(method("PUT")).respond_with(ResponseTemplate::new(200)).mount(&server).await;
        let client = logged_in_client(Some(server.uri())).await;

        // If the initial mode is `AllMessages`
        let mut ruleset = get_server_default_ruleset();
        ruleset.underride.remove(PredefinedUnderrideRuleId::PollStart.as_str());

        let settings = NotificationSettings::new(client, ruleset);
        assert_eq!(
            settings.get_default_room_notification_mode(IsEncrypted::No, IsOneToOne::No).await,
            RoomNotificationMode::AllMessages
        );

        // After setting the default mode to `MentionsAndKeywordsOnly`
        settings
            .set_default_room_notification_mode(
                IsEncrypted::No,
                IsOneToOne::No,
                RoomNotificationMode::MentionsAndKeywordsOnly,
            )
            .await
            .unwrap();

        // the new mode returned by `get_default_room_notification_mode()` should
        // reflect the change.
        assert_matches!(
            settings.get_default_room_notification_mode(IsEncrypted::No, IsOneToOne::No).await,
            RoomNotificationMode::MentionsAndKeywordsOnly
        );
    }
}
