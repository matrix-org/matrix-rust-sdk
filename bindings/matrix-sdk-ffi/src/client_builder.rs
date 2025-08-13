use std::{fs, num::NonZeroUsize, path::Path, sync::Arc, time::Duration};

#[cfg(not(target_family = "wasm"))]
use matrix_sdk::reqwest::Certificate;
use matrix_sdk::{
    crypto::{CollectStrategy, DecryptionSettings, TrustRequirement},
    encryption::{BackupDownloadStrategy, EncryptionSettings},
    event_cache::EventCacheError,
    ruma::{ServerName, UserId},
    sliding_sync::{
        Error as MatrixSlidingSyncError, VersionBuilder as MatrixSlidingSyncVersionBuilder,
        VersionBuilderError,
    },
    Client as MatrixClient, ClientBuildError as MatrixClientBuildError, HttpError, IdParseError,
    RumaApiError, SqliteStoreConfig, ThreadingSupport,
};
use ruma::api::error::{DeserializationError, FromHttpResponseError};
use tracing::{debug, error};
use zeroize::Zeroizing;

use super::client::Client;
use crate::{client::ClientSessionDelegate, error::ClientError, helpers::unwrap_or_clone_arc};

/// A list of bytes containing a certificate in DER or PEM form.
pub type CertificateBytes = Vec<u8>;

#[derive(Debug, Clone)]
enum HomeserverConfig {
    Url(String),
    ServerName(String),
    ServerNameOrUrl(String),
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
    sliding_sync_version_builder: SlidingSyncVersionBuilder,
    disable_automatic_token_refresh: bool,
    cross_process_store_locks_holder_name: Option<String>,
    enable_oidc_refresh_lock: bool,
    session_delegate: Option<Arc<dyn ClientSessionDelegate>>,
    encryption_settings: EncryptionSettings,
    room_key_recipient_strategy: CollectStrategy,
    decryption_settings: DecryptionSettings,
    enable_share_history_on_invite: bool,
    request_config: Option<RequestConfig>,

    #[cfg(not(target_family = "wasm"))]
    user_agent: Option<String>,
    #[cfg(not(target_family = "wasm"))]
    proxy: Option<String>,
    #[cfg(not(target_family = "wasm"))]
    disable_ssl_verification: bool,
    #[cfg(not(target_family = "wasm"))]
    disable_built_in_root_certificates: bool,
    #[cfg(not(target_family = "wasm"))]
    additional_root_certificates: Vec<Vec<u8>>,

    threading_support: ThreadingSupport,
}

/// The timeout applies to each read operation, and resets after a successful
/// read. This is more appropriate for detecting stalled connections when the
/// size isn’t known beforehand.
const DEFAULT_READ_TIMEOUT: Duration = Duration::from_secs(60);

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
            decryption_settings: DecryptionSettings {
                sender_device_trust_requirement: TrustRequirement::Untrusted,
            },
            enable_share_history_on_invite: false,
            request_config: Default::default(),
            threading_support: ThreadingSupport::Disabled,
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

    pub fn sliding_sync_version_builder(
        self: Arc<Self>,
        version_builder: SlidingSyncVersionBuilder,
    ) -> Arc<Self> {
        let mut builder = unwrap_or_clone_arc(self);
        builder.sliding_sync_version_builder = version_builder;
        Arc::new(builder)
    }

    pub fn disable_automatic_token_refresh(self: Arc<Self>) -> Arc<Self> {
        let mut builder = unwrap_or_clone_arc(self);
        builder.disable_automatic_token_refresh = true;
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
    pub fn decryption_settings(
        self: Arc<Self>,
        decryption_settings: DecryptionSettings,
    ) -> Arc<Self> {
        let mut builder = unwrap_or_clone_arc(self);
        builder.decryption_settings = decryption_settings;
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

    /// Whether the client should support threads client-side or not, and enable
    /// experimental support for MSC4306 (threads subscriptions) or not.
    pub fn threads_enabled(
        self: Arc<Self>,
        enabled: bool,
        thread_subscriptions: bool,
    ) -> Arc<Self> {
        let mut builder = unwrap_or_clone_arc(self);
        let support = if enabled {
            ThreadingSupport::Enabled { with_subscriptions: thread_subscriptions }
        } else {
            ThreadingSupport::Disabled
        };
        builder.threading_support = support;
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

        #[cfg(not(target_family = "wasm"))]
        {
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

            if let Some(user_agent) = builder.user_agent {
                inner_builder = inner_builder.user_agent(user_agent);
            }
        }

        if !builder.disable_automatic_token_refresh {
            inner_builder = inner_builder.handle_refresh_tokens();
        }

        inner_builder = inner_builder
            .with_encryption_settings(builder.encryption_settings)
            .with_room_key_recipient_strategy(builder.room_key_recipient_strategy)
            .with_decryption_settings(builder.decryption_settings)
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
            updated_config = updated_config.read_timeout(DEFAULT_READ_TIMEOUT);
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

        inner_builder = inner_builder.with_threading_support(builder.threading_support);

        let sdk_client = inner_builder.build().await?;

        // Log server version information at info level.
        if let Ok(server_info) = sdk_client.server_vendor_info().await {
            tracing::info!(
                server_name = %server_info.server_name,
                version = %server_info.version,
                "Connected to Matrix server"
            );
        } else {
            tracing::warn!("Could not retrieve server version information");
        }

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
}

#[cfg(not(target_family = "wasm"))]
#[matrix_sdk_ffi_macros::export]
impl ClientBuilder {
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

    pub fn user_agent(self: Arc<Self>, user_agent: String) -> Arc<Self> {
        let mut builder = unwrap_or_clone_arc(self);
        builder.user_agent = Some(user_agent);
        Arc::new(builder)
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
