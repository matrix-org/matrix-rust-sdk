use std::{collections::HashMap, fmt, fmt::Display};

use matrix_sdk::{
    encryption::CryptoStoreError, event_cache::EventCacheError, oidc::OidcError, reqwest,
    room::edit::EditError, send_queue::RoomSendQueueError, HttpError, IdParseError,
    NotificationSettingsError as SdkNotificationSettingsError,
    QueueWedgeError as SdkQueueWedgeError, StoreError,
};
use matrix_sdk_ui::{encryption_sync_service, notification_client, sync_service, timeline};
use uniffi::UnexpectedUniFFICallbackError;

use crate::room_list::RoomListError;

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

impl From<reqwest::Error> for ClientError {
    fn from(e: reqwest::Error) -> Self {
        Self::new(e)
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

impl From<timeline::UnsupportedEditItem> for ClientError {
    fn from(e: timeline::UnsupportedEditItem) -> Self {
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

impl From<RoomListError> for ClientError {
    fn from(e: RoomListError) -> Self {
        Self::new(e)
    }
}

impl From<EventCacheError> for ClientError {
    fn from(e: EventCacheError) -> Self {
        Self::new(e)
    }
}

impl From<EditError> for ClientError {
    fn from(e: EditError) -> Self {
        Self::new(e)
    }
}

impl From<RoomSendQueueError> for ClientError {
    fn from(e: RoomSendQueueError) -> Self {
        Self::new(e)
    }
}

impl From<NotYetImplemented> for ClientError {
    fn from(_: NotYetImplemented) -> Self {
        Self::new("This functionality is not implemented yet.")
    }
}

/// Bindings version of the sdk type replacing OwnedUserId/DeviceIds with simple
/// String.
///
/// Represent a failed to send unrecoverable error of an event sent via the
/// send_queue. It is a serializable representation of a client error, see
/// `From` implementation for more details. These errors can not be
/// automatically retried, but yet some manual action can be taken before retry
/// sending. If not the only solution is to delete the local event.
#[derive(Debug, Clone, uniffi::Enum)]
pub enum QueueWedgeError {
    /// This error occurs when there are some insecure devices in the room, and
    /// the current encryption setting prohibit sharing with them.
    InsecureDevices {
        /// The insecure devices as a Map of userID to deviceID.
        user_device_map: HashMap<String, Vec<String>>,
    },

    /// This error occurs when a previously verified user is not anymore, and
    /// the current encryption setting prohibit sharing when it happens.
    IdentityViolations {
        /// The users that are expected to be verified but are not.
        users: Vec<String>,
    },

    /// It is required to set up cross-signing and properly erify the current
    /// session before sending.
    CrossVerificationRequired,

    /// Some media content to be sent has disappeared from the cache.
    MissingMediaContent,

    /// Some mime type couldn't be parsed.
    InvalidMimeType { mime_type: String },

    /// Other errors.
    GenericApiError { msg: String },
}

/// Simple display implementation that strips out userIds/DeviceIds to avoid
/// accidental logging.
impl Display for QueueWedgeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            QueueWedgeError::InsecureDevices { .. } => {
                f.write_str("There are insecure devices in the room")
            }
            QueueWedgeError::IdentityViolations { .. } => {
                f.write_str("Some users that were previously verified are not anymore")
            }
            QueueWedgeError::CrossVerificationRequired => {
                f.write_str("Own verification is required")
            }
            QueueWedgeError::MissingMediaContent => {
                f.write_str("Media to be sent disappeared from local storage")
            }
            QueueWedgeError::InvalidMimeType { mime_type } => {
                write!(f, "Invalid mime type '{mime_type}' for media upload")
            }
            QueueWedgeError::GenericApiError { msg } => f.write_str(msg),
        }
    }
}

impl From<SdkQueueWedgeError> for QueueWedgeError {
    fn from(value: SdkQueueWedgeError) -> Self {
        match value {
            SdkQueueWedgeError::InsecureDevices { user_device_map } => Self::InsecureDevices {
                user_device_map: user_device_map
                    .iter()
                    .map(|(user_id, devices)| {
                        (
                            user_id.to_string(),
                            devices.iter().map(|device_id| device_id.to_string()).collect(),
                        )
                    })
                    .collect(),
            },
            SdkQueueWedgeError::IdentityViolations { users } => Self::IdentityViolations {
                users: users.iter().map(ruma::OwnedUserId::to_string).collect(),
            },
            SdkQueueWedgeError::CrossVerificationRequired => Self::CrossVerificationRequired,
            SdkQueueWedgeError::MissingMediaContent => Self::MissingMediaContent,
            SdkQueueWedgeError::InvalidMimeType { mime_type } => {
                Self::InvalidMimeType { mime_type }
            }
            SdkQueueWedgeError::GenericApiError { msg } => Self::GenericApiError { msg },
        }
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

/// Something has not been implemented yet.
#[derive(thiserror::Error, Debug)]
#[error("not implemented yet")]
pub struct NotYetImplemented;
