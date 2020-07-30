use std::{convert::TryFrom, env, process::exit};

use url::Url;

use matrix_sdk::{
    self, api::r0::profile, identifiers::UserId, Client, ClientConfig, Result as MatrixResult,
};

#[derive(Debug)]
struct UserProfile {
    avatar_url: Option<String>,
    displayname: Option<String>,
}

/// This function calls the GET profile endpoint
/// Spec: https://matrix.org/docs/spec/client_server/r0.6.1#get-matrix-client-r0-profile-userid
/// Ruma: https://docs.rs/ruma-client-api/0.9.0/ruma_client_api/r0/profile/get_profile/index.html
async fn get_profile(client: Client, mxid: UserId) -> MatrixResult<UserProfile> {
    // First construct the request you want to make
    // See https://docs.rs/ruma-client-api/0.9.0/ruma_client_api/index.html for all available Endpoints
    let request = profile::get_profile::Request {
        user_id: mxid.clone(),
    };

    // Start the request using matrix_sdk::Client::send
    let resp = client.send(request).await?;

    // Use the response and construct a UserProfile struct.
    // See https://docs.rs/ruma-client-api/0.9.0/ruma_client_api/r0/profile/get_profile/struct.Response.html
    // for details on the Response for this Request
    let user_profile = UserProfile {
        avatar_url: resp.avatar_url,
        displayname: resp.displayname,
    };
    Ok(user_profile)
}

async fn login(
    homeserver_url: String,
    username: String,
    password: String,
) -> Result<Client, matrix_sdk::Error> {
    let client_config = ClientConfig::new()
        .proxy("http://localhost:8080")?
        .disable_ssl_verification();
    let homeserver_url = Url::parse(&homeserver_url).expect("Couldn't parse the homeserver URL");
    let client = Client::new_with_config(homeserver_url, client_config).unwrap();

    client
        .login(username, password, None, Some("rust-sdk".to_string()))
        .await?;

    Ok(client)
}

#[tokio::main]
async fn main() -> Result<(), matrix_sdk::Error> {
    tracing_subscriber::fmt::init();

    let (homeserver_url, username, password) =
        match (env::args().nth(1), env::args().nth(2), env::args().nth(3)) {
            (Some(a), Some(b), Some(c)) => (a, b, c),
            _ => {
                eprintln!(
                    "Usage: {} <homeserver_url> <mxid> <password>",
                    env::args().next().unwrap()
                );
                exit(1)
            }
        };

    let client = login(homeserver_url, username.clone(), password).await?;

    let user_id = UserId::try_from(username.clone()).expect("Couldn't parse the MXID");
    let profile = get_profile(client, user_id).await?;
    println!("{:#?}", profile);
    Ok(())
}
