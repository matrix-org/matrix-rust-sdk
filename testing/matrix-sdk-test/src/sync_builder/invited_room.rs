use ruma::{
    OwnedRoomId, RoomId, api::client::sync::sync_events::v3::InvitedRoom,
    events::AnyStrippedStateEvent, serde::Raw,
};

use crate::DEFAULT_TEST_ROOM_ID;

pub struct InvitedRoomBuilder {
    pub(super) room_id: OwnedRoomId,
    pub(super) inner: InvitedRoom,
}

impl InvitedRoomBuilder {
    /// Create a new `InvitedRoomBuilder` for the given room ID.
    ///
    /// If the room ID is [`DEFAULT_TEST_ROOM_ID`],
    /// [`InvitedRoomBuilder::default()`] can be used instead.
    pub fn new(room_id: &RoomId) -> Self {
        Self { room_id: room_id.to_owned(), inner: Default::default() }
    }

    /// Get the room ID of this [`InvitedRoomBuilder`].
    pub fn room_id(&self) -> &RoomId {
        &self.room_id
    }

    /// Add an event to the state.
    pub fn add_state_event(mut self, event: impl Into<Raw<AnyStrippedStateEvent>>) -> Self {
        self.inner.invite_state.events.push(event.into());
        self
    }

    /// Add events to the state in bulk.
    pub fn add_state_bulk<I>(mut self, events: I) -> Self
    where
        I: IntoIterator<Item = Raw<AnyStrippedStateEvent>>,
    {
        self.inner.invite_state.events.extend(events);
        self
    }
}

impl Default for InvitedRoomBuilder {
    fn default() -> Self {
        Self::new(&DEFAULT_TEST_ROOM_ID)
    }
}
