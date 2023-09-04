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

use async_std::sync::Mutex;
use eyeball::SharedObservable;
use imbl::Vector;
use matrix_sdk::{
    deserialized_responses::SyncTimelineEvent, executor::spawn, sync::RoomUpdate, Room,
};
use ruma::events::{
    receipt::{ReceiptThread, ReceiptType},
    AnySyncTimelineEvent,
};
use tokio::sync::{broadcast, mpsc};
use tracing::{error, info, info_span, trace, warn, Instrument};

#[cfg(feature = "e2e-encryption")]
use super::to_device::{handle_forwarded_room_key_event, handle_room_key_event};
use super::{
    inner::{TimelineInner, TimelineInnerSettings},
    queue::send_queued_messages,
    BackPaginationStatus, Timeline, TimelineDropHandle,
};

/// Builder that allows creating and configuring various parts of a
/// [`Timeline`].
#[must_use]
#[derive(Debug)]
pub struct TimelineBuilder {
    room: Room,
    prev_token: Option<String>,
    events: Vector<SyncTimelineEvent>,
    settings: TimelineInnerSettings,
}

impl TimelineBuilder {
    pub(super) fn new(room: &Room) -> Self {
        Self {
            room: room.clone(),
            prev_token: None,
            events: Vector::new(),
            settings: TimelineInnerSettings::default(),
        }
    }

    /// Add initial events to the timeline.
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
        self.settings.track_read_receipts = true;
        self
    }

    /// Use the given filter to choose whether to add events to the timeline.
    ///
    /// # Arguments
    ///
    /// * `filter` - A function that takes a deserialized event, and should
    ///   return `true` if the event should be added to the `Timeline`.
    ///
    /// If this is not overridden, the timeline uses the default filter that
    /// allows every event.
    ///
    /// Note that currently:
    ///
    /// - Not all event types have a representation as a `TimelineItem` so these
    ///   are not added no matter what the filter returns.
    /// - It is not possible to filter out `m.room.encrypted` events (otherwise
    ///   they couldn't by decrypted when the appropriate room key arrives)
    pub fn event_filter<F>(mut self, filter: F) -> Self
    where
        F: Fn(&AnySyncTimelineEvent) -> bool + Send + Sync + 'static,
    {
        self.settings.event_filter = Arc::new(filter);
        self
    }

    /// Whether to add events that failed to deserialize to the timeline.
    ///
    /// Defaults to `true`.
    pub fn add_failed_to_parse(mut self, add: bool) -> Self {
        self.settings.add_failed_to_parse = add;
        self
    }

    /// Create a [`Timeline`] with the options set on this builder.
    #[tracing::instrument(
        skip(self),
        fields(
            room_id = ?self.room.room_id(),
            events_length = self.events.len(),
            track_read_receipts = self.settings.track_read_receipts,
            prev_token = self.prev_token,
        )
    )]
    pub async fn build(self) -> Timeline {
        let Self { room, prev_token, events, settings } = self;
        let has_events = !events.is_empty();
        let track_read_marker_and_receipts = settings.track_read_receipts;

        let mut inner = TimelineInner::new(room).with_settings(settings);

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
                    inner.set_initial_user_receipt(ReceiptType::Read, read_receipt).await;
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
                    inner
                        .set_initial_user_receipt(ReceiptType::ReadPrivate, private_read_receipt)
                        .await;
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
        if track_read_marker_and_receipts {
            inner.load_fully_read_event().await;
        }

        let room = inner.room();
        let client = room.client();

        let start_token = Arc::new(Mutex::new(prev_token));
        let end_token = Arc::new(Mutex::new(None));

        let mut room_update_rx = room.subscribe_to_updates();
        let room_update_join_handle = spawn({
            let inner = inner.clone();
            let start_token = start_token.clone();
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

                    trace!("Handling a room update");

                    let update_start_token = |prev_batch: &Option<_>| {
                        // Only update start_token if it's not currently locked.
                        // If it is locked, pagination is currently in progress.
                        if let Some(mut start_token) = start_token.try_lock() {
                            if start_token.is_none() && prev_batch.is_some() {
                                *start_token = prev_batch.clone();
                            }
                        }
                    };

                    match update {
                        RoomUpdate::Left { updates, .. } => {
                            update_start_token(&updates.timeline.prev_batch);
                            inner.handle_sync_timeline(updates.timeline).await;
                        }
                        RoomUpdate::Joined { updates, .. } => {
                            update_start_token(&updates.timeline.prev_batch);
                            inner.handle_joined_room_update(updates).await;
                        }
                        RoomUpdate::Invited { .. } => {
                            warn!("Room is in invited state, can't build or update its timeline");
                        }
                    }
                }
            }
            .instrument(info_span!("room_update_handler", room_id = ?room.room_id()))
        });

        let mut ignore_user_list_stream = client.subscribe_to_ignore_user_list_changes();
        let ignore_user_list_update_join_handle = spawn({
            let inner = inner.clone();
            let start_token = start_token.clone();
            let end_token = end_token.clone();
            async move {
                while ignore_user_list_stream.next().await.is_some() {
                    // Same as `Timeline::reset`, but Timeline is s not clonable
                    // and we need to avoid circular references
                    let mut start_lock = start_token.lock().await;
                    let mut end_lock = end_token.lock().await;

                    *start_lock = None;
                    *end_lock = None;

                    inner.clear().await;
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

        let handles = vec![
            #[cfg(feature = "e2e-encryption")]
            room_key_handle,
            #[cfg(feature = "e2e-encryption")]
            forwarded_room_key_handle,
        ];

        let (msg_sender, msg_receiver) = mpsc::channel(1);
        info!("Starting message-sending loop");
        spawn(send_queued_messages(inner.clone(), room.clone(), msg_receiver));

        let timeline = Timeline {
            inner,
            start_token,
            start_token_condvar: Default::default(),
            back_pagination_status: SharedObservable::new(BackPaginationStatus::Idle),
            _end_token: end_token,
            msg_sender,
            drop_handle: Arc::new(TimelineDropHandle {
                client,
                event_handler_handles: handles,
                room_update_join_handle,
                ignore_user_list_update_join_handle,
            }),
        };

        #[cfg(feature = "e2e-encryption")]
        if has_events {
            // The events we're injecting might be encrypted events, but we might
            // have received the room key to decrypt them while nobody was listening to the
            // `m.room_key` event, let's retry now.
            timeline.retry_decryption_for_all_events().await;
        }

        timeline
    }
}
