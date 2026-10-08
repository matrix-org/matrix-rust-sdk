use std::env;

use anyhow::{Result, bail};
use futures_util::StreamExt;
use matrix_sdk::{
    Client,
    authentication::oauth::{
        DeviceCodeLoginProgress,
        registration::{ApplicationType, ClientMetadata, Localized, OAuthGrantType},
    },
    ruma::serde::Raw,
};
use url::Url;

/// A minimal example showcasing how to log in with the OAuth 2.0 Device
/// Authorization Grant ([RFC 8628]).
///
/// This login method is meant for clients that cannot open a browser, like
/// bots, bridges or command-line applications. The user needs to open the
/// printed URL on another device to approve the login.
///
/// Usage: `cargo run -p example-oauth-device-code -- <homeserver_url>`
///
/// [RFC 8628]: https://datatracker.ietf.org/doc/html/rfc8628
#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    let Some(homeserver) = env::args().nth(1) else {
        bail!("Usage: {} <homeserver_url>", env::args().next().unwrap_or_default());
    };

    let client = Client::builder().server_name_or_homeserver_url(homeserver).build().await?;
    let oauth = client.oauth();

    let login = oauth.login_with_device_code(None, Some(client_metadata().into()), None);
    let mut progress = login.subscribe_to_progress();

    // Show the verification URI and the user code while we wait for the user to
    // approve the login.
    let progress_task = tokio::spawn(async move {
        while let Some(state) = progress.next().await {
            if let DeviceCodeLoginProgress::WaitingForToken {
                verification_uri,
                verification_uri_complete,
                user_code,
                expires_in,
            } = state
            {
                match verification_uri_complete {
                    Some(uri) => println!("To log in, open {uri}"),
                    None => println!("To log in, open {verification_uri}"),
                }
                println!("and confirm the code {user_code}.");
                println!("The code expires in {} seconds.", expires_in.as_secs());
            }
        }
    });

    let result = login.await;
    progress_task.abort();
    result?;

    let user_id = client.user_id().expect("we should be logged in");
    let device_id = client.device_id().expect("we should be logged in");
    println!("Logged in as {user_id} with device {device_id}");

    // The session can be persisted with `client.oauth().full_session()` and
    // restored later with `client.oauth().restore_session()`.

    Ok(())
}

/// Generate the OAuth 2.0 client metadata.
fn client_metadata() -> Raw<ClientMetadata> {
    let client_uri = Localized::new(
        Url::parse("https://github.com/matrix-org/matrix-rust-sdk")
            .expect("Couldn't parse client URI"),
        None,
    );

    let metadata = ClientMetadata {
        // This should be displayed in the authorization server's web UI to ask
        // for the user's consent, so it should contain real data.
        client_name: Some(Localized::new("matrix-rust-sdk-device-code".to_owned(), None)),
        policy_uri: Some(client_uri.clone()),
        tos_uri: Some(client_uri.clone()),
        ..ClientMetadata::new(
            // This is a native application, in contrast to a web application
            // that runs in a browser.
            ApplicationType::Native,
            // We are going to use the Device Authorization Grant.
            vec![OAuthGrantType::DeviceCode],
            client_uri,
        )
    };

    Raw::new(&metadata).expect("Couldn't serialize client metadata")
}
