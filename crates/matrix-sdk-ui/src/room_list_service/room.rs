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
// See the License for that specific language governing permissions and
// limitations under the License.

//! The `Room` type.

use std::{ops::Deref, sync::Arc};

use async_once_cell::OnceCell as AsyncOnceCell;
use matrix_sdk::{event_cache, SlidingSync, SlidingSyncRoom};
use ruma::{api::client::sync::sync_events::v4::RoomSubscription, events::StateEventType, RoomId};

use super::Error;
use crate::{
    timeline::{EventTimelineItem, SlidingSyncRoomExt, TimelineBuilder},
    Timeline,
};

/// A room in the room list.
///
/// It's cheap to clone this type.
#[derive(Clone, Debug)]
pub struct Room {
    inner: Arc<RoomInner>,
}

#[derive(Debug)]
struct RoomInner {
    /// The Sliding Sync where everything comes from.
    sliding_sync: Arc<SlidingSync>,

    /// The Sliding Sync room.
    sliding_sync_room: SlidingSyncRoom,

    /// The underlying client room.
    room: matrix_sdk::Room,

    /// The timeline of the room.
    timeline: AsyncOnceCell<Arc<Timeline>>,
}

impl Deref for Room {
    type Target = matrix_sdk::Room;

    fn deref(&self) -> &Self::Target {
        &self.inner.room
    }
}

impl Room {
    /// Create a new `Room`.
    pub(super) fn new(
        sliding_sync: Arc<SlidingSync>,
        sliding_sync_room: SlidingSyncRoom,
    ) -> Result<Self, Error> {
        let room = sliding_sync_room
            .client()
            .get_room(sliding_sync_room.room_id())
            .ok_or_else(|| Error::RoomNotFound(sliding_sync_room.room_id().to_owned()))?;

        Ok(Self {
            inner: Arc::new(RoomInner {
                sliding_sync,
                sliding_sync_room,
                room,
                timeline: AsyncOnceCell::new(),
            }),
        })
    }

    /// Get the room ID.
    pub fn id(&self) -> &RoomId {
        self.inner.room.room_id()
    }

    /// Get the name of the room if it exists.
    pub async fn name(&self) -> Option<String> {
        Some(self.inner.room.display_name().await.ok()?.to_string())
    }

    /// Get the underlying [`matrix_sdk::Room`].
    pub fn inner_room(&self) -> &matrix_sdk::Room {
        &self.inner.room
    }

    /// Subscribe to this room.
    ///
    /// It means that all events from this room will be received every time, no
    /// matter how the `RoomList` is configured.
    pub fn subscribe(&self, settings: Option<RoomSubscription>) {
        let mut settings = settings.unwrap_or_default();

        // Make sure to always include the room creation event in the required state
        // events, to know what the room version is.
        if !settings
            .required_state
            .iter()
            .any(|(event_type, _state_key)| *event_type == StateEventType::RoomCreate)
        {
            settings.required_state.push((StateEventType::RoomCreate, "".to_owned()));
        }

        self.inner
            .sliding_sync
            .subscribe_to_room(self.inner.room.room_id().to_owned(), Some(settings))
    }

    /// Unsubscribe to this room.
    ///
    /// It's the opposite method of [Self::subscribe`].
    pub fn unsubscribe(&self) {
        self.inner.sliding_sync.unsubscribe_from_room(self.inner.room.room_id().to_owned())
    }

    /// Get the timeline of the room if one exists.
    pub fn timeline(&self) -> Option<Arc<Timeline>> {
        self.inner.timeline.get().cloned()
    }

    /// Get whether the timeline has been already initialised or not.
    pub fn is_timeline_initialized(&self) -> bool {
        self.inner.timeline.get().is_some()
    }

    /// Initialize the timeline of the room with an event type filter so only
    /// some events are returned. If a previous timeline exists, it'll
    /// return an error. Otherwise, a Timeline will be returned.
    pub async fn init_timeline_with_builder(&self, builder: TimelineBuilder) -> Result<(), Error> {
        if self.inner.timeline.get().is_some() {
            Err(Error::TimelineAlreadyExists(self.inner.room.room_id().to_owned()))
        } else {
            self.inner
                .timeline
                .get_or_try_init(async { Ok(Arc::new(builder.build().await?)) })
                .await
                .map_err(Error::InitializingTimeline)?;
            Ok(())
        }
    }

    /// Get the latest event in the timeline.
    ///
    /// The latest event comes first from the `Timeline` if it exists and if it
    /// contains a local event. Otherwise, it comes from the cache. This method
    /// does not fetch any events or calculate anything — if it's not
    /// already available, we return `None`.
    pub async fn latest_event(&self) -> Option<EventTimelineItem> {
        // Look for a local event in the `Timeline`.
        //
        // First off, let's see if a `Timeline` exists…
        if let Some(timeline) = self.inner.timeline.get() {
            // If it contains a `latest_event`…
            if let Some(timeline_last_event) = timeline.latest_event().await {
                // If it's a local echo…
                if timeline_last_event.is_local_echo() {
                    return Some(timeline_last_event);
                }
            }
        }

        // Otherwise, fallback to the classical path.
        self.inner.sliding_sync_room.latest_timeline_item().await
    }

    /// Create a new [`TimelineBuilder`] with the default configuration.
    pub async fn default_room_timeline_builder(&self) -> event_cache::Result<TimelineBuilder> {
        // TODO we can remove this once the event cache handles his own cache.
        self.inner
            .room
            .client()
            .event_cache()
            .add_initial_events(
                self.inner.room.room_id(),
                self.inner.sliding_sync_room.timeline_queue().iter().cloned().collect(),
                self.inner.sliding_sync_room.prev_batch(),
            )
            .await?;

        Ok(Timeline::builder(&self.inner.room).track_read_marker_and_receipts())
    }
}
