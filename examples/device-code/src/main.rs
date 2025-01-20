use std::collections::HashMap;

use anyhow::Result;
use clap::Parser;
use matrix_sdk::{
    oidc::{
        registrations::OidcRegistrations,
        types::{
            iana::oauth::OAuthClientAuthenticationMethod,
            oidc::ApplicationType,
            registration::{ClientMetadata, Localized, VerifiedClientMetadata},
            requests::GrantType,
            scope::ScopeToken,
        },
    },
    Client,
};
use url::Url;

/// A command line example showcasing how to login using a device code.
///
/// Another device, will verify the device code.
#[derive(Parser, Debug)]
struct Cli {
    /// Set the homeserver that should be used for authentication.
    #[clap(long, required = true)]
    homeserver: Url,

    /// Add extra scopes to the request.
    #[clap(long)]
    custom_scopes: Option<Vec<ScopeToken>>,

    /// Enable verbose logging output.
    #[clap(short, long, action)]
    verbose: bool,
}

/// Generate the OIDC client metadata.
///
/// For simplicity, we use most of the default values here, but usually this
/// should be adapted to the provider metadata to make interactions as secure as
/// possible, for example by using the most secure signing algorithms supported
/// by the provider.
fn client_metadata() -> VerifiedClientMetadata {
    let client_uri = Url::parse("https://github.com/matrix-org/matrix-rust-sdk")
        .expect("Couldn't parse client URI");

    ClientMetadata {
        // This is a native application (in contrast to a web application, that runs in a browser).
        application_type: Some(ApplicationType::Native),
        // Native clients should be able to register the loopback interface and then point to any
        // port when needing a redirect URI. An alternative is to use a custom URI scheme registered
        // with the OS.
        redirect_uris: None,
        // We are going to use the Authorization Code flow, and of course we want to be able to
        // refresh our access token.
        grant_types: Some(vec![GrantType::RefreshToken, GrantType::DeviceCode]),
        // A native client shouldn't use authentication as the credentials could be intercepted.
        // Other protections are in place for the different requests.
        token_endpoint_auth_method: Some(OAuthClientAuthenticationMethod::None),
        // The following fields should be displayed in the OIDC provider interface as part of the
        // process to get the user's consent. It means that these should contain real data so the
        // user can make sure that they allow the proper application.
        // We are cheating here because this is an example.
        client_name: Some(Localized::new("matrix-rust-sdk-device-code-login".to_owned(), [])),
        contacts: Some(vec!["root@127.0.0.1".to_owned()]),
        client_uri: Some(Localized::new(client_uri.clone(), [])),
        policy_uri: Some(Localized::new(client_uri.clone(), [])),
        tos_uri: Some(Localized::new(client_uri, [])),
        ..Default::default()
    }
    .validate()
    .unwrap()
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    if cli.verbose {
        tracing_subscriber::fmt::init();
    }

    let server_name = cli.homeserver;
    let client = Client::builder().server_name_or_homeserver_url(server_name).build().await?;

    let metadata = client_metadata();

    let data_dir = dirs::data_dir().expect("no data_dir directory found");
    let registrations_file = data_dir.join("matrix_sdk/oidc").join("registrations.json");

    let static_registrations = HashMap::new();

    let registrations =
        OidcRegistrations::new(&registrations_file, client_metadata(), static_registrations)?;

    let oidc = client.oidc();

    let mut login_device_code = oidc.login_with_device_code(metadata, registrations);

    let auth_grant_response = login_device_code.device_code_for_login(cli.custom_scopes).await?;

    println!(
        "Log in using this {}",
        auth_grant_response.verification_uri_complete().unwrap().clone().into_secret()
    );
    println!(
        "You can also go to {} and type in the code {}",
        auth_grant_response.verification_uri(),
        auth_grant_response.user_code().clone().into_secret()
    );

    login_device_code.wait_finish_login().await?;

    let user_id = client.user_id().unwrap();

    println!("Successfully logged in as {user_id} using the device code");

    Ok(())
}
