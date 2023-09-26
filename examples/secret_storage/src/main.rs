use anyhow::Result;
use clap::{Parser, Subcommand};
use matrix_sdk::{
    encryption::secret_storage::SecretStore,
    matrix_auth::{MatrixSession, MatrixSessionTokens},
    ruma::{events::secret::request::SecretName, OwnedDeviceId, OwnedUserId},
    AuthSession, Client, SessionMeta,
};
use url::Url;

/// A command line example showcasing how the secret storage support works in
/// the Matrix Rust SDK.
///
/// Secret storage is an account data backed encrypted key/value store. You can
/// put or get secrets from the store.
#[derive(Parser, Debug)]
struct Cli {
    /// The homeserver to connect to.
    #[clap(value_parser)]
    homeserver: Url,

    /// The user ID that should be used to restore the session.
    #[clap(value_parser)]
    user_id: OwnedUserId,

    /// The user name that should be used for the login.
    #[clap(value_parser)]
    device_id: OwnedDeviceId,

    /// The password that should be used for the login.
    #[clap(value_parser)]
    access_token: String,

    /// Set the proxy that should be used for the connection.
    #[clap(short, long)]
    proxy: Option<Url>,

    /// Enable verbose logging output.
    #[clap(short, long, action)]
    verbose: bool,

    /// The secret storage key, this key will be used to open the secret-store.
    #[clap(long, action)]
    secret_store_key: String,

    /// The sub-command to run.
    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Retrieve a secret from the homeserver.
    GetSecret { secret_name: SecretName },
    /// Upload a secret to the homeserver.
    SetSecret { secret_name: SecretName, secret: String },
    /// Import all known and specced secrets from the secret store into the
    /// local database.
    ///
    /// **Note**: This command won't strictly do the right thing, as we are
    /// reusing a device ID and access token from a different device. It will
    /// import the secrets correctly, but it will sign device keys which don't
    /// belong to the provided device ID.
    ImportKnownSecrets,
}

async fn get_secret(secret_store: SecretStore, secret_name: SecretName) -> Result<()> {
    let secret = secret_store.get_secret(secret_name.to_owned()).await?;

    if let Some(secret) = secret {
        println!("Secret: {secret}");
    } else {
        println!("No secret with the name {secret_name} found")
    }

    Ok(())
}

async fn set_secret(
    secret_store: SecretStore,
    secret_name: SecretName,
    secret: &str,
) -> Result<()> {
    secret_store.put_secret(secret_name.to_owned(), secret).await?;

    println!("Secret {secret_name} was successfully encrypted and stored on the homeserver");

    Ok(())
}

async fn import_known_secrets(client: Client, secret_store: SecretStore) -> Result<()> {
    secret_store.import_secrets().await?;

    let status = client
        .encryption()
        .cross_signing_status()
        .await
        .expect("We should be able to get our cross-signing status");

    if status.is_complete() {
        println!("Successfully imported all the cross-signing keys");
    } else {
        eprintln!("Couldn't import all the cross-signing keys: {status:?}");
    }

    Ok(())
}

async fn restore_client(cli: &Cli) -> Result<Client> {
    let builder = Client::builder().homeserver_url(&cli.homeserver);

    let builder = if let Some(proxy) = cli.proxy.as_ref() { builder.proxy(proxy) } else { builder };
    let client = builder.build().await?;

    // TODO: We should be able to get the device id from `/whoami`.
    let session = AuthSession::Matrix(MatrixSession {
        meta: SessionMeta { user_id: cli.user_id.to_owned(), device_id: cli.device_id.to_owned() },
        tokens: MatrixSessionTokens {
            access_token: cli.access_token.to_owned(),
            refresh_token: None,
        },
    });

    client.restore_session(session).await?;

    Ok(client)
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    if cli.verbose {
        tracing_subscriber::fmt::init();
    }

    let client = restore_client(&cli).await?;
    let secret_store =
        client.encryption().secret_storage().open_secret_store(&cli.secret_store_key).await?;

    match cli.command {
        Commands::GetSecret { secret_name } => get_secret(secret_store, secret_name).await,
        Commands::SetSecret { secret_name, secret } => {
            set_secret(secret_store, secret_name, &secret).await
        }
        Commands::ImportKnownSecrets => import_known_secrets(client, secret_store).await,
    }
}
