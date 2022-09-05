use std::{env, process::exit};

use matrix_sdk::{
    ruma::{api::client::profile, OwnedMxcUri, UserId},
    Client, Result as MatrixResult,
};
use url::Url;

#[derive(Debug)]
#[allow(dead_code)]
struct UserProfile {
    avatar_url: Option<OwnedMxcUri>,
    displayname: Option<String>,
}

/// This function calls the GET profile endpoint
/// Spec: <https://matrix.org/docs/spec/client_server/r0.6.1#get-matrix-client-r0-profile-userid>
/// Ruma: <https://docs.rs/ruma-client-api/0.9.0/ruma_client_api/r0/profile/get_profile/index.html>
async fn get_profile(client: Client, mxid: &UserId) -> MatrixResult<UserProfile> {
    // First construct the request you want to make
    // See https://docs.rs/ruma-client-api/0.9.0/ruma_client_api/index.html for all available Endpoints
    let request = profile::get_profile::v3::Request::new(mxid);

    // Start the request using matrix_sdk::Client::send
    let resp = client.send(request, None).await?;

    // Use the response and construct a UserProfile struct.
    // See https://docs.rs/ruma-client-api/0.9.0/ruma_client_api/r0/profile/get_profile/struct.Response.html
    // for details on the Response for this Request
    let user_profile = UserProfile { avatar_url: resp.avatar_url, displayname: resp.displayname };
    Ok(user_profile)
}

async fn login(
    homeserver_url: String,
    username: &str,
    password: &str,
) -> matrix_sdk::Result<Client> {
    let homeserver_url = Url::parse(&homeserver_url).expect("Couldn't parse the homeserver URL");
    let client = Client::new(homeserver_url).await.unwrap();

    client
        .login_username(username, password)
        .initial_device_display_name("rust-sdk")
        .send()
        .await?;

    Ok(client)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
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

    let client = login(homeserver_url, &username, &password).await?;

    let user_id = UserId::parse(username).expect("Couldn't parse the MXID");
    let profile = get_profile(client, &user_id).await?;
    println!("{:#?}", profile);
    Ok(())
}
