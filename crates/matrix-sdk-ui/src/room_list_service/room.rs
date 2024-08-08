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

use core::fmt;
use std::{ops::Deref, sync::Arc};

use async_once_cell::OnceCell as AsyncOnceCell;
use matrix_sdk::SlidingSync;
use ruma::RoomId;

use super::Error;
use crate::{
    timeline::{EventTimelineItem, TimelineBuilder},
    Timeline,
};

/// A room in the room list.
///
/// It's cheap to clone this type.
#[derive(Clone)]
pub struct Room {
    inner: Arc<RoomInner>,
}

impl fmt::Debug for Room {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_tuple("Room").field(&self.id().to_owned()).finish()
    }
}

struct RoomInner {
    /// The Sliding Sync where everything comes from.
    sliding_sync: Arc<SlidingSync>,

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
    pub(super) fn new(room: matrix_sdk::Room, sliding_sync: &Arc<SlidingSync>) -> Self {
        Self {
            inner: Arc::new(RoomInner {
                sliding_sync: sliding_sync.clone(),
                room,
                timeline: AsyncOnceCell::new(),
            }),
        }
    }

    /// Get the room ID.
    pub fn id(&self) -> &RoomId {
        self.inner.room.room_id()
    }

    /// Get a computed room name for the room.
    pub fn cached_display_name(&self) -> Option<String> {
        Some(self.inner.room.cached_display_name()?.to_string())
    }

    /// Get the underlying [`matrix_sdk::Room`].
    pub fn inner_room(&self) -> &matrix_sdk::Room {
        &self.inner.room
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
    /// The latest event comes first from the `Timeline`, it can be a local or a
    /// remote event. Note that the `Timeline` can have more information esp. if
    /// it has run a backpagination for example. Otherwise if the `Timeline`
    /// doesn't have any latest event, it comes from the cache. This method
    /// does not fetch any events or calculate anything — if it's not already
    /// available, we return `None`.
    ///
    /// Reminder: this method also returns `None` is the latest event is not
    /// suitable for use in a message preview.
    pub async fn latest_event(&self) -> Option<EventTimelineItem> {
        // Look for a local event in the `Timeline`.
        //
        // First off, let's see if a `Timeline` exists…
        if let Some(timeline) = self.inner.timeline.get() {
            // If it contains a `latest_event`…
            if let Some(timeline_last_event) = timeline.latest_event().await {
                // If it's a local event or a remote event, we return it.
                return Some(timeline_last_event);
            }
        }

        // Otherwise, fallback to the classical path.
        if let Some(latest_event) = self.inner.room.latest_event() {
            EventTimelineItem::from_latest_event(
                self.inner.room.client(),
                self.inner.room.room_id(),
                latest_event,
            )
            .await
        } else {
            None
        }
    }

    /// Create a new [`TimelineBuilder`] with the default configuration.
    pub async fn default_room_timeline_builder(&self) -> Result<TimelineBuilder, Error> {
        // TODO we can remove this once the event cache handles his own cache.

        let sliding_sync_room =
            self.inner
                .sliding_sync
                .get_room(self.inner.room.room_id())
                .await
                .ok_or_else(|| Error::RoomNotFound(self.inner.room.room_id().to_owned()))?;

        self.inner
            .room
            .client()
            .event_cache()
            .add_initial_events(
                self.inner.room.room_id(),
                sliding_sync_room.timeline_queue().iter().cloned().collect(),
                sliding_sync_room.prev_batch(),
            )
            .await
            .map_err(Error::EventCache)?;

        Ok(Timeline::builder(&self.inner.room).track_read_marker_and_receipts())
    }
}
