use std::io::Write;

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};
use futures_util::StreamExt;
use matrix_sdk::{
    authentication::qrcode::LoginProgress,
    crypto::qr_login::QrCodeData,
    oidc::types::{
        iana::oauth::OAuthClientAuthenticationMethod,
        oidc::ApplicationType,
        registration::{ClientMetadata, Localized, VerifiedClientMetadata},
        requests::GrantType,
    },
    Client,
};
use url::Url;

/// A command line example showcasing how the secret storage support works in
/// the Matrix Rust SDK.
///
/// Secret storage is an account data backed encrypted key/value store. You can
/// put or get secrets from the store.
#[derive(Parser, Debug)]
struct Cli {
    /// Set the proxy that should be used for the connection.
    #[clap(short, long)]
    proxy: Option<Url>,

    /// Enable verbose logging output.
    #[clap(short, long, action)]
    verbose: bool,

    #[command(subcommand)]
    mode: Mode,
}

#[derive(Debug, Subcommand)]
enum Mode {
    Login {
        #[clap(value_parser)]
        rendezvous_url: Url,
    },
    LoginAndScan {},
    Reciprocate(ReciprocateSettings),
    ReciprocateAndScan(ReciprocateSettings),
}

#[derive(Debug, Args)]
pub struct ReciprocateSettings {
    /// The homeserver to connect to.
    #[clap(value_parser)]
    homeserver: Url,

    /// The user name that should be used for the login.
    #[clap(value_parser)]
    user_name: String,

    /// The password that should be used for the login.
    #[clap(value_parser)]
    password: String,
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
        client_name: Some(Localized::new("matrix-rust-sdk-qrlogin".to_owned(), [])),
        contacts: Some(vec!["root@127.0.0.1".to_owned()]),
        client_uri: Some(Localized::new(client_uri.clone(), [])),
        policy_uri: Some(Localized::new(client_uri.clone(), [])),
        tos_uri: Some(Localized::new(client_uri, [])),
        ..Default::default()
    }
    .validate()
    .unwrap()
}

async fn login_and_scan(proxy: Option<Url>) -> Result<()> {
    println!("Please enter the base64 string other device is displaying: ");

    let mut input = String::new();
    std::io::stdin().read_line(&mut input).expect("error: unable to read user input");

    let input = input.trim();

    let data = QrCodeData::from_base64(input).context("Couldn't parse the base64 QR code data")?;

    // TODO: Get the homeserver from the QR code.
    let mut client = Client::builder().homeserver_url("https://synapse-oidc.lab.element.dev/");

    if let Some(proxy) = proxy {
        client = client.proxy(proxy).disable_ssl_verification();
    }

    let client = client.build().await?;
    let metadata = client_metadata();
    let oidc = client.oidc();

    let login_client = oidc.login_with_qr_code(data, metadata);
    let mut subscriber = login_client.subscribe_to_progress();

    let task = tokio::spawn(async move {
        while let Some(state) = subscriber.next().await {
            match state {
                LoginProgress::Starting => (),
                LoginProgress::EstablishingSecureChannel { check_code } => {
                    let code = check_code.to_digit();
                    println!("Please enter the following code into the other device {code:02}");
                }
                LoginProgress::WaitingForToken => {
                    println!("Please use your other device to confirm the log in")
                }
                LoginProgress::Done => break,
            }
        }

        std::io::stdout().flush().expect("Unable to write to stdout");
    });

    let result = login_client.await;
    task.abort();

    result?;

    let status = client.encryption().cross_signing_status().await.unwrap();
    let user_id = client.user_id().unwrap();
    println!(
        "Successfully logged in as {user_id} using the qr code, cross-signing status: {status:?}"
    );

    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    if cli.verbose {
        tracing_subscriber::fmt::init();
    }

    match cli.mode {
        Mode::LoginAndScan {} => login_and_scan(cli.proxy).await?,
        _ => todo!(),
    }

    Ok(())
}
