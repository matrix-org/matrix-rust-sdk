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
    io::{self, Write},
    net::{Ipv4Addr, Ipv6Addr},
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::bail;
use futures_util::StreamExt;
use matrix_sdk::{
    Client, ClientBuildError, Result, RoomState,
    authentication::oauth::{
        AccountManagementActionFull, ClientId, OAuthAuthorizationData, OAuthError, OAuthSession,
        UrlOrQuery, UserSession,
        error::OAuthClientRegistrationError,
        registration::{ApplicationType, ClientMetadata, Localized, OAuthGrantType},
    },
    config::SyncSettings,
    encryption::{CrossSigningResetAuthType, recovery::RecoveryState},
    room::Room,
    ruma::{
        events::room::message::{MessageType, OriginalSyncRoomMessageEvent},
        serde::Raw,
    },
    utils::local_server::{LocalServerBuilder, LocalServerRedirectHandle, QueryString},
};
use matrix_sdk_ui::sync_service::SyncService;
use rand::{Rng, distributions::Alphanumeric, thread_rng};
use serde::{Deserialize, Serialize};
use tokio::{fs, io::AsyncBufReadExt as _};
use url::Url;

/// A command-line tool to demonstrate the steps requiring an interaction with
/// an OAuth 2.0 authorization server for a Matrix client, using the
/// Authorization Code flow.
///
/// You can test this against one of the servers from the OIDC playground:
/// <https://github.com/element-hq/oidc-playground>.
///
/// To use this, just run `cargo run -p example-oauth-cli`, and everything
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
        dirs::data_dir().expect("no data_dir directory found").join("matrix_sdk/oauth_cli");
    // The file where the session is persisted.
    let session_file = data_dir.join("session.json");

    let cli = if session_file.exists() {
        OAuthCli::from_stored_session(session_file).await?
    } else {
        OAuthCli::new(&data_dir, session_file).await?
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

/// The full session to persist.
#[derive(Debug, Serialize, Deserialize)]
struct StoredSession {
    /// The data to re-build the client.
    client_session: ClientSession,

    /// The OAuth 2.0 user session.
    user_session: UserSession,

    /// The OAuth 2.0 client ID.
    client_id: ClientId,
}

/// An OAuth 2.0 CLI.
#[derive(Clone, Debug)]
struct OAuthCli {
    /// The Matrix client.
    client: Client,

    /// Whether this is a restored client.
    restored: bool,

    /// The path to the file storing the session.
    session_file: PathBuf,
}

impl OAuthCli {
    /// Create a new session by logging in.
    async fn new(data_dir: &Path, session_file: PathBuf) -> anyhow::Result<Self> {
        println!("No previous session found, logging in…");

        let (client, client_session) = build_client(data_dir).await?;
        let cli = Self { client, restored: false, session_file };

        if let Err(error) = cli.register_and_login().await {
            if let Some(error) = error.downcast_ref::<OAuthError>()
                && let OAuthError::ClientRegistration(OAuthClientRegistrationError::NotSupported) =
                    error
            {
                // This would require to register with the authorization server manually, which
                // we don't support here.
                bail!(
                    "This server doesn't support dynamic registration.\n\
                     Please select another homeserver."
                );
            } else {
                return Err(error);
            }
        }

        // Persist the session to reuse it later.
        // This is not very secure, for simplicity. If the system provides a way of
        // storing secrets securely, it should be used instead.
        let full_session =
            cli.client.oauth().full_session().expect("A logged-in client should have a session");

        let serialized_session = serde_json::to_string(&StoredSession {
            client_session,
            user_session: full_session.user,
            client_id: full_session.client_id,
        })?;
        fs::write(&cli.session_file, serialized_session).await?;

        println!("Session persisted in {}", cli.session_file.to_string_lossy());

        cli.setup_background_save();

        Ok(cli)
    }

    /// Register the client and log in the user via the OAuth 2.0 Authorization
    /// Code flow.
    async fn register_and_login(&self) -> anyhow::Result<()> {
        let oauth = self.client.oauth();

        // We create a loop here so the user can retry if an error happens.
        loop {
            // Here we spawn a server to listen on the loopback interface. Another option
            // would be to register a custom URI scheme with the system and handle
            // the redirect when the custom URI scheme is opened.
            let (redirect_uri, server_handle) = LocalServerBuilder::new().spawn().await?;

            let OAuthAuthorizationData { url, .. } = oauth
                .login(redirect_uri, None, Some(client_metadata().into()), None)
                .build()
                .await?;

            let query_string =
                use_auth_url(&url, server_handle).await.map(|query| query.0).unwrap_or_default();

            match oauth.finish_login(UrlOrQuery::Query(query_string)).await {
                Ok(()) => {
                    let user_id = self.client.user_id().expect("Got a user ID");
                    println!("Logged in as {user_id}");
                    break;
                }
                Err(err) => {
                    println!("Error: failed to login: {err}");
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
        let StoredSession { client_session, user_session, client_id } =
            serde_json::from_str(&serialized_session)?;

        // Build the client with the previous settings from the session.
        let client = Client::builder()
            .homeserver_url(client_session.homeserver)
            .handle_refresh_tokens()
            .sqlite_store(client_session.db_path, Some(&client_session.passphrase))
            .build()
            .await?;

        println!("Restoring session for {}…", user_session.meta.user_id);

        let session = OAuthSession { client_id, user: user_session };
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
            }
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
                    unimplemented!(
                        "This should never happen, this is after all the OAuth 2.0 example."
                    )
                }
                CrossSigningResetAuthType::OAuth(o) => {
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

        let user_id = client.user_id().expect("A logged in client has a user ID");
        let device_id = client.device_id().expect("A logged in client has a device ID");
        let homeserver = client.homeserver();

        println!("\nUser ID: {user_id}");
        println!("Device ID: {device_id}");
        println!("Homeserver URL: {homeserver}");
    }

    /// Get the account management URL.
    async fn account(&self, action: Option<AccountManagementActionFull>) {
        let Ok(Some(mut url_builder)) = self.client.oauth().fetch_account_management_url().await
        else {
            println!("\nThis homeserver does not provide the URL to manage your account");
            return;
        };

        if let Some(action) = action {
            url_builder = url_builder.action(action);
        }

        let url = url_builder.build();
        println!("\nTo manage your account, visit: {url}");
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

                                matrix_sdk_ui::sync_service::State::Error(_) | matrix_sdk_ui::sync_service::State::Offline => {
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
                        sync_service.stop().await;

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

        let user_session: UserSession =
            self.client.oauth().user_session().expect("A logged in client has a session");
        session.user_session = user_session;

        let serialized_session = serde_json::to_string(&session)?;
        fs::write(&self.session_file, serialized_session).await?;

        println!("Updating the stored session: done!");
        Ok(())
    }

    /// Refresh the access token.
    async fn refresh_token(&self) -> anyhow::Result<()> {
        self.client.oauth().refresh_access_token().await?;

        // The session will automatically be refreshed because of the task persisting
        // the full session upon refresh in `setup_background_save`.

        println!("\nToken refreshed successfully");

        Ok(())
    }

    /// Log out from this session.
    async fn logout(&self) -> anyhow::Result<()> {
        // Log out via OAuth 2.0.
        self.client.logout().await?;

        // Delete the stored session and database.
        let data_dir = self.session_file.parent().expect("The file has a parent directory");
        fs::remove_dir_all(data_dir).await?;

        println!("\nLogged out successfully");
        println!("\nExiting…");

        Ok(())
    }
}

/// Build a new client.
///
/// Returns the client and the data required to restore the client.
async fn build_client(data_dir: &Path) -> anyhow::Result<(Client, ClientSession)> {
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
                // Check if the homeserver advertises OAuth 2.0 server metadata.
                match client.oauth().server_metadata().await {
                    Ok(server_metadata) => {
                        println!(
                            "Found OAuth 2.0 server metadata with issuer: {}",
                            server_metadata.issuer
                        );

                        let homeserver = client.homeserver().to_string();
                        return Ok((client, ClientSession { homeserver, db_path, passphrase }));
                    }
                    Err(error) => {
                        if error.is_not_supported() {
                            println!(
                                "This homeserver doesn't advertise OAuth 2.0 server metadata."
                            );
                        } else {
                            println!("Error fetching the OAuth 2.0 server metadata: {error:?}");
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

/// Generate the OAuth 2.0 client metadata.
fn client_metadata() -> Raw<ClientMetadata> {
    // Native clients should be able to register the IPv4 and IPv6 loopback
    // interfaces and then point to any port when needing a redirect URI. An
    // alternative is to use a custom URI scheme registered with the OS.
    let ipv4_localhost_uri = Url::parse(&format!("http://{}/", Ipv4Addr::LOCALHOST))
        .expect("Couldn't parse IPv4 redirect URI");
    let ipv6_localhost_uri = Url::parse(&format!("http://[{}]/", Ipv6Addr::LOCALHOST))
        .expect("Couldn't parse IPv6 redirect URI");
    let client_uri = Localized::new(
        Url::parse("https://github.com/matrix-org/matrix-rust-sdk")
            .expect("Couldn't parse client URI"),
        None,
    );

    let metadata = ClientMetadata {
        // The following fields should be displayed in the OAuth 2.0 authorization server's
        // web UI as part of the process to get the user's consent. It means that these
        // should contain real data so the user can make sure that they allow the proper
        // application. We are cheating here because this is an example.
        client_name: Some(Localized::new("matrix-rust-sdk-oauth-cli".to_owned(), [])),
        policy_uri: Some(client_uri.clone()),
        tos_uri: Some(client_uri.clone()),
        ..ClientMetadata::new(
            // This is a native application (in contrast to a web application, that runs in a
            // browser).
            ApplicationType::Native,
            // We are going to use the Authorization Code flow.
            vec![OAuthGrantType::AuthorizationCode {
                redirect_uris: vec![ipv4_localhost_uri, ipv6_localhost_uri],
            }],
            client_uri,
        )
    };

    Raw::new(&metadata).expect("Couldn't serialize client metadata")
}

/// Open the authorization URL and wait for it to be complete.
///
/// Returns the code to obtain the access token.
async fn use_auth_url(url: &Url, server_handle: LocalServerRedirectHandle) -> Option<QueryString> {
    println!("\nPlease authenticate yourself at: {url}\n");
    println!("Then proceed to the authorization.\n");

    server_handle.await
}

/// Handle room messages.
async fn on_room_message(event: OriginalSyncRoomMessageEvent, room: Room) {
    // We only want to log text messages in joined rooms.
    if room.state() != RoomState::Joined {
        return;
    }
    let MessageType::Text(text_content) = &event.content.msgtype else { return };

    let room_name = match room.display_name().await {
        Ok(room_name) => room_name.to_string(),
        Err(error) => {
            println!("Error getting room display name: {error}");
            // Let's fallback to the room ID.
            room.room_id().to_string()
        }
    };

    println!("[{room_name}] {}: {}", event.sender, text_content.body)
}
