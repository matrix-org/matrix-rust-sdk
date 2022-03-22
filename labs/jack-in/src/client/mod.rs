use std::io::stdout;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;

use futures::{StreamExt, pin_mut};

use eyre::{eyre, WrapErr, Result};
use tuirealm::tui::backend::CrosstermBackend;
use tuirealm::tui::Terminal;

use log::{warn, error};

pub mod state;

use futures_signals::signal::SignalExt;

use matrix_sdk::{Client, SlidingSyncState, ruma::RoomId};

pub async fn run_sliding_sync(client: Client, sliding_sync_proxy: String, ssync_state: Arc<RwLock<state::SlidingSyncState>>) -> Result<()> {

    warn!("Starting sliding sync now");
    let mut builder = client.sliding_sync();
    let syncer = builder
        .homeserver(sliding_sync_proxy.parse().wrap_err("can't parse sync proxy")?)
        .add_fullsync_view()
        .build()?;
    let (cancel, stream) = syncer.stream().expect("we can build the stream");
    let view = syncer.views.lock_ref().first().expect("we have the full syncer there").clone();
    let state = view.state.clone();
    pin_mut!(stream);
    {
        ssync_state.write().await.start(view.clone());
    }
    let first_poll = stream.next().await;
    let view_state = state.read_only().get_cloned();
    if  view_state != SlidingSyncState::CatchingUp {
        warn!("Sliding Query failed: {:#?}", view_state);
        return Ok(())
    }

    {
        ssync_state.write().await.set_first_render_now();
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
        ssync_state.write().await.set_full_sync_now();
    }

    let mut err_counter = 0;
    let mut prev_selected_room : Option<Box<RoomId>> = None;

    while let Some(update) = stream.next().await {
        warn!("live next");
        {
            let selected_room  = {
                ssync_state.read().await.selected_room.clone()
            };
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
                warn!("Live update: {:?}", u);
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

pub async fn run_client(client: Client, sliding_sync_proxy: String, ssync_state: Arc<tokio::sync::RwLock<state::SlidingSyncState>>) -> Result<()> {

    let username = match client.account().get_display_name().await? {
        Some(u) => u,
        None => client.user_id().await.ok_or_else(||eyre!("Looks like you didn't login"))?.to_string()
    };

    let homeserver = client.homeserver().await;

    run_sliding_sync(client, sliding_sync_proxy, ssync_state).await?;
    Ok(())
}

// pub async fn run_syncv2(client: Client,  app: Arc<tokio::sync::Mutex<App>>) -> Result<()> {
//     {
//         let mut app = app.lock().await;
//         app.state_mut().start_v2();
//     }

//     warn!("Starting v2 sync now");
//     let res = client.sync_once(Default::default()).await?;
//     warn!("Done v2 sync");

//     {
//         let mut app = app.lock().await;
//         let v2 = app.state_mut().get_v2_mut().expect("we started this before!");
//         v2.set_first_render_now();
//         let total_rooms = res.rooms.join.len() + res.rooms.leave.len() + res.rooms.invite.len();
//         v2.set_rooms_count(total_rooms as u32); 
//     }

//     Ok(())
// }
