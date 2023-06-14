//! The `Room` type.

use std::sync::Arc;

use async_once_cell::OnceCell as AsyncOnceCell;
use matrix_sdk::SlidingSyncRoom;

use super::Error;
use crate::{timeline::EventTimelineItem, Timeline};

/// A room in the room list.
///
/// It's cheap to clone this type.
#[derive(Clone, Debug)]
pub struct Room {
    inner: Arc<RoomInner>,
}

#[derive(Debug)]
struct RoomInner {
    /// The Sliding Sync room.
    sliding_sync_room: SlidingSyncRoom,

    /// The underlying client room.
    room: matrix_sdk::room::Room,

    /// The timeline of the room.
    timeline: AsyncOnceCell<Timeline>,

    /// The “sneaky” timeline of the room, i.e. this timeline doesn't track the
    /// read marker nor the receipts.
    sneaky_timeline: AsyncOnceCell<Timeline>,
}

impl Room {
    /// Create a new `Room`.
    pub(super) async fn new(sliding_sync_room: SlidingSyncRoom) -> Result<Self, Error> {
        let room = sliding_sync_room
            .client()
            .get_room(sliding_sync_room.room_id())
            .ok_or_else(|| Error::RoomNotFound(sliding_sync_room.room_id().to_owned()))?;

        Ok(Self {
            inner: Arc::new(RoomInner {
                sliding_sync_room,
                room,
                timeline: AsyncOnceCell::new(),
                sneaky_timeline: AsyncOnceCell::new(),
            }),
        })
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

    /// Get the underlying [`matrix_sdk::room::Room`].
    pub fn inner_room(&self) -> &matrix_sdk::room::Room {
        &self.inner.room
    }

    /// Get the timeline of the room.
    pub async fn timeline(&self) -> &Timeline {
        self.inner
            .timeline
            .get_or_init(async {
                Timeline::builder(&self.inner.room)
                    .events(
                        self.inner.sliding_sync_room.prev_batch(),
                        self.inner.sliding_sync_room.timeline_queue(),
                    )
                    .track_read_marker_and_receipts()
                    .build()
                    .await
            })
            .await
    }

    /// Get the latest event of the timeline.
    ///
    /// It's different from `Self::timeline().latest_event()` as it won't track
    /// the read marker and receipts.
    pub async fn latest_event(&self) -> Option<EventTimelineItem> {
        self.inner
            .sneaky_timeline
            .get_or_init(async {
                Timeline::builder(&self.inner.room)
                    .events(
                        self.inner.sliding_sync_room.prev_batch(),
                        self.inner.sliding_sync_room.timeline_queue(),
                    )
                    .build()
                    .await
            })
            .await
            .latest_event()
            .await
    }
}
