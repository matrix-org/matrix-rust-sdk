use std::sync::Arc;

use matrix_sdk::{
    event_handler::EventHandlerHandle,
    notification_settings::{
        NotificationSettings as SdkNotificationSettings,
        RoomNotificationMode as SdkRoomNotificationMode,
    },
    ruma::events::push_rules::PushRulesEvent,
    Client as MatrixClient,
};
use ruma::{
    push::{PredefinedOverrideRuleId, PredefinedUnderrideRuleId, RuleKind},
    RoomId,
};
use tokio::sync::RwLock;

use super::RUNTIME;
use crate::error::NotificationSettingsError;

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
#[uniffi::export(callback_interface)]
pub trait NotificationSettingsDelegate: Sync + Send {
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
    sdk_notification_settings: Arc<RwLock<SdkNotificationSettings>>,
    pushrules_event_handler: Arc<RwLock<Option<EventHandlerHandle>>>,
}

impl NotificationSettings {
    pub(crate) fn new(
        sdk_client: MatrixClient,
        sdk_notification_settings: SdkNotificationSettings,
    ) -> Self {
        let sdk_notification_settings = Arc::new(RwLock::new(sdk_notification_settings));
        Self {
            sdk_client,
            sdk_notification_settings,
            pushrules_event_handler: Arc::new(RwLock::new(None)),
        }
    }
}

impl Drop for NotificationSettings {
    fn drop(&mut self) {
        // Remove the event handler on the sdk_client.
        RUNTIME.block_on(async move {
            if let Some(event_handler) = self.pushrules_event_handler.read().await.as_ref() {
                self.sdk_client.remove_event_handler(event_handler.clone());
            }
        });
    }
}

#[uniffi::export(async_runtime = "tokio")]
impl NotificationSettings {
    pub fn set_delegate(&self, delegate: Option<Box<dyn NotificationSettingsDelegate>>) {
        if let Some(delegate) = delegate {
            let delegate: Arc<dyn NotificationSettingsDelegate> = Arc::from(delegate);

            // Add an event handler to listen to `PushRulesEvent`
            let event_handler =
                self.sdk_client.add_event_handler(move |_: PushRulesEvent| async move {
                    delegate.settings_did_change();
                });

            RUNTIME.block_on(async move {
                *self.pushrules_event_handler.write().await = Some(event_handler);
            });
        } else {
            // Remove the event handler if there is no delegate
            RUNTIME.block_on(async move {
                let event_handler = &mut *self.pushrules_event_handler.write().await;
                if let Some(event_handler) = event_handler {
                    self.sdk_client.remove_event_handler(event_handler.clone());
                }
                *event_handler = None;
            });
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
        let notification_settings = self.sdk_notification_settings.read().await;
        let parsed_room_id = RoomId::parse(&room_id)
            .map_err(|_e| NotificationSettingsError::InvalidRoomId(room_id))?;
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

    pub fn get_room_notification_settings_blocking(
        self: Arc<Self>,
        room_id: String,
        is_encrypted: bool,
        is_one_to_one: bool,
    ) -> Result<RoomNotificationSettings, NotificationSettingsError> {
        RUNTIME.block_on(async move {
            self.get_room_notification_settings(room_id, is_encrypted, is_one_to_one).await
        })
    }

    /// Set the notification mode for a room.
    pub async fn set_room_notification_mode(
        &self,
        room_id: String,
        mode: RoomNotificationMode,
    ) -> Result<(), NotificationSettingsError> {
        let notification_settings = self.sdk_notification_settings.read().await;
        let parsed_room_id = RoomId::parse(&room_id)
            .map_err(|_e| NotificationSettingsError::InvalidRoomId(room_id))?;
        notification_settings.set_room_notification_mode(&parsed_room_id, mode.into()).await?;
        Ok(())
    }

    pub fn set_room_notification_mode_blocking(
        self: Arc<Self>,
        room_id: String,
        mode: RoomNotificationMode,
    ) -> Result<(), NotificationSettingsError> {
        RUNTIME.block_on(async move { self.set_room_notification_mode(room_id, mode).await })
    }

    /// Get the user defined room notification mode
    pub async fn get_user_defined_room_notification_mode(
        &self,
        room_id: String,
    ) -> Result<Option<RoomNotificationMode>, NotificationSettingsError> {
        let notification_settings = self.sdk_notification_settings.read().await;
        let parsed_room_id = RoomId::parse(&room_id)
            .map_err(|_e| NotificationSettingsError::InvalidRoomId(room_id))?;
        // Get the current user defined mode for this room
        if let Some(mode) =
            notification_settings.get_user_defined_room_notification_mode(&parsed_room_id).await
        {
            Ok(Some(mode.into()))
        } else {
            Ok(None)
        }
    }

    pub fn get_user_defined_room_notification_mode_blocking(
        self: Arc<Self>,
        room_id: String,
    ) -> Result<Option<RoomNotificationMode>, NotificationSettingsError> {
        RUNTIME.block_on(async move { self.get_user_defined_room_notification_mode(room_id).await })
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

    pub fn get_default_room_notification_mode_blocking(
        self: Arc<Self>,
        is_encrypted: bool,
        is_one_to_one: bool,
    ) -> RoomNotificationMode {
        RUNTIME.block_on(async move {
            self.get_default_room_notification_mode(is_encrypted, is_one_to_one).await
        })
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

    pub fn set_default_room_notification_mode_blocking(
        self: Arc<Self>,
        is_encrypted: bool,
        is_one_to_one: bool,
        mode: RoomNotificationMode,
    ) -> Result<(), NotificationSettingsError> {
        RUNTIME.block_on(async move {
            self.set_default_room_notification_mode(is_encrypted, is_one_to_one, mode).await
        })
    }

    /// Restore the default notification mode for a room
    pub async fn restore_default_room_notification_mode(
        &self,
        room_id: String,
    ) -> Result<(), NotificationSettingsError> {
        let notification_settings = self.sdk_notification_settings.read().await;
        let parsed_room_id = RoomId::parse(&room_id)
            .map_err(|_e| NotificationSettingsError::InvalidRoomId(room_id))?;
        notification_settings.delete_user_defined_room_rules(&parsed_room_id).await?;
        Ok(())
    }

    pub fn restore_default_room_notification_mode_blocking(
        self: Arc<Self>,
        room_id: String,
    ) -> Result<(), NotificationSettingsError> {
        RUNTIME.block_on(async move { self.restore_default_room_notification_mode(room_id).await })
    }

    /// Get all room IDs for which a user-defined rule exists.
    pub async fn get_rooms_with_user_defined_rules(&self, enabled: Option<bool>) -> Vec<String> {
        let notification_settings = self.sdk_notification_settings.read().await;
        notification_settings.get_rooms_with_user_defined_rules(enabled).await
    }

    pub fn get_rooms_with_user_defined_rules_blocking(
        self: Arc<Self>,
        enabled: Option<bool>,
    ) -> Vec<String> {
        RUNTIME.block_on(async move { self.get_rooms_with_user_defined_rules(enabled).await })
    }

    /// Get whether some enabled keyword rules exist.
    pub async fn contains_keywords_rules(&self) -> bool {
        let notification_settings = self.sdk_notification_settings.read().await;
        notification_settings.contains_keyword_rules().await
    }

    pub fn contains_keywords_rules_blocking(self: Arc<Self>) -> bool {
        RUNTIME.block_on(async move { self.contains_keywords_rules().await })
    }

    /// Get whether room mentions are enabled.
    pub async fn is_room_mention_enabled(&self) -> Result<bool, NotificationSettingsError> {
        let notification_settings = self.sdk_notification_settings.read().await;
        let enabled = notification_settings
            .is_push_rule_enabled(
                RuleKind::Override,
                PredefinedOverrideRuleId::IsRoomMention.as_str(),
            )
            .await?;
        Ok(enabled)
    }

    pub fn is_room_mention_enabled_blocking(
        self: Arc<Self>,
    ) -> Result<bool, NotificationSettingsError> {
        RUNTIME.block_on(async move { self.is_room_mention_enabled().await })
    }

    /// Set whether room mentions are enabled.
    pub async fn set_room_mention_enabled(
        &self,
        enabled: bool,
    ) -> Result<(), NotificationSettingsError> {
        let notification_settings = self.sdk_notification_settings.read().await;
        notification_settings
            .set_push_rule_enabled(
                RuleKind::Override,
                PredefinedOverrideRuleId::IsRoomMention.as_str(),
                enabled,
            )
            .await?;
        Ok(())
    }

    pub fn set_room_mention_enabled_blocking(
        self: Arc<Self>,
        enabled: bool,
    ) -> Result<(), NotificationSettingsError> {
        RUNTIME.block_on(async move { self.set_room_mention_enabled(enabled).await })
    }

    /// Get whether user mentions are enabled.
    pub async fn is_user_mention_enabled(&self) -> Result<bool, NotificationSettingsError> {
        let notification_settings = self.sdk_notification_settings.read().await;
        let enabled = notification_settings
            .is_push_rule_enabled(
                RuleKind::Override,
                PredefinedOverrideRuleId::IsUserMention.as_str(),
            )
            .await?;
        Ok(enabled)
    }

    pub fn is_user_mention_enabled_blocking(
        self: Arc<Self>,
    ) -> Result<bool, NotificationSettingsError> {
        RUNTIME.block_on(async move { self.is_user_mention_enabled().await })
    }

    /// Set whether user mentions are enabled.
    pub async fn set_user_mention_enabled(
        &self,
        enabled: bool,
    ) -> Result<(), NotificationSettingsError> {
        let notification_settings = self.sdk_notification_settings.read().await;
        notification_settings
            .set_push_rule_enabled(
                RuleKind::Override,
                PredefinedOverrideRuleId::IsUserMention.as_str(),
                enabled,
            )
            .await?;
        Ok(())
    }

    pub fn set_user_mention_enabled_blocking(
        self: Arc<Self>,
        enabled: bool,
    ) -> Result<(), NotificationSettingsError> {
        RUNTIME.block_on(async move { self.set_user_mention_enabled(enabled).await })
    }

    /// Get whether the `.m.rule.call` push rule is enabled
    pub async fn is_call_enabled(&self) -> Result<bool, NotificationSettingsError> {
        let notification_settings = self.sdk_notification_settings.read().await;
        let enabled = notification_settings
            .is_push_rule_enabled(RuleKind::Underride, PredefinedUnderrideRuleId::Call.as_str())
            .await?;
        Ok(enabled)
    }

    pub fn is_call_enabled_blocking(self: Arc<Self>) -> Result<bool, NotificationSettingsError> {
        RUNTIME.block_on(async move { self.is_call_enabled().await })
    }

    /// Set whether the `.m.rule.call` push rule is enabled
    pub async fn set_call_enabled(&self, enabled: bool) -> Result<(), NotificationSettingsError> {
        let notification_settings = self.sdk_notification_settings.read().await;
        notification_settings
            .set_push_rule_enabled(
                RuleKind::Underride,
                PredefinedUnderrideRuleId::Call.as_str(),
                enabled,
            )
            .await?;
        Ok(())
    }

    pub fn set_call_enabled_blocking(
        self: Arc<Self>,
        enabled: bool,
    ) -> Result<(), NotificationSettingsError> {
        RUNTIME.block_on(async move { self.set_call_enabled(enabled).await })
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
            .map_err(|_e| NotificationSettingsError::InvalidRoomId(room_id))?;
        notification_settings
            .unmute_room(&parsed_room_id, is_encrypted.into(), is_one_to_one.into())
            .await?;
        Ok(())
    }

    pub fn unmute_room_blocking(
        self: Arc<Self>,
        room_id: String,
        is_encrypted: bool,
        is_one_to_one: bool,
    ) -> Result<(), NotificationSettingsError> {
        RUNTIME
            .block_on(async move { self.unmute_room(room_id, is_encrypted, is_one_to_one).await })
    }
}
