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

use async_trait::async_trait;
use matrix_sdk::SlidingSyncRoom;
use tracing::{error, instrument};

use super::{EventTimelineItem, Timeline, TimelineBuilder};

#[async_trait]
pub trait SlidingSyncRoomExt {
    /// Get a `Timeline` for this room.
    async fn timeline(&self) -> Option<Timeline>;

    /// Get the latest timeline item of this room.
    ///
    /// Use `Timeline::latest_event` instead if you already have a timeline for
    /// this `SlidingSyncRoom`.
    async fn latest_event(&self) -> Option<EventTimelineItem>;
}

#[async_trait]
impl SlidingSyncRoomExt for SlidingSyncRoom {
    async fn timeline(&self) -> Option<Timeline> {
        Some(sliding_sync_timeline_builder(self)?.track_read_marker_and_receipts().build().await)
    }

    #[instrument(skip_all)]
    async fn latest_event(&self) -> Option<EventTimelineItem> {
        sliding_sync_timeline_builder(self)?.read_only().build().await.latest_event().await
    }
}

fn sliding_sync_timeline_builder(room: &SlidingSyncRoom) -> Option<TimelineBuilder> {
    let room_id = room.room_id();
    match room.client().get_room(room_id) {
        Some(r) => Some(Timeline::builder(&r).events(room.prev_batch(), room.timeline_queue())),
        None => {
            error!(?room_id, "Room not found in client. Can't provide a timeline for it");
            None
        }
    }
}
