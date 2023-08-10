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

use std::sync::Arc;

use async_once_cell::OnceCell as AsyncOnceCell;
use matrix_sdk::{SlidingSync, SlidingSyncRoom};
use ruma::{
    api::client::sync::sync_events::{v4::RoomSubscription, UnreadNotificationsCount},
    OwnedMxcUri, RoomId,
};

use super::Error;
use crate::{
    timeline::{EventTimelineItem, SlidingSyncRoomExt},
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

    /// Get the best possible name for the room.
    ///
    /// If the sliding sync room has received a name from the server, then use
    /// it, otherwise, let's calculate a name.
    pub async fn name(&self) -> Option<String> {
        Some(match self.inner.sliding_sync_room.name() {
            Some(name) => name,
            None => self.inner.room.display_name().await.ok()?.to_string(),
        })
    }

    /// Get the best possible avatar for the room.
    ///
    /// If the sliding sync room has received an avatar from the server, then
    /// use it, otherwise, let's try to find one from `Room`.
    pub fn avatar_url(&self) -> Option<OwnedMxcUri> {
        self.inner.sliding_sync_room.avatar_url().or_else(|| self.inner.room.avatar_url())
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
        self.inner.sliding_sync.subscribe_to_room(self.inner.room.room_id().to_owned(), settings)
    }

    /// Unsubscribe to this room.
    ///
    /// It's the opposite method of [Self::subscribe`].
    pub fn unsubscribe(&self) {
        self.inner.sliding_sync.unsubscribe_from_room(self.inner.room.room_id().to_owned())
    }

    /// Get the timeline of the room.
    pub async fn timeline(&self) -> Arc<Timeline> {
        self.inner
            .timeline
            .get_or_init(async {
                Arc::new(
                    Timeline::builder(&self.inner.room)
                        .events(
                            self.inner.sliding_sync_room.prev_batch(),
                            self.inner.sliding_sync_room.timeline_queue(),
                        )
                        .track_read_marker_and_receipts()
                        .build()
                        .await,
                )
            })
            .await
            .clone()
    }

    /// Get the latest event in the timeline.
    ///
    /// The latest event comes first from the `Timeline` if it exists.
    /// Otherwise, it comes from the cache. This method does not fetch any
    /// events or calculate anything — if it's not already available, we
    /// return `None`.
    ///
    /// It's different from `Self::timeline().latest_event()` as it won't track
    /// the read marker and receipts.
    pub async fn latest_event(&self) -> Option<EventTimelineItem> {
        // Look for a remote or **local** event in the `Timeline`.
        //
        // First off, let's see if a `Timeline` exists…
        if let Some(timeline) = self.inner.timeline.get() {
            // If it contains a `latest_event`…
            if let Some(timeline_last_event) = timeline.latest_event().await {
                return Some(timeline_last_event);
            }
        }

        // Otherwise, fallback to the classical path.
        self.inner.sliding_sync_room.latest_timeline_item().await
    }

    /// Is there any unread notifications?
    pub fn has_unread_notifications(&self) -> bool {
        self.inner.sliding_sync_room.has_unread_notifications()
    }

    /// Get unread notifications.
    pub fn unread_notifications(&self) -> UnreadNotificationsCount {
        self.inner.sliding_sync_room.unread_notifications()
    }
}
