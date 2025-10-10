use std::io::Write;

use anyhow::{Context, Result, bail};
use clap::Parser;
use futures_util::StreamExt;
use matrix_sdk::{
    Client,
    authentication::oauth::{
        qrcode::{LoginProgress, QrCodeData, QrCodeModeData, QrProgress},
        registration::{ApplicationType, ClientMetadata, Localized, OAuthGrantType},
    },
    ruma::serde::Raw,
};
use url::Url;

/// A command line example showcasing how to login using a QR code.
///
/// Another device, which will display the QR code is needed to use this
/// example.
#[derive(Parser, Debug)]
struct Cli {
    /// Set the proxy that should be used for the connection.
    #[clap(short, long)]
    proxy: Option<Url>,

    /// Enable verbose logging output.
    #[clap(short, long, action)]
    verbose: bool,
}

/// Generate the OAuth 2.0 client metadata.
fn client_metadata() -> Raw<ClientMetadata> {
    let client_uri = Localized::new(
        Url::parse("https://github.com/matrix-org/matrix-rust-sdk")
            .expect("Couldn't parse client URI"),
        None,
    );

    let metadata = ClientMetadata {
        // The following fields should be displayed in the OAuth 2.0 authorization server's web UI
        // as part of the process to get the user's consent. It means that these should
        // contain real data so the user can make sure that they allow the proper
        // application. We are cheating here because this is an example.
        client_name: Some(Localized::new("matrix-rust-sdk-qrlogin".to_owned(), [])),
        policy_uri: Some(client_uri.clone()),
        tos_uri: Some(client_uri.clone()),
        ..ClientMetadata::new(
            // This is a native application (in contrast to a web application, that runs in a
            // browser).
            ApplicationType::Native,
            // We are going to use the Device Authorization flow.
            vec![OAuthGrantType::DeviceCode],
            client_uri,
        )
    };

    Raw::new(&metadata).expect("Couldn't serialize client metadata")
}

async fn print_devices(client: &Client) -> Result<()> {
    let user_id = client.user_id().unwrap();
    let own_device =
        client.encryption().get_own_device().await?.expect("We should have our own device by now");

    println!(
        "Status of our own device {}",
        if own_device.is_cross_signed_by_owner() { "✅" } else { "❌" }
    );

    println!("Devices of user {user_id}");

    for device in client.encryption().get_user_devices(user_id).await?.devices() {
        if device.device_id()
            == client.device_id().expect("We should be logged in now and know our device id")
        {
            continue;
        }

        println!(
            "   {:<10} {:<30} {:<}",
            device.device_id(),
            device.display_name().unwrap_or("-"),
            if device.is_verified() { "✅" } else { "❌" }
        );
    }

    Ok(())
}

async fn login(proxy: Option<Url>) -> Result<()> {
    println!("Please scan the QR code and convert the data to base64 before entering it here.");
    println!("On Linux/Wayland, this can be achieved using the following command line:");
    println!(
        "    $ grim -g \"$(slurp)\" - | zbarimg --oneshot -Sbinary PNG:- | base64 -w 0 | wl-copy"
    );
    println!("Paste the QR code data here: ");

    let mut input = String::new();
    std::io::stdin().read_line(&mut input).expect("error: unable to read user input");
    let input = input.trim();

    let data = QrCodeData::from_base64(input).context("Couldn't parse the base64 QR code data")?;

    let QrCodeModeData::Reciprocate { server_name } = &data.mode_data else {
        bail!("The QR code is invalid, we did not receive a homeserver in the QR code.");
    };
    let mut client = Client::builder().server_name_or_homeserver_url(server_name);

    if let Some(proxy) = proxy {
        client = client.proxy(proxy).disable_ssl_verification();
    }

    let client = client.build().await?;

    let registration_data = client_metadata().into();
    let oauth = client.oauth();

    let login_client = oauth.login_with_qr_code(Some(&registration_data)).scan(&data);
    let mut subscriber = login_client.subscribe_to_progress();

    let task = tokio::spawn(async move {
        while let Some(state) = subscriber.next().await {
            match state {
                LoginProgress::Starting | LoginProgress::SyncingSecrets => (),
                LoginProgress::EstablishingSecureChannel(QrProgress { check_code }) => {
                    let code = check_code.to_digit();
                    println!("Please enter the following code into the other device {code:02}");
                }
                LoginProgress::WaitingForToken { user_code } => {
                    println!("Please use your other device to confirm the log in {user_code}")
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

    print_devices(&client).await?;

    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    if cli.verbose {
        tracing_subscriber::fmt::init();
    }

    login(cli.proxy).await?;

    Ok(())
}
