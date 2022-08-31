use anyhow::Result;
use clap::Parser;
use matrix_sdk::{
    ruma::{api::client::room::create_room, assign},
    Client,
};
use url::Url;

#[derive(Parser, Debug)]
#[clap()]
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
}

async fn login(cli: Cli) -> Result<Client> {
    let builder = Client::builder().homeserver_url(cli.homeserver);

    let builder = if let Some(proxy) = cli.proxy { builder.proxy(proxy) } else { builder };

    let client = builder.build().await?;

    client
        .login_username(&cli.user_name, &cli.password)
        .initial_device_display_name("rust-sdk")
        .send()
        .await?;

    Ok(client)
}

async fn wait_for_input(client: &Client) -> Result<bool> {
    println!("Input a name to create a room, or `quit` to exit the example");

    let mut input = String::new();
    std::io::stdin().read_line(&mut input).expect("error: unable to read user input");

    match input.trim().to_lowercase().as_ref() {
        "quit" | "exit" | "cancel" => Ok(false),
        _ => {
            let room_name = input;
            let request = assign!(create_room::v3::Request::new(), { name: Some(&room_name) });
            let room = client.create_room(request).await?;

            println!(
                "Created a room with the name {}, room_id {}",
                room.display_name().await?,
                room.room_id()
            );

            Ok(true)
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    let cli = Cli::parse();
    let client = login(cli).await?;
    let sync_client = client.clone();

    tokio::spawn(async move {
        sync_client.sync(Default::default()).await;
    });

    while wait_for_input(&client).await? {}

    Ok(())
}
