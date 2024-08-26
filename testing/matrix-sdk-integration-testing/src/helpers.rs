use std::{
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
    ruma::{
        api::client::{account::register::v3::Request as RegistrationRequest, uiaa},
        RoomId,
    },
    sliding_sync::VersionBuilder,
    Client, ClientBuilder, Room,
};
use once_cell::sync::Lazy;
use rand::Rng as _;
use reqwest::Url;
use tempfile::{tempdir, TempDir};
use tokio::{sync::Mutex, time::sleep};

/// This global maintains temp directories alive for the whole lifetime of the
/// process.
static TMP_DIRS: Lazy<Mutex<Vec<TempDir>>> = Lazy::new(Mutex::default);

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
    pub fn new(username: impl AsRef<str>) -> Self {
        let suffix: u128 = rand::thread_rng().gen();
        let randomized_username = format!("{}{}", username.as_ref(), suffix);
        Self::with_exact_username(randomized_username)
    }

    pub fn with_exact_username(username: String) -> Self {
        Self {
            username,
            use_sqlite_dir: None,
            encryption_settings: Default::default(),
            http_proxy: None,
        }
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

    fn common_client_builder(&self) -> ClientBuilder {
        let homeserver_url =
            option_env!("HOMESERVER_URL").unwrap_or("http://localhost:8228").to_owned();
        let sliding_sync_proxy_url =
            option_env!("SLIDING_SYNC_PROXY_URL").unwrap_or("http://localhost:8338").to_owned();

        let mut client_builder = Client::builder()
            .user_agent("matrix-sdk-integration-tests")
            .homeserver_url(homeserver_url)
            // Disable Simplified MSC3575 for the integration tests as, at the time of writing
            // (2024-07-15), we use a Synapse version that doesn't support Simplified MSC3575.
            .sliding_sync_version_builder(VersionBuilder::Proxy {
                url: Url::parse(&sliding_sync_proxy_url)
                    .expect("Sliding sync proxy URL is invalid"),
            })
            .with_encryption_settings(self.encryption_settings)
            .request_config(RequestConfig::short_retry());

        if let Some(proxy) = &self.http_proxy {
            client_builder = client_builder.proxy(proxy);
        }

        client_builder
    }

    /// Create a new Client that is a copy of the supplied one, created using
    /// [`Client::restore_session`].
    pub async fn duplicate(self, other: &Client) -> Result<Client> {
        let client_builder = self.common_client_builder();

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
        let client_builder = self.common_client_builder();
        let client = match self.use_sqlite_dir {
            None => client_builder.build().await?,
            Some(SqlitePath::Random) => {
                let tmp_dir = tempdir()?;
                let client = client_builder.sqlite_store(tmp_dir.path(), None).build().await?;
                TMP_DIRS.lock().await.push(tmp_dir);
                client
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

/// Waits for a room to arrive from a sync, for ~2 seconds.
///
/// Note: this doesn't run any sync, it assumes a sync has been running in the
/// background.
pub async fn wait_for_room(client: &Client, room_id: &RoomId) -> Room {
    for i in 1..5 {
        // Linear backoff: wait at most (1+2+3+4+5)*150ms > 2s for the response to come
        // back from the server.
        if let Some(room) = client.get_room(room_id) {
            return room;
        };
        tracing::warn!("attempt #{i}, alice couldn't see the room, retrying in a few…");
        sleep(Duration::from_millis(i * 150)).await;
    }
    panic!("room couldn't be fetched after all attempts");
}
