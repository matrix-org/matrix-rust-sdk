use std::{env, process::exit};

use matrix_sdk::{
    Client, HttpError, Result as MatrixResult, RumaApiError,
    reqwest::StatusCode,
    ruma::{
        OwnedMxcUri, UserId,
        api::{
            client::profile::{self, AvatarUrl, DisplayName},
            error::FromHttpResponseError,
        },
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
/// Spec: <https://spec.matrix.org/latest/client-server-api/#get_matrixclientv3profileuserid>
/// Ruma: <https://docs.rs/ruma-client-api/latest/ruma_client_api/profile/get_profile/v3/index.html>
/// The Matrix spec does not require authentication for this endpooint. However, some server configurations (e.g. Synapse's
/// `require_auth_for_profile_requests`) enfonce auth to prevent user enumeration, which will cause `client.send()` to
/// return a 401 error.
async fn get_profile(client: Client, mxid: &UserId) -> MatrixResult<UserProfile> {
    // First construct the request you want to make
    // See https://docs.rs/ruma-client-api/latest/ruma_client_api/index.html for all available Endpoints
    let request = profile::get_profile::v3::Request::new(mxid.to_owned());

    // Start the request using matrix_sdk::Client::send
    // To avoid having to deal with auth errors, you can also use account().fetch_user_profile() which handles auth correctly
    let resp = client.send(request).await?;

    // Use the response and construct a UserProfile struct.
    // See https://docs.rs/ruma-client-api/latest/ruma_client_api/profile/get_profile/v3/struct.Response.html
    // for details on the Response for this Request
    let user_profile = UserProfile {
        avatar_url: resp.get_static::<AvatarUrl>()?,
        displayname: resp.get_static::<DisplayName>()?,
    };
    Ok(user_profile)
}

// Helper function to avoid having a lot of nested errors in main() when trying to get profile.
fn is_auth_error(e: &matrix_sdk::Error) -> bool {
    if let matrix_sdk::Error::Http(http_err) = e
        && let HttpError::Api(resp_err) = http_err.as_ref()
        && let FromHttpResponseError::Server(ruma_err) = resp_err.as_ref()
        && let RumaApiError::ClientApi(inner_err) = ruma_err
    {
        return inner_err.status_code == StatusCode::UNAUTHORIZED;
    }
    false
}

/// This function calls the GET profile endpoint using the authenticated client. It should succeed even if the server requires auth for profile requests.
async fn get_profile_authenticated(client: Client) -> MatrixResult<UserProfile> {
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

    // parse the command line for homeserver, username and password
    let (Some(homeserver_url), Some(username), Some(password)) =
        (env::args().nth(1), env::args().nth(2), env::args().nth(3))
    else {
        eprintln!("Usage: {} <homeserver_url> <mxid> <password>", env::args().next().unwrap());
        exit(1)
    };

    let client = login(homeserver_url, &username, &password).await?;

    let user_id = UserId::parse(username).expect("Couldn't parse the MXID");
    let profile = match get_profile(client.clone(), &user_id).await {
        Ok(profile) => profile,
        Err(e) => {
            if is_auth_error(&e) {
                eprintln!(
                    "Authentication error: {e}. Check if the server requires authentication for profile requests. Trying to fetch profile using the authenticated client instead..."
                );
                get_profile_authenticated(client).await?
            } else {
                eprintln!("Error fetching profile: {e}");
                UserProfile { avatar_url: None, displayname: None }
            }
        }
    };
    
    // get_profile(client, &user_id).await?;
    println!("{profile:#?}");
    Ok(())
}
