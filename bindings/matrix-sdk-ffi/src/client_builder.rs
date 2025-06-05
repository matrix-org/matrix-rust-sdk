use std::{fs, num::NonZeroUsize, path::Path, sync::Arc, time::Duration};

use futures_util::StreamExt;
use matrix_sdk::{
    authentication::oauth::qrcode::{self, DeviceCodeErrorResponseType, LoginFailureReason},
    crypto::{
        types::qr_login::{LoginQrCodeDecodeError, QrCodeModeData},
        CollectStrategy, TrustRequirement,
    },
    encryption::{BackupDownloadStrategy, EncryptionSettings},
    event_cache::EventCacheError,
    reqwest::Certificate,
    ruma::{ServerName, UserId},
    sliding_sync::{
        Error as MatrixSlidingSyncError, VersionBuilder as MatrixSlidingSyncVersionBuilder,
        VersionBuilderError,
    },
    Client as MatrixClient, ClientBuildError as MatrixClientBuildError, HttpError, IdParseError,
    RumaApiError, SqliteStoreConfig,
};
use matrix_sdk_common::{SendOutsideWasm, SyncOutsideWasm};
use ruma::api::error::{DeserializationError, FromHttpResponseError};
use tracing::{debug, error};
use zeroize::Zeroizing;

use super::client::Client;
use crate::{
    authentication::OidcConfiguration, client::ClientSessionDelegate, error::ClientError,
    helpers::unwrap_or_clone_arc, runtime::get_runtime_handle, task_handle::TaskHandle,
};

/// A list of bytes containing a certificate in DER or PEM form.
pub type CertificateBytes = Vec<u8>;

#[derive(Debug, Clone)]
enum HomeserverConfig {
    Url(String),
    ServerName(String),
    ServerNameOrUrl(String),
}

/// Data for the QR code login mechanism.
///
/// The [`QrCodeData`] can be serialized and encoded as a QR code or it can be
/// decoded from a QR code.
#[derive(Debug, uniffi::Object)]
pub struct QrCodeData {
    inner: qrcode::QrCodeData,
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

#[derive(Debug, thiserror::Error, uniffi::Error)]
#[uniffi(flat_error)]
pub enum ClientBuildError {
    #[error("The supplied server name is invalid.")]
    InvalidServerName,
    #[error(transparent)]
    ServerUnreachable(HttpError),
    #[error(transparent)]
    WellKnownLookupFailed(RumaApiError),
    #[error(transparent)]
    WellKnownDeserializationError(DeserializationError),
    #[error(transparent)]
    #[allow(dead_code)] // rustc's drunk, this is used
    SlidingSync(MatrixSlidingSyncError),
    #[error(transparent)]
    SlidingSyncVersion(VersionBuilderError),
    #[error(transparent)]
    Sdk(MatrixClientBuildError),
    #[error(transparent)]
    EventCache(#[from] EventCacheError),
    #[error("Failed to build the client: {message}")]
    Generic { message: String },
}

impl From<MatrixClientBuildError> for ClientBuildError {
    fn from(e: MatrixClientBuildError) -> Self {
        match e {
            MatrixClientBuildError::InvalidServerName => ClientBuildError::InvalidServerName,
            MatrixClientBuildError::Http(e) => ClientBuildError::ServerUnreachable(e),
            MatrixClientBuildError::AutoDiscovery(FromHttpResponseError::Server(e)) => {
                ClientBuildError::WellKnownLookupFailed(e)
            }
            MatrixClientBuildError::AutoDiscovery(FromHttpResponseError::Deserialization(e)) => {
                ClientBuildError::WellKnownDeserializationError(e)
            }
            MatrixClientBuildError::SlidingSyncVersion(e) => {
                ClientBuildError::SlidingSyncVersion(e)
            }
            _ => ClientBuildError::Sdk(e),
        }
    }
}

impl From<IdParseError> for ClientBuildError {
    fn from(e: IdParseError) -> ClientBuildError {
        ClientBuildError::Generic { message: format!("{e:#}") }
    }
}

impl From<std::io::Error> for ClientBuildError {
    fn from(e: std::io::Error) -> ClientBuildError {
        ClientBuildError::Generic { message: format!("{e:#}") }
    }
}

impl From<url::ParseError> for ClientBuildError {
    fn from(e: url::ParseError) -> ClientBuildError {
        ClientBuildError::Generic { message: format!("{e:#}") }
    }
}

impl From<ClientError> for ClientBuildError {
    fn from(e: ClientError) -> ClientBuildError {
        ClientBuildError::Generic { message: format!("{e:#}") }
    }
}

#[derive(Clone, uniffi::Object)]
pub struct ClientBuilder {
    session_paths: Option<SessionPaths>,
    session_passphrase: Zeroizing<Option<String>>,
    session_pool_max_size: Option<usize>,
    session_cache_size: Option<u32>,
    session_journal_size_limit: Option<u32>,
    system_is_memory_constrained: bool,
    username: Option<String>,
    homeserver_cfg: Option<HomeserverConfig>,
    user_agent: Option<String>,
    sliding_sync_version_builder: SlidingSyncVersionBuilder,
    proxy: Option<String>,
    disable_ssl_verification: bool,
    disable_automatic_token_refresh: bool,
    cross_process_store_locks_holder_name: Option<String>,
    enable_oidc_refresh_lock: bool,
    session_delegate: Option<Arc<dyn ClientSessionDelegate>>,
    additional_root_certificates: Vec<Vec<u8>>,
    disable_built_in_root_certificates: bool,
    encryption_settings: EncryptionSettings,
    room_key_recipient_strategy: CollectStrategy,
    decryption_trust_requirement: TrustRequirement,
    enable_share_history_on_invite: bool,
    request_config: Option<RequestConfig>,
}

#[matrix_sdk_ffi_macros::export]
impl ClientBuilder {
    #[uniffi::constructor]
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            session_paths: None,
            session_passphrase: Zeroizing::new(None),
            session_pool_max_size: None,
            session_cache_size: None,
            session_journal_size_limit: None,
            system_is_memory_constrained: false,
            username: None,
            homeserver_cfg: None,
            user_agent: None,
            sliding_sync_version_builder: SlidingSyncVersionBuilder::None,
            proxy: None,
            disable_ssl_verification: false,
            disable_automatic_token_refresh: false,
            cross_process_store_locks_holder_name: None,
            enable_oidc_refresh_lock: false,
            session_delegate: None,
            additional_root_certificates: Default::default(),
            disable_built_in_root_certificates: false,
            encryption_settings: EncryptionSettings {
                auto_enable_cross_signing: false,
                backup_download_strategy:
                    matrix_sdk::encryption::BackupDownloadStrategy::AfterDecryptionFailure,
                auto_enable_backups: false,
            },
            room_key_recipient_strategy: Default::default(),
            decryption_trust_requirement: TrustRequirement::Untrusted,
            enable_share_history_on_invite: false,
            request_config: Default::default(),
        })
    }

    pub fn cross_process_store_locks_holder_name(
        self: Arc<Self>,
        holder_name: String,
    ) -> Arc<Self> {
        let mut builder = unwrap_or_clone_arc(self);
        builder.cross_process_store_locks_holder_name = Some(holder_name);
        Arc::new(builder)
    }

    pub fn enable_oidc_refresh_lock(self: Arc<Self>) -> Arc<Self> {
        let mut builder = unwrap_or_clone_arc(self);
        builder.enable_oidc_refresh_lock = true;
        Arc::new(builder)
    }

    pub fn set_session_delegate(
        self: Arc<Self>,
        session_delegate: Box<dyn ClientSessionDelegate>,
    ) -> Arc<Self> {
        let mut builder = unwrap_or_clone_arc(self);
        builder.session_delegate = Some(session_delegate.into());
        Arc::new(builder)
    }

    /// Sets the paths that the client will use to store its data and caches.
    /// Both paths **must** be unique per session as the SDK stores aren't
    /// capable of handling multiple users, however it is valid to use the
    /// same path for both stores on a single session.
    ///
    /// Leaving this unset tells the client to use an in-memory data store.
    pub fn session_paths(self: Arc<Self>, data_path: String, cache_path: String) -> Arc<Self> {
        let mut builder = unwrap_or_clone_arc(self);
        builder.session_paths = Some(SessionPaths { data_path, cache_path });
        Arc::new(builder)
    }

    /// Set the passphrase for the stores given to
    /// [`ClientBuilder::session_paths`].
    pub fn session_passphrase(self: Arc<Self>, passphrase: Option<String>) -> Arc<Self> {
        let mut builder = unwrap_or_clone_arc(self);
        builder.session_passphrase = Zeroizing::new(passphrase);
        Arc::new(builder)
    }

    /// Set the pool max size for the SQLite stores given to
    /// [`ClientBuilder::session_paths`].
    ///
    /// Each store exposes an async pool of connections. This method controls
    /// the size of the pool. The larger the pool is, the more memory is
    /// consumed, but also the more the app is reactive because it doesn't need
    /// to wait on a pool to be available to run queries.
    ///
    /// See [`SqliteStoreConfig::pool_max_size`] to learn more.
    pub fn session_pool_max_size(self: Arc<Self>, pool_max_size: Option<u32>) -> Arc<Self> {
        let mut builder = unwrap_or_clone_arc(self);
        builder.session_pool_max_size = pool_max_size
            .map(|size| size.try_into().expect("`pool_max_size` is too large to fit in `usize`"));
        Arc::new(builder)
    }

    /// Set the cache size for the SQLite stores given to
    /// [`ClientBuilder::session_paths`].
    ///
    /// Each store exposes a SQLite connection. This method controls the cache
    /// size, in **bytes (!)**.
    ///
    /// The cache represents data SQLite holds in memory at once per open
    /// database file. The default cache implementation does not allocate the
    /// full amount of cache memory all at once. Cache memory is allocated
    /// in smaller chunks on an as-needed basis.
    ///
    /// See [`SqliteStoreConfig::cache_size`] to learn more.
    pub fn session_cache_size(self: Arc<Self>, cache_size: Option<u32>) -> Arc<Self> {
        let mut builder = unwrap_or_clone_arc(self);
        builder.session_cache_size = cache_size;
        Arc::new(builder)
    }

    /// Set the size limit for the SQLite WAL files of stores given to
    /// [`ClientBuilder::session_paths`].
    ///
    /// Each store uses the WAL journal mode. This method controls the size
    /// limit of the WAL files, in **bytes (!)**.
    ///
    /// See [`SqliteStoreConfig::journal_size_limit`] to learn more.
    pub fn session_journal_size_limit(self: Arc<Self>, limit: Option<u32>) -> Arc<Self> {
        let mut builder = unwrap_or_clone_arc(self);
        builder.session_journal_size_limit = limit;
        Arc::new(builder)
    }

    /// Tell the client that the system is memory constrained, like in a push
    /// notification process for example.
    ///
    /// So far, at the time of writing (2025-04-07), it changes the defaults of
    /// [`SqliteStoreConfig`], so one might not need to call
    /// [`ClientBuilder::session_cache_size`] and siblings for example. Please
    /// check [`SqliteStoreConfig::with_low_memory_config`].
    pub fn system_is_memory_constrained(self: Arc<Self>) -> Arc<Self> {
        let mut builder = unwrap_or_clone_arc(self);
        builder.system_is_memory_constrained = true;
        Arc::new(builder)
    }

    pub fn username(self: Arc<Self>, username: String) -> Arc<Self> {
        let mut builder = unwrap_or_clone_arc(self);
        builder.username = Some(username);
        Arc::new(builder)
    }

    pub fn server_name(self: Arc<Self>, server_name: String) -> Arc<Self> {
        let mut builder = unwrap_or_clone_arc(self);
        builder.homeserver_cfg = Some(HomeserverConfig::ServerName(server_name));
        Arc::new(builder)
    }

    pub fn homeserver_url(self: Arc<Self>, url: String) -> Arc<Self> {
        let mut builder = unwrap_or_clone_arc(self);
        builder.homeserver_cfg = Some(HomeserverConfig::Url(url));
        Arc::new(builder)
    }

    pub fn server_name_or_homeserver_url(self: Arc<Self>, server_name_or_url: String) -> Arc<Self> {
        let mut builder = unwrap_or_clone_arc(self);
        builder.homeserver_cfg = Some(HomeserverConfig::ServerNameOrUrl(server_name_or_url));
        Arc::new(builder)
    }

    pub fn user_agent(self: Arc<Self>, user_agent: String) -> Arc<Self> {
        let mut builder = unwrap_or_clone_arc(self);
        builder.user_agent = Some(user_agent);
        Arc::new(builder)
    }

    pub fn sliding_sync_version_builder(
        self: Arc<Self>,
        version_builder: SlidingSyncVersionBuilder,
    ) -> Arc<Self> {
        let mut builder = unwrap_or_clone_arc(self);
        builder.sliding_sync_version_builder = version_builder;
        Arc::new(builder)
    }

    pub fn proxy(self: Arc<Self>, url: String) -> Arc<Self> {
        let mut builder = unwrap_or_clone_arc(self);
        builder.proxy = Some(url);
        Arc::new(builder)
    }

    pub fn disable_ssl_verification(self: Arc<Self>) -> Arc<Self> {
        let mut builder = unwrap_or_clone_arc(self);
        builder.disable_ssl_verification = true;
        Arc::new(builder)
    }

    pub fn disable_automatic_token_refresh(self: Arc<Self>) -> Arc<Self> {
        let mut builder = unwrap_or_clone_arc(self);
        builder.disable_automatic_token_refresh = true;
        Arc::new(builder)
    }

    pub fn add_root_certificates(
        self: Arc<Self>,
        certificates: Vec<CertificateBytes>,
    ) -> Arc<Self> {
        let mut builder = unwrap_or_clone_arc(self);
        builder.additional_root_certificates = certificates;

        Arc::new(builder)
    }

    /// Don't trust any system root certificates, only trust the certificates
    /// provided through
    /// [`add_root_certificates`][ClientBuilder::add_root_certificates].
    pub fn disable_built_in_root_certificates(self: Arc<Self>) -> Arc<Self> {
        let mut builder = unwrap_or_clone_arc(self);
        builder.disable_built_in_root_certificates = true;
        Arc::new(builder)
    }

    pub fn auto_enable_cross_signing(
        self: Arc<Self>,
        auto_enable_cross_signing: bool,
    ) -> Arc<Self> {
        let mut builder = unwrap_or_clone_arc(self);
        builder.encryption_settings.auto_enable_cross_signing = auto_enable_cross_signing;
        Arc::new(builder)
    }

    /// Select a strategy to download room keys from the backup. By default
    /// we download after a decryption failure.
    ///
    /// Take a look at the [`BackupDownloadStrategy`] enum for more options.
    pub fn backup_download_strategy(
        self: Arc<Self>,
        backup_download_strategy: BackupDownloadStrategy,
    ) -> Arc<Self> {
        let mut builder = unwrap_or_clone_arc(self);
        builder.encryption_settings.backup_download_strategy = backup_download_strategy;
        Arc::new(builder)
    }

    /// Automatically create a backup version if no backup exists.
    pub fn auto_enable_backups(self: Arc<Self>, auto_enable_backups: bool) -> Arc<Self> {
        let mut builder = unwrap_or_clone_arc(self);
        builder.encryption_settings.auto_enable_backups = auto_enable_backups;
        Arc::new(builder)
    }

    /// Set the strategy to be used for picking recipient devices when sending
    /// an encrypted message.
    pub fn room_key_recipient_strategy(self: Arc<Self>, strategy: CollectStrategy) -> Arc<Self> {
        let mut builder = unwrap_or_clone_arc(self);
        builder.room_key_recipient_strategy = strategy;
        Arc::new(builder)
    }

    /// Set the trust requirement to be used when decrypting events.
    pub fn room_decryption_trust_requirement(
        self: Arc<Self>,
        trust_requirement: TrustRequirement,
    ) -> Arc<Self> {
        let mut builder = unwrap_or_clone_arc(self);
        builder.decryption_trust_requirement = trust_requirement;
        Arc::new(builder)
    }

    /// Set whether to enable the experimental support for sending and receiving
    /// encrypted room history on invite, per [MSC4268].
    ///
    /// [MSC4268]: https://github.com/matrix-org/matrix-spec-proposals/pull/4268
    pub fn enable_share_history_on_invite(
        self: Arc<Self>,
        enable_share_history_on_invite: bool,
    ) -> Arc<Self> {
        let mut builder = unwrap_or_clone_arc(self);
        builder.enable_share_history_on_invite = enable_share_history_on_invite;
        Arc::new(builder)
    }

    /// Add a default request config to this client.
    pub fn request_config(self: Arc<Self>, config: RequestConfig) -> Arc<Self> {
        let mut builder = unwrap_or_clone_arc(self);
        builder.request_config = Some(config);
        Arc::new(builder)
    }

    pub async fn build(self: Arc<Self>) -> Result<Arc<Client>, ClientBuildError> {
        let builder = unwrap_or_clone_arc(self);
        let mut inner_builder = MatrixClient::builder();

        if let Some(holder_name) = &builder.cross_process_store_locks_holder_name {
            inner_builder =
                inner_builder.cross_process_store_locks_holder_name(holder_name.clone());
        }

        let store_path = if let Some(session_paths) = &builder.session_paths {
            // This is the path where both the state store and the crypto store will live.
            let data_path = Path::new(&session_paths.data_path);
            // This is the path where the event cache store will live.
            let cache_path = Path::new(&session_paths.cache_path);

            debug!(
                data_path = %data_path.to_string_lossy(),
                event_cache_path = %cache_path.to_string_lossy(),
                "Creating directories for data (state and crypto) and cache stores.",
            );

            fs::create_dir_all(data_path)?;
            fs::create_dir_all(cache_path)?;

            let mut sqlite_store_config = if builder.system_is_memory_constrained {
                SqliteStoreConfig::with_low_memory_config(data_path)
            } else {
                SqliteStoreConfig::new(data_path)
            };

            sqlite_store_config =
                sqlite_store_config.passphrase(builder.session_passphrase.as_deref());

            if let Some(size) = builder.session_pool_max_size {
                sqlite_store_config = sqlite_store_config.pool_max_size(size);
            }

            if let Some(size) = builder.session_cache_size {
                sqlite_store_config = sqlite_store_config.cache_size(size);
            }

            if let Some(limit) = builder.session_journal_size_limit {
                sqlite_store_config = sqlite_store_config.journal_size_limit(limit);
            }

            inner_builder = inner_builder
                .sqlite_store_with_config_and_cache_path(sqlite_store_config, Some(cache_path));

            Some(data_path.to_owned())
        } else {
            debug!("Not using a store path.");
            None
        };

        // Determine server either from URL, server name or user ID.
        inner_builder = match builder.homeserver_cfg {
            Some(HomeserverConfig::Url(url)) => inner_builder.homeserver_url(url),
            Some(HomeserverConfig::ServerName(server_name)) => {
                let server_name = ServerName::parse(server_name)?;
                inner_builder.server_name(&server_name)
            }
            Some(HomeserverConfig::ServerNameOrUrl(server_name_or_url)) => {
                inner_builder.server_name_or_homeserver_url(server_name_or_url)
            }
            None => {
                if let Some(username) = builder.username {
                    let user = UserId::parse(username)?;
                    inner_builder.server_name(user.server_name())
                } else {
                    return Err(ClientBuildError::Generic {
                        message: "Failed to build: One of homeserver_url, server_name, server_name_or_homeserver_url or username must be called.".to_owned(),
                    });
                }
            }
        };

        let mut certificates = Vec::new();

        for certificate in builder.additional_root_certificates {
            // We don't really know what type of certificate we may get here, so let's try
            // first one type, then the other.
            match Certificate::from_der(&certificate) {
                Ok(cert) => {
                    certificates.push(cert);
                }
                Err(der_error) => {
                    let cert = Certificate::from_pem(&certificate).map_err(|pem_error| {
                        ClientBuildError::Generic {
                            message: format!("Failed to add a root certificate as DER ({der_error:?}) or PEM ({pem_error:?})"),
                        }
                    })?;
                    certificates.push(cert);
                }
            }
        }

        inner_builder = inner_builder.add_root_certificates(certificates);

        if builder.disable_built_in_root_certificates {
            inner_builder = inner_builder.disable_built_in_root_certificates();
        }

        if let Some(proxy) = builder.proxy {
            inner_builder = inner_builder.proxy(proxy);
        }

        if builder.disable_ssl_verification {
            inner_builder = inner_builder.disable_ssl_verification();
        }

        if !builder.disable_automatic_token_refresh {
            inner_builder = inner_builder.handle_refresh_tokens();
        }

        if let Some(user_agent) = builder.user_agent {
            inner_builder = inner_builder.user_agent(user_agent);
        }

        inner_builder = inner_builder
            .with_encryption_settings(builder.encryption_settings)
            .with_room_key_recipient_strategy(builder.room_key_recipient_strategy)
            .with_decryption_trust_requirement(builder.decryption_trust_requirement)
            .with_enable_share_history_on_invite(builder.enable_share_history_on_invite);

        match builder.sliding_sync_version_builder {
            SlidingSyncVersionBuilder::None => {
                inner_builder = inner_builder
                    .sliding_sync_version_builder(MatrixSlidingSyncVersionBuilder::None)
            }
            SlidingSyncVersionBuilder::Native => {
                inner_builder = inner_builder
                    .sliding_sync_version_builder(MatrixSlidingSyncVersionBuilder::Native)
            }
            SlidingSyncVersionBuilder::DiscoverNative => {
                inner_builder = inner_builder
                    .sliding_sync_version_builder(MatrixSlidingSyncVersionBuilder::DiscoverNative)
            }
        }

        if let Some(config) = builder.request_config {
            let mut updated_config = matrix_sdk::config::RequestConfig::default();
            if let Some(retry_limit) = config.retry_limit {
                updated_config =
                    updated_config.retry_limit(retry_limit.try_into().unwrap_or(usize::MAX));
            }
            if let Some(timeout) = config.timeout {
                updated_config = updated_config.timeout(Duration::from_millis(timeout));
            }
            if let Some(max_concurrent_requests) = config.max_concurrent_requests {
                if max_concurrent_requests > 0 {
                    updated_config = updated_config.max_concurrent_requests(NonZeroUsize::new(
                        max_concurrent_requests as usize,
                    ));
                }
            }
            if let Some(max_retry_time) = config.max_retry_time {
                updated_config =
                    updated_config.max_retry_time(Duration::from_millis(max_retry_time));
            }
            inner_builder = inner_builder.request_config(updated_config);
        }

        let sdk_client = inner_builder.build().await?;

        Ok(Arc::new(
            Client::new(
                sdk_client,
                builder.enable_oidc_refresh_lock,
                builder.session_delegate,
                store_path,
            )
            .await?,
        ))
    }

    /// Finish the building of the client and attempt to log in using the
    /// provided [`QrCodeData`].
    ///
    /// This method will build the client and immediately attempt to log the
    /// client in using the provided [`QrCodeData`] using the login
    /// mechanism described in [MSC4108]. As such this methods requires OAuth
    /// 2.0 support as well as sliding sync support.
    ///
    /// The usage of the progress_listener is required to transfer the
    /// [`CheckCode`] to the existing client.
    ///
    /// [MSC4108]: https://github.com/matrix-org/matrix-spec-proposals/pull/4108
    pub async fn build_with_qr_code(
        self: Arc<Self>,
        qr_code_data: &QrCodeData,
        oidc_configuration: &OidcConfiguration,
        progress_listener: Box<dyn QrLoginProgressListener>,
    ) -> Result<Arc<Client>, HumanQrLoginError> {
        let QrCodeModeData::Reciprocate { server_name } = &qr_code_data.inner.mode_data else {
            return Err(HumanQrLoginError::OtherDeviceNotSignedIn);
        };

        let builder = self.server_name_or_homeserver_url(server_name.to_owned());

        let client = builder.build().await.map_err(|e| match e {
            ClientBuildError::SlidingSync(_) => HumanQrLoginError::SlidingSyncNotAvailable,
            _ => {
                error!("Couldn't build the client {e:?}");
                HumanQrLoginError::Unknown
            }
        })?;

        let registration_data = oidc_configuration
            .registration_data()
            .map_err(|_| HumanQrLoginError::OidcMetadataInvalid)?;

        let oauth = client.inner.oauth();
        let login = oauth.login_with_qr_code(&qr_code_data.inner, Some(&registration_data));

        let mut progress = login.subscribe_to_progress();

        // We create this task, which will get cancelled once it's dropped, just in case
        // the progress stream doesn't end.
        let _progress_task = TaskHandle::new(get_runtime_handle().spawn(async move {
            while let Some(state) = progress.next().await {
                progress_listener.on_update(state.into());
            }
        }));

        login.await?;

        Ok(client)
    }
}

/// The store paths the client will use when built.
#[derive(Clone)]
struct SessionPaths {
    /// The path that the client will use to store its data.
    data_path: String,
    /// The path that the client will use to store its caches. This path can be
    /// the same as the data path if you prefer to keep everything in one place.
    cache_path: String,
}

#[derive(Clone, uniffi::Record)]
/// The config to use for HTTP requests by default in this client.
pub struct RequestConfig {
    /// Max number of retries.
    retry_limit: Option<u64>,
    /// Timeout for a request in milliseconds.
    timeout: Option<u64>,
    /// Max number of concurrent requests. No value means no limits.
    max_concurrent_requests: Option<u64>,
    /// Base delay between retries.
    max_retry_time: Option<u64>,
}

#[derive(Clone, uniffi::Enum)]
pub enum SlidingSyncVersionBuilder {
    None,
    Native,
    DiscoverNative,
}
