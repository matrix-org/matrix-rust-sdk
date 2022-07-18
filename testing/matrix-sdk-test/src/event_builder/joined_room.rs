use ruma::{api::client::sync::sync_events::v3::JoinedRoom, OwnedRoomId};
use serde_json::{from_value as from_json_value, Value as JsonValue};

use super::{EphemeralTestEvent, RoomAccountDataTestEvent, StateTestEvent, TimelineTestEvent};
use crate::test_json;

pub struct JoinedRoomBuilder {
    pub(super) room_id: OwnedRoomId,
    pub(super) inner: JoinedRoom,
}

impl JoinedRoomBuilder {
    /// Create a new `JoinedRoomBuilder` for the given room ID.
    ///
    /// If the room ID is [`test_json::DEFAULT_SYNC_ROOM_ID`],
    /// [`JoinedRoomBuilder::default()`] can be used instead.
    pub fn new(room_id: impl Into<OwnedRoomId>) -> Self {
        Self { room_id: room_id.into(), inner: Default::default() }
    }

    /// Add an event to the timeline.
    pub fn add_timeline_event(mut self, event: TimelineTestEvent) -> Self {
        self.inner.timeline.events.push(event.into_raw_event());
        self
    }

    /// Set the timeline as limited.
    pub fn set_timeline_limited(mut self) -> Self {
        self.inner.timeline.limited = true;
        self
    }

    /// Set the `prev_batch` of the timeline.
    pub fn set_timeline_prev_batch(mut self, prev_batch: String) -> Self {
        self.inner.timeline.prev_batch = Some(prev_batch);
        self
    }

    /// Add an event to the state.
    pub fn add_state_event(mut self, event: StateTestEvent) -> Self {
        self.inner.state.events.push(event.into_raw_event());
        self
    }

    /// Add an ephemeral event.
    pub fn add_ephemeral_event(mut self, event: EphemeralTestEvent) -> Self {
        self.inner.ephemeral.events.push(event.into_raw_event());
        self
    }

    /// Add room account data.
    pub fn add_account_data(mut self, event: RoomAccountDataTestEvent) -> Self {
        self.inner.account_data.events.push(event.into_raw_event());
        self
    }

    /// Set the room summary.
    pub fn set_room_summary(mut self, summary: JsonValue) -> Self {
        self.inner.summary = from_json_value(summary).unwrap();
        self
    }

    /// Set the unread notifications count.
    pub fn set_unread_notifications_count(mut self, unread_notifications: JsonValue) -> Self {
        self.inner.unread_notifications = from_json_value(unread_notifications).unwrap();
        self
    }
}

impl Default for JoinedRoomBuilder {
    fn default() -> Self {
        Self::new(test_json::DEFAULT_SYNC_ROOM_ID.to_owned())
    }
}
