// Copyright 2026 The Matrix.org Foundation C.I.C.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! A dedicated sliding sync connection carrying only room subscriptions, for
//! the rooms currently visible in the room-list viewport.
//!
//! Room subscriptions used to ride the main `room-list` connection, which
//! serializes them behind the in-flight long-poll round: during a catch-up, a
//! subscription's data only arrives when a round that can take many seconds to
//! compute completes, and every viewport change cancels and restarts that
//! round. This connection has no lists and a zero poll timeout: a request is
//! only sent when the viewport changes, and the server answers immediately
//! with the subscribed rooms' data (a screenful of timeline plus the required
//! state), so previews and timeline preload for visible rooms arrive within
//! one short round-trip, independently of the main loop.
//!
//! This is a prefetch mechanism, not a standing sync loop: between viewport
//! changes no request is in flight, and live updates for these rooms keep
//! flowing on the main connection's lists.

use std::{
    sync::{Arc, RwLock as StdRwLock},
    time::Duration,
};

use futures_util::{StreamExt, pin_mut};
use matrix_sdk::{Client, SlidingSync};
use matrix_sdk_common::executor::{AbortOnDrop, JoinHandleExt as _, spawn};
use ruma::{OwnedRoomId, RoomId, api::client::sync::sync_events::v5 as http};
use tokio::sync::watch;
use tracing::{debug, warn};

use super::Error;

/// The dedicated viewport sliding sync connection ID.
const VIEWPORT_CONNECTION_ID: &str = "viewport";

#[derive(Debug)]
pub(super) struct Viewport {
    sliding_sync: Arc<SlidingSync>,

    /// The current viewport, re-applied after a session expiry (which clears
    /// the connection's subscriptions).
    current_rooms: Arc<StdRwLock<Vec<OwnedRoomId>>>,

    /// Wakes the sync task up for a round; sends coalesce.
    kick_sender: watch::Sender<()>,

    _task: AbortOnDrop<()>,
}


impl Viewport {
    pub async fn new(client: &Client) -> Result<Self, Error> {
        let sliding_sync = Arc::new(
            client
                .sliding_sync(VIEWPORT_CONNECTION_ID)
                .map_err(Error::SlidingSync)?
                // Fetch-style: never long-poll. A request is only sent when
                // the viewport changes, and the server answers immediately.
                .poll_timeout(Duration::ZERO)
                .with_receipt_extension(ruma::assign!(http::request::Receipts::default(), {
                    enabled: Some(true),
                    rooms: Some(vec![http::request::ExtensionRoomConfig::AllSubscribed])
                }))
                .build()
                .await
                .map_err(Error::SlidingSync)?,
        );

        let (kick_sender, kick_receiver) = watch::channel(());
        let current_rooms = Arc::new(StdRwLock::new(Vec::new()));

        let task = spawn(sync_task(
            sliding_sync.clone(),
            current_rooms.clone(),
            kick_receiver,
        ))
        .abort_on_drop();

        Ok(Self { sliding_sync, current_rooms, kick_sender, _task: task })
    }

    /// Set the viewport to `room_ids` (in display order) and trigger a sync
    /// round for it.
    ///
    /// Subscriptions for rooms no longer in the viewport are removed
    /// client-side; already-synced data stays in the caches.
    ///
    /// The in-flight round (if any) is NEVER cancelled: rounds are short, and
    /// aborting a response the server has already computed loses data for the
    /// subscriptions that rode it (the server does not re-send it on the
    /// pos-reverted retry, so the affected rooms would stay blank forever;
    /// observed against synapse). A kick during a round coalesces into one
    /// follow-up round carrying the newest viewport.
    pub fn subscribe(&self, room_ids: &[&RoomId], settings: http::request::RoomSubscription) {
        *self.current_rooms.write().unwrap() =
            room_ids.iter().map(|room_id| (*room_id).to_owned()).collect();

        self.sliding_sync.resubscribe_to_rooms(room_ids, Some(settings), false);

        let _ = self.kick_sender.send(());
    }
}

/// The sync task: one short round per viewport change, nothing in between.
async fn sync_task(
    sliding_sync: Arc<SlidingSync>,
    current_rooms: Arc<StdRwLock<Vec<OwnedRoomId>>>,
    mut kick_receiver: watch::Receiver<()>,
) {
    loop {
        if kick_receiver.changed().await.is_err() {
            // The `Viewport` has been dropped.
            break;
        }

        // One sync round per kick burst: kicks arriving while a round is
        // running coalesce into a single follow-up round (the watch channel
        // only retains the latest notification).
        loop {
            kick_receiver.mark_unchanged();

            let sync = sliding_sync.sync();
            pin_mut!(sync);

            match sync.next().await {
                Some(Ok(update_summary)) => {
                    debug!(
                        rooms = update_summary.rooms.len(),
                        "viewport sync round completed"
                    );
                }

                Some(Err(error)) => {
                    warn!(?error, "viewport sync round failed");

                    // Whatever the error (an expired `pos` being the expected
                    // one), reset the session and re-apply the current
                    // viewport: the next round starts from scratch. Do NOT
                    // retry in a loop here; the next kick (or the follow-up
                    // round below, if one is already pending) retries.
                    sliding_sync.expire_session().await;

                    let rooms = current_rooms.read().unwrap().clone();
                    sliding_sync.resubscribe_to_rooms(
                        &rooms.iter().map(AsRef::as_ref).collect::<Vec<_>>(),
                        Some(super::RoomListService::room_subscription_settings()),
                        false,
                    );

                    break;
                }

                None => break,
            }

            if !matches!(kick_receiver.has_changed(), Ok(true)) {
                break;
            }
        }
    }
}
