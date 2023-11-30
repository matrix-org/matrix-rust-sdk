use std::{fs, path::PathBuf, sync::Arc};

use matrix_sdk::{
    encryption::{BackupDownloadStrategy, EncryptionSettings},
    ruma::{
        api::{error::UnknownVersionError, MatrixVersion},
        ServerName, UserId,
    },
    Client as MatrixClient, ClientBuilder as MatrixClientBuilder,
};
use sanitize_filename_reader_friendly::sanitize;
use url::Url;
use zeroize::Zeroizing;

use super::{client::Client, RUNTIME};
use crate::{client::ClientSessionDelegate, error::ClientError, helpers::unwrap_or_clone_arc};

#[derive(Clone)]
pub(crate) enum UrlScheme {
    Http,
    Https,
}

#[derive(Clone, uniffi::Object)]
pub struct ClientBuilder {
    base_path: Option<String>,
    username: Option<String>,
    server_name: Option<(String, UrlScheme)>,
    homeserver_url: Option<String>,
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
}

#[uniffi::export]
impl ClientBuilder {
    #[uniffi::constructor]
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
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
        // Assume HTTPS if no protocol is provided.
        builder.server_name = Some((server_name, UrlScheme::Https));
        Arc::new(builder)
    }

    pub fn homeserver_url(self: Arc<Self>, url: String) -> Arc<Self> {
        let mut builder = unwrap_or_clone_arc(self);
        builder.homeserver_url = Some(url);
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

    pub fn build(self: Arc<Self>) -> Result<Arc<Client>, ClientError> {
        Ok(self.build_inner()?)
    }
}

impl ClientBuilder {
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

    pub(crate) fn server_name_with_protocol(
        self: Arc<Self>,
        server_name: String,
        protocol: UrlScheme,
    ) -> Arc<Self> {
        let mut builder = unwrap_or_clone_arc(self);
        builder.server_name = Some((server_name, protocol));
        Arc::new(builder)
    }

    pub(crate) fn build_inner(self: Arc<Self>) -> anyhow::Result<Arc<Client>> {
        let builder = unwrap_or_clone_arc(self);
        let mut inner_builder = builder.inner;

        if let (Some(base_path), Some(username)) = (builder.base_path, &builder.username) {
            // Determine store path
            let data_path = PathBuf::from(base_path).join(sanitize(username));
            fs::create_dir_all(&data_path)?;

            inner_builder = inner_builder.sqlite_store(&data_path, builder.passphrase.as_deref());
        }

        // Determine server either from URL, server name or user ID.
        if let Some(homeserver_url) = builder.homeserver_url {
            inner_builder = inner_builder.homeserver_url(homeserver_url);
        } else if let Some((server_name, protocol)) = builder.server_name {
            let server_name = ServerName::parse(server_name)?;
            inner_builder = match protocol {
                UrlScheme::Http => inner_builder.insecure_server_name_no_tls(&server_name),
                UrlScheme::Https => inner_builder.server_name(&server_name),
            };
        } else if let Some(username) = builder.username {
            let user = UserId::parse(username)?;
            inner_builder = inner_builder.server_name(user.server_name());
        } else {
            anyhow::bail!(
                "Failed to build: One of homeserver_url, server_name or username must be called."
            );
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

        if let Some(server_versions) = builder.server_versions {
            inner_builder = inner_builder.server_versions(
                server_versions
                    .iter()
                    .map(|s| MatrixVersion::try_from(s.as_str()))
                    .collect::<Result<Vec<MatrixVersion>, UnknownVersionError>>()?,
            );
        }

        let sdk_client = RUNTIME.block_on(async move { inner_builder.build().await })?;

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

impl Default for ClientBuilder {
    fn default() -> Self {
        let encryption_settings = EncryptionSettings {
            auto_enable_cross_signing: true,
            auto_enable_backups: true,
            backup_download_strategy: BackupDownloadStrategy::AfterDecryptionFailure,
        };
        let inner = MatrixClient::builder().with_encryption_settings(encryption_settings);

        Self {
            base_path: None,
            username: None,
            server_name: None,
            homeserver_url: None,
            server_versions: None,
            passphrase: Zeroizing::new(None),
            user_agent: None,
            sliding_sync_proxy: None,
            proxy: None,
            disable_ssl_verification: false,
            disable_automatic_token_refresh: false,
            inner,
            cross_process_refresh_lock_id: None,
            session_delegate: None,
        }
    }
}
