use std::io::stdout;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, RwLock};

use futures::{StreamExt, pin_mut};

use eyre::{eyre, WrapErr, Result};
use tuirealm::tui::backend::CrosstermBackend;
use tuirealm::tui::Terminal;


use log::{warn, error};

pub mod state;

use futures_signals::signal::SignalExt;

use matrix_sdk::{Client, SlidingSyncState, SlidingSyncViewBuilder, ruma::RoomId};

pub async fn run_client(client: Client, sliding_sync_proxy: String, tx: mpsc::Sender<state::SlidingSyncState>) -> Result<()> {

    let username = match client.account().get_display_name().await? {
        Some(u) => u,
        None => client.user_id().await.ok_or_else(||eyre!("Looks like you didn't login"))?.to_string()
    };

    let homeserver = client.homeserver().await;

    warn!("Starting sliding sync now");
    let mut builder = client.sliding_sync();
    let full_sync_view = SlidingSyncViewBuilder::default_with_fullsync()
        .timeline_limit(10u32)
        .build()?;
    let syncer = builder
        .homeserver(sliding_sync_proxy.parse().wrap_err("can't parse sync proxy")?)
        .add_view(full_sync_view)
        .build()?;
    let (cancel, stream) = syncer.stream().expect("we can build the stream");
    let view = syncer.views.lock_ref().first().expect("we have the full syncer there").clone();
    let state = view.state.clone();
    let mut ssync_state = state::SlidingSyncState::new(view);
    tx.send(ssync_state.clone()).await;

    pin_mut!(stream);
    let first_poll = stream.next().await;
    let view_state = state.read_only().get_cloned();
    if  view_state != SlidingSyncState::CatchingUp {
        warn!("Sliding Query failed: {:#?}", view_state);
        return Ok(())
    }

    {
        ssync_state.set_first_render_now();
        tx.send(ssync_state.clone()).await;
    }
    warn!("Done initial sliding sync");

    loop {
        match stream.next().await {
            Some(Ok(_)) => {
                // we are switching into live updates mode next. ignoring

                if state.read_only().get_cloned() == SlidingSyncState::Live {
                    warn!("Reached live sync");
                    break
                }
                let _ = tx.send(ssync_state.clone()).await;
            }
            Some(Err(e)) => {
                warn!("Error: {:}", e);
                break
            }
            None => {
                warn!("Never reached live state");
                break;
            }
        }
    }

    {
        ssync_state.set_full_sync_now();
        tx.send(ssync_state.clone()).await;
    }

    let mut err_counter = 0;
    let mut prev_selected_room : Option<Box<RoomId>> = None;

    while let Some(update) = stream.next().await {
        warn!("live next");
        {
            let selected_room  = ssync_state.selected_room.lock_ref().clone();
            if let Some(room_id) = selected_room {
                if let Some(prev) = &prev_selected_room {
                    if prev != &room_id {
                        syncer.unsubscribe(prev.clone());
                        syncer.subscribe(room_id.clone(), None);
                        prev_selected_room = Some(room_id.clone());
                    }
                } else {
                    syncer.subscribe(room_id.clone(), None);
                    prev_selected_room = Some(room_id.clone());
                }
            }
        }
        warn!("after next");
        match update {
            Ok(u) => {
                warn!("Live update received");
                tx.send(ssync_state.clone()).await;
                err_counter = 0;
            }
            Err(e) => {
                warn!("Live update error: {:?}", e);
                err_counter += 1;
                if err_counter > 3 {
                    error!("Received 3 errors in a row. stopping.");
                    break
                }
            }
        }
    }
    Ok(())
}
