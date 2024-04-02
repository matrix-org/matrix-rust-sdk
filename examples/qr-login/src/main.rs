use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};
use matrix_sdk::{
    authentication::qrcode::secure_channel::{EstablishedSecureChannel, SecureChannel},
    crypto::qr_login::{QrCodeData, QrCodeMode},
    reqwest::{self, Proxy},
    Client,
};
use qrcode::{
    render::unicode::{self, Dense1x2},
    QrCode,
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

fn build_http_client(proxy: Option<Url>) -> Result<reqwest::Client> {
    let mut builder = reqwest::Client::builder();

    if let Some(proxy) = proxy {
        builder = builder
            .proxy(Proxy::all(proxy).context("Couldn't configure the proxy")?)
            .danger_accept_invalid_certs(true);
    }

    let http_client = builder.build().context("Couldn't create a HTTP client")?;

    Ok(http_client)
}

async fn login(proxy: Option<Url>, rendezvous_url: Url) -> Result<()> {
    let http_client = build_http_client(proxy)?;

    let channel = SecureChannel::login(http_client, rendezvous_url)
        .await
        .context("Could not create a secure channel")?;

    let channel = create_outobund_channel(channel).await?;

    Ok(())
}

async fn login_and_scan(proxy: Option<Url>) -> Result<()> {
    let http_client = build_http_client(proxy)?;

    println!("Please ender the base64 string other device is displaying: ");

    let mut input = String::new();
    std::io::stdin().read_line(&mut input).expect("error: unable to read user input");

    let input = input.trim();

    let data = QrCodeData::from_base64(input).context("Couldn't parse the base64 QR code data")?;

    let channel = EstablishedSecureChannel::from_qr_code(http_client, &data, QrCodeMode::Login)
        .await
        .context("Couldn't establish the secure channel")?;

    let code = channel.check_code().to_digit();

    println!("Successfully established the secure channel.");
    println!("Please enter the following code into the other device {code:02}");

    Ok(())
}

async fn create_outobund_channel(channel: SecureChannel) -> Result<EstablishedSecureChannel> {
    let data = channel.qr_code_data();

    let qr_code = QrCode::new(data.to_bytes()).context("Failed to render the QR code")?;
    let data_base64 = data.to_base64();

    let image = qr_code
        .render::<Dense1x2>()
        .dark_color(unicode::Dense1x2::Light)
        .light_color(unicode::Dense1x2::Dark)
        .build();

    println!("{image}");
    println!("Please scan this QR code on an existing device, or paste the following base64 string {data_base64}");

    let almost = channel
        .connect()
        .await
        .context("The secure channel could not have been connected to the other side")?;

    println!("Please enter the check code the other device is displaying: ");

    let mut input = String::new();
    std::io::stdin().read_line(&mut input).expect("error: unable to read user input");

    let input = input.trim().to_lowercase();

    let input: u8 = input.parse().context("Could not parse the check code")?;

    let established = almost.confirm(input).context("The check code did not match")?;

    println!("Successfully established the secure channel.");

    Ok(established)
}

async fn reciprocate(proxy: Option<Url>, settings: ReciprocateSettings) -> Result<()> {
    let builder = Client::builder().homeserver_url(settings.homeserver);

    let builder = if let Some(proxy) = proxy {
        builder.proxy(proxy).disable_ssl_verification()
    } else {
        builder
    };

    let client = builder.build().await?;

    // TODO this needs to be OIDC instead.
    client
        .matrix_auth()
        .login_username(&settings.user_name, &settings.password)
        .initial_device_display_name("rust-sdk")
        .await?;

    let channel = client.qr_code_foo().await?;
    let channel = create_outobund_channel(channel).await?;

    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    if cli.verbose {
        tracing_subscriber::fmt::init();
    }

    match cli.mode {
        Mode::Login { rendezvous_url } => login(cli.proxy, rendezvous_url).await?,
        Mode::LoginAndScan {} => login_and_scan(cli.proxy).await?,
        Mode::Reciprocate(settings) => reciprocate(cli.proxy, settings).await?,
        _ => todo!(),
    }

    Ok(())
}
