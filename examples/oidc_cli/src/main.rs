// Copyright 2023 Kévin Commaille.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::{
    convert::Infallible,
    future::IntoFuture,
    io::{self, Write},
    ops::Range,
    path::{Path, PathBuf},
    str::FromStr,
    sync::{Arc, Mutex},
};

use anyhow::{anyhow, bail};
use axum::{
    http::{Method, Request, StatusCode},
    response::IntoResponse,
    routing::any_service,
};
use futures_util::StreamExt;
use matrix_sdk::{
    config::SyncSettings,
    encryption::{recovery::RecoveryState, CrossSigningResetAuthType},
    oidc::{
        requests::account_management::AccountManagementActionFull,
        types::{
            client_credentials::ClientCredentials,
            iana::oauth::OAuthClientAuthenticationMethod,
            oidc::ApplicationType,
            registration::{ClientMetadata, Localized, VerifiedClientMetadata},
            requests::GrantType,
            scope::{Scope, ScopeToken},
        },
        AuthorizationCode, AuthorizationResponse, OidcAuthorizationData, OidcSession, UserSession,
    },
    room::Room,
    ruma::events::room::message::{MessageType, OriginalSyncRoomMessageEvent},
    Client, ClientBuildError, Result, RoomState,
};
use matrix_sdk_ui::sync_service::SyncService;
use rand::{distributions::Alphanumeric, thread_rng, Rng};
use serde::{Deserialize, Serialize};
use tokio::{fs, io::AsyncBufReadExt as _, net::TcpListener, sync::oneshot};
use tower::service_fn;
use url::Url;

/// A command-line tool to demonstrate the steps requiring an interaction with
/// an OpenID Connect provider for a Matrix client, using the Authorization Code
/// flow.
///
/// You can test this against one of the servers from the OIDC playground:
/// <https://github.com/element-hq/oidc-playground>.
///
/// To use this, just run `cargo run -p example-oidc-cli`, and everything
/// is interactive after that. You might want to set the `RUST_LOG` environment
/// variable to `warn` to reduce the noise in the logs. The program exits
/// whenever an unexpected error occurs.
///
/// To reset the login, simply use the `logout` command or delete the folder
/// containing the session file, the location is shown in the logs. Note that
/// the database must be deleted too as it can't be reused.
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    // The folder containing this example's data.
    let data_dir =
        dirs::data_dir().expect("no data_dir directory found").join("matrix_sdk/oidc_cli");
    // The file where the session is persisted.
    let session_file = data_dir.join("session.json");

    let cli = if session_file.exists() {
        OidcCli::from_stored_session(session_file).await?
    } else {
        OidcCli::new(&data_dir, session_file).await?
    };

    cli.run().await
}

/// The available commands once the client is logged in.
fn help() {
    println!("Usage: [command] [args…]\n");
    println!("Commands:");
    println!("  whoami                 Get information about this session");
    println!("  account                Get the URL to manage this account");
    println!("  watch [sliding?]       Watch new incoming messages until an error occurs");
    println!("  authorize [scope…]     Authorize the given scope");
    println!("  refresh                Refresh the access token");
    println!("  recover                Recover the E2EE secrets from secret storage");
    println!("  logout                 Log out of this account");
    println!("  exit                   Exit this program");
    println!("  help                   Show this message\n");
}

/// The data needed to re-build a client.
#[derive(Debug, Serialize, Deserialize)]
struct ClientSession {
    /// The URL of the homeserver of the user.
    homeserver: String,

    /// The path of the database.
    db_path: PathBuf,

    /// The passphrase of the database.
    passphrase: String,
}

/// The data needed to restore an OpenID Connect session.
#[derive(Debug, Serialize, Deserialize)]
struct Credentials {
    /// The client ID obtained after registration.
    client_id: String,
}

/// The full session to persist.
#[derive(Debug, Serialize, Deserialize)]
struct StoredSession {
    /// The data to re-build the client.
    client_session: ClientSession,

    /// The OIDC user session.
    user_session: UserSession,

    /// The OIDC client credentials.
    client_credentials: Credentials,
}

/// An OpenID Connect CLI.
#[derive(Clone, Debug)]
struct OidcCli {
    /// The Matrix client.
    client: Client,

    /// Whether this is a restored client.
    restored: bool,

    /// The path to the file storing the session.
    session_file: PathBuf,
}

impl OidcCli {
    /// Create a new session by logging in.
    async fn new(data_dir: &Path, session_file: PathBuf) -> anyhow::Result<Self> {
        println!("No previous session found, logging in…");

        let (client, client_session, issuer) = build_client(data_dir).await?;
        let cli = Self { client, restored: false, session_file };

        let client_id = cli.register_client(issuer).await?;
        cli.login().await?;

        // Persist the session to reuse it later.
        // This is not very secure, for simplicity. If the system provides a way of
        // storing secrets securely, it should be used instead.
        // Note that we could also build the user session from the login response.
        let user_session =
            cli.client.oidc().user_session().expect("A logged-in client should have a session");

        // The client registration data should be persisted separately than the user
        // session, to be reused for other sessions or user accounts with the same
        // issuer.
        // Also, client metadata should be persisted as it might change dependending on
        // the provider metadata.
        let client_credentials = Credentials { client_id };

        let serialized_session = serde_json::to_string(&StoredSession {
            client_session,
            user_session,
            client_credentials,
        })?;
        fs::write(&cli.session_file, serialized_session).await?;

        println!("Session persisted in {}", cli.session_file.to_string_lossy());

        cli.setup_background_save();

        Ok(cli)
    }

    /// Register the OIDC client with the provider.
    ///
    /// Returns the ID of the client returned by the provider.
    async fn register_client(&self, issuer: String) -> anyhow::Result<String> {
        let oidc = self.client.oidc();

        let provider_metadata = oidc.given_provider_metadata(&issuer).await?;

        if provider_metadata.registration_endpoint.is_none() {
            // This would require to register with the provider manually, which
            // we don't support here.
            bail!(
                "This provider doesn't support dynamic registration.\n\
                Please select another homeserver."
            );
        }

        let metadata = client_metadata();

        // During registration, we have the option of providing a software statement,
        // which is a digitally signed version of the client metadata. That would allow
        // to update the metadata later without changing the client ID, but requires to
        // have a way to serve public keys online to validate the signature of
        // the JWT.
        let res = oidc.register_client(&issuer, metadata.clone(), None).await?;

        oidc.restore_registered_client(
            issuer,
            metadata,
            ClientCredentials::None { client_id: res.client_id.clone() },
        );

        println!("\nRegistered successfully");

        Ok(res.client_id)
    }

    /// Login via the OIDC Authorization Code flow.
    async fn login(&self) -> anyhow::Result<()> {
        let oidc = self.client.oidc();

        // We create a loop here so the user can retry if an error happens.
        loop {
            // Here we spawn a server to listen on the loopback interface. Another option
            // would be to register a custom URI scheme with the system and handle
            // the redirect when the custom URI scheme is opened.
            let (redirect_uri, data_rx, signal_tx) = spawn_local_server().await?;

            let OidcAuthorizationData { url, state } =
                oidc.login(redirect_uri, None)?.build().await?;

            let authorization_code = match use_auth_url(&url, &state, data_rx, signal_tx).await {
                Ok(code) => code,
                Err(err) => {
                    oidc.abort_authorization(&state).await;
                    return Err(err);
                }
            };

            let res = oidc.finish_authorization(authorization_code).await;

            if let Err(err) = res {
                println!("Error: failed to login: {err}");
                println!("Please try again.\n");
                continue;
            }

            match oidc.finish_login().await {
                Ok(()) => {
                    let user_id = self.client.user_id().expect("Got a user ID");
                    println!("Logged in as {user_id}");
                    break;
                }
                Err(err) => {
                    println!("Error: failed to finish login: {err}");
                    println!("Please try again.\n");
                    continue;
                }
            }
        }

        Ok(())
    }

    /// Restore a previous session from a file.
    async fn from_stored_session(session_file: PathBuf) -> anyhow::Result<Self> {
        println!("Previous session found in '{}'", session_file.to_string_lossy());

        // The session was serialized as JSON in a file.
        let serialized_session = fs::read_to_string(&session_file).await?;
        let StoredSession { client_session, user_session, client_credentials } =
            serde_json::from_str(&serialized_session)?;

        // Build the client with the previous settings from the session.
        let client = Client::builder()
            .homeserver_url(client_session.homeserver)
            .handle_refresh_tokens()
            .sqlite_store(client_session.db_path, Some(&client_session.passphrase))
            .build()
            .await?;

        println!("Restoring session for {}…", user_session.meta.user_id);

        let session = OidcSession {
            credentials: ClientCredentials::None { client_id: client_credentials.client_id },
            metadata: client_metadata(),
            user: user_session,
        };
        // Restore the Matrix user session.
        client.restore_session(session).await?;

        let this = Self { client, restored: true, session_file };

        this.setup_background_save();

        Ok(this)
    }

    /// Run the main program.
    async fn run(&self) -> anyhow::Result<()> {
        help();

        loop {
            let mut input = String::new();

            print!("\nEnter command: ");
            io::stdout().flush().expect("Unable to write to stdout");

            io::stdin().read_line(&mut input).expect("Unable to read user input");

            let mut args = input.trim().split_ascii_whitespace();
            let cmd = args.next();

            match cmd {
                Some("whoami") => {
                    self.whoami();
                }
                Some("account") => {
                    self.account(None).await;
                }
                Some("profile") => {
                    self.account(Some(AccountManagementActionFull::Profile)).await;
                }
                Some("sessions") => {
                    self.account(Some(AccountManagementActionFull::SessionsList)).await;
                }
                Some("watch") => match args.next() {
                    Some(sub) => {
                        if sub == "sliding" {
                            self.watch_sliding_sync().await?;
                        } else {
                            println!("unknown subcommand for watch: available is 'sliding'");
                        }
                    }
                    None => self.watch().await?,
                },
                Some("authorize") => {
                    let mut scopes = args.peekable();
                    if scopes.peek().is_some() {
                        self.authorize(scopes).await?;
                    } else {
                        println!("Error: missing arguments, expected at least 1 scope\n");
                    }
                }
                Some("refresh") => {
                    self.refresh_token().await?;
                }
                Some("recover") => {
                    self.recover().await?;
                }
                Some("reset-cross-signing") => {
                    self.reset_cross_signing().await?;
                }
                Some("logout") => {
                    self.logout().await?;
                    break;
                }
                Some("exit") => {
                    break;
                }
                Some("help") => {
                    help();
                }
                Some(cmd) => {
                    println!("Error: unknown command '{cmd}'\n");
                    help();
                }
                None => {
                    println!("Error: no command\n");
                    help()
                }
            };
        }

        Ok(())
    }

    async fn recover(&self) -> anyhow::Result<()> {
        let recovery = self.client.encryption().recovery();

        println!("Please enter your recovery key:");

        let mut input = String::new();
        io::stdin().read_line(&mut input).expect("error: unable to read user input");

        let input = input.trim();

        recovery.recover(input).await?;

        match recovery.state() {
            RecoveryState::Enabled => println!("Successfully recovered all the E2EE secrets."),
            RecoveryState::Disabled => println!("Error recovering, recovery is disabled."),
            RecoveryState::Incomplete => println!("Couldn't recover all E2EE secrets."),
            _ => unreachable!("We should know our recovery state by now"),
        }

        Ok(())
    }

    async fn reset_cross_signing(&self) -> Result<()> {
        let encryption = self.client.encryption();

        if let Some(handle) = encryption.reset_cross_signing().await? {
            match handle.auth_type() {
                CrossSigningResetAuthType::Uiaa(_) => {
                    unimplemented!("This should never happen, this is after all the OIDC example.")
                }
                CrossSigningResetAuthType::Oidc(o) => {
                    println!(
                        "To reset your end-to-end encryption cross-signing identity, \
                        you first need to approve it at {}",
                        o.approval_url
                    );
                    handle.auth(None).await?;
                }
            }
        }

        print!("Successfully reset cross-signing");

        Ok(())
    }

    /// Get information about this session.
    fn whoami(&self) {
        let client = &self.client;
        let oidc = client.oidc();

        let user_id = client.user_id().expect("A logged in client has a user ID");
        let device_id = client.device_id().expect("A logged in client has a device ID");
        let homeserver = client.homeserver();
        let issuer = oidc.issuer().expect("A logged in OIDC client has an issuer");

        println!("\nUser ID: {user_id}");
        println!("Device ID: {device_id}");
        println!("Homeserver URL: {homeserver}");
        println!("OpenID Connect provider: {issuer}");
    }

    /// Get the account management URL.
    async fn account(&self, action: Option<AccountManagementActionFull>) {
        match self.client.oidc().account_management_url(action).await {
            Ok(Some(url)) => {
                println!("\nTo manage your account, visit: {url}");
            }
            _ => {
                println!("\nThis homeserver does not provide the URL to manage your account")
            }
        }
    }

    /// Watch incoming messages.
    async fn watch(&self) -> anyhow::Result<()> {
        let client = &self.client;

        // If this is a new client, ignore previous messages to not fill the logs.
        // Note that this might not work as intended, the initial sync might have failed
        // in a previous session.
        if !self.restored {
            client.sync_once(SyncSettings::default()).await.unwrap();
        }

        // Listen to room messages.
        let handle = client.add_event_handler(on_room_message);

        // Sync.
        let mut sync_stream = Box::pin(client.sync_stream(SyncSettings::default()).await);
        while let Some(res) = sync_stream.next().await {
            if let Err(err) = res {
                client.remove_event_handler(handle);
                return Err(err.into());
            }
        }

        Ok(())
    }

    /// This watches for incoming responses using the high-level sliding sync
    /// helpers (`SyncService`).
    async fn watch_sliding_sync(&self) -> anyhow::Result<()> {
        let sync_service = Arc::new(SyncService::builder(self.client.clone()).build().await?);

        sync_service.start().await;

        println!("press enter to exit the sync loop");

        let mut sync_service_state = sync_service.state();

        let sync_service_clone = sync_service.clone();
        let task = tokio::spawn(async move {
            // Only fail after getting 5 errors in a row. When we're in an always-refail
            // scenario, we move from the Error to the Running state for a bit
            // until we fail again, so we need to track both failure state and
            // running state, hence `num_errors` and `num_running`:
            // - if we failed and num_running was 1, then this is a failure following a
            //   failure.
            // - otherwise, we recovered from the failure and we can plain continue.

            let mut num_errors = 0;
            let mut num_running = 0;

            let mut _unused = String::new();
            let mut stdin = tokio::io::BufReader::new(tokio::io::stdin());

            loop {
                // Concurrently wait for an update from the sync service OR for the user to
                // press enter and leave early.
                tokio::select! {
                    res = sync_service_state.next() => {
                        if let Some(state) = res {
                            match state {
                                matrix_sdk_ui::sync_service::State::Idle
                                | matrix_sdk_ui::sync_service::State::Terminated => {
                                    num_errors = 0;
                                    num_running = 0;
                                }

                                matrix_sdk_ui::sync_service::State::Running => {
                                    num_running += 1;
                                    if num_running > 1 {
                                        num_errors = 0;
                                    }
                                }

                                matrix_sdk_ui::sync_service::State::Error => {
                                    num_errors += 1;
                                    num_running = 0;

                                    if num_errors == 5 {
                                        println!("ran into 5 errors in a row, terminating");
                                        break;
                                    }

                                    sync_service_clone.start().await;
                                }
                            }
                            println!("New sync service state update: {state:?}");
                        } else {
                            break;
                        }
                    }

                    _ = stdin.read_line(&mut _unused) => {
                        println!("Stopping loop because of user request");

                        if let Err(err) = sync_service.stop().await {
                            println!("Error when stopping the sync service: {err}");
                        }

                        break;
                    }
                }
            }
        });

        println!("waiting for sync service to stop...");
        task.await.unwrap();

        println!("done!");
        Ok(())
    }

    /// Sets up this client so that it automatically saves the session onto disk
    /// whenever there are new tokens that have been received.
    ///
    /// This should always be set up whenever automatic refresh is happening.
    fn setup_background_save(&self) {
        let this = self.clone();
        tokio::spawn(async move {
            while let Ok(update) = this.client.subscribe_to_session_changes().recv().await {
                match update {
                    matrix_sdk::SessionChange::UnknownToken { soft_logout } => {
                        println!("Received an unknown token error; soft logout? {soft_logout:?}");
                    }
                    matrix_sdk::SessionChange::TokensRefreshed => {
                        // The tokens have been refreshed, persist them to disk.
                        if let Err(err) = this.update_stored_session().await {
                            println!("Unable to store a session in the background: {err}");
                        }
                    }
                }
            }
        });
    }

    /// Update the session stored on the system.
    ///
    /// This should be called everytime the access token (and possibly refresh
    /// token) has changed.
    async fn update_stored_session(&self) -> anyhow::Result<()> {
        println!("Updating the stored session...");

        let serialized_session = fs::read_to_string(&self.session_file).await?;
        let mut session = serde_json::from_str::<StoredSession>(&serialized_session)?;

        let user_session =
            self.client.oidc().user_session().expect("A logged in client has a session");
        session.user_session = user_session;

        let serialized_session = serde_json::to_string(&session)?;
        fs::write(&self.session_file, serialized_session).await?;

        println!("Updating the stored session: done!");
        Ok(())
    }

    /// Authorize the given scopes using the OIDC Authorization Code flow.
    async fn authorize(&self, scopes: impl IntoIterator<Item = &str>) -> anyhow::Result<()> {
        // Here we spawn a server to listen on the loopback interface. Another option
        // would be to register a custom URI scheme with the system and handle the
        // redirect when the custom URI scheme is opened.
        let (redirect_uri, data_rx, signal_tx) = spawn_local_server().await?;

        let oidc = self.client.oidc();
        let scope = scopes
            .into_iter()
            .map(|s| ScopeToken::from_str(s).map_err(|_| anyhow!("invalid scope {s}")))
            .collect::<Result<Scope, _>>()?;

        let OidcAuthorizationData { url, state } =
            oidc.authorize_scope(scope, redirect_uri).build().await?;

        let authorization_code = match use_auth_url(&url, &state, data_rx, signal_tx).await {
            Ok(code) => code,
            Err(err) => {
                oidc.abort_authorization(&state).await;
                return Err(err);
            }
        };

        oidc.finish_authorization(authorization_code).await?;

        // Now we store the latest session to always have the latest tokens.
        self.update_stored_session().await?;

        println!("\nAuthorized successfully");

        Ok(())
    }

    /// Refresh the access token.
    async fn refresh_token(&self) -> anyhow::Result<()> {
        self.client.oidc().refresh_access_token().await?;

        // The session will automatically be refreshed because of the task persisting
        // the full session upon refresh in `setup_background_save`.

        println!("\nToken refreshed successfully");

        Ok(())
    }

    /// Log out from this session.
    async fn logout(&self) -> anyhow::Result<()> {
        // Log out via OIDC.
        let url_builder = self.client.oidc().logout().await?;

        // Delete the stored session and database.
        let data_dir = self.session_file.parent().expect("The file has a parent directory");
        fs::remove_dir_all(data_dir).await?;

        println!("\nLogged out successfully");

        if let Some(url_builder) = url_builder {
            let data = url_builder.build()?;
            println!(
                "\nTo log out from your account in the provider's interface, visit: {}",
                data.url
            );
        }

        println!("\nExiting…");

        Ok(())
    }
}

/// Build a new client.
///
/// Returns the client, the data required to restore the client, and the OIDC
/// issuer advertised by the homeserver.
async fn build_client(data_dir: &Path) -> anyhow::Result<(Client, ClientSession, String)> {
    let db_path = data_dir.join("db");

    // Generate a random passphrase.
    let mut rng = thread_rng();
    let passphrase: String =
        (&mut rng).sample_iter(Alphanumeric).take(32).map(char::from).collect();

    // We create a loop here so the user can retry if an error happens.
    loop {
        let mut homeserver = String::new();

        print!("\nHomeserver: ");
        io::stdout().flush().expect("Unable to write to stdout");
        io::stdin().read_line(&mut homeserver).expect("Unable to read user input");

        let homeserver = homeserver.trim();

        println!("\nChecking homeserver…");

        match Client::builder()
            // Try autodiscovery or test the URL.
            .server_name_or_homeserver_url(homeserver)
            // Make sure to automatically refresh tokens if needs be.
            .handle_refresh_tokens()
            // We use the sqlite store, which is available by default. This is the crucial part to
            // persist the encryption setup.
            // Note that other store backends are available and you can even implement your own.
            .sqlite_store(&db_path, Some(&passphrase))
            .build()
            .await
        {
            Ok(client) => {
                // Check if the homeserver advertises an OIDC Provider.
                // This can be bypassed by providing the issuer manually, but it should be the
                // most common case for public homeservers.
                match client.oidc().fetch_authentication_issuer().await {
                    Ok(issuer) => {
                        println!("Found issuer: {issuer}");

                        let homeserver = client.homeserver().to_string();
                        return Ok((
                            client,
                            ClientSession { homeserver, db_path, passphrase },
                            issuer,
                        ));
                    }
                    Err(error) => {
                        if error
                            .as_client_api_error()
                            .is_some_and(|err| err.status_code == StatusCode::NOT_FOUND)
                        {
                            println!("This homeserver doesn't advertise an authentication issuer.");
                        } else {
                            println!("Error fetching the authentication issuer: {error:?}");
                        }
                        // The client already initialized the store so we need to remove it.
                        fs::remove_dir_all(data_dir).await?;
                    }
                }
            }
            Err(error) => match &error {
                ClientBuildError::AutoDiscovery(_)
                | ClientBuildError::Url(_)
                | ClientBuildError::Http(_) => {
                    println!("Error checking the homeserver: {error}");
                    println!("Please try again\n");
                    // The client already initialized the store so we need to remove it.
                    fs::remove_dir_all(data_dir).await?;
                }
                ClientBuildError::InvalidServerName => {
                    println!("Error: not a valid server name");
                    println!("Please try again\n");
                }
                _ => {
                    // Forward other errors, it's unlikely we can retry with a different outcome.
                    return Err(error.into());
                }
            },
        }
    }
}

/// Generate the OIDC client metadata.
///
/// For simplicity, we use most of the default values here, but usually this
/// should be adapted to the provider metadata to make interactions as secure as
/// possible, for example by using the most secure signing algorithms supported
/// by the provider.
fn client_metadata() -> VerifiedClientMetadata {
    let redirect_uri = Url::parse("http://127.0.0.1").expect("Couldn't parse redirect URI");
    let client_uri = Url::parse("https://github.com/matrix-org/matrix-rust-sdk")
        .expect("Couldn't parse client URI");

    ClientMetadata {
        // This is a native application (in contrast to a web application, that runs in a browser).
        application_type: Some(ApplicationType::Native),
        // Native clients should be able to register the loopback interface and then point to any
        // port when needing a redirect URI. An alternative is to use a custom URI scheme registered
        // with the OS.
        redirect_uris: Some(vec![redirect_uri]),
        // We are going to use the Authorization Code flow, and of course we want to be able to
        // refresh our access token.
        grant_types: Some(vec![GrantType::RefreshToken, GrantType::AuthorizationCode]),
        // A native client shouldn't use authentication as the credentials could be intercepted.
        // Other protections are in place for the different requests.
        token_endpoint_auth_method: Some(OAuthClientAuthenticationMethod::None),
        // The following fields should be displayed in the OIDC provider interface as part of the
        // process to get the user's consent. It means that these should contain real data so the
        // user can make sure that they allow the proper application.
        // We are cheating here because this is an example.
        client_name: Some(Localized::new("matrix-rust-sdk-oidc-cli".to_owned(), [])),
        contacts: Some(vec!["root@127.0.0.1".to_owned()]),
        client_uri: Some(Localized::new(client_uri.clone(), [])),
        policy_uri: Some(Localized::new(client_uri.clone(), [])),
        tos_uri: Some(Localized::new(client_uri, [])),
        ..Default::default()
    }
    .validate()
    .unwrap()
}

/// Open the authorization URL and wait for it to be complete.
///
/// Returns the code to obtain the access token.
async fn use_auth_url(
    url: &Url,
    state: &str,
    data_rx: oneshot::Receiver<String>,
    signal_tx: oneshot::Sender<()>,
) -> anyhow::Result<AuthorizationCode> {
    println!("\nPlease authenticate yourself at: {url}\n");
    println!("Then proceed to the authorization.\n");

    let response_query = data_rx.await?;
    signal_tx.send(()).expect("Receiver is still alive");

    let code = match AuthorizationResponse::parse_query(&response_query)? {
        AuthorizationResponse::Success(code) => code,
        AuthorizationResponse::Error(err) => {
            let err = err.error;
            return Err(anyhow!("{}: {:?}", err.error, err.error_description));
        }
    };

    // Here we only manage one authorization at a time so, if the state string is
    // wrong, it is an error. Some clients might want to allow several
    // authorizations at once, in which case the state string can be used to
    // identify the session that was authorized.
    if code.state != state {
        bail!("State strings don't match")
    }

    Ok(code)
}

/// Spawn a local server to listen on redirects at the end of the authorization
/// process.
///
/// Returns the URL the server listens to, the receiver that will receive the
/// data returned by the provider and a sender to shutdown the server.
async fn spawn_local_server(
) -> anyhow::Result<(Url, oneshot::Receiver<String>, oneshot::Sender<()>)> {
    /// The range of ports the SSO server will try to bind to randomly.
    ///
    /// This is used to avoid binding to a port blocked by browsers.
    /// See <https://fetch.spec.whatwg.org/#port-blocking>.
    const SSO_SERVER_BIND_RANGE: Range<u16> = 20000..30000;
    /// The number of times the SSO server will try to bind to a random port
    const SSO_SERVER_BIND_TRIES: u8 = 10;

    // The channel used to shutdown the server when we are done with it.
    let (signal_tx, signal_rx) = oneshot::channel::<()>();
    // The channel used to transmit the data received a the redirect URL.
    let (data_tx, data_rx) = oneshot::channel::<String>();
    let data_tx_mutex = Arc::new(Mutex::new(Some(data_tx)));

    // We bind to the IPv4 loopback interface.
    let mut redirect_url = Url::parse("http://127.0.0.1:0/")
        .expect("Couldn't parse good known loopback interface URL");

    // Bind a TCP listener to a random port.
    let listener = {
        let host = redirect_url.host_str().expect("The redirect URL doesn't have a host");
        let mut n = 0u8;

        loop {
            let port = thread_rng().gen_range(SSO_SERVER_BIND_RANGE);
            match TcpListener::bind((host, port)).await {
                Ok(l) => {
                    redirect_url
                        .set_port(Some(port))
                        .expect("Could not set new port on redirect URL");
                    break l;
                }
                Err(_) if n < SSO_SERVER_BIND_TRIES => {
                    n += 1;
                }
                Err(e) => {
                    return Err(e.into());
                }
            }
        }
    };

    // Set up the server.
    let router = any_service(service_fn(move |request: Request<_>| {
        let data_tx_mutex = data_tx_mutex.clone();
        async move {
            // Reject methods others than HEAD or GET.
            if request.method() != Method::HEAD && request.method() != Method::GET {
                return Ok::<_, Infallible>(StatusCode::METHOD_NOT_ALLOWED.into_response());
            }

            // We only need to get the first response so we consume the transmitter the
            // first time.
            if let Some(data_tx) = data_tx_mutex.lock().unwrap().take() {
                let query_string = request.uri().query().unwrap_or_default();

                data_tx.send(query_string.to_owned()).expect("The receiver is still alive");
            }

            Ok("The authorization step is complete. You can close this page and go back to the oidc-cli.".into_response())
        }
    }));

    let server = axum::serve(listener, router)
        .with_graceful_shutdown(async {
            signal_rx.await.ok();
        })
        .into_future();

    tokio::spawn(server);

    Ok((redirect_url, data_rx, signal_tx))
}

/// Handle room messages.
async fn on_room_message(event: OriginalSyncRoomMessageEvent, room: Room) {
    // We only want to log text messages in joined rooms.
    if room.state() != RoomState::Joined {
        return;
    }
    let MessageType::Text(text_content) = &event.content.msgtype else { return };

    let room_name = match room.compute_display_name().await {
        Ok(room_name) => room_name.to_string(),
        Err(error) => {
            println!("Error getting room display name: {error}");
            // Let's fallback to the room ID.
            room.room_id().to_string()
        }
    };

    println!("[{room_name}] {}: {}", event.sender, text_content.body)
}
