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

use ruma::RoomId;

use super::Error;
use crate::timeline::{EventTimelineItem, TimelineBuilder};

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
    /// The underlying client room.
    room: matrix_sdk::Room,
}

impl Deref for Room {
    type Target = matrix_sdk::Room;

    fn deref(&self) -> &Self::Target {
        &self.inner.room
    }
}

impl Room {
    /// Create a new `Room`.
    pub(super) fn new(room: matrix_sdk::Room) -> Self {
        Self { inner: Arc::new(RoomInner { room }) }
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

    /// Get the room's latest event from the cache. This method does not fetch
    /// any events or calculate anything — if it's not already available, we
    /// return `None`.
    ///
    /// Reminder: this method also returns `None` is the latest event is not
    /// suitable for use in a message preview.
    pub async fn latest_event(&self) -> Option<EventTimelineItem> {
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
    ///
    /// If the room was synced before some initial events will be added to the
    /// [`TimelineBuilder`].
    pub fn default_room_timeline_builder(&self) -> Result<TimelineBuilder, Error> {
        Ok(TimelineBuilder::new(&self.inner.room).track_read_marker_and_receipts())
    }
}
