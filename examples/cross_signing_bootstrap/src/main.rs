use std::{
    env, io,
    process::exit,
    sync::atomic::{AtomicBool, Ordering},
};

use anyhow::Result;
use matrix_sdk::{
    Client, LoopCtrl,
    config::SyncSettings,
    encryption::CrossSigningResetAuthType,
    ruma::{OwnedUserId, api::client::uiaa},
};
use url::Url;

async fn bootstrap(client: Client, user_id: OwnedUserId, password: String) -> Result<()> {
    println!("Bootstrapping a new cross signing identity, press enter to continue.");

    let mut input = String::new();

    io::stdin().read_line(&mut input).expect("error: unable to read user input");

    if let Some(handle) = client.encryption().reset_cross_signing().await? {
        match handle.auth_type() {
            CrossSigningResetAuthType::Uiaa(uiaa) => {
                let mut password = uiaa::Password::new(user_id.into(), password);
                password.session = uiaa.session.clone();
                handle.auth(Some(uiaa::AuthData::Password(password))).await?;
            }
            CrossSigningResetAuthType::OAuth(oauth) => {
                println!(
                    "To reset your end-to-end encryption cross-signing identity, \
                    you first need to approve it at {}",
                    oauth.approval_url
                );
                handle.auth(None).await?;
            }
        }
    }

    Ok(())
}

async fn login(homeserver_url: String, username: &str, password: &str) -> matrix_sdk::Result<()> {
    let homeserver_url = Url::parse(&homeserver_url).expect("Couldn't parse the homeserver URL");
    let client = Client::new(homeserver_url).await.unwrap();

    let response = client
        .matrix_auth()
        .login_username(username, password)
        .initial_device_display_name("rust-sdk")
        .await?;

    let user_id = &response.user_id;
    let client_ref = &client;
    let asked = AtomicBool::new(false);
    let asked_ref = &asked;

    client
        .sync_with_callback(SyncSettings::new(), |_| async move {
            let asked = asked_ref;
            let client = &client_ref;
            let user_id = &user_id;

            // Wait for sync to be done then ask the user to bootstrap.
            if !asked.load(Ordering::SeqCst) {
                tokio::spawn(bootstrap((*client).clone(), (*user_id).clone(), password.to_owned()));
            }

            asked.store(true, Ordering::SeqCst);
            LoopCtrl::Continue
        })
        .await?;

    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    let (Some(homeserver_url), Some(username), Some(password)) =
        (env::args().nth(1), env::args().nth(2), env::args().nth(3))
    else {
        eprintln!("Usage: {} <homeserver_url> <username> <password>", env::args().next().unwrap());
        exit(1)
    };

    login(homeserver_url, &username, &password).await?;

    Ok(())
}
