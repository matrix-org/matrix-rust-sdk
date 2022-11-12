use anyhow::Result;
use clap::Parser;
use futures::StreamExt;
use futures_signals::signal_vec::SignalVecExt;
use matrix_sdk::{self, config::SyncSettings, ruma::OwnedRoomId, Client};
use url::Url;

#[derive(Parser, Debug)]
struct Cli {
    /// The homeserver to connect to.
    #[clap(value_parser)]
    homeserver: Url,

    /// The user name that should be used for the login.
    #[clap(value_parser)]
    user_name: String,

    /// The password that should be used for the login.
    #[clap(value_parser)]
    password: String,

    /// Set the proxy that should be used for the connection.
    #[clap(short, long)]
    proxy: Option<Url>,

    /// Enable verbose logging output.
    #[clap(short, long, action)]
    verbose: bool,

    /// The room id that we should listen for the,
    #[clap(value_parser)]
    room_id: OwnedRoomId,
}

async fn login(cli: Cli) -> Result<Client> {
    let mut builder =
        Client::builder().homeserver_url(cli.homeserver).sled_store("./", Some("some password"));

    if let Some(proxy) = cli.proxy {
        builder = builder.proxy(proxy);
    }

    let client = builder.build().await?;

    client
        .login_username(&cli.user_name, &cli.password)
        .initial_device_display_name("rust-sdk")
        .send()
        .await?;

    Ok(client)
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    let cli = Cli::parse();
    let room_id = cli.room_id.clone();
    let client = login(cli).await?;

    let sync_settings = SyncSettings::default();

    // Wait for the first sync response
    println!("Wait for the first sync");

    client.sync_once(sync_settings.clone()).await?;

    // Get the timeline stream and listen to it.
    let room = client.get_room(&room_id).unwrap();
    let timeline = room.timeline().await;
    let mut timeline_stream = timeline.signal().to_stream();

    tokio::spawn(async move {
        while let Some(diff) = timeline_stream.next().await {
            println!("Received a timeline diff {diff:#?}");
        }
    });

    // Sync forever
    client.sync(sync_settings).await?;

    Ok(())
}
