use ruma::{
    OwnedRoomId, RoomId,
    api::client::sync::sync_events::v3::JoinedRoom,
    events::{
        AnyRoomAccountDataEvent, AnySyncStateEvent, AnySyncTimelineEvent,
        receipt::ReceiptEventContent, typing::TypingEventContent,
    },
    serde::Raw,
};
use serde_json::{Value as JsonValue, from_value as from_json_value};

use super::{RoomAccountDataTestEvent, StateMutExt};
use crate::{DEFAULT_TEST_ROOM_ID, event_factory::EventBuilder};

#[derive(Debug, Clone)]
pub struct JoinedRoomBuilder {
    pub(super) room_id: OwnedRoomId,
    pub(super) inner: JoinedRoom,
}

impl JoinedRoomBuilder {
    /// Create a new `JoinedRoomBuilder` for the given room ID.
    ///
    /// If the room ID is [`DEFAULT_TEST_ROOM_ID`],
    /// [`JoinedRoomBuilder::default()`] can be used instead.
    pub fn new(room_id: &RoomId) -> Self {
        Self { room_id: room_id.to_owned(), inner: Default::default() }
    }

    /// Get the room ID of this [`JoinedRoomBuilder`].
    pub fn room_id(&self) -> &RoomId {
        &self.room_id
    }

    /// Add an event to the timeline.
    ///
    /// The raw event can be created with the
    /// [`sync_timeline_event`](crate::sync_timeline_event) macro.
    pub fn add_timeline_event(mut self, event: impl Into<Raw<AnySyncTimelineEvent>>) -> Self {
        self.inner.timeline.events.push(event.into());
        self
    }

    /// Add events in bulk to the timeline.
    pub fn add_timeline_bulk<I>(mut self, events: I) -> Self
    where
        I: IntoIterator<Item = Raw<AnySyncTimelineEvent>>,
    {
        self.inner.timeline.events.extend(events);
        self
    }

    /// Add state events in bulk to the timeline.
    ///
    /// This is a convenience method that casts `Raw<AnySyncStateEvent>` to
    /// `Raw<AnySyncTimelineEvent>` and calls `JoinedRoom::add_timeline_bulk()`.
    pub fn add_timeline_state_bulk<I>(self, events: I) -> Self
    where
        I: IntoIterator<Item = Raw<AnySyncStateEvent>>,
    {
        let events = events.into_iter().map(|event| event.cast());
        self.add_timeline_bulk(events)
    }

    /// Set the timeline as limited.
    pub fn set_timeline_limited(mut self) -> Self {
        self.inner.timeline.limited = true;
        self
    }

    /// Set the `prev_batch` of the timeline.
    pub fn set_timeline_prev_batch(mut self, prev_batch: impl Into<String>) -> Self {
        self.inner.timeline.prev_batch = Some(prev_batch.into());
        self
    }

    /// Add state events to the `state_after` field rather than `state`.
    pub fn use_state_after(mut self) -> Self {
        self.inner.state.use_state_after();
        self
    }

    /// Add an event to the state.
    pub fn add_state_event(mut self, event: impl Into<Raw<AnySyncStateEvent>>) -> Self {
        self.inner.state.events_mut().push(event.into());
        self
    }

    /// Add events in bulk to the state.
    pub fn add_state_bulk<I>(mut self, events: I) -> Self
    where
        I: IntoIterator<Item = Raw<AnySyncStateEvent>>,
    {
        self.inner.state.events_mut().extend(events);
        self
    }

    /// Add a single read receipt to the joined room's ephemeral events.
    pub fn add_receipt(mut self, f: EventBuilder<ReceiptEventContent>) -> Self {
        self.inner.ephemeral.events.push(f.into());
        self
    }

    /// Add a typing notification event for this sync.
    pub fn add_typing(mut self, f: EventBuilder<TypingEventContent>) -> Self {
        self.inner.ephemeral.events.push(f.into());
        self
    }

    /// Add room account data.
    pub fn add_account_data(mut self, event: RoomAccountDataTestEvent) -> Self {
        self.inner.account_data.events.push(event.into());
        self
    }

    /// Add room account data in bulk.
    pub fn add_account_data_bulk<I>(mut self, events: I) -> Self
    where
        I: IntoIterator<Item = Raw<AnyRoomAccountDataEvent>>,
    {
        self.inner.account_data.events.extend(events);
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

    /// Set the joined members count in the room summary.
    pub fn set_joined_members_count(mut self, count: u32) -> Self {
        self.inner.summary.joined_member_count = Some(count.into());
        self
    }
}

impl Default for JoinedRoomBuilder {
    fn default() -> Self {
        Self::new(&DEFAULT_TEST_ROOM_ID)
    }
}
