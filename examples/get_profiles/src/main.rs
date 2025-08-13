use std::{env, process::exit};

use matrix_sdk::{
    Client, Result as MatrixResult,
    ruma::{
        OwnedMxcUri, UserId,
        api::client::profile::{self, AvatarUrl, DisplayName},
    },
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
    let request = profile::get_profile::v3::Request::new(mxid.to_owned());

    // Start the request using matrix_sdk::Client::send
    let resp = client.send(request).await?;

    // Use the response and construct a UserProfile struct.
    // See https://docs.rs/ruma-client-api/0.9.0/ruma_client_api/r0/profile/get_profile/struct.Response.html
    // for details on the Response for this Request
    let user_profile = UserProfile {
        avatar_url: resp.get_static::<AvatarUrl>()?,
        displayname: resp.get_static::<DisplayName>()?,
    };
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
        .matrix_auth()
        .login_username(username, password)
        .initial_device_display_name("rust-sdk")
        .await?;

    Ok(client)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    // parse the command line for homeserver, username and password
    let (Some(homeserver_url), Some(username), Some(password)) =
        (env::args().nth(1), env::args().nth(2), env::args().nth(3))
    else {
        eprintln!("Usage: {} <homeserver_url> <mxid> <password>", env::args().next().unwrap());
        exit(1)
    };

    let client = login(homeserver_url, &username, &password).await?;

    let user_id = UserId::parse(username).expect("Couldn't parse the MXID");
    let profile = get_profile(client, &user_id).await?;
    println!("{profile:#?}");
    Ok(())
}
