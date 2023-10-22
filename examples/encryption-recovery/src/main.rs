use std::io::Write;

use anyhow::Result;
use clap::Parser;
use futures_util::StreamExt;
use matrix_sdk::{
    config::SyncSettings,
    encryption::{backups::BackupState, recovery::RecoveryState, EncryptionSettings},
    Client,
};
use tokio::io::{AsyncBufReadExt, BufReader};
use url::Url;
use zeroize::Zeroize;

/// A command line example showcasing how the encryption recovery works in the
/// Matrix Rust SDK.
#[derive(Parser, Debug)]
struct Cli {
    /// The homeserver to connect to.
    #[clap(value_parser)]
    homeserver: Url,

    /// The user ID that should be used to restore the session.
    #[clap(value_parser)]
    user_name: String,

    /// The password that should be used for the login.
    #[clap(value_parser)]
    password: String,

    /// Set the proxy that should be used for the connection.
    #[clap(short, long)]
    proxy: Option<Url>,

    /// Enable verbose logging output.
    #[clap(short, long, action)]
    verbose: bool,
}

#[derive(Parser, Debug)]
#[command(no_binary_name = true)]
enum Command {
    /// Disable backups and recovery, whatever the later means.
    Disable,
    /// Enable backups, don't wait for the backup to be done.
    EnableBackup,
    /// Change the recovery key, we generate a new one and present the base58
    /// string to the user.
    ChangeRecoveryKey {
        /// The passphrase, which can be used to recover, in addition to the
        /// recovery key.
        #[clap(long, action)]
        passphrase: Option<String>,
    },
    /// Logout, if recovery isn't enabled, ask the user if they want to do so
    /// now.
    Logout,
    /// Logout without setting up recovery, still wait for room keys to be
    /// uploaded.
    LogoutNoRecovery,
    Recover {
        /// The recovery key, AKA the secret storage key, this key will be used
        /// to open the secret-store. Not to be confused with the
        /// Recovery key from the spec.
        #[clap(long, action)]
        recovery_key: String,
    },
}

async fn recover(client: &Client, recovery_key: &str) -> Result<()> {
    // You see how this name isn't really fitting?
    client.encryption().recovery().recover(recovery_key).await?;

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

async fn login(cli: &Cli) -> Result<Client> {
    let encryption_settings = EncryptionSettings {
        auto_enable_cross_signing: true,
        auto_download_from_backup: true,
        auto_enable_backups: true,
    };

    let builder = Client::builder()
        .homeserver_url(&cli.homeserver)
        .with_encryption_settings(encryption_settings);

    let builder = if let Some(proxy) = &cli.proxy { builder.proxy(proxy) } else { builder };

    let client = builder.build().await?;

    client
        .matrix_auth()
        .login_username(&cli.user_name, &cli.password)
        .initial_device_display_name("rust-sdk")
        .await?;

    Ok(client)
}

async fn listen_for_backup_state_changes(client: Client) {
    let mut stream = client.encryption().backups().state_stream();

    while let Some(state) = stream.next().await {
        let Ok(state) = state else {
            panic!("Receive error while waiting for updates to the backup state")
        };

        match state {
            BackupState::Unknown => (),
            BackupState::Enabling => println!("Trying to enable backups"),
            BackupState::Resuming => println!("Trying to resume backups"),
            BackupState::Enabled => println!("Successfully enabled backups"),
            BackupState::Downloading => println!("Downloading the room keys from the backup"),
            BackupState::Disabling => println!("Disabling the backup"),
            BackupState::Disabled => println!("Backup successfully disabled"),
            BackupState::Creating => println!("Trying to create a new backup"),
        }
    }
}

async fn listen_for_recovery_state_changes(client: Client) {
    let mut stream = client.encryption().recovery().state_stream();

    while let Some(state) = stream.next().await {
        match state {
            RecoveryState::Unknown => println!("The recovery state is now unknown"),
            RecoveryState::Enabled => println!("Recovery is now enabled"),
            RecoveryState::Disabled => println!("Recovery is now disabled"),
            RecoveryState::Incomplete => println!("Recovery is now incomplete"),
        }
    }
}

async fn logout(client: &Client) -> Result<()> {
    let recovery = client.encryption().recovery();

    if recovery.are_we_the_last_man_standing().await? {
        let enable_recovery = recovery.enable().wait_for_backups_to_upload();

        let mut progress = enable_recovery.subscribe_to_progress();

        let task = tokio::spawn(async move {
            while let Some(update) = progress.next().await {
                println!("Hello world {update:?}");
            }
        });

        let recovery_key = enable_recovery.await?;
        println!("Successfully created a recovery key: `{recovery_key}`.");

        task.abort();
    }

    client.logout().await?;

    std::process::exit(0);
}

async fn logout_no_recovery(client: &Client) -> Result<()> {
    let backups = client.encryption().backups();

    let wait = backups.wait_for_steady_state();
    let mut progress = wait.subscribe_to_progress();

    let task = tokio::spawn(async move {
        while let Some(update) = progress.next().await {
            println!("Hello world {update:?}");
            std::io::stdout().flush().expect("Unable to write to stdout");
        }
    });

    println!("Waiting for the room keys to upload.");

    if let Err(e) = wait.await {
        println!("Error waiting for room keys to upload {e:?}");
    }

    task.abort();

    client.logout().await?;
    println!("Successfully logged out. Exiting...");
    std::io::stdout().flush().expect("Unable to write to stdout");

    std::process::exit(0);
}

async fn enable_backup(client: &Client) -> Result<()> {
    let recovery = client.encryption().recovery();
    let enable_backup = recovery.enable_backup();

    enable_backup.await?;

    println!("Successfully enabled backups.");

    Ok(())
}

async fn disable(client: &Client) -> Result<()> {
    client.encryption().recovery().disable().await?;

    println!("Successfully disable recovery.");

    Ok(())
}

async fn reset_key(client: &Client, passphrase: Option<&str>) -> Result<()> {
    // TODO: Check if we should be able to reset the key and then ask for the
    // existing one if we're not supposed to.
    let mut recovery_key = if let Some(passphrase) = passphrase {
        client.encryption().recovery().reset_key().with_passphrase(passphrase).await?
    } else {
        client.encryption().recovery().reset_key().await?
    };

    println!("Successfully changed the recovery key, new key: `{recovery_key}`.");
    recovery_key.zeroize();

    Ok(())
}

async fn run_command(client: &Client, command: Command) -> Result<()> {
    // TODO: Convert this into a proper REPL, perhaps even a status at the top would
    // be nice.
    match command {
        Command::Disable => disable(client).await,
        Command::EnableBackup => enable_backup(client).await,
        Command::ChangeRecoveryKey { mut passphrase } => {
            let ret = reset_key(client, passphrase.as_deref()).await;
            passphrase.zeroize();
            ret
        }
        Command::Recover { mut recovery_key } => {
            let ret = recover(client, &recovery_key).await;
            recovery_key.zeroize();

            ret
        }
        Command::Logout => logout(client).await,
        Command::LogoutNoRecovery => logout_no_recovery(client).await,
    }
}

async fn get_command(client: Client) -> Result<()> {
    let stdin = tokio::io::stdin();
    let reader = BufReader::new(stdin);

    let mut lines = reader.lines();

    while let Some(line) = lines.next_line().await? {
        let line = line.split_whitespace();

        match Command::try_parse_from(line) {
            Ok(command) => run_command(&client, command).await?,
            Err(e) => println!("{e}"),
        }
    }

    Ok(())
}

async fn sync(client: Client) {
    loop {
        if let Err(e) = client.sync(SyncSettings::new()).await {
            eprintln!("Error syncing, what the fuck is going on with this synapse {e:?}")
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    if cli.verbose {
        tracing_subscriber::fmt::init();
    }

    let client = login(&cli).await?;

    client.sync_once(Default::default()).await?;

    let _task = tokio::spawn({
        let client = client.to_owned();
        async move { listen_for_backup_state_changes(client.to_owned()).await }
    });

    let _recovery_state_task = tokio::spawn({
        let client = client.to_owned();
        async move { listen_for_recovery_state_changes(client.to_owned()).await }
    });

    let _sync_task = tokio::spawn({
        let client = client.to_owned();
        async move { sync(client).await }
    });

    get_command(client).await?;

    Ok(())
}
