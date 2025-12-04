use std::{
    future::Future,
    ops::Deref,
    option_env,
    path::{Path, PathBuf},
    sync::{Arc, Mutex as StdMutex},
    time::Duration,
};

use anyhow::Result;
use assign::assign;
use matrix_sdk::{
    Client, ClientBuilder, Room,
    config::{RequestConfig, SyncSettings},
    encryption::EncryptionSettings,
    ruma::{
        RoomId,
        api::client::{account::register::v3::Request as RegistrationRequest, uiaa},
        time::Instant,
    },
    sliding_sync::VersionBuilder,
    sync::SyncResponse,
    timeout::ElapsedError,
};
use matrix_sdk_base::crypto::{CollectStrategy, DecryptionSettings, TrustRequirement};
use once_cell::sync::Lazy;
use rand::Rng as _;
use tempfile::{TempDir, tempdir};
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
    decryption_settings: Option<DecryptionSettings>,
    room_key_recipient_strategy: CollectStrategy,
    encryption_settings: EncryptionSettings,
    enable_share_history_on_invite: bool,
    http_proxy: Option<String>,
    cross_process_store_locks_holder_name: Option<String>,
}

impl TestClientBuilder {
    pub fn new(username: impl AsRef<str>) -> Self {
        let suffix: u128 = rand::thread_rng().r#gen();
        let randomized_username = format!("{}{}", username.as_ref(), suffix);
        Self::with_exact_username(randomized_username)
    }

    pub fn with_exact_username(username: String) -> Self {
        Self {
            username,
            use_sqlite_dir: None,
            decryption_settings: None,
            encryption_settings: Default::default(),
            room_key_recipient_strategy: Default::default(),
            enable_share_history_on_invite: false,
            http_proxy: None,
            cross_process_store_locks_holder_name: None,
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

    pub fn enable_share_history_on_invite(mut self, enable_share_history_on_invite: bool) -> Self {
        self.enable_share_history_on_invite = enable_share_history_on_invite;
        self
    }

    /// Simulate the behaviour of the clients when the "exclude insecure
    /// devices" (MSC4153) labs flag is enabled.
    pub fn exclude_insecure_devices(mut self, exclude_insecure_devices: bool) -> Self {
        let (sender_device_trust_requirement, room_key_recipient_strategy) =
            if exclude_insecure_devices {
                (TrustRequirement::CrossSignedOrLegacy, CollectStrategy::IdentityBasedStrategy)
            } else {
                (TrustRequirement::Untrusted, CollectStrategy::AllDevices)
            };
        self.decryption_settings = Some(DecryptionSettings { sender_device_trust_requirement });
        self.room_key_recipient_strategy = room_key_recipient_strategy;

        self
    }

    pub fn cross_process_store_locks_holder_name(mut self, holder_name: String) -> Self {
        self.cross_process_store_locks_holder_name = Some(holder_name);
        self
    }

    fn common_client_builder(&self) -> ClientBuilder {
        let homeserver_url =
            option_env!("HOMESERVER_URL").unwrap_or("http://localhost:8228").to_owned();

        let mut client_builder = Client::builder()
            .user_agent("matrix-sdk-integration-tests")
            .homeserver_url(homeserver_url)
            .sliding_sync_version_builder(VersionBuilder::Native)
            .with_encryption_settings(self.encryption_settings)
            .with_room_key_recipient_strategy(self.room_key_recipient_strategy.clone())
            .with_enable_share_history_on_invite(self.enable_share_history_on_invite)
            .request_config(RequestConfig::short_retry());

        if let Some(decryption_settings) = &self.decryption_settings {
            client_builder = client_builder.with_decryption_settings(decryption_settings.clone())
        }

        if let Some(holder_name) = &self.cross_process_store_locks_holder_name {
            client_builder =
                client_builder.cross_process_store_locks_holder_name(holder_name.clone());
        }

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

    pub async fn sync_once(&self) -> Result<SyncResponse> {
        let mut settings = SyncSettings::default().timeout(Duration::from_secs(1));

        let token = { self.token.lock().unwrap().clone() };
        if let Some(token) = token {
            settings = settings.token(token);
        }

        let response = self.client.sync_once(settings).await?;

        let mut prev_token = self.token.lock().unwrap();
        if prev_token.as_ref() != Some(&response.next_batch) {
            *prev_token = Some(response.next_batch.clone());
        }
        Ok(response)
    }
}

impl Deref for SyncTokenAwareClient {
    type Target = Client;

    fn deref(&self) -> &Self::Target {
        &self.client
    }
}

/// Repeatedly run the given callback, at increasing intervals, until it returns
/// `Some`, or the given timeout has elapsed.
///
/// The callback `cb` is called with the attempt number as an argument.
///
/// # Returns
///
/// The result of `cb` if it returned `Some`. If the timeout elapses, an error
/// of type [`ElapsedError`].
pub async fn wait_until_some<F, Fut, R>(
    mut cb: F,
    timeout_duration: Duration,
) -> Result<R, ElapsedError>
where
    F: FnMut(u32) -> Fut,
    Fut: Future<Output = Option<R>>,
{
    let start = Instant::now();
    let mut attempt_count = 1;
    loop {
        if let Some(r) = cb(attempt_count).await {
            return Ok(r);
        }
        if start - Instant::now() > timeout_duration {
            return Err(ElapsedError());
        }
        sleep(Duration::from_millis(125) * attempt_count).await;
        attempt_count += 1;
    }
}

/// Waits for a room to arrive from a sync, for ~2 seconds.
///
/// Note: this doesn't run any sync, it assumes a sync has been running in the
/// background.
pub async fn wait_for_room(client: &Client, room_id: &RoomId) -> Room {
    let cb = async |i| {
        client.get_room(room_id).or_else(|| {
            tracing::warn!("attempt #{i}, alice couldn't see the room, retrying in a few…");
            None
        })
    };

    wait_until_some(cb, Duration::from_secs(2))
        .await
        .expect("room couldn't be fetched after all attempts")
}
