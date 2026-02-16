use ruma::{event_id, events::AnyRoomAccountDataEvent, serde::Raw};
use serde_json::Value as JsonValue;

use crate::event_factory::EventFactory;

/// Test events that can be added to the room account data.
pub enum RoomAccountDataTestEvent {
    FullyRead,
    MarkedUnread,
    Custom(JsonValue),
}

impl From<RoomAccountDataTestEvent> for Raw<AnyRoomAccountDataEvent> {
    fn from(val: RoomAccountDataTestEvent) -> Self {
        let f = EventFactory::new();
        match val {
            RoomAccountDataTestEvent::FullyRead => {
                f.fully_read(event_id!("$someplace:example.org")).into()
            }
            RoomAccountDataTestEvent::MarkedUnread => f.marked_unread(true).into(),
            RoomAccountDataTestEvent::Custom(json) => {
                serde_json::from_value(json).expect("Custom JSON should be valid")
            }
        }
    }
}
