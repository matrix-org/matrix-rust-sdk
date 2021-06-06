use std::{
    collections::BTreeMap,
    env, io,
    process::exit,
    sync::atomic::{AtomicBool, Ordering},
};

use matrix_sdk::{
    self, api::r0::uiaa::AuthData, identifiers::UserId, Client, LoopCtrl, SyncSettings,
};
use serde_json::json;
use url::Url;

fn auth_data<'a>(user: &UserId, password: &str, session: Option<&'a str>) -> AuthData<'a> {
    let mut auth_parameters = BTreeMap::new();
    let identifier = json!({
        "type": "m.id.user",
        "user": user,
    });

    auth_parameters.insert("identifier".to_owned(), identifier);
    auth_parameters.insert("password".to_owned(), password.to_owned().into());

    AuthData::DirectRequest { kind: "m.login.password", auth_parameters, session }
}

async fn bootstrap(client: Client, user_id: UserId, password: String) {
    println!("Bootstrapping a new cross signing identity, press enter to continue.");

    let mut input = String::new();

    io::stdin().read_line(&mut input).expect("error: unable to read user input");

    #[cfg(feature = "encryption")]
    if let Err(e) = client.bootstrap_cross_signing(None).await {
        if let Some(response) = e.uiaa_response() {
            let auth_data = auth_data(&user_id, &password, response.session.as_deref());
            client
                .bootstrap_cross_signing(Some(auth_data))
                .await
                .expect("Couldn't bootstrap cross signing")
        } else {
            panic!("Error during cross-signing bootstrap {:#?}", e);
        }
    }

    #[cfg(not(feature = "encryption"))]
    panic!("Cross signing requires the encryption feature to be enabled");
}

async fn login(
    homeserver_url: String,
    username: &str,
    password: &str,
) -> Result<(), matrix_sdk::Error> {
    let homeserver_url = Url::parse(&homeserver_url).expect("Couldn't parse the homeserver URL");
    let client = Client::new(homeserver_url).unwrap();

    let response = client.login(username, password, None, Some("rust-sdk")).await?;

    let user_id = &response.user_id;
    let client_ref = &client;
    let asked = AtomicBool::new(false);
    let asked_ref = &asked;

    client
        .sync_with_callback(SyncSettings::new(), |_| async move {
            let asked = asked_ref;
            let client = &client_ref;
            let user_id = &user_id;
            let password = &password;

            // Wait for sync to be done then ask the user to bootstrap.
            if !asked.load(Ordering::SeqCst) {
                tokio::spawn(bootstrap(
                    (*client).clone(),
                    (*user_id).clone(),
                    password.to_string(),
                ));
            }

            asked.store(true, Ordering::SeqCst);
            LoopCtrl::Continue
        })
        .await;

    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), matrix_sdk::Error> {
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

    login(homeserver_url, &username, &password).await
}
