use std::fmt::Display;

use matrix_sdk::{
    self, encryption::CryptoStoreError, HttpError, IdParseError,
    NotificationSettingsError as SdkNotificationSettingsError, StoreError,
};

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("client error: {msg}")]
    Generic { msg: String },
}

impl ClientError {
    fn new<E: Display>(error: E) -> Self {
        Self::Generic { msg: error.to_string() }
    }
}

impl From<anyhow::Error> for ClientError {
    fn from(e: anyhow::Error) -> ClientError {
        ClientError::Generic { msg: format!("{e:#}") }
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

impl From<NotificationSettingsError> for ClientError {
    fn from(e: NotificationSettingsError) -> Self {
        anyhow::Error::from(e).into()
    }
}

#[derive(Debug, thiserror::Error, uniffi::Error)]
#[uniffi(flat_error)]
pub enum RoomError {
    #[error("Invalid attachment data")]
    InvalidAttachmentData,
    #[error("Invalid attachment mime type")]
    InvalidAttachmentMimeType,
    #[error("Timeline unavailable")]
    TimelineUnavailable,
    #[error("Invalid thumbnail data")]
    InvalidThumbnailData,
    #[error("Failed sending attachment")]
    FailedSendingAttachment,
}

#[derive(Debug, thiserror::Error, uniffi::Error)]
#[uniffi(flat_error)]
pub enum TimelineError {
    #[error("Required value missing from the media info")]
    MissingMediaInfoField,
    #[error("Media info field invalid")]
    InvalidMediaInfoField,
}

#[derive(Debug, thiserror::Error, uniffi::Error)]
#[uniffi(flat_error)]
pub enum NotificationSettingsError {
    #[error("client error: {msg}")]
    Generic { msg: String },
    /// Invalid room id.
    #[error("Invalid room ID `{0}`")]
    InvalidRoomId(String),
    /// Room not found
    #[error("Room not found")]
    RoomNotFound,
    /// Rule not found
    #[error("Rule not found")]
    RuleNotFound,
    /// Unable to add push rule.
    #[error("Unable to add push rule")]
    UnableToAddPushRule,
    /// Unable to remove push rule.
    #[error("Unable to remove push rule")]
    UnableToRemovePushRule,
    /// Unable to save the push rules
    #[error("Unable to save push rules")]
    UnableToSavePushRules,
}

impl From<SdkNotificationSettingsError> for NotificationSettingsError {
    fn from(value: SdkNotificationSettingsError) -> Self {
        match value {
            SdkNotificationSettingsError::RuleNotFound => Self::RuleNotFound,
            SdkNotificationSettingsError::UnableToAddPushRule => Self::UnableToAddPushRule,
            SdkNotificationSettingsError::UnableToRemovePushRule => Self::UnableToRemovePushRule,
            SdkNotificationSettingsError::UnableToSavePushRules => Self::UnableToSavePushRules,
        }
    }
}

impl From<matrix_sdk::Error> for NotificationSettingsError {
    fn from(e: matrix_sdk::Error) -> Self {
        Self::Generic { msg: e.to_string() }
    }
}
