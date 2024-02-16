use std::collections::BTreeMap;

use async_trait::async_trait;
use matrix_sdk_base::deserialized_responses::SyncTimelineEvent;
use ruma::{OwnedRoomId, RoomId};
use tokio::sync::RwLock;

use super::Result;

/// A store that can be remember information about the event cache.
///
/// It really acts as a cache, in the sense that clearing the backing data
/// should not have any irremediable effect, other than providing a lesser user
/// experience.
#[async_trait]
pub trait EventCacheStore: Send + Sync {
    /// Returns all the known events for the given room.
    async fn room_events(&self, room: &RoomId) -> Result<Vec<SyncTimelineEvent>>;

    /// Adds all the events to the given room.
    async fn add_room_events(&self, room: &RoomId, events: Vec<SyncTimelineEvent>) -> Result<()>;

    /// Clear all the events from the given room.
    async fn clear_room_events(&self, room: &RoomId) -> Result<()>;
}

/// An [`EventCacheStore`] implementation that keeps all the information in
/// memory.
pub(crate) struct MemoryStore {
    /// All the events per room, in sync order.
    by_room: RwLock<BTreeMap<OwnedRoomId, Vec<SyncTimelineEvent>>>,
}

impl MemoryStore {
    /// Create a new empty [`MemoryStore`].
    pub fn new() -> Self {
        Self { by_room: Default::default() }
    }
}

#[async_trait]
impl EventCacheStore for MemoryStore {
    async fn room_events(&self, room: &RoomId) -> Result<Vec<SyncTimelineEvent>> {
        Ok(self.by_room.read().await.get(room).cloned().unwrap_or_default())
    }

    async fn add_room_events(&self, room: &RoomId, events: Vec<SyncTimelineEvent>) -> Result<()> {
        self.by_room.write().await.entry(room.to_owned()).or_default().extend(events);
        Ok(())
    }

    async fn clear_room_events(&self, room: &RoomId) -> Result<()> {
        let _ = self.by_room.write().await.remove(room);
        Ok(())
    }
}
