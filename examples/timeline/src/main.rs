use anyhow::Result;
use clap::Parser;
use futures_util::StreamExt;
use matrix_sdk::{Client, config::SyncSettings, ruma::OwnedRoomId};
use matrix_sdk_ui::timeline::RoomExt;
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
    // Note that when encryption is enabled, you should use a persistent store to be
    // able to restore the session with a working encryption setup.
    // See the `persist_session` example.
    let mut builder = Client::builder().homeserver_url(cli.homeserver);

    if let Some(proxy) = cli.proxy {
        builder = builder.proxy(proxy);
    }

    let client = builder.build().await?;

    client
        .matrix_auth()
        .login_username(&cli.user_name, &cli.password)
        .initial_device_display_name("rust-sdk")
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
    let timeline = room.timeline().await.unwrap();
    let (timeline_items, mut timeline_stream) = timeline.subscribe().await;

    println!("Initial timeline items: {timeline_items:#?}");
    tokio::spawn(async move {
        while let Some(diffs) = timeline_stream.next().await {
            println!("Received timeline diffs: {diffs:#?}");
        }
    });

    // Sync forever
    client.sync(sync_settings).await?;

    Ok(())
}
