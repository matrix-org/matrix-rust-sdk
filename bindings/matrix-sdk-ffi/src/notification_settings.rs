use std::sync::{Arc, RwLock};

use matrix_sdk::{
    event_handler::EventHandlerHandle,
    notification_settings::{
        NotificationSettings as SdkNotificationSettings,
        RoomNotificationMode as SdkRoomNotificationMode,
    },
    ruma::events::push_rules::PushRulesEvent,
    Client as MatrixClient,
};
use matrix_sdk_common::{SendOutsideWasm, SyncOutsideWasm};
use ruma::{
    events::push_rules::PushRulesEventContent,
    push::{
        Action as SdkAction, ComparisonOperator as SdkComparisonOperator, PredefinedOverrideRuleId,
        PredefinedUnderrideRuleId, PushCondition as SdkPushCondition, RoomMemberCountIs,
        RuleKind as SdkRuleKind, ScalarJsonValue as SdkJsonValue, Tweak as SdkTweak,
    },
    Int, RoomId, UInt,
};
use tokio::sync::RwLock as AsyncRwLock;

use crate::error::{ClientError, NotificationSettingsError};

#[derive(Clone, Default, uniffi::Enum)]
pub enum ComparisonOperator {
    /// Equals
    #[default]
    Eq,

    /// Less than
    Lt,

    /// Greater than
    Gt,

    /// Greater or equal
    Ge,

    /// Less or equal
    Le,
}

impl From<SdkComparisonOperator> for ComparisonOperator {
    fn from(value: SdkComparisonOperator) -> Self {
        match value {
            SdkComparisonOperator::Eq => Self::Eq,
            SdkComparisonOperator::Lt => Self::Lt,
            SdkComparisonOperator::Gt => Self::Gt,
            SdkComparisonOperator::Ge => Self::Ge,
            SdkComparisonOperator::Le => Self::Le,
        }
    }
}

impl From<ComparisonOperator> for SdkComparisonOperator {
    fn from(value: ComparisonOperator) -> Self {
        match value {
            ComparisonOperator::Eq => Self::Eq,
            ComparisonOperator::Lt => Self::Lt,
            ComparisonOperator::Gt => Self::Gt,
            ComparisonOperator::Ge => Self::Ge,
            ComparisonOperator::Le => Self::Le,
        }
    }
}

#[derive(Debug, Clone, Default, uniffi::Enum)]
pub enum JsonValue {
    /// Represents a `null` value.
    #[default]
    Null,

    /// Represents a boolean.
    Bool { value: bool },

    /// Represents an integer.
    Integer { value: i64 },

    /// Represents a string.
    String { value: String },
}

impl From<SdkJsonValue> for JsonValue {
    fn from(value: SdkJsonValue) -> Self {
        match value {
            SdkJsonValue::Null => Self::Null,
            SdkJsonValue::Bool(b) => Self::Bool { value: b },
            SdkJsonValue::Integer(i) => Self::Integer { value: i.into() },
            SdkJsonValue::String(s) => Self::String { value: s },
        }
    }
}

impl From<JsonValue> for SdkJsonValue {
    fn from(value: JsonValue) -> Self {
        match value {
            JsonValue::Null => Self::Null,
            JsonValue::Bool { value } => Self::Bool(value),
            JsonValue::Integer { value } => Self::Integer(Int::new(value).unwrap_or_default()),
            JsonValue::String { value } => Self::String(value),
        }
    }
}

#[derive(Clone, uniffi::Enum)]
pub enum PushCondition {
    /// A glob pattern match on a field of the event.
    EventMatch {
        /// The [dot-separated path] of the property of the event to match.
        ///
        /// [dot-separated path]: https://spec.matrix.org/latest/appendices/#dot-separated-property-paths
        key: String,

        /// The glob-style pattern to match against.
        ///
        /// Patterns with no special glob characters should be treated as having
        /// asterisks prepended and appended when testing the condition.
        pattern: String,
    },

    /// Matches unencrypted messages where `content.body` contains the owner's
    /// display name in that room.
    ContainsDisplayName,

    /// Matches the current number of members in the room.
    RoomMemberCount { prefix: ComparisonOperator, count: u64 },

    /// Takes into account the current power levels in the room, ensuring the
    /// sender of the event has high enough power to trigger the
    /// notification.
    SenderNotificationPermission {
        /// The field in the power level event the user needs a minimum power
        /// level for.
        ///
        /// Fields must be specified under the `notifications` property in the
        /// power level event's `content`.
        key: String,
    },

    /// Exact value match on a property of the event.
    EventPropertyIs {
        /// The [dot-separated path] of the property of the event to match.
        ///
        /// [dot-separated path]: https://spec.matrix.org/latest/appendices/#dot-separated-property-paths
        key: String,

        /// The value to match against.
        value: JsonValue,
    },

    /// Exact value match on a value in an array property of the event.
    EventPropertyContains {
        /// The [dot-separated path] of the property of the event to match.
        ///
        /// [dot-separated path]: https://spec.matrix.org/latest/appendices/#dot-separated-property-paths
        key: String,

        /// The value to match against.
        value: JsonValue,
    },
}

impl TryFrom<SdkPushCondition> for PushCondition {
    type Error = String;

    fn try_from(value: SdkPushCondition) -> Result<Self, Self::Error> {
        Ok(match value {
            SdkPushCondition::EventMatch { key, pattern } => Self::EventMatch { key, pattern },
            #[allow(deprecated)]
            SdkPushCondition::ContainsDisplayName => Self::ContainsDisplayName,
            SdkPushCondition::RoomMemberCount { is } => {
                Self::RoomMemberCount { prefix: is.prefix.into(), count: is.count.into() }
            }
            SdkPushCondition::SenderNotificationPermission { key } => {
                Self::SenderNotificationPermission { key: key.to_string() }
            }
            SdkPushCondition::EventPropertyIs { key, value } => {
                Self::EventPropertyIs { key, value: value.into() }
            }
            SdkPushCondition::EventPropertyContains { key, value } => {
                Self::EventPropertyContains { key, value: value.into() }
            }
            _ => return Err("Unsupported condition type".to_owned()),
        })
    }
}

impl From<PushCondition> for SdkPushCondition {
    fn from(value: PushCondition) -> Self {
        match value {
            PushCondition::EventMatch { key, pattern } => Self::EventMatch { key, pattern },
            #[allow(deprecated)]
            PushCondition::ContainsDisplayName => Self::ContainsDisplayName,
            PushCondition::RoomMemberCount { prefix, count } => Self::RoomMemberCount {
                is: RoomMemberCountIs {
                    prefix: prefix.into(),
                    count: UInt::new(count).unwrap_or_default(),
                },
            },
            PushCondition::SenderNotificationPermission { key } => {
                Self::SenderNotificationPermission { key: key.into() }
            }
            PushCondition::EventPropertyIs { key, value } => {
                Self::EventPropertyIs { key, value: value.into() }
            }
            PushCondition::EventPropertyContains { key, value } => {
                Self::EventPropertyContains { key, value: value.into() }
            }
        }
    }
}

#[derive(Clone, uniffi::Enum)]
pub enum RuleKind {
    /// User-configured rules that override all other kinds.
    Override,

    /// Lowest priority user-defined rules.
    Underride,

    /// Sender-specific rules.
    Sender,

    /// Room-specific rules.
    Room,

    /// Content-specific rules.
    Content,

    Custom {
        value: String,
    },
}

impl From<SdkRuleKind> for RuleKind {
    fn from(value: SdkRuleKind) -> Self {
        match value {
            SdkRuleKind::Override => Self::Override,
            SdkRuleKind::Underride => Self::Underride,
            SdkRuleKind::Sender => Self::Sender,
            SdkRuleKind::Room => Self::Room,
            SdkRuleKind::Content => Self::Content,
            SdkRuleKind::_Custom(_) => Self::Custom { value: value.as_str().to_owned() },
            _ => Self::Custom { value: value.to_string() },
        }
    }
}

impl From<RuleKind> for SdkRuleKind {
    fn from(value: RuleKind) -> Self {
        match value {
            RuleKind::Override => Self::Override,
            RuleKind::Underride => Self::Underride,
            RuleKind::Sender => Self::Sender,
            RuleKind::Room => Self::Room,
            RuleKind::Content => Self::Content,
            RuleKind::Custom { value } => SdkRuleKind::from(value),
        }
    }
}

#[derive(Clone, uniffi::Enum)]
/// Enum representing the push notification tweaks for a rule.
pub enum Tweak {
    /// A string representing the sound to be played when this notification
    /// arrives.
    ///
    /// A value of "default" means to play a default sound. A device may choose
    /// to alert the user by some other means if appropriate, eg. vibration.
    Sound { value: String },

    /// A boolean representing whether or not this message should be highlighted
    /// in the UI.
    Highlight { value: bool },

    /// A custom tweak
    Custom {
        /// The name of the custom tweak (`set_tweak` field)
        name: String,

        /// The value of the custom tweak as an encoded JSON string
        value: String,
    },
}

impl TryFrom<SdkTweak> for Tweak {
    type Error = String;

    fn try_from(value: SdkTweak) -> Result<Self, Self::Error> {
        Ok(match value {
            SdkTweak::Sound(sound) => Self::Sound { value: sound },
            SdkTweak::Highlight(highlight) => Self::Highlight { value: highlight },
            SdkTweak::Custom { name, value } => {
                let json_string = serde_json::to_string(&value)
                    .map_err(|e| format!("Failed to serialize custom tweak value: {e}"))?;

                Self::Custom { name, value: json_string }
            }
            _ => return Err("Unsupported tweak type".to_owned()),
        })
    }
}

impl TryFrom<Tweak> for SdkTweak {
    type Error = String;

    fn try_from(value: Tweak) -> Result<Self, Self::Error> {
        Ok(match value {
            Tweak::Sound { value } => Self::Sound(value),
            Tweak::Highlight { value } => Self::Highlight(value),
            Tweak::Custom { name, value } => {
                let json_value: serde_json::Value = serde_json::from_str(&value)
                    .map_err(|e| format!("Failed to deserialize custom tweak value: {e}"))?;
                let value = serde_json::from_value(json_value)
                    .map_err(|e| format!("Failed to convert JSON value: {e}"))?;

                Self::Custom { name, value }
            }
        })
    }
}

#[derive(Clone, uniffi::Enum)]
/// Enum representing the push notification actions for a rule.
pub enum Action {
    /// Causes matching events to generate a notification.
    Notify,
    /// Sets an entry in the 'tweaks' dictionary sent to the push gateway.
    SetTweak { value: Tweak },
}

impl TryFrom<SdkAction> for Action {
    type Error = String;

    fn try_from(value: SdkAction) -> Result<Self, Self::Error> {
        Ok(match value {
            SdkAction::Notify => Self::Notify,
            SdkAction::SetTweak(tweak) => Self::SetTweak {
                value: tweak.try_into().map_err(|e| format!("Failed to convert tweak: {e}"))?,
            },
            _ => return Err("Unsupported action type".to_owned()),
        })
    }
}

impl TryFrom<Action> for SdkAction {
    type Error = String;

    fn try_from(value: Action) -> Result<Self, Self::Error> {
        Ok(match value {
            Action::Notify => Self::Notify,
            Action::SetTweak { value } => Self::SetTweak(
                value.try_into().map_err(|e| format!("Failed to convert tweak: {e}"))?,
            ),
        })
    }
}

/// Enum representing the push notification modes for a room.
#[derive(Clone, uniffi::Enum)]
pub enum RoomNotificationMode {
    /// Receive notifications for all messages.
    AllMessages,
    /// Receive notifications for mentions and keywords only.
    MentionsAndKeywordsOnly,
    /// Do not receive any notifications.
    Mute,
}

impl From<SdkRoomNotificationMode> for RoomNotificationMode {
    fn from(value: SdkRoomNotificationMode) -> Self {
        match value {
            SdkRoomNotificationMode::AllMessages => Self::AllMessages,
            SdkRoomNotificationMode::MentionsAndKeywordsOnly => Self::MentionsAndKeywordsOnly,
            SdkRoomNotificationMode::Mute => Self::Mute,
        }
    }
}

impl From<RoomNotificationMode> for SdkRoomNotificationMode {
    fn from(value: RoomNotificationMode) -> Self {
        match value {
            RoomNotificationMode::AllMessages => Self::AllMessages,
            RoomNotificationMode::MentionsAndKeywordsOnly => Self::MentionsAndKeywordsOnly,
            RoomNotificationMode::Mute => Self::Mute,
        }
    }
}

/// Delegate to notify of changes in push rules
#[matrix_sdk_ffi_macros::export(callback_interface)]
pub trait NotificationSettingsDelegate: SyncOutsideWasm + SendOutsideWasm {
    fn settings_did_change(&self);
}

/// `RoomNotificationSettings` represents the current settings for a `Room`
#[derive(Clone, uniffi::Record)]
pub struct RoomNotificationSettings {
    /// The room notification mode
    mode: RoomNotificationMode,
    /// Whether the mode is the default one
    is_default: bool,
}

impl RoomNotificationSettings {
    fn new(mode: RoomNotificationMode, is_default: bool) -> Self {
        RoomNotificationSettings { mode, is_default }
    }
}

#[derive(Clone, uniffi::Object)]
pub struct NotificationSettings {
    sdk_client: MatrixClient,
    sdk_notification_settings: Arc<AsyncRwLock<SdkNotificationSettings>>,
    pushrules_event_handler: Arc<RwLock<Option<EventHandlerHandle>>>,
}

impl NotificationSettings {
    pub(crate) fn new(
        sdk_client: MatrixClient,
        sdk_notification_settings: SdkNotificationSettings,
    ) -> Self {
        Self {
            sdk_client,
            sdk_notification_settings: Arc::new(AsyncRwLock::new(sdk_notification_settings)),
            pushrules_event_handler: Arc::new(RwLock::new(None)),
        }
    }
}

impl Drop for NotificationSettings {
    fn drop(&mut self) {
        // Remove the event handler on the sdk_client.
        if let Some(event_handler) = self.pushrules_event_handler.read().unwrap().as_ref() {
            self.sdk_client.remove_event_handler(event_handler.clone());
        }
    }
}

#[matrix_sdk_ffi_macros::export]
impl NotificationSettings {
    pub fn set_delegate(&self, delegate: Option<Box<dyn NotificationSettingsDelegate>>) {
        if let Some(delegate) = delegate {
            let delegate: Arc<dyn NotificationSettingsDelegate> = Arc::from(delegate);

            // Add an event handler to listen to `PushRulesEvent`
            let event_handler =
                self.sdk_client.add_event_handler(move |_: PushRulesEvent| async move {
                    delegate.settings_did_change();
                });

            *self.pushrules_event_handler.write().unwrap() = Some(event_handler);
        } else {
            // Remove the event handler if there is no delegate
            let event_handler = &mut *self.pushrules_event_handler.write().unwrap();
            if let Some(event_handler) = event_handler {
                self.sdk_client.remove_event_handler(event_handler.clone());
            }
            *event_handler = None;
        }
    }

    /// Get the notification settings for a room.
    ///
    /// # Arguments
    ///
    /// * `room_id` - the room ID
    /// * `is_encrypted` - whether the room is encrypted
    /// * `is_one_to_one` - whether the room is a direct chat involving two
    ///   people
    pub async fn get_room_notification_settings(
        &self,
        room_id: String,
        is_encrypted: bool,
        is_one_to_one: bool,
    ) -> Result<RoomNotificationSettings, NotificationSettingsError> {
        let parsed_room_id = RoomId::parse(&room_id)
            .map_err(|_e| NotificationSettingsError::InvalidRoomId { room_id })?;

        let notification_settings = self.sdk_notification_settings.read().await;

        // Get the current user defined mode for this room
        if let Some(mode) =
            notification_settings.get_user_defined_room_notification_mode(&parsed_room_id).await
        {
            return Ok(RoomNotificationSettings::new(mode.into(), false));
        }

        // If the user has not defined a notification mode, return the default one for
        // this room
        let mode = notification_settings
            .get_default_room_notification_mode(is_encrypted.into(), is_one_to_one.into())
            .await;

        Ok(RoomNotificationSettings::new(mode.into(), true))
    }

    /// Set the notification mode for a room.
    pub async fn set_room_notification_mode(
        &self,
        room_id: String,
        mode: RoomNotificationMode,
    ) -> Result<(), NotificationSettingsError> {
        let parsed_room_id = RoomId::parse(&room_id)
            .map_err(|_e| NotificationSettingsError::InvalidRoomId { room_id })?;

        self.sdk_notification_settings
            .read()
            .await
            .set_room_notification_mode(&parsed_room_id, mode.into())
            .await?;

        Ok(())
    }

    /// Get the user defined room notification mode
    pub async fn get_user_defined_room_notification_mode(
        &self,
        room_id: String,
    ) -> Result<Option<RoomNotificationMode>, NotificationSettingsError> {
        let notification_settings = self.sdk_notification_settings.read().await;
        let parsed_room_id = RoomId::parse(&room_id)
            .map_err(|_e| NotificationSettingsError::InvalidRoomId { room_id })?;
        // Get the current user defined mode for this room
        if let Some(mode) =
            notification_settings.get_user_defined_room_notification_mode(&parsed_room_id).await
        {
            Ok(Some(mode.into()))
        } else {
            Ok(None)
        }
    }

    /// Get the default room notification mode
    ///
    /// The mode will depend on the associated `PushRule` based on whether the
    /// room is encrypted or not, and on the number of members.
    ///
    /// # Arguments
    ///
    /// * `is_encrypted` - whether the room is encrypted
    /// * `is_one_to_one` - whether the room is a direct chats involving two
    ///   people
    pub async fn get_default_room_notification_mode(
        &self,
        is_encrypted: bool,
        is_one_to_one: bool,
    ) -> RoomNotificationMode {
        let notification_settings = self.sdk_notification_settings.read().await;
        let mode = notification_settings
            .get_default_room_notification_mode(is_encrypted.into(), is_one_to_one.into())
            .await;
        mode.into()
    }

    /// Set the default room notification mode
    ///
    /// # Arguments
    ///
    /// * `is_encrypted` - whether the mode is for encrypted rooms
    /// * `is_one_to_one` - whether the mode is for direct chats involving two
    ///   people
    /// * `mode` - the new default mode
    pub async fn set_default_room_notification_mode(
        &self,
        is_encrypted: bool,
        is_one_to_one: bool,
        mode: RoomNotificationMode,
    ) -> Result<(), NotificationSettingsError> {
        let notification_settings = self.sdk_notification_settings.read().await;
        notification_settings
            .set_default_room_notification_mode(
                is_encrypted.into(),
                is_one_to_one.into(),
                mode.into(),
            )
            .await?;
        Ok(())
    }

    /// Restore the default notification mode for a room
    pub async fn restore_default_room_notification_mode(
        &self,
        room_id: String,
    ) -> Result<(), NotificationSettingsError> {
        let notification_settings = self.sdk_notification_settings.read().await;
        let parsed_room_id = RoomId::parse(&room_id)
            .map_err(|_e| NotificationSettingsError::InvalidRoomId { room_id })?;
        notification_settings.delete_user_defined_room_rules(&parsed_room_id).await?;
        Ok(())
    }

    /// Get all room IDs for which a user-defined rule exists.
    pub async fn get_rooms_with_user_defined_rules(&self, enabled: Option<bool>) -> Vec<String> {
        let notification_settings = self.sdk_notification_settings.read().await;
        notification_settings.get_rooms_with_user_defined_rules(enabled).await
    }

    /// Get whether some enabled keyword rules exist.
    pub async fn contains_keywords_rules(&self) -> bool {
        let notification_settings = self.sdk_notification_settings.read().await;
        notification_settings.contains_keyword_rules().await
    }

    /// Get whether room mentions are enabled.
    pub async fn is_room_mention_enabled(&self) -> Result<bool, NotificationSettingsError> {
        let notification_settings = self.sdk_notification_settings.read().await;
        let enabled = notification_settings
            .is_push_rule_enabled(SdkRuleKind::Override, PredefinedOverrideRuleId::IsRoomMention)
            .await?;
        Ok(enabled)
    }

    /// Set whether room mentions are enabled.
    pub async fn set_room_mention_enabled(
        &self,
        enabled: bool,
    ) -> Result<(), NotificationSettingsError> {
        let notification_settings = self.sdk_notification_settings.read().await;
        notification_settings
            .set_push_rule_enabled(
                SdkRuleKind::Override,
                PredefinedOverrideRuleId::IsRoomMention,
                enabled,
            )
            .await?;
        Ok(())
    }

    /// Get whether user mentions are enabled.
    pub async fn is_user_mention_enabled(&self) -> Result<bool, NotificationSettingsError> {
        let notification_settings = self.sdk_notification_settings.read().await;
        let enabled = notification_settings
            .is_push_rule_enabled(SdkRuleKind::Override, PredefinedOverrideRuleId::IsUserMention)
            .await?;
        Ok(enabled)
    }

    /// Returns true if [MSC 4028 push rule][rule] is supported and enabled.
    ///
    /// [rule]: https://github.com/matrix-org/matrix-spec-proposals/blob/giomfo/push_encrypted_events/proposals/4028-push-all-encrypted-events-except-for-muted-rooms.md
    pub async fn can_push_encrypted_event_to_device(&self) -> bool {
        let notification_settings = self.sdk_notification_settings.read().await;
        // Check stable identifier
        if let Ok(enabled) = notification_settings
            .is_push_rule_enabled(SdkRuleKind::Override, ".m.rule.encrypted_event")
            .await
        {
            enabled
        } else {
            // Check unstable identifier
            notification_settings
                .is_push_rule_enabled(SdkRuleKind::Override, ".org.matrix.msc4028.encrypted_event")
                .await
                .unwrap_or(false)
        }
    }

    /// Check whether [MSC 4028 push rule][rule] is enabled on the homeserver.
    ///
    /// [rule]: https://github.com/matrix-org/matrix-spec-proposals/blob/giomfo/push_encrypted_events/proposals/4028-push-all-encrypted-events-except-for-muted-rooms.md
    pub async fn can_homeserver_push_encrypted_event_to_device(&self) -> bool {
        self.sdk_client.can_homeserver_push_encrypted_event_to_device().await.unwrap()
    }

    /// Set whether user mentions are enabled.
    pub async fn set_user_mention_enabled(
        &self,
        enabled: bool,
    ) -> Result<(), NotificationSettingsError> {
        let notification_settings = self.sdk_notification_settings.read().await;
        notification_settings
            .set_push_rule_enabled(
                SdkRuleKind::Override,
                PredefinedOverrideRuleId::IsUserMention,
                enabled,
            )
            .await?;
        Ok(())
    }

    /// Get whether the `.m.rule.call` push rule is enabled
    pub async fn is_call_enabled(&self) -> Result<bool, NotificationSettingsError> {
        let notification_settings = self.sdk_notification_settings.read().await;
        let enabled = notification_settings
            .is_push_rule_enabled(SdkRuleKind::Underride, PredefinedUnderrideRuleId::Call)
            .await?;
        Ok(enabled)
    }

    /// Set whether the `.m.rule.call` push rule is enabled
    pub async fn set_call_enabled(&self, enabled: bool) -> Result<(), NotificationSettingsError> {
        let notification_settings = self.sdk_notification_settings.read().await;
        notification_settings
            .set_push_rule_enabled(SdkRuleKind::Underride, PredefinedUnderrideRuleId::Call, enabled)
            .await?;
        Ok(())
    }

    /// Get whether the `.m.rule.invite_for_me` push rule is enabled
    pub async fn is_invite_for_me_enabled(&self) -> Result<bool, NotificationSettingsError> {
        let notification_settings = self.sdk_notification_settings.read().await;
        let enabled = notification_settings
            .is_push_rule_enabled(
                SdkRuleKind::Override,
                PredefinedOverrideRuleId::InviteForMe.as_str(),
            )
            .await?;
        Ok(enabled)
    }

    /// Set whether the `.m.rule.invite_for_me` push rule is enabled
    pub async fn set_invite_for_me_enabled(
        &self,
        enabled: bool,
    ) -> Result<(), NotificationSettingsError> {
        let notification_settings = self.sdk_notification_settings.read().await;
        notification_settings
            .set_push_rule_enabled(
                SdkRuleKind::Override,
                PredefinedOverrideRuleId::InviteForMe.as_str(),
                enabled,
            )
            .await?;
        Ok(())
    }

    /// Sets a custom push rule with the given actions and conditions.
    pub async fn set_custom_push_rule(
        &self,
        rule_id: String,
        rule_kind: RuleKind,
        actions: Vec<Action>,
        conditions: Vec<PushCondition>,
    ) -> Result<(), NotificationSettingsError> {
        let notification_settings = self.sdk_notification_settings.read().await;
        let actions: Result<Vec<_>, _> =
            actions.into_iter().map(|action| action.try_into()).collect();
        let actions = actions.map_err(|e| NotificationSettingsError::Generic { msg: e })?;

        notification_settings
            .create_custom_conditional_push_rule(
                rule_id,
                rule_kind.into(),
                actions,
                conditions.into_iter().map(|condition| condition.into()).collect(),
            )
            .await?;
        Ok(())
    }

    /// Unmute a room.
    ///
    /// # Arguments
    ///
    /// * `room_id` - the room to unmute
    /// * `is_encrypted` - whether the room is encrypted
    /// * `is_one_to_one` - whether the room is a direct chat involving two
    ///   people
    pub async fn unmute_room(
        &self,
        room_id: String,
        is_encrypted: bool,
        is_one_to_one: bool,
    ) -> Result<(), NotificationSettingsError> {
        let notification_settings = self.sdk_notification_settings.read().await;
        let parsed_room_id = RoomId::parse(&room_id)
            .map_err(|_e| NotificationSettingsError::InvalidRoomId { room_id })?;
        notification_settings
            .unmute_room(&parsed_room_id, is_encrypted.into(), is_one_to_one.into())
            .await?;
        Ok(())
    }

    /// Returns the raw push rules in JSON format.
    pub async fn get_raw_push_rules(&self) -> Result<Option<String>, ClientError> {
        let raw_push_rules =
            self.sdk_client.account().account_data::<PushRulesEventContent>().await?;
        Ok(raw_push_rules.map(|raw| serde_json::to_string(&raw)).transpose()?)
    }
}
