use std::{
    collections::HashMap,
    ops::Deref,
    option_env,
    path::{Path, PathBuf},
    sync::{Arc, Mutex as StdMutex},
    time::Duration,
};

use anyhow::Result;
use assign::assign;
use matrix_sdk::{
    config::{RequestConfig, SyncSettings},
    encryption::EncryptionSettings,
    ruma::api::client::{account::register::v3::Request as RegistrationRequest, uiaa},
    Client,
};
use once_cell::sync::Lazy;
use rand::Rng as _;
use tempfile::{tempdir, TempDir};
use tokio::sync::Mutex;

static USERS: Lazy<Mutex<HashMap<String, (Client, TempDir)>>> = Lazy::new(Mutex::default);

enum SqlitePath {
    Random,
    Path(PathBuf),
}

pub struct TestClientBuilder {
    username: String,
    use_sqlite_dir: Option<SqlitePath>,
    encryption_settings: EncryptionSettings,
    http_proxy: Option<String>,
}

impl TestClientBuilder {
    pub fn new(username: impl Into<String>) -> Self {
        Self {
            username: username.into(),
            use_sqlite_dir: None,
            encryption_settings: Default::default(),
            http_proxy: None,
        }
    }

    pub fn randomize_username(mut self) -> Self {
        let suffix: u128 = rand::thread_rng().gen();
        self.username = format!("{}{}", self.username, suffix);
        self
    }

    pub fn use_sqlite(mut self) -> Self {
        self.use_sqlite_dir = Some(SqlitePath::Random);
        self
    }

    /// Create or re-use a Sqlite store (with no passphrase) in the supplied
    /// directory. Note: this path must remain valid throughout the use of
    /// the constructed Client, so if you created a TempDir you must hang on
    /// to a reference to it throughout the test.
    pub fn use_sqlite_dir(mut self, path: &Path) -> Self {
        self.use_sqlite_dir = Some(SqlitePath::Path(path.to_owned()));
        self
    }

    pub fn encryption_settings(mut self, encryption_settings: EncryptionSettings) -> Self {
        self.encryption_settings = encryption_settings;
        self
    }

    pub fn http_proxy(mut self, proxy: String) -> Self {
        self.http_proxy = Some(proxy);
        self
    }

    /// Create a new Client that is a copy of the supplied one, created using
    /// [`Client::restore_session`].
    pub async fn duplicate(self, other: &Client) -> Result<Client> {
        let homeserver_url =
            option_env!("HOMESERVER_URL").unwrap_or("http://localhost:8228").to_owned();
        let sliding_sync_proxy_url =
            option_env!("SLIDING_SYNC_PROXY_URL").unwrap_or("http://localhost:8338").to_owned();

        let mut client_builder = Client::builder()
            .user_agent("matrix-sdk-integration-tests")
            .homeserver_url(homeserver_url)
            .sliding_sync_proxy(sliding_sync_proxy_url)
            .with_encryption_settings(self.encryption_settings)
            .request_config(RequestConfig::short_retry());

        if let Some(proxy) = self.http_proxy {
            client_builder = client_builder.proxy(proxy);
        }

        let client = match self.use_sqlite_dir {
            Some(SqlitePath::Path(path_buf)) => {
                client_builder.sqlite_store(&path_buf, None).build().await?
            }
            _ => {
                panic!("You must call use_sqlite_dir for a duplicate client!");
            }
        };

        client
            .restore_session(
                other.session().expect("Session must be logged in before we can duplicate it"),
            )
            .await
            .expect("Failed to restore session");

        Ok(client)
    }

    pub async fn build(self) -> Result<Client> {
        let mut users = USERS.lock().await;
        if let Some((client, _)) = users.get(&self.username) {
            return Ok(client.clone());
        }

        let homeserver_url =
            option_env!("HOMESERVER_URL").unwrap_or("http://localhost:8228").to_owned();
        let sliding_sync_proxy_url =
            option_env!("SLIDING_SYNC_PROXY_URL").unwrap_or("http://localhost:8338").to_owned();

        let tmp_dir = tempdir()?;

        let mut client_builder = Client::builder()
            .user_agent("matrix-sdk-integration-tests")
            .homeserver_url(homeserver_url)
            .sliding_sync_proxy(sliding_sync_proxy_url)
            .with_encryption_settings(self.encryption_settings)
            .request_config(RequestConfig::short_retry());

        if let Some(proxy) = self.http_proxy {
            client_builder = client_builder.proxy(proxy);
        }

        let client = match self.use_sqlite_dir {
            None => client_builder.build().await?,
            Some(SqlitePath::Random) => {
                client_builder.sqlite_store(tmp_dir.path(), None).build().await?
            }
            Some(SqlitePath::Path(path_buf)) => {
                client_builder.sqlite_store(&path_buf, None).build().await?
            }
        };

        // safe to assume we have not registered this user yet, but ignore if we did

        let auth = client.matrix_auth();
        let mut try_login = true;
        if let Err(resp) = auth.register(RegistrationRequest::new()).await {
            // FIXME: do actually check the registration types...
            if let Some(_response) = resp.as_uiaa_response() {
                let request = assign!(RegistrationRequest::new(), {
                    username: Some(self.username.clone()),
                    password: Some(self.username.clone()),

                    auth: Some(uiaa::AuthData::Dummy(uiaa::Dummy::new())),
                });
                // if this failed, we will attempt to login after.
                try_login = auth.register(request).await.is_err();
            }
        }
        if try_login {
            auth.login_username(&self.username, &self.username).await?;
        }
        users.insert(self.username, (client.clone(), tmp_dir)); // keeping temp dir around so it doesn't get destroyed yet

        Ok(client)
    }
}

/// Client that correctly maintains and propagates sync token values.
#[derive(Clone)]
pub struct SyncTokenAwareClient {
    client: Client,
    token: Arc<StdMutex<Option<String>>>,
}

impl SyncTokenAwareClient {
    pub fn new(client: Client) -> Self {
        Self { client, token: Arc::new(None.into()) }
    }

    pub async fn sync_once(&self) -> Result<()> {
        let mut settings = SyncSettings::default().timeout(Duration::from_secs(1));

        let token = { self.token.lock().unwrap().clone() };
        if let Some(token) = token {
            settings = settings.token(token);
        }

        let response = self.client.sync_once(settings).await?;

        let mut prev_token = self.token.lock().unwrap();
        if prev_token.as_ref() != Some(&response.next_batch) {
            *prev_token = Some(response.next_batch);
        }
        Ok(())
    }
}

impl Deref for SyncTokenAwareClient {
    type Target = Client;

    fn deref(&self) -> &Self::Target {
        &self.client
    }
}
