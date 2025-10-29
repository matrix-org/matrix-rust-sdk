use std::{collections::HashMap, error::Error, fmt, fmt::Display};

use matrix_sdk::{
    authentication::oauth::OAuthError,
    encryption::{identities::RequestVerificationError, CryptoStoreError},
    event_cache::EventCacheError,
    reqwest,
    room::{calls::CallError, edit::EditError},
    send_queue::RoomSendQueueError,
    HttpError, IdParseError, NotificationSettingsError as SdkNotificationSettingsError,
    QueueWedgeError as SdkQueueWedgeError, StoreError,
};
use matrix_sdk_ui::{encryption_sync_service, notification_client, spaces, sync_service, timeline};
use ruma::{
    api::client::error::{ErrorBody, ErrorKind as RumaApiErrorKind, RetryAfter, StandardErrorBody},
    MilliSecondsSinceUnixEpoch,
};
use tracing::warn;
use uniffi::UnexpectedUniFFICallbackError;

use crate::{room_list::RoomListError, timeline::FocusEventError};

#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum ClientError {
    #[error("client error: {msg}")]
    Generic { msg: String, details: Option<String> },
    #[error("api error {code}: {msg}")]
    MatrixApi { kind: ErrorKind, code: String, msg: String, details: Option<String> },
}

impl ClientError {
    pub(crate) fn from_str<E: Display>(error: E, details: Option<String>) -> Self {
        warn!("Error: {error}");
        Self::Generic { msg: error.to_string(), details }
    }

    pub(crate) fn from_err<E: Error>(e: E) -> Self {
        let details = Some(format!("{e:?}"));
        Self::from_str(e, details)
    }
}

impl From<anyhow::Error> for ClientError {
    fn from(e: anyhow::Error) -> ClientError {
        let details = format!("{e:?}");
        ClientError::Generic { msg: format!("{e:#}"), details: Some(details) }
    }
}

impl From<reqwest::Error> for ClientError {
    fn from(e: reqwest::Error) -> Self {
        Self::from_err(e)
    }
}

impl From<UnexpectedUniFFICallbackError> for ClientError {
    fn from(e: UnexpectedUniFFICallbackError) -> Self {
        Self::from_err(e)
    }
}

impl From<matrix_sdk::Error> for ClientError {
    fn from(e: matrix_sdk::Error) -> Self {
        match e {
            matrix_sdk::Error::Http(http_error) => {
                if let Some(api_error) = http_error.as_client_api_error() {
                    if let ErrorBody::Standard(StandardErrorBody { kind, message, .. }) =
                        &api_error.body
                    {
                        let code = kind.errcode().to_string();
                        let Ok(kind) = kind.to_owned().try_into() else {
                            // We couldn't parse the API error, so we return a generic one instead
                            return (*http_error).into();
                        };
                        return Self::MatrixApi {
                            kind,
                            code,
                            msg: message.to_owned(),
                            details: Some(format!("{api_error:?}")),
                        };
                    }
                }
                (*http_error).into()
            }
            _ => Self::from_err(e),
        }
    }
}

impl From<StoreError> for ClientError {
    fn from(e: StoreError) -> Self {
        Self::from_err(e)
    }
}

impl From<CryptoStoreError> for ClientError {
    fn from(e: CryptoStoreError) -> Self {
        Self::from_err(e)
    }
}

impl From<HttpError> for ClientError {
    fn from(e: HttpError) -> Self {
        Self::from_err(e)
    }
}

impl From<IdParseError> for ClientError {
    fn from(e: IdParseError) -> Self {
        Self::from_err(e)
    }
}

impl From<serde_json::Error> for ClientError {
    fn from(e: serde_json::Error) -> Self {
        Self::from_err(e)
    }
}

impl From<url::ParseError> for ClientError {
    fn from(e: url::ParseError) -> Self {
        Self::from_err(e)
    }
}

impl From<mime::FromStrError> for ClientError {
    fn from(e: mime::FromStrError) -> Self {
        Self::from_err(e)
    }
}

impl From<encryption_sync_service::Error> for ClientError {
    fn from(e: encryption_sync_service::Error) -> Self {
        Self::from_err(e)
    }
}

impl From<timeline::Error> for ClientError {
    fn from(e: timeline::Error) -> Self {
        Self::from_err(e)
    }
}

impl From<timeline::UnsupportedEditItem> for ClientError {
    fn from(e: timeline::UnsupportedEditItem) -> Self {
        Self::from_err(e)
    }
}

impl From<notification_client::Error> for ClientError {
    fn from(e: notification_client::Error) -> Self {
        Self::from_err(e)
    }
}

impl From<sync_service::Error> for ClientError {
    fn from(e: sync_service::Error) -> Self {
        Self::from_err(e)
    }
}

impl From<OAuthError> for ClientError {
    fn from(e: OAuthError) -> Self {
        Self::from_err(e)
    }
}

impl From<RoomError> for ClientError {
    fn from(e: RoomError) -> Self {
        Self::from_err(e)
    }
}

impl From<RoomListError> for ClientError {
    fn from(e: RoomListError) -> Self {
        Self::from_err(e)
    }
}

impl From<EventCacheError> for ClientError {
    fn from(e: EventCacheError) -> Self {
        Self::from_err(e)
    }
}

impl From<EditError> for ClientError {
    fn from(e: EditError) -> Self {
        Self::from_err(e)
    }
}

impl From<CallError> for ClientError {
    fn from(e: CallError) -> Self {
        Self::from_err(e)
    }
}

impl From<RoomSendQueueError> for ClientError {
    fn from(e: RoomSendQueueError) -> Self {
        Self::from_err(e)
    }
}

impl From<NotYetImplemented> for ClientError {
    fn from(_: NotYetImplemented) -> Self {
        Self::from_str("This functionality is not implemented yet.", None)
    }
}

impl From<FocusEventError> for ClientError {
    fn from(e: FocusEventError) -> Self {
        Self::from_err(e)
    }
}

impl From<RequestVerificationError> for ClientError {
    fn from(e: RequestVerificationError) -> Self {
        Self::from_err(e)
    }
}

impl From<spaces::Error> for ClientError {
    fn from(e: spaces::Error) -> Self {
        Self::from_err(e)
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
    #[error("Invalid replied to event ID")]
    InvalidRepliedToEventId,
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

#[derive(Clone, Debug, PartialEq, Eq, uniffi::Enum)]
// Please keep the variants sorted alphabetically.
pub enum ErrorKind {
    /// `M_BAD_ALIAS`
    ///
    /// One or more [room aliases] within the `m.room.canonical_alias` event do
    /// not point to the room ID for which the state event is to be sent to.
    ///
    /// [room aliases]: https://spec.matrix.org/latest/client-server-api/#room-aliases
    BadAlias,

    /// `M_BAD_JSON`
    ///
    /// The request contained valid JSON, but it was malformed in some way, e.g.
    /// missing required keys, invalid values for keys.
    BadJson,

    /// `M_BAD_STATE`
    ///
    /// The state change requested cannot be performed, such as attempting to
    /// unban a user who is not banned.
    BadState,

    /// `M_BAD_STATUS`
    ///
    /// The application service returned a bad status.
    BadStatus {
        /// The HTTP status code of the response.
        status: Option<u16>,

        /// The body of the response.
        body: Option<String>,
    },

    /// `M_CANNOT_LEAVE_SERVER_NOTICE_ROOM`
    ///
    /// The user is unable to reject an invite to join the [server notices]
    /// room.
    ///
    /// [server notices]: https://spec.matrix.org/latest/client-server-api/#server-notices
    CannotLeaveServerNoticeRoom,

    /// `M_CANNOT_OVERWRITE_MEDIA`
    ///
    /// The [`create_content_async`] endpoint was called with a media ID that
    /// already has content.
    ///
    /// [`create_content_async`]: crate::media::create_content_async
    CannotOverwriteMedia,

    /// `M_CAPTCHA_INVALID`
    ///
    /// The Captcha provided did not match what was expected.
    CaptchaInvalid,

    /// `M_CAPTCHA_NEEDED`
    ///
    /// A Captcha is required to complete the request.
    CaptchaNeeded,

    /// `M_CONNECTION_FAILED`
    ///
    /// The connection to the application service failed.
    ConnectionFailed,

    /// `M_CONNECTION_TIMEOUT`
    ///
    /// The connection to the application service timed out.
    ConnectionTimeout,

    /// `M_DUPLICATE_ANNOTATION`
    ///
    /// The request is an attempt to send a [duplicate annotation].
    ///
    /// [duplicate annotation]: https://spec.matrix.org/latest/client-server-api/#avoiding-duplicate-annotations
    DuplicateAnnotation,

    /// `M_EXCLUSIVE`
    ///
    /// The resource being requested is reserved by an application service, or
    /// the application service making the request has not created the
    /// resource.
    Exclusive,

    /// `M_FORBIDDEN`
    ///
    /// Forbidden access, e.g. joining a room without permission, failed login.
    Forbidden,

    /// `M_GUEST_ACCESS_FORBIDDEN`
    ///
    /// The room or resource does not permit [guests] to access it.
    ///
    /// [guests]: https://spec.matrix.org/latest/client-server-api/#guest-access
    GuestAccessForbidden,

    /// `M_INCOMPATIBLE_ROOM_VERSION`
    ///
    /// The client attempted to join a room that has a version the server does
    /// not support.
    IncompatibleRoomVersion {
        /// The room's version.
        room_version: String,
    },

    /// `M_INVALID_PARAM`
    ///
    /// A parameter that was specified has the wrong value. For example, the
    /// server expected an integer and instead received a string.
    InvalidParam,

    /// `M_INVALID_ROOM_STATE`
    ///
    /// The initial state implied by the parameters to the [`create_room`]
    /// request is invalid, e.g. the user's `power_level` is set below that
    /// necessary to set the room name.
    ///
    /// [`create_room`]: crate::room::create_room
    InvalidRoomState,

    /// `M_INVALID_USERNAME`
    ///
    /// The desired user name is not valid.
    InvalidUsername,

    /// `M_LIMIT_EXCEEDED`
    ///
    /// The request has been refused due to [rate limiting]: too many requests
    /// have been sent in a short period of time.
    ///
    /// [rate limiting]: https://spec.matrix.org/latest/client-server-api/#rate-limiting
    LimitExceeded {
        /// How long a client should wait before they can try again.
        retry_after_ms: Option<u64>,
    },

    /// `M_MISSING_PARAM`
    ///
    /// A required parameter was missing from the request.
    MissingParam,

    /// `M_MISSING_TOKEN`
    ///
    /// No [access token] was specified for the request, but one is required.
    ///
    /// [access token]: https://spec.matrix.org/latest/client-server-api/#client-authentication
    MissingToken,

    /// `M_NOT_FOUND`
    ///
    /// No resource was found for this request.
    NotFound,

    /// `M_NOT_JSON`
    ///
    /// The request did not contain valid JSON.
    NotJson,

    /// `M_NOT_YET_UPLOADED`
    ///
    /// An `mxc:` URI generated with the [`create_mxc_uri`] endpoint was used
    /// and the content is not yet available.
    ///
    /// [`create_mxc_uri`]: crate::media::create_mxc_uri
    NotYetUploaded,

    /// `M_RESOURCE_LIMIT_EXCEEDED`
    ///
    /// The request cannot be completed because the homeserver has reached a
    /// resource limit imposed on it. For example, a homeserver held in a
    /// shared hosting environment may reach a resource limit if it starts
    /// using too much memory or disk space.
    ResourceLimitExceeded {
        /// A URI giving a contact method for the server administrator.
        admin_contact: String,
    },

    /// `M_ROOM_IN_USE`
    ///
    /// The [room alias] specified in the [`create_room`] request is already
    /// taken.
    ///
    /// [`create_room`]: crate::room::create_room
    /// [room alias]: https://spec.matrix.org/latest/client-server-api/#room-aliases
    RoomInUse,

    /// `M_SERVER_NOT_TRUSTED`
    ///
    /// The client's request used a third-party server, e.g. identity server,
    /// that this server does not trust.
    ServerNotTrusted,

    /// `M_THREEPID_AUTH_FAILED`
    ///
    /// Authentication could not be performed on the [third-party identifier].
    ///
    /// [third-party identifier]: https://spec.matrix.org/latest/client-server-api/#adding-account-administrative-contact-information
    ThreepidAuthFailed,

    /// `M_THREEPID_DENIED`
    ///
    /// The server does not permit this [third-party identifier]. This may
    /// happen if the server only permits, for example, email addresses from
    /// a particular domain.
    ///
    /// [third-party identifier]: https://spec.matrix.org/latest/client-server-api/#adding-account-administrative-contact-information
    ThreepidDenied,

    /// `M_THREEPID_IN_USE`
    ///
    /// The [third-party identifier] is already in use by another user.
    ///
    /// [third-party identifier]: https://spec.matrix.org/latest/client-server-api/#adding-account-administrative-contact-information
    ThreepidInUse,

    /// `M_THREEPID_MEDIUM_NOT_SUPPORTED`
    ///
    /// The homeserver does not support adding a [third-party identifier] of the
    /// given medium.
    ///
    /// [third-party identifier]: https://spec.matrix.org/latest/client-server-api/#adding-account-administrative-contact-information
    ThreepidMediumNotSupported,

    /// `M_THREEPID_NOT_FOUND`
    ///
    /// No account matching the given [third-party identifier] could be found.
    ///
    /// [third-party identifier]: https://spec.matrix.org/latest/client-server-api/#adding-account-administrative-contact-information
    ThreepidNotFound,

    /// `M_TOO_LARGE`
    ///
    /// The request or entity was too large.
    TooLarge,

    /// `M_UNABLE_TO_AUTHORISE_JOIN`
    ///
    /// The room is [restricted] and none of the conditions can be validated by
    /// the homeserver. This can happen if the homeserver does not know
    /// about any of the rooms listed as conditions, for example.
    ///
    /// [restricted]: https://spec.matrix.org/latest/client-server-api/#restricted-rooms
    UnableToAuthorizeJoin,

    /// `M_UNABLE_TO_GRANT_JOIN`
    ///
    /// A different server should be attempted for the join. This is typically
    /// because the resident server can see that the joining user satisfies
    /// one or more conditions, such as in the case of [restricted rooms],
    /// but the resident server would be unable to meet the authorization
    /// rules.
    ///
    /// [restricted rooms]: https://spec.matrix.org/latest/client-server-api/#restricted-rooms
    UnableToGrantJoin,

    /// `M_UNAUTHORIZED`
    ///
    /// The request was not correctly authorized. Usually due to login failures.
    Unauthorized,

    /// `M_UNKNOWN`
    ///
    /// An unknown error has occurred.
    Unknown,

    /// `M_UNKNOWN_TOKEN`
    ///
    /// The [access or refresh token] specified was not recognized.
    ///
    /// [access or refresh token]: https://spec.matrix.org/latest/client-server-api/#client-authentication
    UnknownToken {
        /// If this is `true`, the client is in a "[soft logout]" state, i.e.
        /// the server requires re-authentication but the session is not
        /// invalidated. The client can acquire a new access token by
        /// specifying the device ID it is already using to the login API.
        ///
        /// [soft logout]: https://spec.matrix.org/latest/client-server-api/#soft-logout
        soft_logout: bool,
    },

    /// `M_UNRECOGNIZED`
    ///
    /// The server did not understand the request.
    ///
    /// This is expected to be returned with a 404 HTTP status code if the
    /// endpoint is not implemented or a 405 HTTP status code if the
    /// endpoint is implemented, but the incorrect HTTP method is used.
    Unrecognized,

    /// `M_UNSUPPORTED_ROOM_VERSION`
    ///
    /// The request to [`create_room`] used a room version that the server does
    /// not support.
    ///
    /// [`create_room`]: crate::room::create_room
    UnsupportedRoomVersion,

    /// `M_URL_NOT_SET`
    ///
    /// The application service doesn't have a URL configured.
    UrlNotSet,

    /// `M_USER_DEACTIVATED`
    ///
    /// The user ID associated with the request has been deactivated.
    UserDeactivated,

    /// `M_USER_IN_USE`
    ///
    /// The desired user ID is already taken.
    UserInUse,

    /// `M_USER_LOCKED`
    ///
    /// The account has been [locked] and cannot be used at this time.
    ///
    /// [locked]: https://spec.matrix.org/latest/client-server-api/#account-locking
    UserLocked,

    /// `M_USER_SUSPENDED`
    ///
    /// The account has been [suspended] and can only be used for limited
    /// actions at this time.
    ///
    /// [suspended]: https://spec.matrix.org/latest/client-server-api/#account-suspension
    UserSuspended,

    /// `M_WEAK_PASSWORD`
    ///
    /// The password was [rejected] by the server for being too weak.
    ///
    /// [rejected]: https://spec.matrix.org/latest/client-server-api/#notes-on-password-management
    WeakPassword,

    /// `M_WRONG_ROOM_KEYS_VERSION`
    ///
    /// The version of the [room keys backup] provided in the request does not
    /// match the current backup version.
    ///
    /// [room keys backup]: https://spec.matrix.org/latest/client-server-api/#server-side-key-backups
    WrongRoomKeysVersion {
        /// The currently active backup version.
        current_version: Option<String>,
    },

    /// A custom API error.
    Custom { errcode: String },
}

impl TryFrom<RumaApiErrorKind> for ErrorKind {
    type Error = NotYetImplemented;
    fn try_from(value: RumaApiErrorKind) -> Result<Self, Self::Error> {
        match &value {
            RumaApiErrorKind::BadAlias => Ok(ErrorKind::BadAlias),
            RumaApiErrorKind::BadJson => Ok(ErrorKind::BadJson),
            RumaApiErrorKind::BadState => Ok(ErrorKind::BadState),
            RumaApiErrorKind::BadStatus { status, body } => Ok(ErrorKind::BadStatus {
                status: status.map(|code| code.clone().as_u16()),
                body: body.clone(),
            }),
            RumaApiErrorKind::CannotLeaveServerNoticeRoom => {
                Ok(ErrorKind::CannotLeaveServerNoticeRoom)
            }
            RumaApiErrorKind::CannotOverwriteMedia => Ok(ErrorKind::CannotOverwriteMedia),
            RumaApiErrorKind::CaptchaInvalid => Ok(ErrorKind::CaptchaInvalid),
            RumaApiErrorKind::CaptchaNeeded => Ok(ErrorKind::CaptchaNeeded),
            RumaApiErrorKind::ConnectionFailed => Ok(ErrorKind::ConnectionFailed),
            RumaApiErrorKind::ConnectionTimeout => Ok(ErrorKind::ConnectionTimeout),
            RumaApiErrorKind::DuplicateAnnotation => Ok(ErrorKind::DuplicateAnnotation),
            RumaApiErrorKind::Exclusive => Ok(ErrorKind::Exclusive),
            RumaApiErrorKind::Forbidden { .. } => Ok(ErrorKind::Forbidden),
            RumaApiErrorKind::GuestAccessForbidden => Ok(ErrorKind::GuestAccessForbidden),
            RumaApiErrorKind::IncompatibleRoomVersion { room_version } => {
                Ok(ErrorKind::IncompatibleRoomVersion { room_version: room_version.to_string() })
            }
            RumaApiErrorKind::InvalidParam => Ok(ErrorKind::InvalidParam),
            RumaApiErrorKind::InvalidRoomState => Ok(ErrorKind::InvalidRoomState),
            RumaApiErrorKind::InvalidUsername => Ok(ErrorKind::InvalidUsername),
            RumaApiErrorKind::LimitExceeded { retry_after } => {
                let retry_after_ms = match retry_after {
                    Some(RetryAfter::Delay(duration)) => Some(duration.as_millis() as u64),
                    Some(RetryAfter::DateTime(system_time)) => {
                        let duration = MilliSecondsSinceUnixEpoch::now()
                            .to_system_time()
                            .and_then(|now| system_time.duration_since(now).ok());
                        duration.map(|duration| duration.as_millis() as u64)
                    }
                    None => None,
                };
                Ok(ErrorKind::LimitExceeded { retry_after_ms })
            }
            RumaApiErrorKind::MissingParam => Ok(ErrorKind::MissingParam),
            RumaApiErrorKind::MissingToken => Ok(ErrorKind::MissingToken),
            RumaApiErrorKind::NotFound => Ok(ErrorKind::NotFound),
            RumaApiErrorKind::NotJson => Ok(ErrorKind::NotJson),
            RumaApiErrorKind::NotYetUploaded => Ok(ErrorKind::NotYetUploaded),
            RumaApiErrorKind::ResourceLimitExceeded { admin_contact } => {
                Ok(ErrorKind::ResourceLimitExceeded { admin_contact: admin_contact.to_owned() })
            }
            RumaApiErrorKind::RoomInUse => Ok(ErrorKind::RoomInUse),
            RumaApiErrorKind::ServerNotTrusted => Ok(ErrorKind::ServerNotTrusted),
            RumaApiErrorKind::ThreepidAuthFailed => Ok(ErrorKind::ThreepidAuthFailed),
            RumaApiErrorKind::ThreepidDenied => Ok(ErrorKind::ThreepidDenied),
            RumaApiErrorKind::ThreepidInUse => Ok(ErrorKind::ThreepidInUse),
            RumaApiErrorKind::ThreepidMediumNotSupported => {
                Ok(ErrorKind::ThreepidMediumNotSupported)
            }
            RumaApiErrorKind::ThreepidNotFound => Ok(ErrorKind::ThreepidNotFound),
            RumaApiErrorKind::TooLarge => Ok(ErrorKind::TooLarge),
            RumaApiErrorKind::UnableToAuthorizeJoin => Ok(ErrorKind::UnableToAuthorizeJoin),
            RumaApiErrorKind::UnableToGrantJoin => Ok(ErrorKind::UnableToGrantJoin),
            RumaApiErrorKind::Unauthorized => Ok(ErrorKind::Unauthorized),
            RumaApiErrorKind::Unknown => Ok(ErrorKind::Unknown),
            RumaApiErrorKind::UnknownToken { soft_logout } => {
                Ok(ErrorKind::UnknownToken { soft_logout: soft_logout.to_owned() })
            }
            RumaApiErrorKind::Unrecognized => Ok(ErrorKind::Unrecognized),
            RumaApiErrorKind::UnsupportedRoomVersion => Ok(ErrorKind::UnsupportedRoomVersion),
            RumaApiErrorKind::UrlNotSet => Ok(ErrorKind::UrlNotSet),
            RumaApiErrorKind::UserDeactivated => Ok(ErrorKind::UserDeactivated),
            RumaApiErrorKind::UserInUse => Ok(ErrorKind::UserInUse),
            RumaApiErrorKind::UserLocked => Ok(ErrorKind::UserLocked),
            RumaApiErrorKind::UserSuspended => Ok(ErrorKind::UserSuspended),
            RumaApiErrorKind::WeakPassword => Ok(ErrorKind::WeakPassword),
            RumaApiErrorKind::WrongRoomKeysVersion { current_version } => {
                Ok(ErrorKind::WrongRoomKeysVersion { current_version: current_version.to_owned() })
            }
            RumaApiErrorKind::_Custom { .. } => {
                // There is no way to map the extra values since they're private, so we omit
                // them
                Ok(ErrorKind::Custom { errcode: value.errcode().to_string() })
            }
            // In any other case, return it as the mapping not being yet implemented
            _ => Err(NotYetImplemented),
        }
    }
}
