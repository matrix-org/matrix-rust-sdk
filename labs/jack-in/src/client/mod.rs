use eyre::{Result, WrapErr};
use futures::{pin_mut, StreamExt};
use tokio::sync::mpsc;
use tracing::{error, info, warn};

pub mod state;

use matrix_sdk::{
    ruma::{api::client::error::ErrorKind, OwnedRoomId},
    Client, SlidingSyncState, SlidingSyncViewBuilder,
};

pub async fn run_client(
    client: Client,
    tx: mpsc::Sender<state::SlidingSyncState>,
    config: crate::SlidingSyncConfig,
) -> Result<()> {
    info!("Starting sliding sync now");
    let builder = client.sliding_sync().await;
    let mut full_sync_view_builder = SlidingSyncViewBuilder::default_with_fullsync()
        .timeline_limit(10u32)
        .sync_mode(config.full_sync_mode.into());
    if let Some(size) = config.batch_size {
        full_sync_view_builder = full_sync_view_builder.batch_size(size);
    }

    if let Some(limit) = config.limit {
        full_sync_view_builder = full_sync_view_builder.limit(limit);
    }
    if let Some(limit) = config.timeline_limit {
        full_sync_view_builder = full_sync_view_builder.timeline_limit(limit);
    }

    let full_sync_view = full_sync_view_builder.build()?;

    let syncer = builder
        .homeserver(config.proxy.parse().wrap_err("can't parse sync proxy")?)
        .add_view(full_sync_view)
        .with_common_extensions()
        .cold_cache("jack-in-default")
        .build()
        .await?;
    let stream = syncer.stream();
    let view = syncer.view("full-sync").expect("we have the full syncer there").clone();
    let mut ssync_state = state::SlidingSyncState::new(syncer.clone(), view.clone());
    tx.send(ssync_state.clone()).await?;

    info!("starting polling");

    pin_mut!(stream);
    if let Some(Err(e)) = stream.next().await {
        error!("Stopped: Initial Query on sliding sync failed: {e:?}");
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
                let state = view.state();
                ssync_state.set_view_state(state.clone());

                if state == SlidingSyncState::Live {
                    info!("Reached live sync");
                    break;
                }
                let _ = tx.send(ssync_state.clone()).await;
            }
            Some(Err(e)) => {
                if e.client_api_error_kind() != Some(&ErrorKind::UnknownPos) {
                    error!("Error: {e}");
                    break;
                }
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
            let selected_room = ssync_state.selected_room.read().unwrap().clone();
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
                info!("Live update received: {update:?}");
                tx.send(ssync_state.clone()).await?;
                err_counter = 0;
            }
            Err(e) => {
                warn!("Live update error: {e:?}");
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
