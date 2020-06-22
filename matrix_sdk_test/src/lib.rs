use std::collections::HashMap;
use std::convert::TryFrom;
use std::panic;

use http::Response;

use matrix_sdk_common::api::r0::sync::sync_events::Response as SyncResponse;
use matrix_sdk_common::events::{
    collections::{
        all::{RoomEvent, StateEvent},
        only::Event,
    },
    presence::PresenceEvent,
    stripped::AnyStrippedStateEvent,
    EventJson, TryFromRaw,
};
use matrix_sdk_common::identifiers::RoomId;
use serde_json::Value as JsonValue;

pub use matrix_sdk_test_macros::async_test;

pub mod test_json;

/// Static `serde_json::Value`s
#[derive(Debug)]
pub enum EventsJson {
    Alias,
    Aliases,
    Create,
    FullyRead,
    HistoryVisibility,
    JoinRules,
    Member,
    MessageEmote,
    MessageNotice,
    MessageText,
    Name,
    PowerLevels,
    Presence,
    RedactedInvalid,
    RedactedState,
    Redacted,
    Redaction,
    RoomAvatar,
    Tag,
    Topic,
    Typing,
}

/// Easily create events to stream into either a Client or a `Room` for testing.
#[derive(Default)]
pub struct EventBuilder {
    /// The events that determine the state of a `Room`.
    joined_room_events: HashMap<RoomId, Vec<RoomEvent>>,
    /// The events that determine the state of a `Room`.
    invited_room_events: HashMap<RoomId, Vec<AnyStrippedStateEvent>>,
    /// The events that determine the state of a `Room`.
    left_room_events: HashMap<RoomId, Vec<RoomEvent>>,
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
    pub fn add_ephemeral<Ev: TryFromRaw>(
        mut self,
        json: EventsJson,
        variant: fn(Ev) -> Event,
    ) -> Self {
        let val: &JsonValue = match json {
            EventsJson::Typing => &test_json::TYPING,
            _ => panic!("unknown ephemeral event {:?}", json),
        };

        let event = serde_json::from_value::<EventJson<Ev>>(val.clone())
            .unwrap()
            .deserialize()
            .unwrap();
        self.ephemeral.push(variant(event));
        self
    }

    /// Add an event to the room events `Vec`.
    #[allow(clippy::match_single_binding, unused)]
    pub fn add_account<Ev: TryFromRaw>(
        mut self,
        json: EventsJson,
        variant: fn(Ev) -> Event,
    ) -> Self {
        let val: &JsonValue = match json {
            _ => panic!("unknown account event {:?}", json),
        };

        let event = serde_json::from_value::<EventJson<Ev>>(val.clone())
            .unwrap()
            .deserialize()
            .unwrap();
        self.account_data.push(variant(event));
        self
    }

    /// Add an event to the room events `Vec`.
    pub fn add_room_event<Ev: TryFromRaw>(
        mut self,
        json: EventsJson,
        variant: fn(Ev) -> RoomEvent,
    ) -> Self {
        let val: &JsonValue = match json {
            EventsJson::Member => &test_json::MEMBER,
            EventsJson::PowerLevels => &test_json::POWER_LEVELS,
            _ => panic!("unknown room event json {:?}", json),
        };

        let event = serde_json::from_value::<EventJson<Ev>>(val.clone())
            .unwrap()
            .deserialize()
            .unwrap();
        self.add_joined_event(
            &RoomId::try_from("!SVkFJHzfwvuaIEawgC:localhost").unwrap(),
            variant(event),
        );
        self
    }

    pub fn add_custom_joined_event<Ev: TryFromRaw>(
        mut self,
        room_id: &RoomId,
        event: serde_json::Value,
        variant: fn(Ev) -> RoomEvent,
    ) -> Self {
        let event = serde_json::from_value::<EventJson<Ev>>(event)
            .unwrap()
            .deserialize()
            .unwrap();
        self.add_joined_event(room_id, variant(event));
        self
    }

    fn add_joined_event(&mut self, room_id: &RoomId, event: RoomEvent) {
        self.joined_room_events
            .entry(room_id.clone())
            .or_insert_with(Vec::new)
            .push(event);
    }

    pub fn add_custom_invited_event<Ev: TryFromRaw>(
        mut self,
        room_id: &RoomId,
        event: serde_json::Value,
        variant: fn(Ev) -> AnyStrippedStateEvent,
    ) -> Self {
        let event = serde_json::from_value::<EventJson<Ev>>(event)
            .unwrap()
            .deserialize()
            .unwrap();
        self.invited_room_events
            .entry(room_id.clone())
            .or_insert_with(Vec::new)
            .push(variant(event));
        self
    }

    pub fn add_custom_left_event<Ev: TryFromRaw>(
        mut self,
        room_id: &RoomId,
        event: serde_json::Value,
        variant: fn(Ev) -> RoomEvent,
    ) -> Self {
        let event = serde_json::from_value::<EventJson<Ev>>(event)
            .unwrap()
            .deserialize()
            .unwrap();
        self.left_room_events
            .entry(room_id.clone())
            .or_insert_with(Vec::new)
            .push(variant(event));
        self
    }

    /// Add a state event to the state events `Vec`.
    pub fn add_state_event<Ev: TryFromRaw>(
        mut self,
        json: EventsJson,
        variant: fn(Ev) -> StateEvent,
    ) -> Self {
        let val: &JsonValue = match json {
            EventsJson::Alias => &test_json::ALIAS,
            EventsJson::Aliases => &test_json::ALIASES,
            EventsJson::Name => &test_json::NAME,
            _ => panic!("unknown state event {:?}", json),
        };

        let event = serde_json::from_value::<EventJson<Ev>>(val.clone())
            .unwrap()
            .deserialize()
            .unwrap();
        self.state_events.push(variant(event));
        self
    }

    /// Add an presence event to the presence events `Vec`.
    pub fn add_presence_event(mut self, json: EventsJson) -> Self {
        let val: &JsonValue = match json {
            EventsJson::Presence => &test_json::PRESENCE,
            _ => panic!("unknown presence event {:?}", json),
        };

        let event = serde_json::from_value::<EventJson<PresenceEvent>>(val.clone())
            .unwrap()
            .deserialize()
            .unwrap();
        self.presence_events.push(event);
        self
    }

    /// Consumes `ResponseBuilder and returns SyncResponse.
    pub fn build_sync_response(mut self) -> SyncResponse {
        let main_room_id = RoomId::try_from("!SVkFJHzfwvuaIEawgC:localhost").unwrap();

        // TODO generalize this.
        let joined_room = serde_json::json!({
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
                "events": self.joined_room_events.remove(&main_room_id).unwrap_or_default(),
                "limited": true,
                "prev_batch": "t392-516_47314_0_7_1_1_1_11444_1"
            },
            "unread_notifications": {
                "highlight_count": 0,
                "notification_count": 11
            }
        });

        let mut joined_rooms: HashMap<RoomId, serde_json::Value> = HashMap::new();

        joined_rooms.insert(main_room_id, joined_room);

        for (room_id, events) in self.joined_room_events.drain() {
            let joined_room = serde_json::json!({
                "summary": {},
                "account_data": {
                    "events": [],
                },
                "ephemeral": {
                    "events": [],
                },
                "state": {
                    "events": [],
                },
                "timeline": {
                    "events": events,
                    "limited": true,
                    "prev_batch": "t392-516_47314_0_7_1_1_1_11444_1"
                },
                "unread_notifications": {
                    "highlight_count": 0,
                    "notification_count": 11
                }
            });
            joined_rooms.insert(room_id, joined_room);
        }

        let mut left_rooms: HashMap<RoomId, serde_json::Value> = HashMap::new();

        for (room_id, events) in self.left_room_events.drain() {
            let room = serde_json::json!({
                "state": {
                    "events": [],
                },
                "timeline": {
                    "events": events,
                    "limited": false,
                    "prev_batch": "t392-516_47314_0_7_1_1_1_11444_1"
                },
            });
            left_rooms.insert(room_id, room);
        }

        let mut invited_rooms: HashMap<RoomId, serde_json::Value> = HashMap::new();

        for (room_id, events) in self.invited_room_events.drain() {
            let room = serde_json::json!({
                "invite_state": {
                    "events": events,
                },
            });
            invited_rooms.insert(room_id, room);
        }

        let body = serde_json::json! {
            {
                "device_one_time_keys_count": {},
                "next_batch": "s526_47314_0_7_1_1_1_11444_1",
                "device_lists": {
                    "changed": [],
                    "left": []
                },
                "rooms": {
                    "invite": invited_rooms,
                    "join": joined_rooms,
                    "leave": left_rooms,
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

/// Embedded sync reponse files
pub enum SyncResponseFile {
    All,
    Default,
    DefaultWithSummary,
    Invite,
    Leave,
}

/// Get specific API responses for testing
pub fn sync_response(kind: SyncResponseFile) -> SyncResponse {
    let data: &JsonValue = match kind {
        SyncResponseFile::All => &test_json::MORE_SYNC,
        SyncResponseFile::Default => &test_json::SYNC,
        SyncResponseFile::DefaultWithSummary => &test_json::DEFAULT_SYNC_SUMMARY,
        SyncResponseFile::Invite => &test_json::INVITE_SYNC,
        SyncResponseFile::Leave => &test_json::LEAVE_SYNC,
    };

    let response = Response::builder()
        .body(data.to_string().as_bytes().to_vec())
        .unwrap();
    SyncResponse::try_from(response).unwrap()
}
