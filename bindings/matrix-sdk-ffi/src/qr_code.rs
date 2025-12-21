use std::sync::Arc;

use matrix_sdk::authentication::oauth::{
    qrcode::{
        self, CheckCodeSender as SdkCheckCodeSender, CheckCodeSenderError,
        DeviceCodeErrorResponseType, GeneratedQrProgress, LoginFailureReason, QrProgress,
    },
    OAuth,
};
use matrix_sdk_common::{stream::StreamExt, SendOutsideWasm, SyncOutsideWasm};

use crate::{
    authentication::OidcConfiguration, runtime::get_runtime_handle, task_handle::TaskHandle,
};

/// Handler for logging in with a QR code.
#[derive(uniffi::Object)]
pub struct LoginWithQrCodeHandler {
    oauth: OAuth,
    oidc_configuration: OidcConfiguration,
}

impl LoginWithQrCodeHandler {
    pub(crate) fn new(oauth: OAuth, oidc_configuration: OidcConfiguration) -> Self {
        Self { oauth, oidc_configuration }
    }
}

#[matrix_sdk_ffi_macros::export]
impl LoginWithQrCodeHandler {
    /// This method allows you to log in with a scanned QR code.
    ///
    /// The existing device needs to display the QR code which this device can
    /// scan, call this method and handle its progress updates to log in.
    ///
    /// For the login to succeed, the [`Client`] associated with the
    /// [`LoginWithQrCodeHandler`] must have been built with
    /// [`QrCodeData::server_name`] as the server name.
    ///
    /// This method uses the login mechanism described in [MSC4108]. As such,
    /// it requires OAuth 2.0 support.
    ///
    /// For the reverse flow where this device generates the QR code for the
    /// existing device to scan, use [`LoginWithQrCodeHandler::generate`].
    ///
    /// # Arguments
    ///
    /// * `qr_code_data` - The [`QrCodeData`] scanned from the QR code.
    /// * `progress_listener` - A progress listener that must also be used to
    ///   transfer the [`CheckCode`] to the existing device.
    ///
    /// [MSC4108]: https://github.com/matrix-org/matrix-spec-proposals/pull/4108
    pub async fn scan(
        self: Arc<Self>,
        qr_code_data: &QrCodeData,
        progress_listener: Box<dyn QrLoginProgressListener>,
    ) -> Result<(), HumanQrLoginError> {
        let registration_data = self
            .oidc_configuration
            .registration_data()
            .map_err(|_| HumanQrLoginError::OidcMetadataInvalid)?;

        let login =
            self.oauth.login_with_qr_code(Some(&registration_data)).scan(&qr_code_data.inner);

        let mut progress = login.subscribe_to_progress();

        // We create this task, which will get cancelled once it's dropped, just in case
        // the progress stream doesn't end.
        let _progress_task = TaskHandle::new(get_runtime_handle().spawn(async move {
            while let Some(state) = progress.next().await {
                progress_listener.on_update(state.into());
            }
        }));

        login.await?;

        Ok(())
    }

    /// This method allows you to log in by generating a QR code.
    ///
    /// This device needs to call this method and handle its progress updates to
    /// generate a QR code which the existing device can scan and grant the
    /// log in.
    ///
    /// This method uses the login mechanism described in [MSC4108]. As such,
    /// it requires OAuth 2.0 support.
    ///
    /// For the reverse flow where the existing device generates the QR code
    /// for this device to scan, use [`LoginWithQrCodeHandler::scan`].
    ///
    /// # Arguments
    ///
    /// * `progress_listener` - A progress listener that must also be used to
    ///   obtain the [`QrCodeData`] and collect the [`CheckCode`] from the user.
    ///
    /// [MSC4108]: https://github.com/matrix-org/matrix-spec-proposals/pull/4108
    pub async fn generate(
        self: Arc<Self>,
        progress_listener: Box<dyn GeneratedQrLoginProgressListener>,
    ) -> Result<(), HumanQrLoginError> {
        let registration_data = self
            .oidc_configuration
            .registration_data()
            .map_err(|_| HumanQrLoginError::OidcMetadataInvalid)?;

        let login = self.oauth.login_with_qr_code(Some(&registration_data)).generate();

        let mut progress = login.subscribe_to_progress();

        // We create this task, which will get cancelled once it's dropped, just in case
        // the progress stream doesn't end.
        let _progress_task = TaskHandle::new(get_runtime_handle().spawn(async move {
            while let Some(state) = progress.next().await {
                progress_listener.on_update(state.into());
            }
        }));

        login.await?;

        Ok(())
    }
}

/// Handler for granting login in with a QR code.
#[derive(uniffi::Object)]
pub struct GrantLoginWithQrCodeHandler {
    oauth: OAuth,
}

impl GrantLoginWithQrCodeHandler {
    pub(crate) fn new(oauth: OAuth) -> Self {
        Self { oauth }
    }
}

#[matrix_sdk_ffi_macros::export]
impl GrantLoginWithQrCodeHandler {
    /// This method allows you to grant login with a scanned QR code.
    ///
    /// The new device needs to display the QR code which this device can
    /// scan, call this method and handle its progress updates to grant the
    /// login.
    ///
    /// This method uses the login mechanism described in [MSC4108]. As such,
    /// it requires OAuth 2.0 support.
    ///
    /// For the reverse flow where this device generates the QR code for the
    /// existing device to scan, use [`GrantLoginWithQrCodeHandler::generate`].
    ///
    /// # Arguments
    ///
    /// * `qr_code_data` - The [`QrCodeData`] scanned from the QR code.
    /// * `progress_listener` - A progress listener that must also be used to
    ///   transfer the [`CheckCode`] to the new device.
    ///
    /// [MSC4108]: https://github.com/matrix-org/matrix-spec-proposals/pull/4108
    pub async fn scan(
        self: Arc<Self>,
        qr_code_data: &QrCodeData,
        progress_listener: Box<dyn GrantQrLoginProgressListener>,
    ) -> Result<(), HumanQrGrantLoginError> {
        let grant = self.oauth.grant_login_with_qr_code().scan(&qr_code_data.inner);

        let mut progress = grant.subscribe_to_progress();

        // We create this task, which will get cancelled once it's dropped, just in case
        // the progress stream doesn't end.
        let _progress_task = TaskHandle::new(get_runtime_handle().spawn(async move {
            while let Some(state) = progress.next().await {
                progress_listener.on_update(state.into());
            }
        }));

        grant.await?;

        Ok(())
    }

    /// This method allows you to grant login by generating a QR code.
    ///
    /// This device needs to call this method and handle its progress updates to
    /// generate a QR code which the new device can scan to log in.
    ///
    /// This method uses the login mechanism described in [MSC4108]. As such,
    /// it requires OAuth 2.0 support.
    ///
    /// For the reverse flow where the existing device generates the QR code
    /// for this device to scan, use [`GrantLoginWithQrCodeHandler::scan`].
    ///
    /// # Arguments
    ///
    /// * `progress_listener` - A progress listener that must also be used to
    ///   obtain the [`QrCodeData`] and collect the [`CheckCode`] from the user.
    ///
    /// [MSC4108]: https://github.com/matrix-org/matrix-spec-proposals/pull/4108
    pub async fn generate(
        self: Arc<Self>,
        progress_listener: Box<dyn GrantGeneratedQrLoginProgressListener>,
    ) -> Result<(), HumanQrGrantLoginError> {
        let grant = self.oauth.grant_login_with_qr_code().generate();

        let mut progress = grant.subscribe_to_progress();

        // We create this task, which will get cancelled once it's dropped, just in case
        // the progress stream doesn't end.
        let _progress_task = TaskHandle::new(get_runtime_handle().spawn(async move {
            while let Some(state) = progress.next().await {
                progress_listener.on_update(state.into());
            }
        }));

        grant.await?;

        Ok(())
    }
}

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

    /// Serialize the [`QrCodeData`] into a byte vector for encoding as a QR
    /// code.
    pub fn to_bytes(&self) -> Vec<u8> {
        self.inner.to_bytes()
    }

    /// The server name contained within the scanned QR code data.
    ///
    /// Note: This value is only present when scanning a QR code the belongs to
    /// a logged in client. The mode where the new client shows the QR code
    /// will return `None`.
    pub fn server_name(&self) -> Option<String> {
        match &self.inner.mode_data {
            qrcode::QrCodeModeData::Reciprocate { server_name } => Some(server_name.to_owned()),
            qrcode::QrCodeModeData::Login => None,
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
        error: qrcode::LoginQrCodeDecodeError,
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
    #[error("The check code was already sent.")]
    CheckCodeAlreadySent,
    #[error("The check code could not be sent.")]
    CheckCodeCannotBeSent,
    #[error("The rendezvous session was not found and might have expired")]
    NotFound,
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
                | SecureChannelError::InvalidCheckCode
                | SecureChannelError::CannotReceiveCheckCode => {
                    HumanQrLoginError::ConnectionInsecure
                }
                SecureChannelError::InvalidIntent => HumanQrLoginError::OtherDeviceNotSignedIn,
            },

            QRCodeLoginError::UnexpectedMessage { .. }
            | QRCodeLoginError::CrossProcessRefreshLock(_)
            | QRCodeLoginError::DeviceKeyUpload(_)
            | QRCodeLoginError::SessionTokens(_)
            | QRCodeLoginError::UserIdDiscovery(_)
            | QRCodeLoginError::SecretImport(_)
            | QRCodeLoginError::ServerReset(_) => HumanQrLoginError::Unknown,

            QRCodeLoginError::NotFound => HumanQrLoginError::NotFound,
        }
    }
}

impl From<CheckCodeSenderError> for HumanQrLoginError {
    fn from(value: CheckCodeSenderError) -> Self {
        match value {
            CheckCodeSenderError::AlreadySent => HumanQrLoginError::CheckCodeAlreadySent,
            CheckCodeSenderError::CannotSend => HumanQrLoginError::CheckCodeCannotBeSent,
        }
    }
}

#[derive(Debug, thiserror::Error, uniffi::Error)]
#[uniffi(flat_error)]
pub enum HumanQrGrantLoginError {
    /// The requested device ID is already in use.
    #[error("The requested device ID is already in use.")]
    DeviceIDAlreadyInUse,

    /// The check code was incorrect.
    #[error("The check code was incorrect.")]
    InvalidCheckCode,

    /// The other client proposed an unsupported protocol.
    #[error("Unsupported protocol: {0}")]
    UnsupportedProtocol(String),

    /// Secrets backup not set up properly.
    #[error("Secrets backup not set up: {0}")]
    MissingSecretsBackup(String),

    /// The rendezvous session was not found and might have expired.
    #[error("The rendezvous session was not found and might have expired")]
    NotFound,

    /// The device could not be created.
    #[error("The device could not be created.")]
    UnableToCreateDevice,

    /// An unknown error has happened.
    #[error("An unknown error has happened.")]
    Unknown(String),
}

impl From<qrcode::QRCodeGrantLoginError> for HumanQrGrantLoginError {
    fn from(value: qrcode::QRCodeGrantLoginError) -> Self {
        use qrcode::QRCodeGrantLoginError;

        match value {
            QRCodeGrantLoginError::DeviceIDAlreadyInUse => Self::DeviceIDAlreadyInUse,
            QRCodeGrantLoginError::InvalidCheckCode => Self::InvalidCheckCode,
            QRCodeGrantLoginError::UnableToCreateDevice => Self::UnableToCreateDevice,
            QRCodeGrantLoginError::UnsupportedProtocol(protocol) => {
                Self::UnsupportedProtocol(protocol.to_string())
            }
            QRCodeGrantLoginError::MissingSecretsBackup(error) => {
                Self::MissingSecretsBackup(error.map_or("other".to_owned(), |e| e.to_string()))
            }
            QRCodeGrantLoginError::NotFound => Self::NotFound,
            QRCodeGrantLoginError::Unknown(string) => Self::Unknown(string),
        }
    }
}

/// Enum describing the progress of logging in by scanning a QR code that was
/// generated on an existing device.
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
    /// We are syncing secrets.
    SyncingSecrets,
    /// The login has successfully finished.
    Done,
}

#[matrix_sdk_ffi_macros::export(callback_interface)]
pub trait QrLoginProgressListener: SyncOutsideWasm + SendOutsideWasm {
    fn on_update(&self, state: QrLoginProgress);
}

impl From<qrcode::LoginProgress<QrProgress>> for QrLoginProgress {
    fn from(value: qrcode::LoginProgress<QrProgress>) -> Self {
        use qrcode::LoginProgress;

        match value {
            LoginProgress::Starting => Self::Starting,
            LoginProgress::EstablishingSecureChannel(QrProgress { check_code }) => {
                let check_code = check_code.to_digit();

                Self::EstablishingSecureChannel {
                    check_code,
                    check_code_string: format!("{check_code:02}"),
                }
            }
            LoginProgress::WaitingForToken { user_code } => Self::WaitingForToken { user_code },
            LoginProgress::SyncingSecrets => Self::SyncingSecrets,
            LoginProgress::Done => Self::Done,
        }
    }
}

/// Enum describing the progress of logging in by generating a QR code and
/// having an existing device scan it.
#[derive(Debug, Default, Clone, uniffi::Enum)]
pub enum GeneratedQrLoginProgress {
    /// The login process is starting.
    #[default]
    Starting,
    /// We have established the secure channel and now need to display the
    /// QR code so that the existing device can scan it.
    QrReady { qr_code: Arc<QrCodeData> },
    /// The existing device has scanned the QR code and is displaying the
    /// checkcode. We now need to ask the user to enter the checkcode so that
    /// we can verify that the channel is indeed secure.
    QrScanned { check_code_sender: Arc<CheckCodeSender> },
    /// We are waiting for the login and for the OAuth 2.0 authorization server
    /// to give us an access token.
    WaitingForToken { user_code: String },
    /// We are syncing secrets.
    SyncingSecrets,
    /// The login has successfully finished.
    Done,
}

#[matrix_sdk_ffi_macros::export(callback_interface)]
pub trait GeneratedQrLoginProgressListener: SyncOutsideWasm + SendOutsideWasm {
    fn on_update(&self, state: GeneratedQrLoginProgress);
}

impl From<qrcode::LoginProgress<GeneratedQrProgress>> for GeneratedQrLoginProgress {
    fn from(value: qrcode::LoginProgress<GeneratedQrProgress>) -> Self {
        use qrcode::LoginProgress;

        match value {
            LoginProgress::Starting => Self::Starting,
            LoginProgress::EstablishingSecureChannel(GeneratedQrProgress::QrReady(inner)) => {
                Self::QrReady { qr_code: Arc::new(QrCodeData { inner }) }
            }
            LoginProgress::EstablishingSecureChannel(GeneratedQrProgress::QrScanned(inner)) => {
                Self::QrScanned { check_code_sender: Arc::new(CheckCodeSender { inner }) }
            }
            LoginProgress::WaitingForToken { user_code } => Self::WaitingForToken { user_code },
            LoginProgress::SyncingSecrets => Self::SyncingSecrets,
            LoginProgress::Done => Self::Done,
        }
    }
}

/// Enum describing the progress of granting login in by scanning a QR code that
/// was generated on a new device.
#[derive(Debug, Default, Clone, uniffi::Enum)]
pub enum GrantQrLoginProgress {
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
    /// The secure channel has been confirmed using the [`CheckCode`] and this
    /// device is waiting for the authorization to complete.
    WaitingForAuth {
        /// A URI to open in a (secure) system browser to verify the new login.
        verification_uri: String,
    },
    /// We are syncing secrets.
    SyncingSecrets,
    /// The login has successfully finished.
    Done,
}

#[matrix_sdk_ffi_macros::export(callback_interface)]
pub trait GrantQrLoginProgressListener: SyncOutsideWasm + SendOutsideWasm {
    fn on_update(&self, state: GrantQrLoginProgress);
}

impl From<qrcode::GrantLoginProgress<QrProgress>> for GrantQrLoginProgress {
    fn from(value: qrcode::GrantLoginProgress<QrProgress>) -> Self {
        use qrcode::GrantLoginProgress;

        match value {
            GrantLoginProgress::Starting => Self::Starting,
            GrantLoginProgress::EstablishingSecureChannel(QrProgress { check_code }) => {
                let check_code = check_code.to_digit();

                Self::EstablishingSecureChannel {
                    check_code,
                    check_code_string: format!("{check_code:02}"),
                }
            }
            GrantLoginProgress::WaitingForAuth { verification_uri } => {
                Self::WaitingForAuth { verification_uri: verification_uri.into() }
            }
            GrantLoginProgress::SyncingSecrets => Self::SyncingSecrets,
            GrantLoginProgress::Done => Self::Done,
        }
    }
}

/// Enum describing the progress of granting login by generating a QR code to
/// be scanned on the new device.
#[derive(Debug, Default, Clone, uniffi::Enum)]
pub enum GrantGeneratedQrLoginProgress {
    /// The login process is starting.
    #[default]
    Starting,
    /// We have established the secure channel and now need to display the
    /// QR code so that the existing device can scan it.
    QrReady { qr_code: Arc<QrCodeData> },
    /// The existing device has scanned the QR code and is displaying the
    /// checkcode. We now need to ask the user to enter the checkcode so that
    /// we can verify that the channel is indeed secure.
    QrScanned { check_code_sender: Arc<CheckCodeSender> },
    /// The secure channel has been confirmed using the [`CheckCode`] and this
    /// device is waiting for the authorization to complete.
    WaitingForAuth {
        /// A URI to open in a (secure) system browser to verify the new login.
        verification_uri: String,
    },
    /// We are syncing secrets.
    SyncingSecrets,
    /// The login has successfully finished.
    Done,
}

#[matrix_sdk_ffi_macros::export(callback_interface)]
pub trait GrantGeneratedQrLoginProgressListener: SyncOutsideWasm + SendOutsideWasm {
    fn on_update(&self, state: GrantGeneratedQrLoginProgress);
}

impl From<qrcode::GrantLoginProgress<GeneratedQrProgress>> for GrantGeneratedQrLoginProgress {
    fn from(value: qrcode::GrantLoginProgress<GeneratedQrProgress>) -> Self {
        use qrcode::GrantLoginProgress;

        match value {
            GrantLoginProgress::Starting => Self::Starting,
            GrantLoginProgress::EstablishingSecureChannel(GeneratedQrProgress::QrReady(inner)) => {
                Self::QrReady { qr_code: Arc::new(QrCodeData { inner }) }
            }
            GrantLoginProgress::EstablishingSecureChannel(GeneratedQrProgress::QrScanned(
                inner,
            )) => Self::QrScanned { check_code_sender: Arc::new(CheckCodeSender { inner }) },
            GrantLoginProgress::WaitingForAuth { verification_uri } => {
                Self::WaitingForAuth { verification_uri: verification_uri.into() }
            }
            GrantLoginProgress::SyncingSecrets => Self::SyncingSecrets,
            GrantLoginProgress::Done => Self::Done,
        }
    }
}

#[derive(Debug, uniffi::Object)]
/// Used to pass back the [`CheckCode`] entered by the user to verify that the
/// secure channel is indeed secure.
pub struct CheckCodeSender {
    inner: SdkCheckCodeSender,
}

#[matrix_sdk_ffi_macros::export]
impl CheckCodeSender {
    /// Send the [`CheckCode`].
    ///
    /// Calling this method more than once will result in an error.
    ///
    /// # Arguments
    ///
    /// * `check_code` - The check code in digits representation.
    pub async fn send(&self, code: u8) -> Result<(), HumanQrLoginError> {
        self.inner.send(code).await.map_err(HumanQrLoginError::from)
    }
}
