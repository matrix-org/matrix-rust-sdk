use std::{
    env,
    io::{self, Write},
    process::exit,
    sync::{Arc, Mutex},
};

use futures_util::{pin_mut, StreamExt as _};
use matrix_sdk::{
    config::StoreConfig,
    matrix_auth::MatrixSession,
    ruma::{api::client::receipt::create_receipt::v3::ReceiptType, events::receipt::ReceiptThread},
    AuthSession, Client, ServerName, SqliteCryptoStore, SqliteStateStore,
};
use matrix_sdk_ui::sync_service::{self, SyncService};
use tokio::spawn;
use tracing_subscriber::{layer::SubscriberExt as _, util::SubscriberInitExt as _, EnvFilter};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let file_layer = tracing_subscriber::fmt::layer()
        .with_ansi(false)
        .with_writer(tracing_appender::rolling::hourly("/tmp/", "logs-"));

    tracing_subscriber::registry()
        .with(EnvFilter::new(std::env::var("RUST_LOG").unwrap_or("".into())))
        .with(file_layer)
        .init();

    let Some(server_name) = env::args().nth(1) else {
        eprintln!("Usage: {} <server_name>", env::args().next().unwrap());
        exit(1)
    };

    login_and_sync(server_name).await?;

    Ok(())
}

/// Log in to the given homeserver and sync.
async fn login_and_sync(server_name: String) -> anyhow::Result<()> {
    let server_name = ServerName::parse(&server_name)?;

    let client = Client::builder()
        .store_config(
            StoreConfig::default()
                .crypto_store(SqliteCryptoStore::open("/tmp/crypto.sqlite", None).await?)
                .state_store(SqliteStateStore::open("/tmp/state.sqlite", None).await?),
        )
        .server_name(&server_name)
        .build()
        .await?;

    // Try reading from /tmp/session.json
    if let Ok(serialized) = std::fs::read_to_string("/tmp/session.json") {
        let session: MatrixSession = serde_json::from_str(&serialized)?;
        client.restore_session(session).await?;
        println!("restored session");
    } else {
        login_with_password(&client).await?;
        println!("new login");
    }

    let sync_service = SyncService::builder(client.clone()).build().await?;

    let room_list_service = sync_service.room_list_service();

    let all_rooms = room_list_service.all_rooms().await?;
    let (rooms, stream) = all_rooms.entries();

    let rooms = Arc::new(Mutex::new(rooms.clone()));

    // This will sync (with encryption) until an error happens or the program is
    // killed.
    sync_service.start().await;

    let c = client.clone();
    let r = rooms.clone();
    let handle = spawn(async move {
        pin_mut!(stream);
        let rooms = r;
        let client = c;

        while let Some(diffs) = stream.next().await {
            let mut rooms = rooms.lock().unwrap();
            for diff in diffs {
                diff.apply(&mut rooms);
            }
            println!("New update!");
            for (id, room) in rooms.iter().enumerate() {
                if let Some(room) = room.as_room_id().and_then(|room_id| client.get_room(room_id)) {
                    println!("> #{id} {}: {:?}", room.room_id(), room.read_receipts());
                }
            }
        }
    });

    loop {
        let mut command = String::new();

        print!("$ ");
        let _ = io::stdout().flush();
        io::stdin().read_line(&mut command).expect("Unable to read user input");

        match command.trim() {
            "rooms" => {
                let rooms = rooms.lock().unwrap();
                for (id, room) in rooms.iter().enumerate() {
                    if let Some(room) =
                        room.as_room_id().and_then(|room_id| client.get_room(room_id))
                    {
                        println!("> #{id} {}: {:?}", room.room_id(), room.read_receipts());
                    }
                }
            }

            "start" => {
                sync_service.start().await;
                println!("> sync service started!");
            }

            "stop" => {
                sync_service.stop().await?;
                println!("> sync service stopped!");
            }

            "" | "exit" => {
                break;
            }

            _ => {
                if let Some((_, id)) = command.split_once("send ") {
                    let id = id.trim().parse::<usize>()?;
                    let room_id = { rooms.lock().unwrap()[id].as_room_id().map(ToOwned::to_owned) };
                    if let Some(room_id) = &room_id {
                        let room = room_list_service.room(room_id).await?;
                        room.init_timeline_with_builder(room.default_room_timeline_builder().await)
                            .await?;
                        let timeline = room.timeline().unwrap();

                        if let Some(latest) = timeline.latest_event().await {
                            let event_id = latest.event_id().expect("event id");

                            let did = timeline
                                .send_single_receipt(
                                    ReceiptType::Read,
                                    ReceiptThread::Unthreaded,
                                    event_id.to_owned(),
                                )
                                .await?;
                            println!("> did {}send a read receipt!", if did { "" } else { "not " });
                        } else {
                            println!("no latest event");
                        }
                    }
                } else {
                    println!("unknown command");
                }
            }
        }
    }

    println!("Closing sync service...");

    let sync_service = Arc::new(sync_service);
    let s = sync_service.clone();
    let wait_for_termination = spawn(async move {
        while let Some(state) = s.state().next().await {
            if !matches!(state, sync_service::State::Running) {
                break;
            }
        }
    });

    sync_service.stop().await?;
    handle.abort();
    wait_for_termination.await.unwrap();

    if let Some(session) = client.session() {
        let AuthSession::Matrix(session) = session else { panic!("unexpected oidc session") };
        let serialized = serde_json::to_string(&session)?;
        std::fs::write("/tmp/session.json", serialized)?;
        println!("saved session");
    }

    println!("okthxbye!");

    Ok(())
}

async fn login_with_password(client: &Client) -> anyhow::Result<()> {
    println!("Logging in with username and password…");

    loop {
        print!("\nUsername: ");
        io::stdout().flush().expect("Unable to write to stdout");
        let mut username = String::new();
        io::stdin().read_line(&mut username).expect("Unable to read user input");
        username = username.trim().to_owned();

        print!("Password: ");
        io::stdout().flush().expect("Unable to write to stdout");
        let mut password = String::new();
        io::stdin().read_line(&mut password).expect("Unable to read user input");
        password = password.trim().to_owned();

        match client.matrix_auth().login_username(&username, &password).await {
            Ok(_) => {
                println!("Logged in as {username}");
                break;
            }
            Err(error) => {
                println!("Error logging in: {error}");
                println!("Please try again\n");
            }
        }
    }

    Ok(())
}
