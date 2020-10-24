use std::{
    collections::BTreeMap,
    env, io,
    process::exit,
    sync::atomic::{AtomicBool, Ordering},
};

use serde_json::json;
use url::Url;

use matrix_sdk::{
    self,
    api::r0::uiaa::AuthData,
    identifiers::{user_id, UserId},
    Client, ClientConfig, LoopCtrl, SyncSettings,
};

fn auth_data<'a>(user: &UserId, password: &str, session: Option<&'a str>) -> AuthData<'a> {
    let mut auth_parameters = BTreeMap::new();
    let identifier = json!({
        "type": "m.id.user",
        "user": user,
    });

    auth_parameters.insert("identifier".to_owned(), identifier);
    auth_parameters.insert("password".to_owned(), password.to_owned().into());

    // This is needed because of https://github.com/matrix-org/synapse/issues/5665
    auth_parameters.insert("user".to_owned(), user.as_str().into());

    AuthData::DirectRequest {
        kind: "m.login.password",
        auth_parameters,
        session,
    }
}

async fn bootstrap(client: Client) {
    println!("Bootstrapping a new cross signing identity, press enter to continue.");

    let mut input = String::new();

    io::stdin()
        .read_line(&mut input)
        .expect("error: unable to read user input");

    if let Err(e) = client.bootstrap_cross_signing(None).await {
        if let Some(response) = e.uiaa_response() {
            let auth_data = auth_data(
                &user_id!("@example:localhost"),
                "wordpass",
                response.session.as_deref(),
            );
            client
                .bootstrap_cross_signing(Some(auth_data))
                .await
                .expect("Couldn't bootstrap cross signing")
        } else {
            panic!("Error durign cross signing bootstrap {:#?}", e);
        }
    }
}

async fn login(
    homeserver_url: String,
    username: &str,
    password: &str,
) -> Result<(), matrix_sdk::Error> {
    let client_config = ClientConfig::new()
        .disable_ssl_verification()
        .proxy("http://localhost:8080")
        .unwrap();
    let homeserver_url = Url::parse(&homeserver_url).expect("Couldn't parse the homeserver URL");
    let client = Client::new_with_config(homeserver_url, client_config).unwrap();

    client
        .login(username, password, None, Some("rust-sdk"))
        .await?;

    let client_ref = &client;
    let asked = AtomicBool::new(false);
    let asked_ref = &asked;

    client
        .sync_with_callback(SyncSettings::new(), |_| async move {
            let asked = asked_ref;
            let client = &client_ref;

            // Wait for sync to be done then ask the user to bootstrap.
            if !asked.load(Ordering::SeqCst) {
                tokio::spawn(bootstrap((*client).clone()));
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
