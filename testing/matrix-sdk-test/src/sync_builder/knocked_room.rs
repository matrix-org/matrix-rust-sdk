use ruma::{
    OwnedRoomId, RoomId, api::client::sync::sync_events::v3::KnockedRoom,
    events::AnyStrippedStateEvent, serde::Raw,
};

use super::StrippedStateTestEvent;
use crate::DEFAULT_TEST_ROOM_ID;

pub struct KnockedRoomBuilder {
    pub(super) room_id: OwnedRoomId,
    pub(super) inner: KnockedRoom,
}

impl KnockedRoomBuilder {
    /// Create a new `KnockedRoomBuilder` for the given room ID.
    ///
    /// If the room ID is [`DEFAULT_TEST_ROOM_ID`],
    /// [`KnockedRoomBuilder::default()`] can be used instead.
    pub fn new(room_id: &RoomId) -> Self {
        Self { room_id: room_id.to_owned(), inner: Default::default() }
    }

    /// Get the room ID of this [`KnockedRoomBuilder`].
    pub fn room_id(&self) -> &RoomId {
        &self.room_id
    }

    /// Add an event to the state.
    pub fn add_state_event(mut self, event: StrippedStateTestEvent) -> Self {
        self.inner.knock_state.events.push(event.into());
        self
    }

    /// Add events to the state in bulk.
    pub fn add_state_bulk<I>(mut self, events: I) -> Self
    where
        I: IntoIterator<Item = Raw<AnyStrippedStateEvent>>,
    {
        self.inner.knock_state.events.extend(events);
        self
    }
}

impl Default for KnockedRoomBuilder {
    fn default() -> Self {
        Self::new(&DEFAULT_TEST_ROOM_ID)
    }
}
