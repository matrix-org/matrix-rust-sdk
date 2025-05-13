use std::{
    fmt::Debug,
    sync::{Arc, RwLock},
};

use ruma::{OwnedRoomId, RoomId};
use serde::{Deserialize, Serialize};

/// The state of a [`SlidingSyncRoom`].
#[derive(Copy, Clone, Debug, Default, PartialEq)]
pub enum SlidingSyncRoomState {
    /// The room is not loaded, i.e. not updates have been received yet.
    #[default]
    NotLoaded,

    /// The room has been preloaded, i.e. its values come from the cache, but no
    /// updates have been received yet.
    Preloaded,

    /// The room has received updates.
    Loaded,
}

/// A Sliding Sync Room.
///
/// It contains some information about a specific room, along with a queue of
/// events for the timeline.
///
/// It is OK to clone this type as much as you need: cloning it is cheap, and
/// shallow. All clones of the same value are sharing the same state.
#[derive(Debug, Clone)]
pub struct SlidingSyncRoom {
    inner: Arc<SlidingSyncRoomInner>,
}

impl SlidingSyncRoom {
    /// Create a new `SlidingSyncRoom`.
    pub fn new(room_id: OwnedRoomId) -> Self {
        Self {
            inner: Arc::new(SlidingSyncRoomInner {
                room_id,
                state: RwLock::new(SlidingSyncRoomState::NotLoaded),
            }),
        }
    }

    /// Get the room ID of this `SlidingSyncRoom`.
    pub fn room_id(&self) -> &RoomId {
        &self.inner.room_id
    }

    pub(super) fn update_state(&mut self) {
        let mut state = self.inner.state.write().unwrap();
        *state = SlidingSyncRoomState::Loaded;
    }

    pub(super) fn from_frozen(frozen_room: FrozenSlidingSyncRoom) -> Self {
        let FrozenSlidingSyncRoom { room_id } = frozen_room;

        Self {
            inner: Arc::new(SlidingSyncRoomInner {
                room_id,
                state: RwLock::new(SlidingSyncRoomState::Preloaded),
            }),
        }
    }
}

#[cfg(test)]
impl SlidingSyncRoom {
    fn state(&self) -> SlidingSyncRoomState {
        *self.inner.state.read().unwrap()
    }

    fn set_state(&mut self, state: SlidingSyncRoomState) {
        *self.inner.state.write().unwrap() = state;
    }
}

#[derive(Debug)]
struct SlidingSyncRoomInner {
    /// The room ID.
    room_id: OwnedRoomId,

    /// Internal state of `Self`.
    state: RwLock<SlidingSyncRoomState>,
}

/// A “frozen” [`SlidingSyncRoom`], i.e. that can be written into, or read from
/// a store.
#[derive(Debug, Serialize, Deserialize)]
pub(super) struct FrozenSlidingSyncRoom {
    pub(super) room_id: OwnedRoomId,
}

impl From<&SlidingSyncRoom> for FrozenSlidingSyncRoom {
    fn from(value: &SlidingSyncRoom) -> Self {
        Self { room_id: value.inner.room_id.clone() }
    }
}

#[cfg(test)]
mod tests {
    use matrix_sdk_test::async_test;
    use ruma::room_id;
    use serde_json::json;

    use crate::sliding_sync::{FrozenSlidingSyncRoom, SlidingSyncRoom, SlidingSyncRoomState};

    #[async_test]
    async fn test_state_from_not_loaded() {
        let mut room = SlidingSyncRoom::new(room_id!("!foo:bar.org").to_owned());

        assert_eq!(room.state(), SlidingSyncRoomState::NotLoaded);

        // Update with an empty response, but it doesn't matter.
        room.update_state();

        assert_eq!(room.state(), SlidingSyncRoomState::Loaded);
    }

    #[async_test]
    async fn test_state_from_preloaded() {
        let mut room = SlidingSyncRoom::new(room_id!("!foo:bar.org").to_owned());

        room.set_state(SlidingSyncRoomState::Preloaded);

        // Update with an empty response, but it doesn't matter.
        room.update_state();

        assert_eq!(room.state(), SlidingSyncRoomState::Loaded);
    }

    #[async_test]
    async fn test_room_room_id() {
        let room_id = room_id!("!foo:bar.org");
        let room = SlidingSyncRoom::new(room_id.to_owned());

        assert_eq!(room.room_id(), room_id);
    }

    #[test]
    fn test_frozen_sliding_sync_room_serialization() {
        let frozen_room =
            FrozenSlidingSyncRoom { room_id: room_id!("!29fhd83h92h0:example.com").to_owned() };

        let serialized = serde_json::to_value(&frozen_room).unwrap();

        assert_eq!(
            serialized,
            json!({
                "room_id": "!29fhd83h92h0:example.com",
            })
        );

        let deserialized = serde_json::from_value::<FrozenSlidingSyncRoom>(serialized).unwrap();

        assert_eq!(deserialized.room_id, frozen_room.room_id);
    }
}
