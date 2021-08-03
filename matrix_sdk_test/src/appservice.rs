use ruma::{
    events::{AnyRoomEvent, AnySyncRoomEvent},
    room_id,
};
use serde_json::Value;

use crate::{test_json, EventsJson};

/// The `TransactionBuilder` struct can be used to easily generate valid
/// incoming appservice transactions in json value format for testing.
///
/// Usage is similar to [`super::EventBuilder`]
#[derive(Debug, Default)]
pub struct TransactionBuilder {
    events: Vec<AnyRoomEvent>,
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

        let event = serde_json::from_value::<AnySyncRoomEvent>(val.to_owned())
            .unwrap()
            .into_full_event(room_id!("!SVkFJHzfwvuaIEawgC:localhost"));

        self.events.push(event);
        self
    }

    /// Build the transaction
    #[cfg(feature = "appservice")]
    #[cfg_attr(feature = "docs", doc(cfg(appservice)))]
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
