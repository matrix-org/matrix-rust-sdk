use std::{
    collections::HashMap,
    ops::Deref,
    option_env,
    sync::{Arc, Mutex as StdMutex},
    time::Duration,
};

use anyhow::Result;
use assign::assign;
use matrix_sdk::{
    config::{RequestConfig, SyncSettings},
    ruma::api::client::{account::register::v3::Request as RegistrationRequest, uiaa},
    Client,
};
use once_cell::sync::Lazy;
use rand::Rng as _;
use tempfile::{tempdir, TempDir};
use tokio::sync::Mutex;

static USERS: Lazy<Mutex<HashMap<String, (Client, TempDir)>>> = Lazy::new(Mutex::default);

pub struct TestClientBuilder {
    username: String,
    use_sqlite: bool,
    bootstrap_cross_signing: bool,
}

impl TestClientBuilder {
    pub fn new(username: impl Into<String>) -> Self {
        Self { username: username.into(), use_sqlite: false, bootstrap_cross_signing: false }
    }

    pub fn randomize_username(mut self) -> Self {
        let suffix: u128 = rand::thread_rng().gen();
        self.username = format!("{}{}", self.username, suffix);
        self
    }

    pub fn use_sqlite(mut self) -> Self {
        self.use_sqlite = true;
        self
    }

    pub fn bootstrap_cross_signing(mut self) -> Self {
        self.bootstrap_cross_signing = true;
        self
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
            .request_config(RequestConfig::short_retry());

        if self.bootstrap_cross_signing {
            client_builder = client_builder.with_encryption_settings(
                matrix_sdk::encryption::EncryptionSettings {
                    auto_enable_cross_signing: true,
                    ..Default::default()
                },
            );
        }

        let client = if self.use_sqlite {
            client_builder.sqlite_store(tmp_dir.path(), None).build().await?
        } else {
            client_builder.build().await?
        };

        // safe to assume we have not registered this user yet, but ignore if we did

        let auth = client.matrix_auth();
        if let Err(resp) = auth.register(RegistrationRequest::new()).await {
            // FIXME: do actually check the registration types...
            if let Some(_response) = resp.as_uiaa_response() {
                let request = assign!(RegistrationRequest::new(), {
                    username: Some(self.username.clone()),
                    password: Some(self.username.clone()),

                    auth: Some(uiaa::AuthData::Dummy(uiaa::Dummy::new())),
                });
                // we don't care if this failed, then we just try to login anyways
                let _ = auth.register(request).await;
            }
        }
        auth.login_username(&self.username, &self.username).await?;
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
