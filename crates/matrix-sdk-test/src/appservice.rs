use std::convert::TryFrom;

use ruma::{events::AnyRoomEvent, serde::Raw};
use serde_json::Value;

use crate::{test_json, EventsJson};

/// Clones the given [`Value`] and adds a `room_id` to it
///
/// Adding the `room_id` conditionally with `cfg` directly to the lazy_static
/// test_json values is blocked by "experimental attributes on expressions, see
/// issue #15701 <https://github.com/rust-lang/rust/issues/15701> for more information"
pub fn value_with_room_id(value: &Value) -> Value {
    let mut val = value.clone();
    let room_id = Value::try_from(test_json::DEFAULT_SYNC_ROOM_ID.to_string()).expect("room_id");
    val.as_object_mut().expect("mutable test_json").insert("room_id".to_owned(), room_id);

    val
}

/// The `TransactionBuilder` struct can be used to easily generate valid
/// incoming appservice transactions in json value format for testing.
///
/// Usage is similar to [`super::EventBuilder`]
#[derive(Debug, Default)]
pub struct TransactionBuilder {
    events: Vec<Raw<AnyRoomEvent>>,
}

impl TransactionBuilder {
    pub fn new() -> Self {
        Default::default()
    }

    /// Add a room event.
    pub fn add_room_event(&mut self, json: EventsJson) -> &mut Self {
        let val: &Value = match json {
            EventsJson::Member => &test_json::MEMBER,
            EventsJson::MemberNameChange => &test_json::MEMBER_NAME_CHANGE,
            EventsJson::PowerLevels => &test_json::POWER_LEVELS,
            _ => panic!("unknown event json {:?}", json),
        };

        let val = value_with_room_id(val);

        let event = serde_json::from_value(val).unwrap();

        self.events.push(event);
        self
    }

    /// Build the transaction
    #[cfg(feature = "appservice")]
    pub fn build_json_transaction(&self) -> Value {
        let body = serde_json::json! {
            {
                "events": self.events
            }
        };

        body
    }

    pub fn clear(&mut self) {
        self.events.clear();
    }
}
