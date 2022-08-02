use ruma::{
    api::client::sync::sync_events::v3::InvitedRoom, events::AnyStrippedStateEvent, serde::Raw,
    OwnedRoomId,
};

use super::StrippedStateTestEvent;
use crate::test_json;

pub struct InvitedRoomBuilder {
    pub(super) room_id: OwnedRoomId,
    pub(super) inner: InvitedRoom,
}

impl InvitedRoomBuilder {
    /// Create a new `InvitedRoomBuilder` for the given room ID.
    ///
    /// If the room ID is [`test_json::DEFAULT_SYNC_ROOM_ID`],
    /// [`InvitedRoomBuilder::default()`] can be used instead.
    pub fn new(room_id: impl Into<OwnedRoomId>) -> Self {
        Self { room_id: room_id.into(), inner: Default::default() }
    }

    /// Add an event to the state.
    pub fn add_state_event(mut self, event: StrippedStateTestEvent) -> Self {
        self.inner.invite_state.events.push(event.into_raw_event());
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
        Self::new(test_json::DEFAULT_SYNC_ROOM_ID.to_owned())
    }
}
