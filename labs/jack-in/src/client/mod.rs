use eyre::{Result, WrapErr};
use futures::{pin_mut, StreamExt};
use tokio::sync::mpsc;
use tracing::{error, info, warn};

pub mod state;

use matrix_sdk::{ruma::OwnedRoomId, Client, SlidingSyncState, SlidingSyncViewBuilder};

pub async fn run_client(
    client: Client,
    sliding_sync_proxy: String,
    tx: mpsc::Sender<state::SlidingSyncState>,
) -> Result<()> {
    info!("Starting sliding sync now");
    let builder = client.sliding_sync().await;
    let full_sync_view =
        SlidingSyncViewBuilder::default_with_fullsync().timeline_limit(10u32).build()?;
    let syncer = builder
        .homeserver(sliding_sync_proxy.parse().wrap_err("can't parse sync proxy")?)
        .add_view(full_sync_view)
        .with_common_extensions()
        .build()?;
    let stream = syncer.stream().await.expect("we can build the stream");
    let view = syncer.views.lock_ref().first().expect("we have the full syncer there").clone();
    let state = view.state.clone();
    let mut ssync_state = state::SlidingSyncState::new(view);
    tx.send(ssync_state.clone()).await?;

    info!("starting polling");

    pin_mut!(stream);
    if let Some(Err(e)) = stream.next().await {
        error!("Initial Query on sliding sync failed: {:#?}", e);
        return Ok(());
    }
    let view_state = state.read_only().get_cloned();
    if view_state != SlidingSyncState::CatchingUp {
        warn!("Sliding Query failed: {:#?}", view_state);
        return Ok(());
    }

    {
        ssync_state.set_first_render_now();
        tx.send(ssync_state.clone()).await?;
    }
    info!("Done initial sliding sync");

    loop {
        match stream.next().await {
            Some(Ok(_)) => {
                // we are switching into live updates mode next. ignoring
                let state = state.read_only().get_cloned();

                if state == SlidingSyncState::Live {
                    info!("Reached live sync");
                    break;
                }
                let _ = tx.send(ssync_state.clone()).await;
            }
            Some(Err(e)) => {
                error!("Error: {:}", e);
                break;
            }
            None => {
                error!("Never reached live state");
                break;
            }
        }
    }

    {
        ssync_state.set_full_sync_now();
        tx.send(ssync_state.clone()).await?;
    }

    let mut err_counter = 0;
    let mut prev_selected_room: Option<OwnedRoomId> = None;

    while let Some(update) = stream.next().await {
        {
            let selected_room = ssync_state.selected_room.lock_ref().clone();
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
        match update {
            Ok(update) => {
                info!("Live update received: {:?}", update);
                tx.send(ssync_state.clone()).await?;
                err_counter = 0;
            }
            Err(e) => {
                warn!("Live update error: {:?}", e);
                err_counter += 1;
                if err_counter > 3 {
                    error!("Received 3 errors in a row. stopping.");
                    break;
                }
            }
        }
    }
    Ok(())
}
