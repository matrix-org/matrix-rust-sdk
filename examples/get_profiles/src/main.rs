use std::{env, process::exit};

use matrix_sdk::{
    Client, Result as MatrixResult,
    ruma::{
        OwnedMxcUri,
        api::client::profile::{AvatarUrl, DisplayName},
    },
};
use url::Url;

#[derive(Debug)]
#[allow(dead_code)]
struct UserProfile {
    avatar_url: Option<OwnedMxcUri>,
    displayname: Option<String>,
}

/// Fetch the profile of the currently logged-in user via
/// Account::fetch_user_profile.
///
/// This uses the high-level Account API which automatically handles
/// authentication headers, avoiding 401 errors on hardened homeservers.
///
/// See also: Account::fetch_user_profile_of for fetching another user's
/// profile.
async fn get_profile(client: &Client) -> MatrixResult<UserProfile> {
    let resp = client.account().fetch_user_profile().await?;

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

    let (Some(homeserver_url), Some(username), Some(password)) =
        (env::args().nth(1), env::args().nth(2), env::args().nth(3))
    else {
        eprintln!("Usage: {} <homeserver_url> <username> <password>", env::args().next().unwrap());
        exit(1)
    };

    let client = login(homeserver_url, &username, &password).await?;

    let profile = get_profile(&client).await?;
    println!("{profile:#?}");
    Ok(())
}
