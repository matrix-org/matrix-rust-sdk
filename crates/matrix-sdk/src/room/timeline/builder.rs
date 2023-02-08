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

use matrix_sdk_base::{
    deserialized_responses::{EncryptionInfo, SyncTimelineEvent},
    locks::Mutex,
};
use ruma::events::fully_read::FullyReadEventContent;
use tracing::error;

use super::{
    inner::TimelineInner,
    to_device::{handle_forwarded_room_key_event, handle_room_key_event},
    Timeline,
};
use crate::room;

/// Builder that allows creating and configuring various parts of a
/// [`Timeline`].
#[must_use]
#[derive(Debug)]
pub(crate) struct TimelineBuilder {
    room: room::Common,
    prev_token: Option<String>,
    events: Vec<SyncTimelineEvent>,
    track_fully_read: bool,
}

impl TimelineBuilder {
    pub(super) fn new(room: &room::Common) -> Self {
        Self {
            room: room.clone(),
            prev_token: None,
            events: Vec::default(),
            track_fully_read: false,
        }
    }

    /// Add initial events to the timeline.
    #[cfg(feature = "experimental-sliding-sync")]
    pub(crate) fn events(
        mut self,
        prev_token: Option<String>,
        events: Vec<SyncTimelineEvent>,
    ) -> Self {
        self.prev_token = prev_token;
        self.events = events;
        self
    }

    /// Enable tracking of the fully-read marker on the timeline.
    pub(crate) fn track_fully_read(mut self) -> Self {
        self.track_fully_read = true;
        self
    }

    /// Create a [`Timeline`] with the options set on this builder.
    pub(crate) async fn build(self) -> Timeline {
        let Self { room, prev_token, events, track_fully_read } = self;
        let has_events = !events.is_empty();

        let mut inner = TimelineInner::new(room);

        if has_events {
            inner.add_initial_events(events).await;
        }

        let inner = Arc::new(inner);
        let room = inner.room();

        let timeline_event_handle = room.add_event_handler({
            let inner = inner.clone();
            move |event, encryption_info: Option<EncryptionInfo>| {
                let inner = inner.clone();
                async move {
                    inner.handle_live_event(event, encryption_info).await;
                }
            }
        });

        // Not using room.add_event_handler here because RoomKey events are
        // to-device events that are not received in the context of a room.
        #[cfg(feature = "e2e-encryption")]
        let room_key_handle = room
            .client
            .add_event_handler(handle_room_key_event(inner.clone(), room.room_id().to_owned()));
        #[cfg(feature = "e2e-encryption")]
        let forwarded_room_key_handle = room.client.add_event_handler(
            handle_forwarded_room_key_event(inner.clone(), room.room_id().to_owned()),
        );

        let mut event_handler_handles = vec![
            timeline_event_handle,
            #[cfg(feature = "e2e-encryption")]
            room_key_handle,
            #[cfg(feature = "e2e-encryption")]
            forwarded_room_key_handle,
        ];

        if track_fully_read {
            match room.account_data_static::<FullyReadEventContent>().await {
                Ok(Some(fully_read)) => match fully_read.deserialize() {
                    Ok(fully_read) => {
                        inner.set_fully_read_event(fully_read.content.event_id).await;
                    }
                    Err(e) => {
                        error!("Failed to deserialize fully-read account data: {e}");
                    }
                },
                Err(e) => {
                    error!("Failed to get fully-read account data from the store: {e}");
                }
                _ => {}
            }

            let fully_read_handle = room.add_event_handler({
                let inner = inner.clone();
                move |event| {
                    let inner = inner.clone();
                    async move {
                        inner.handle_fully_read(event).await;
                    }
                }
            });
            event_handler_handles.push(fully_read_handle);
        }

        let timeline = Timeline {
            inner,
            start_token: Mutex::new(prev_token),
            _end_token: Mutex::new(None),
            event_handler_handles,
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
