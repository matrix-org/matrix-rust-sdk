use ruma::{events::AnyRoomEvent, serde::Raw};
use serde_json::Value;

use crate::{event_builder::TimelineTestEvent, test_json};

/// Clones the given [`Value`] and adds a `room_id` to it
///
/// Adding the `room_id` conditionally with `cfg` directly to the lazy_static
/// test_json values is blocked by "experimental attributes on expressions, see
/// issue #15701 <https://github.com/rust-lang/rust/issues/15701> for more information"
pub fn value_with_room_id(value: &mut Value) {
    let room_id = Value::try_from(test_json::DEFAULT_SYNC_ROOM_ID.to_string()).expect("room_id");
    value.as_object_mut().expect("mutable test_json").insert("room_id".to_owned(), room_id);
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
    pub fn add_room_event(&mut self, event: TimelineTestEvent) -> &mut Self {
        let mut val = event.into_json_value();
        value_with_room_id(&mut val);

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
