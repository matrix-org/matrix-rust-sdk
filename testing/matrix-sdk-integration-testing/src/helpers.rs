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
use tempfile::{tempdir, TempDir};
use tokio::sync::Mutex;

static USERS: Lazy<Mutex<HashMap<String, (Client, TempDir)>>> = Lazy::new(Mutex::default);

#[ctor::ctor]
fn init_logging() {
    use tracing::Level;
    use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::builder()
                .with_default_directive(Level::TRACE.into())
                .from_env()
                .unwrap(),
        )
        .with(tracing_subscriber::fmt::layer().with_test_writer())
        .init();
}

pub struct TestClientBuilder {
    username: String,
    use_sqlite: bool,
    bootstrap_cross_signing: bool,
}

impl TestClientBuilder {
    pub fn new(username: String) -> Self {
        Self { username, use_sqlite: false, bootstrap_cross_signing: false }
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
                matrix_sdk::encryption::EncryptionSettings { auto_enable_cross_signing: true },
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
        *self.token.lock().unwrap() = Some(response.next_batch);
        Ok(())
    }
}

impl Deref for SyncTokenAwareClient {
    type Target = Client;

    fn deref(&self) -> &Self::Target {
        &self.client
    }
}
