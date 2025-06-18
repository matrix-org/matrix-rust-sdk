use std::sync::Arc;

use matrix_sdk::{
    authentication::oauth::qrcode::{self, DeviceCodeErrorResponseType, LoginFailureReason},
    crypto::types::qr_login::{LoginQrCodeDecodeError, QrCodeModeData},
};
use matrix_sdk_common::{SendOutsideWasm, SyncOutsideWasm};
use tracing::error;

/// Data for the QR code login mechanism.
///
/// The [`QrCodeData`] can be serialized and encoded as a QR code or it can be
/// decoded from a QR code.
#[derive(Debug, uniffi::Object)]
pub struct QrCodeData {
    pub(crate) inner: qrcode::QrCodeData,
}

#[matrix_sdk_ffi_macros::export]
impl QrCodeData {
    /// Attempt to decode a slice of bytes into a [`QrCodeData`] object.
    ///
    /// The slice of bytes would generally be returned by a QR code decoder.
    #[uniffi::constructor]
    pub fn from_bytes(bytes: Vec<u8>) -> Result<Arc<Self>, QrCodeDecodeError> {
        Ok(Self { inner: qrcode::QrCodeData::from_bytes(&bytes)? }.into())
    }

    /// The server name contained within the scanned QR code data.
    ///
    /// Note: This value is only present when scanning a QR code the belongs to
    /// a logged in client. The mode where the new client shows the QR code
    /// will return `None`.
    pub fn server_name(&self) -> Option<String> {
        match &self.inner.mode_data {
            QrCodeModeData::Reciprocate { server_name } => Some(server_name.to_owned()),
            QrCodeModeData::Login => None,
        }
    }
}

/// Error type for the decoding of the [`QrCodeData`].
#[derive(Debug, thiserror::Error, uniffi::Error)]
#[uniffi(flat_error)]
pub enum QrCodeDecodeError {
    #[error("Error decoding QR code: {error:?}")]
    Crypto {
        #[from]
        error: LoginQrCodeDecodeError,
    },
}

#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum HumanQrLoginError {
    #[error("Linking with this device is not supported.")]
    LinkingNotSupported,
    #[error("The sign in was cancelled.")]
    Cancelled,
    #[error("The sign in was not completed in the required time.")]
    Expired,
    #[error("A secure connection could not have been established between the two devices.")]
    ConnectionInsecure,
    #[error("The sign in was declined.")]
    Declined,
    #[error("An unknown error has happened.")]
    Unknown,
    #[error("The homeserver doesn't provide sliding sync in its configuration.")]
    SlidingSyncNotAvailable,
    #[error("Unable to use OIDC as the supplied client metadata is invalid.")]
    OidcMetadataInvalid,
    #[error("The other device is not signed in and as such can't sign in other devices.")]
    OtherDeviceNotSignedIn,
}

impl From<qrcode::QRCodeLoginError> for HumanQrLoginError {
    fn from(value: qrcode::QRCodeLoginError) -> Self {
        use qrcode::{QRCodeLoginError, SecureChannelError};

        match value {
            QRCodeLoginError::LoginFailure { reason, .. } => match reason {
                LoginFailureReason::UnsupportedProtocol => HumanQrLoginError::LinkingNotSupported,
                LoginFailureReason::AuthorizationExpired => HumanQrLoginError::Expired,
                LoginFailureReason::UserCancelled => HumanQrLoginError::Cancelled,
                _ => HumanQrLoginError::Unknown,
            },

            QRCodeLoginError::OAuth(e) => {
                if let Some(e) = e.as_request_token_error() {
                    match e {
                        DeviceCodeErrorResponseType::AccessDenied => HumanQrLoginError::Declined,
                        DeviceCodeErrorResponseType::ExpiredToken => HumanQrLoginError::Expired,
                        _ => HumanQrLoginError::Unknown,
                    }
                } else {
                    HumanQrLoginError::Unknown
                }
            }

            QRCodeLoginError::SecureChannel(e) => match e {
                SecureChannelError::Utf8(_)
                | SecureChannelError::MessageDecode(_)
                | SecureChannelError::Json(_)
                | SecureChannelError::RendezvousChannel(_) => HumanQrLoginError::Unknown,
                SecureChannelError::SecureChannelMessage { .. }
                | SecureChannelError::Ecies(_)
                | SecureChannelError::InvalidCheckCode => HumanQrLoginError::ConnectionInsecure,
                SecureChannelError::InvalidIntent => HumanQrLoginError::OtherDeviceNotSignedIn,
            },

            QRCodeLoginError::UnexpectedMessage { .. }
            | QRCodeLoginError::CrossProcessRefreshLock(_)
            | QRCodeLoginError::DeviceKeyUpload(_)
            | QRCodeLoginError::SessionTokens(_)
            | QRCodeLoginError::UserIdDiscovery(_)
            | QRCodeLoginError::SecretImport(_) => HumanQrLoginError::Unknown,
        }
    }
}

/// Enum describing the progress of the QR-code login.
#[derive(Debug, Default, Clone, uniffi::Enum)]
pub enum QrLoginProgress {
    /// The login process is starting.
    #[default]
    Starting,
    /// We established a secure channel with the other device.
    EstablishingSecureChannel {
        /// The check code that the device should display so the other device
        /// can confirm that the channel is secure as well.
        check_code: u8,
        /// The string representation of the check code, will be guaranteed to
        /// be 2 characters long, preserving the leading zero if the
        /// first digit is a zero.
        check_code_string: String,
    },
    /// We are waiting for the login and for the OAuth 2.0 authorization server
    /// to give us an access token.
    WaitingForToken { user_code: String },
    /// The login has successfully finished.
    Done,
}

#[matrix_sdk_ffi_macros::export(callback_interface)]
pub trait QrLoginProgressListener: SyncOutsideWasm + SendOutsideWasm {
    fn on_update(&self, state: QrLoginProgress);
}

impl From<qrcode::LoginProgress> for QrLoginProgress {
    fn from(value: qrcode::LoginProgress) -> Self {
        use qrcode::LoginProgress;

        match value {
            LoginProgress::Starting => Self::Starting,
            LoginProgress::EstablishingSecureChannel { check_code } => {
                let check_code = check_code.to_digit();

                Self::EstablishingSecureChannel {
                    check_code,
                    check_code_string: format!("{check_code:02}"),
                }
            }
            LoginProgress::WaitingForToken { user_code } => Self::WaitingForToken { user_code },
            LoginProgress::Done => Self::Done,
        }
    }
}
