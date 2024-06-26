use std::fmt::Display;

use matrix_sdk::{
    encryption::CryptoStoreError, event_cache::EventCacheError, oidc::OidcError, HttpError,
    IdParseError, NotificationSettingsError as SdkNotificationSettingsError, StoreError,
};
use matrix_sdk_ui::{encryption_sync_service, notification_client, sync_service, timeline};
use uniffi::UnexpectedUniFFICallbackError;

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("client error: {msg}")]
    Generic { msg: String },
}

impl ClientError {
    pub(crate) fn new<E: Display>(error: E) -> Self {
        Self::Generic { msg: error.to_string() }
    }
}

impl From<anyhow::Error> for ClientError {
    fn from(e: anyhow::Error) -> ClientError {
        ClientError::Generic { msg: format!("{e:#}") }
    }
}

impl From<UnexpectedUniFFICallbackError> for ClientError {
    fn from(e: UnexpectedUniFFICallbackError) -> Self {
        Self::new(e)
    }
}

impl From<matrix_sdk::Error> for ClientError {
    fn from(e: matrix_sdk::Error) -> Self {
        Self::new(e)
    }
}

impl From<StoreError> for ClientError {
    fn from(e: StoreError) -> Self {
        Self::new(e)
    }
}

impl From<CryptoStoreError> for ClientError {
    fn from(e: CryptoStoreError) -> Self {
        Self::new(e)
    }
}

impl From<HttpError> for ClientError {
    fn from(e: HttpError) -> Self {
        Self::new(e)
    }
}

impl From<IdParseError> for ClientError {
    fn from(e: IdParseError) -> Self {
        Self::new(e)
    }
}

impl From<serde_json::Error> for ClientError {
    fn from(e: serde_json::Error) -> Self {
        Self::new(e)
    }
}

impl From<url::ParseError> for ClientError {
    fn from(e: url::ParseError) -> Self {
        Self::new(e)
    }
}

impl From<mime::FromStrError> for ClientError {
    fn from(e: mime::FromStrError) -> Self {
        Self::new(e)
    }
}

impl From<encryption_sync_service::Error> for ClientError {
    fn from(e: encryption_sync_service::Error) -> Self {
        Self::new(e)
    }
}

impl From<timeline::Error> for ClientError {
    fn from(e: timeline::Error) -> Self {
        Self::new(e)
    }
}

impl From<notification_client::Error> for ClientError {
    fn from(e: notification_client::Error) -> Self {
        Self::new(e)
    }
}

impl From<sync_service::Error> for ClientError {
    fn from(e: sync_service::Error) -> Self {
        Self::new(e)
    }
}

impl From<OidcError> for ClientError {
    fn from(e: OidcError) -> Self {
        Self::new(e)
    }
}

impl From<RoomError> for ClientError {
    fn from(e: RoomError) -> Self {
        Self::new(e)
    }
}

impl From<EventCacheError> for ClientError {
    fn from(e: EventCacheError) -> Self {
        Self::new(e)
    }
}

#[derive(Debug, thiserror::Error, uniffi::Error)]
#[uniffi(flat_error)]
pub enum RoomError {
    #[error("Invalid attachment data")]
    InvalidAttachmentData,
    #[error("Invalid attachment mime type")]
    InvalidAttachmentMimeType,
    #[error("Invalid media info")]
    InvalidMediaInfo,
    #[error("Timeline unavailable")]
    TimelineUnavailable,
    #[error("Invalid thumbnail data")]
    InvalidThumbnailData,
    #[error("Failed sending attachment")]
    FailedSendingAttachment,
}

#[derive(Debug, thiserror::Error, uniffi::Error)]
#[uniffi(flat_error)]
pub enum MediaInfoError {
    #[error("Required value missing from the media info")]
    MissingField,
    #[error("Media info field invalid")]
    InvalidField,
}

#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum NotificationSettingsError {
    #[error("client error: {msg}")]
    Generic { msg: String },
    /// Invalid parameter.
    #[error("Invalid parameter: {msg}")]
    InvalidParameter { msg: String },
    /// Invalid room id.
    #[error("Invalid room ID {room_id}")]
    InvalidRoomId { room_id: String },
    /// Rule not found
    #[error("Rule not found: {rule_id}")]
    RuleNotFound { rule_id: String },
    /// Unable to add push rule.
    #[error("Unable to add push rule")]
    UnableToAddPushRule,
    /// Unable to remove push rule.
    #[error("Unable to remove push rule")]
    UnableToRemovePushRule,
    /// Unable to save the push rules
    #[error("Unable to save push rules")]
    UnableToSavePushRules,
    /// Unable to update push rule.
    #[error("Unable to update push rule")]
    UnableToUpdatePushRule,
}

impl From<SdkNotificationSettingsError> for NotificationSettingsError {
    fn from(value: SdkNotificationSettingsError) -> Self {
        match value {
            SdkNotificationSettingsError::RuleNotFound(rule_id) => Self::RuleNotFound { rule_id },
            SdkNotificationSettingsError::UnableToAddPushRule => Self::UnableToAddPushRule,
            SdkNotificationSettingsError::UnableToRemovePushRule => Self::UnableToRemovePushRule,
            SdkNotificationSettingsError::UnableToSavePushRules => Self::UnableToSavePushRules,
            SdkNotificationSettingsError::InvalidParameter(msg) => Self::InvalidParameter { msg },
            SdkNotificationSettingsError::UnableToUpdatePushRule => Self::UnableToUpdatePushRule,
        }
    }
}

impl From<matrix_sdk::Error> for NotificationSettingsError {
    fn from(e: matrix_sdk::Error) -> Self {
        Self::Generic { msg: e.to_string() }
    }
}
