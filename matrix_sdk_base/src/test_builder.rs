#![cfg(test)]

use std::fs;
use std::panic;
use std::path::Path;

use crate::api::r0::sync::sync_events::Response as SyncResponse;
use crate::events::{
    collections::{
        all::{RoomEvent, StateEvent},
        only::Event,
    },
    presence::PresenceEvent,
    EventJson, TryFromRaw,
};
use http::Response;
use std::convert::TryFrom;

/// Easily create events to stream into either a Client or a `Room` for testing.
#[derive(Default)]
pub struct EventBuilder {
    /// The events that determine the state of a `Room`.
    room_events: Vec<RoomEvent>,
    /// The presence events that determine the presence state of a `RoomMember`.
    presence_events: Vec<PresenceEvent>,
    /// The state events that determine the state of a `Room`.
    state_events: Vec<StateEvent>,
    /// The ephemeral room events that determine the state of a `Room`.
    ephemeral: Vec<Event>,
    /// The account data events that determine the state of a `Room`.
    account_data: Vec<Event>,
}

impl EventBuilder {
    /// Add an event to the room events `Vec`.
    pub fn add_ephemeral_from_file<Ev: TryFromRaw, P: AsRef<Path>>(
        mut self,
        path: P,
        variant: fn(Ev) -> Event,
    ) -> Self {
        let val = fs::read_to_string(path.as_ref())
            .unwrap_or_else(|_| panic!("file not found {:?}", path.as_ref()));
        let event = serde_json::from_str::<EventJson<Ev>>(&val)
            .unwrap()
            .deserialize()
            .unwrap();
        self.ephemeral.push(variant(event));
        self
    }

    /// Add an event to the room events `Vec`.
    pub fn add_account_from_file<Ev: TryFromRaw, P: AsRef<Path>>(
        mut self,
        path: P,
        variant: fn(Ev) -> Event,
    ) -> Self {
        let val = fs::read_to_string(path.as_ref())
            .unwrap_or_else(|_| panic!("file not found {:?}", path.as_ref()));
        let event = serde_json::from_str::<EventJson<Ev>>(&val)
            .unwrap()
            .deserialize()
            .unwrap();
        self.account_data.push(variant(event));
        self
    }

    /// Add an event to the room events `Vec`.
    pub fn add_room_event_from_file<Ev: TryFromRaw, P: AsRef<Path>>(
        mut self,
        path: P,
        variant: fn(Ev) -> RoomEvent,
    ) -> Self {
        let val = fs::read_to_string(path.as_ref())
            .unwrap_or_else(|_| panic!("file not found {:?}", path.as_ref()));
        let event = serde_json::from_str::<EventJson<Ev>>(&val)
            .unwrap()
            .deserialize()
            .unwrap();
        self.room_events.push(variant(event));
        self
    }

    /// Add a state event to the state events `Vec`.
    pub fn add_state_event_from_file<Ev: TryFromRaw, P: AsRef<Path>>(
        mut self,
        path: P,
        variant: fn(Ev) -> StateEvent,
    ) -> Self {
        let val = fs::read_to_string(path.as_ref())
            .unwrap_or_else(|_| panic!("file not found {:?}", path.as_ref()));
        let event = serde_json::from_str::<EventJson<Ev>>(&val)
            .unwrap()
            .deserialize()
            .unwrap();
        self.state_events.push(variant(event));
        self
    }

    /// Add a presence event to the presence events `Vec`.
    pub fn add_presence_event_from_file<P: AsRef<Path>>(mut self, path: P) -> Self {
        let val = fs::read_to_string(path.as_ref())
            .unwrap_or_else(|_| panic!("file not found {:?}", path.as_ref()));
        let event = serde_json::from_str::<EventJson<PresenceEvent>>(&val)
            .unwrap()
            .deserialize()
            .unwrap();
        self.presence_events.push(event);
        self
    }

    /// Consumes `ResponseBuilder and returns SyncResponse.
    pub fn build_sync_response(self) -> SyncResponse {
        let body = serde_json::json! {
            {
                "device_one_time_keys_count": {},
                "next_batch": "s526_47314_0_7_1_1_1_11444_1",
                "device_lists": {
                    "changed": [],
                    "left": []
                },
                "rooms": {
                    "invite": {},
                    "join": {
                        "!SVkFJHzfwvuaIEawgC:localhost": {
                            "summary": {},
                            "account_data": {
                                "events": self.account_data
                            },
                            "ephemeral": {
                                "events": self.ephemeral
                            },
                            "state": {
                                "events": self.state_events
                            },
                            "timeline": {
                                "events": self.room_events,
                                "limited": true,
                                "prev_batch": "t392-516_47314_0_7_1_1_1_11444_1"
                            },
                            "unread_notifications": {
                                "highlight_count": 0,
                                "notification_count": 11
                            }
                        }
                    },
                    "leave": {}
                },
                "to_device": {
                    "events": []
                },
                "presence": {
                    "events": []
                }
            }
        };
        let response = Response::builder()
            .body(serde_json::to_vec(&body).unwrap())
            .unwrap();
        SyncResponse::try_from(response).unwrap()
    }
}
