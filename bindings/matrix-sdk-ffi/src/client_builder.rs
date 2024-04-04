use std::{fs, path::PathBuf, sync::Arc};

use matrix_sdk::{
    encryption::{BackupDownloadStrategy, EncryptionSettings},
    reqwest::Certificate,
    ruma::{
        api::{error::UnknownVersionError, MatrixVersion},
        ServerName, UserId,
    },
    Client as MatrixClient, ClientBuildError as MatrixClientBuildError,
    ClientBuilder as MatrixClientBuilder, IdParseError,
};
use sanitize_filename_reader_friendly::sanitize;
use url::Url;
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
    #[error(transparent)]
    Sdk(#[from] MatrixClientBuildError),
    #[error("Failed to build the client: {message}")]
    Generic { message: String },
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
    base_path: Option<String>,
    username: Option<String>,
    homeserver_cfg: Option<HomeserverConfig>,
    server_versions: Option<Vec<String>>,
    passphrase: Zeroizing<Option<String>>,
    user_agent: Option<String>,
    sliding_sync_proxy: Option<String>,
    proxy: Option<String>,
    disable_ssl_verification: bool,
    disable_automatic_token_refresh: bool,
    inner: MatrixClientBuilder,
    cross_process_refresh_lock_id: Option<String>,
    session_delegate: Option<Arc<dyn ClientSessionDelegate>>,
    additional_root_certificates: Vec<Vec<u8>>,
}

#[uniffi::export(async_runtime = "tokio")]
impl ClientBuilder {
    #[uniffi::constructor]
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            base_path: None,
            username: None,
            homeserver_cfg: None,
            server_versions: None,
            passphrase: Zeroizing::new(None),
            user_agent: None,
            sliding_sync_proxy: None,
            proxy: None,
            disable_ssl_verification: false,
            disable_automatic_token_refresh: false,
            inner: MatrixClient::builder().with_encryption_settings(EncryptionSettings {
                auto_enable_cross_signing: false,
                backup_download_strategy: BackupDownloadStrategy::AfterDecryptionFailure,
                auto_enable_backups: false,
            }),
            cross_process_refresh_lock_id: None,
            session_delegate: None,
            additional_root_certificates: Default::default(),
        })
    }

    pub fn enable_cross_process_refresh_lock(
        self: Arc<Self>,
        process_id: String,
        session_delegate: Box<dyn ClientSessionDelegate>,
    ) -> Arc<Self> {
        self.enable_cross_process_refresh_lock_inner(process_id, session_delegate.into())
    }

    pub fn set_session_delegate(
        self: Arc<Self>,
        session_delegate: Box<dyn ClientSessionDelegate>,
    ) -> Arc<Self> {
        let mut builder = unwrap_or_clone_arc(self);
        builder.session_delegate = Some(session_delegate.into());
        Arc::new(builder)
    }

    pub fn base_path(self: Arc<Self>, path: String) -> Arc<Self> {
        let mut builder = unwrap_or_clone_arc(self);
        builder.base_path = Some(path);
        Arc::new(builder)
    }

    pub fn username(self: Arc<Self>, username: String) -> Arc<Self> {
        let mut builder = unwrap_or_clone_arc(self);
        builder.username = Some(username);
        Arc::new(builder)
    }

    pub fn server_versions(self: Arc<Self>, versions: Vec<String>) -> Arc<Self> {
        let mut builder = unwrap_or_clone_arc(self);
        builder.server_versions = Some(versions);
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

    pub fn passphrase(self: Arc<Self>, passphrase: Option<String>) -> Arc<Self> {
        let mut builder = unwrap_or_clone_arc(self);
        builder.passphrase = Zeroizing::new(passphrase);
        Arc::new(builder)
    }

    pub fn user_agent(self: Arc<Self>, user_agent: String) -> Arc<Self> {
        let mut builder = unwrap_or_clone_arc(self);
        builder.user_agent = Some(user_agent);
        Arc::new(builder)
    }

    pub fn sliding_sync_proxy(self: Arc<Self>, sliding_sync_proxy: Option<String>) -> Arc<Self> {
        let mut builder = unwrap_or_clone_arc(self);
        builder.sliding_sync_proxy = sliding_sync_proxy;
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

    pub async fn build(self: Arc<Self>) -> Result<Arc<Client>, ClientBuildError> {
        Ok(Arc::new(self.build_inner().await?))
    }
}

impl ClientBuilder {
    pub(crate) fn with_encryption_settings(
        self: Arc<Self>,
        settings: EncryptionSettings,
    ) -> Arc<Self> {
        let mut builder = unwrap_or_clone_arc(self);
        builder.inner = builder.inner.with_encryption_settings(settings);
        Arc::new(builder)
    }

    pub(crate) fn enable_cross_process_refresh_lock_inner(
        self: Arc<Self>,
        process_id: String,
        session_delegate: Arc<dyn ClientSessionDelegate>,
    ) -> Arc<Self> {
        let mut builder = unwrap_or_clone_arc(self);
        builder.cross_process_refresh_lock_id = Some(process_id);
        builder.session_delegate = Some(session_delegate);
        Arc::new(builder)
    }

    pub(crate) fn set_session_delegate_inner(
        self: Arc<Self>,
        session_delegate: Arc<dyn ClientSessionDelegate>,
    ) -> Arc<Self> {
        let mut builder = unwrap_or_clone_arc(self);
        builder.session_delegate = Some(session_delegate);
        Arc::new(builder)
    }

    pub(crate) async fn build_inner(self: Arc<Self>) -> Result<Client, ClientBuildError> {
        let builder = unwrap_or_clone_arc(self);
        let mut inner_builder = builder.inner;

        if let (Some(base_path), Some(username)) = (builder.base_path, &builder.username) {
            // Determine store path
            let data_path = PathBuf::from(base_path).join(sanitize(username));
            fs::create_dir_all(&data_path)?;

            inner_builder = inner_builder.sqlite_store(&data_path, builder.passphrase.as_deref());
        }

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
            if let Ok(cert) = Certificate::from_der(&certificate) {
                certificates.push(cert);
            } else {
                let cert =
                    Certificate::from_pem(&certificate).map_err(|e| ClientBuildError::Generic {
                        message: format!("Failed to add a root certificate {e:?}"),
                    })?;
                certificates.push(cert);
            }
        }

        inner_builder = inner_builder.add_root_certificates(certificates);

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

        if let Some(server_versions) = builder.server_versions {
            inner_builder = inner_builder.server_versions(
                server_versions
                    .iter()
                    .map(|s| MatrixVersion::try_from(s.as_str()))
                    .collect::<Result<Vec<MatrixVersion>, UnknownVersionError>>()
                    .map_err(|e| ClientBuildError::Generic { message: e.to_string() })?,
            );
        }

        let sdk_client = inner_builder.build().await?;

        // At this point, `sdk_client` might contain a `sliding_sync_proxy` that has
        // been configured by the homeserver (if it's a `ServerName` and the
        // `.well-known` file is filled as expected).
        //
        // If `builder.sliding_sync_proxy` contains `Some(_)`, it means one wants to
        // overwrite this value. It would be an error to call
        // `sdk_client.set_sliding_sync_proxy()` with `None`, as it would erase the
        // `sliding_sync_proxy` if any, and it's not the intended behavior.
        //
        // So let's call `sdk_client.set_sliding_sync_proxy()` if and only if there is
        // `Some(_)` value in `builder.sliding_sync_proxy`. That's really important: It
        // might not break an existing app session, but it is likely to break a new
        // session, which not immediate to detect if there is no test.
        if let Some(sliding_sync_proxy) = builder.sliding_sync_proxy {
            sdk_client.set_sliding_sync_proxy(Some(Url::parse(&sliding_sync_proxy)?));
        }

        Ok(Client::new(
            sdk_client,
            builder.cross_process_refresh_lock_id,
            builder.session_delegate,
        )?)
    }
}
