use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use matrix_sdk::authentication::qrcode::secure_channel::SecureChannel;
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
    Reciprocate {},
    ReciprocateAndScan {},
}

async fn login(rendezvous_url: Url) -> Result<()> {
    let channel =
        SecureChannel::login(rendezvous_url).await.context("Could not create a secure channel")?;

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

    println!("Please ender the check code the other device is displaying: ");

    let mut input = String::new();
    std::io::stdin().read_line(&mut input).expect("error: unable to read user input");

    let input = input.trim().to_lowercase();

    let input: u8 = input.parse().context("Could not parse the check code")?;

    let established = almost.confirm(check_code);

    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    if cli.verbose {
        tracing_subscriber::fmt::init();
    }

    match cli.mode {
        Mode::Login { rendezvous_url } => login(rendezvous_url).await?,
        _ => todo!(),
    }

    Ok(())
}
