use std::{
    env, io,
    process::exit,
    sync::atomic::{AtomicBool, Ordering},
};

use matrix_sdk::{config::SyncSettings, ruma::OwnedUserId, Client, LoopCtrl};
use url::Url;

async fn bootstrap(client: Client, user_id: OwnedUserId, password: String) {
    println!("Bootstrapping a new cross signing identity, press enter to continue.");

    let mut input = String::new();

    io::stdin().read_line(&mut input).expect("error: unable to read user input");

    if let Err(e) = client.encryption().bootstrap_cross_signing(None).await {
        use matrix_sdk::ruma::api::client::uiaa;

        if let Some(response) = e.as_uiaa_response() {
            let mut password = uiaa::Password::new(user_id.into(), password);
            password.session = response.session.clone();

            client
                .encryption()
                .bootstrap_cross_signing(Some(uiaa::AuthData::Password(password)))
                .await
                .expect("Couldn't bootstrap cross signing")
        } else {
            panic!("Error during cross-signing bootstrap {e:#?}");
        }
    }
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
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    let (homeserver_url, username, password) =
        match (env::args().nth(1), env::args().nth(2), env::args().nth(3)) {
            (Some(a), Some(b), Some(c)) => (a, b, c),
            _ => {
                eprintln!(
                    "Usage: {} <homeserver_url> <username> <password>",
                    env::args().next().unwrap()
                );
                exit(1)
            }
        };

    login(homeserver_url, &username, &password).await?;

    Ok(())
}
