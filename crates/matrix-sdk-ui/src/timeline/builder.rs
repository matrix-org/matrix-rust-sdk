// Copyright 2023 The Matrix.org Foundation C.I.C.
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

use std::sync::Arc;

use imbl::Vector;
use matrix_sdk::{
    deserialized_responses::SyncTimelineEvent, executor::spawn, room, sync::RoomUpdate,
};
use ruma::events::receipt::{ReceiptThread, ReceiptType, SyncReceiptEvent};
use tokio::sync::{broadcast, Mutex};
use tracing::{error, warn};

#[cfg(feature = "e2e-encryption")]
use super::to_device::{handle_forwarded_room_key_event, handle_room_key_event};
use super::{inner::TimelineInner, Timeline, TimelineDropHandle};

/// Builder that allows creating and configuring various parts of a
/// [`Timeline`].
#[must_use]
#[derive(Debug)]
pub(crate) struct TimelineBuilder {
    room: room::Common,
    prev_token: Option<String>,
    events: Vector<SyncTimelineEvent>,
    track_read_marker_and_receipts: bool,
}

impl TimelineBuilder {
    pub(super) fn new(room: &room::Common) -> Self {
        Self {
            room: room.clone(),
            prev_token: None,
            events: Vector::new(),
            track_read_marker_and_receipts: false,
        }
    }

    /// Add initial events to the timeline.
    #[cfg(feature = "experimental-sliding-sync")]
    pub(crate) fn events(
        mut self,
        prev_token: Option<String>,
        events: Vector<SyncTimelineEvent>,
    ) -> Self {
        self.prev_token = prev_token;
        self.events = events;
        self
    }

    /// Enable tracking of the fully-read marker and the read receipts on the
    /// timeline.
    pub(crate) fn track_read_marker_and_receipts(mut self) -> Self {
        self.track_read_marker_and_receipts = true;
        self
    }

    /// Create a [`Timeline`] with the options set on this builder.
    #[tracing::instrument(skip(self))]
    pub(crate) async fn build(self) -> Timeline {
        let Self { room, prev_token, events, track_read_marker_and_receipts } = self;
        let has_events = !events.is_empty();

        let mut inner =
            TimelineInner::new(room).with_read_receipt_tracking(track_read_marker_and_receipts);

        if track_read_marker_and_receipts {
            match inner
                .room()
                .user_receipt(
                    ReceiptType::Read,
                    ReceiptThread::Unthreaded,
                    inner.room().own_user_id(),
                )
                .await
            {
                Ok(Some(read_receipt)) => {
                    inner.set_initial_user_receipt(ReceiptType::Read, read_receipt);
                }
                Err(e) => {
                    error!("Failed to get public read receipt of own user from the store: {e}");
                }
                _ => {}
            }
            match inner
                .room()
                .user_receipt(
                    ReceiptType::ReadPrivate,
                    ReceiptThread::Unthreaded,
                    inner.room().own_user_id(),
                )
                .await
            {
                Ok(Some(private_read_receipt)) => {
                    inner.set_initial_user_receipt(ReceiptType::ReadPrivate, private_read_receipt);
                }
                Err(e) => {
                    error!("Failed to get private read receipt of own user from the store: {e}");
                }
                _ => {}
            }
        }

        if has_events {
            inner.add_initial_events(events).await;
        }

        let inner = Arc::new(inner);
        let room = inner.room();
        let client = room.client();

        let mut room_update_rx = room.subscribe_to_updates();
        let room_update_join_handle = spawn({
            let inner = inner.clone();
            async move {
                loop {
                    let update = match room_update_rx.recv().await {
                        Ok(up) => up,
                        Err(broadcast::error::RecvError::Closed) => break,
                        Err(broadcast::error::RecvError::Lagged(_)) => {
                            warn!("Lagged behind sync responses, resetting timeline");
                            inner.clear().await;
                            continue;
                        }
                    };

                    let timeline = match update {
                        RoomUpdate::Left { updates, .. } => updates.timeline,
                        RoomUpdate::Joined { updates, .. } => updates.timeline,
                        RoomUpdate::Invited { .. } => {
                            warn!("Room is in invited state, can't build or update its timeline");
                            continue;
                        }
                    };

                    for event in timeline.events {
                        inner.handle_live_event(event).await;
                    }
                }
            }
        });

        // Not using room.add_event_handler here because RoomKey events are
        // to-device events that are not received in the context of a room.

        #[cfg(feature = "e2e-encryption")]
        let room_key_handle = client
            .add_event_handler(handle_room_key_event(inner.clone(), room.room_id().to_owned()));
        #[cfg(feature = "e2e-encryption")]
        let forwarded_room_key_handle = client.add_event_handler(handle_forwarded_room_key_event(
            inner.clone(),
            room.room_id().to_owned(),
        ));

        let mut handles = vec![
            #[cfg(feature = "e2e-encryption")]
            room_key_handle,
            #[cfg(feature = "e2e-encryption")]
            forwarded_room_key_handle,
        ];

        if track_read_marker_and_receipts {
            inner.load_fully_read_event().await;

            let fully_read_handle = room.add_event_handler({
                let inner = inner.clone();
                move |event| {
                    let inner = inner.clone();
                    async move {
                        inner.handle_fully_read(event).await;
                    }
                }
            });
            handles.push(fully_read_handle);

            let read_receipts_handle = room.add_event_handler({
                let inner = inner.clone();
                move |read_receipts: SyncReceiptEvent| {
                    let inner = inner.clone();
                    async move {
                        inner.handle_read_receipts(read_receipts.content).await;
                    }
                }
            });
            handles.push(read_receipts_handle);
        }

        let timeline = Timeline {
            inner,
            start_token: Mutex::new(prev_token),
            _end_token: Mutex::new(None),
            drop_handle: Arc::new(TimelineDropHandle {
                client,
                event_handler_handles: handles,
                room_update_join_handle,
            }),
        };

        #[cfg(feature = "e2e-encryption")]
        if has_events {
            // The events we're injecting might be encrypted events, but we might
            // have received the room key to decrypt them while nobody was listening to the
            // `m.room_key` event, let's retry now.
            //
            // TODO: We could spawn a task here and put this into the background, though it
            // might not be worth it depending on the number of events we injected.
            // Some measuring needs to be done.
            timeline.retry_decryption_for_all_events().await;
        }

        timeline
    }
}
