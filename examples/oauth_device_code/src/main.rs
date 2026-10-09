// Copyright 2026 Nordeck IT + Consulting GmbH <info@nordeck.net>
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::env;

use anyhow::{Result, bail};
use matrix_sdk::{
    Client,
    authentication::oauth::registration::{
        ApplicationType, ClientMetadata, Localized, OAuthGrantType,
    },
    ruma::serde::Raw,
};
use url::Url;

/// A minimal example showcasing how to log in with the OAuth 2.0 Device
/// Authorization Grant ([RFC 8628]), as specified for Matrix in [MSC4341].
///
/// This login method is meant for clients that cannot open a browser, like
/// bots, bridges or command-line applications. The user needs to open the
/// printed URL on another device to approve the login.
///
/// Usage: `cargo run -p example-oauth-device-code -- <homeserver_url>`
///
/// [RFC 8628]: https://datatracker.ietf.org/doc/html/rfc8628
/// [MSC4341]: https://github.com/matrix-org/matrix-spec-proposals/pull/4341
#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    let Some(homeserver) = env::args().nth(1) else {
        bail!("Usage: {} <homeserver_url>", env::args().next().unwrap_or_default());
    };

    let client = Client::builder().server_name_or_homeserver_url(homeserver).build().await?;
    let oauth = client.oauth();

    // Request the authorization, and show the verification URI and the user
    // code to the user.
    let login = oauth.login_with_device_code(None, Some(client_metadata().into()), None).await?;

    println!(
        "To log in, open {} and enter the code {}.",
        login.verification_uri(),
        login.user_code()
    );
    if let Some(uri) = login.verification_uri_complete() {
        println!("Alternatively, open {uri} to skip entering the code.");
    }
    println!("The code expires in {} seconds.", login.expires_in().as_secs());

    // Wait for the user to approve the login.
    login.finish().await?;

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
