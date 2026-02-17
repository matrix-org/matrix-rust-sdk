use ruma::{events::AnyRoomAccountDataEvent, serde::Raw};
use serde_json::{Value as JsonValue, from_value as from_json_value};

use crate::test_json;

/// Test events that can be added to the room account data.
pub enum RoomAccountDataTestEvent {
    FullyRead,
    MarkedUnread,
    Custom(JsonValue),
}

impl From<RoomAccountDataTestEvent> for JsonValue {
    fn from(val: RoomAccountDataTestEvent) -> Self {
        match val {
            RoomAccountDataTestEvent::FullyRead => test_json::sync_events::FULLY_READ.to_owned(),
            RoomAccountDataTestEvent::MarkedUnread => {
                test_json::sync_events::MARKED_UNREAD.to_owned()
            }
            RoomAccountDataTestEvent::Custom(json) => json,
        }
    }
}

impl From<RoomAccountDataTestEvent> for Raw<AnyRoomAccountDataEvent> {
    fn from(val: RoomAccountDataTestEvent) -> Self {
        from_json_value(val.into()).unwrap()
    }
}
