// Smoke test for Paginated Sync (MSC4525) against a local synapse:
//
//     cargo run -p example-paginated-sync-smoke -- http://localhost:8008 paginated correct-horse
//
// Logs in, builds a SyncService in paginated mode, starts it, and reports how
// quickly the room list loads (first page vs fully loaded), then waits a few
// seconds in steady state.

use std::time::Instant;

use anyhow::Context;
use matrix_sdk::Client;
use matrix_sdk_ui::{room_list_service::RoomListLoadingState, sync_service::SyncService};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt().init();

    let mut args = std::env::args().skip(1);
    let homeserver = args.next().context("usage: <homeserver> <user> <password>")?;
    let username = args.next().context("missing username")?;
    let password = args.next().context("missing password")?;

    // With a persistent store dir (5th argument), a second run restores the
    // session and the paginated sync `pos`, resuming the connection instead of
    // starting a fresh one.
    let store_path = std::env::args().nth(5);
    let _tempdir;
    let db_path = match &store_path {
        Some(path) => std::path::PathBuf::from(path),
        None => {
            _tempdir = tempfile::tempdir()?;
            _tempdir.path().to_owned()
        }
    };
    let session_file = db_path.join("session.json");

    let start = Instant::now();

    let client = Client::builder()
        .homeserver_url(&homeserver)
        .sqlite_store(&db_path, None)
        .build()
        .await?;

    if let Ok(serialized) = std::fs::read_to_string(&session_file) {
        let session: matrix_sdk::authentication::matrix::MatrixSession =
            serde_json::from_str(&serialized)?;
        client.restore_session(session).await?;
        println!("[{:?}] restored session for {}", start.elapsed(), username);
    } else {
        client
            .matrix_auth()
            .login_username(&username, &password)
            .initial_device_display_name("paginated-sync-smoke")
            .await?;
        if store_path.is_some() {
            let session = client.matrix_auth().session().context("no session after login")?;
            std::fs::write(&session_file, serde_json::to_string(&session)?)?;
        }
        println!("[{:?}] logged in as {}", start.elapsed(), username);
    }

    let sync_service = SyncService::builder(client.clone())
        .with_paginated_sync()
        .build()
        .await?;

    let room_list = sync_service.room_list_service().all_rooms().await?;
    let mut loading_state = room_list.loading_state();

    sync_service.start().await;
    println!("[{:?}] sync service started", start.elapsed());

    // Wait for the first page (the room list flips to Loaded).
    let total = loop {
        match loading_state.next().await {
            None => anyhow::bail!("loading state stream ended"),
            Some(RoomListLoadingState::NotLoaded) => {}
            Some(RoomListLoadingState::Loaded { maximum_number_of_rooms }) => {
                println!(
                    "[{:?}] room list loaded (server total: {maximum_number_of_rooms:?}, \
                     {} rooms known locally)",
                    start.elapsed(),
                    client.rooms().len(),
                );
                break maximum_number_of_rooms;
            }
        }
    };

    // Poll until the backlog has fully drained into the client. The server
    // total arrives with the first response; before that, fall back to the
    // expected count from the command line (default 120).
    let expected: usize =
        std::env::args().nth(4).and_then(|arg| arg.parse().ok()).unwrap_or(120);
    let _ = total;
    loop {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let known_rooms = client.rooms().len();
        println!("[{:?}] client now knows {known_rooms}/{expected} rooms", start.elapsed());
        if known_rooms >= expected {
            println!("[{:?}] FULLY SYNCED: all {known_rooms} rooms known locally", start.elapsed());
            break;
        }
    }

    // Steady state: prove the long-poll keeps ticking without burning requests.
    println!("[{:?}] steady state for 5s...", start.elapsed());
    tokio::time::sleep(std::time::Duration::from_secs(5)).await;

    let rooms = client.rooms();
    let named = rooms
        .iter()
        .filter(|room| room.cached_display_name().is_some())
        .count();
    println!(
        "[{:?}] done: {} rooms known, {} with display names",
        start.elapsed(),
        rooms.len(),
        named
    );

    sync_service.stop().await;
    Ok(())
}
