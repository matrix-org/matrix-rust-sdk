use std::collections::HashMap;
use std::convert::TryFrom;
use std::panic;

use http::Response;

use matrix_sdk_common::api::r0::sync::sync_events::Response as SyncResponse;
use matrix_sdk_common::events::{
    presence::PresenceEvent, AnyBasicEvent, AnyEphemeralRoomEventStub, AnyRoomEventStub,
    AnyStateEventStub,
};
use matrix_sdk_common::identifiers::RoomId;
use serde_json::Value as JsonValue;

pub use matrix_sdk_test_macros::async_test;

pub mod test_json;

/// Embedded event files
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
    joined_room_events: HashMap<RoomId, Vec<AnyRoomEventStub>>,
    /// The events that determine the state of a `Room`.
    invited_room_events: HashMap<RoomId, Vec<AnyStateEventStub>>,
    /// The events that determine the state of a `Room`.
    left_room_events: HashMap<RoomId, Vec<AnyRoomEventStub>>,
    /// The presence events that determine the presence state of a `RoomMember`.
    presence_events: Vec<PresenceEvent>,
    /// The state events that determine the state of a `Room`.
    state_events: Vec<AnyStateEventStub>,
    /// The ephemeral room events that determine the state of a `Room`.
    ephemeral: Vec<AnyEphemeralRoomEventStub>,
    /// The account data events that determine the state of a `Room`.
    account_data: Vec<AnyBasicEvent>,
    /// Internal counter to enable the `prev_batch` and `next_batch` of each sync response to vary.
    batch_counter: i64,
}

impl EventBuilder {
    pub fn new() -> Self {
        let builder: EventBuilder = Default::default();
        builder
    }

    /// Add an event to the room events `Vec`.
    pub fn add_ephemeral(&mut self, json: EventsJson) -> &mut Self {
        let val: &JsonValue = match json {
            EventsJson::Typing => &test_json::TYPING,
            _ => panic!("unknown ephemeral event {:?}", json),
        };

        let event = serde_json::from_value::<AnyEphemeralRoomEventStub>(val.clone()).unwrap();
        self.ephemeral.push(event);
        self
    }

    /// Add an event to the room events `Vec`.
    #[allow(clippy::match_single_binding, unused)]
    pub fn add_account(&mut self, json: EventsJson) -> &mut Self {
        let val: &JsonValue = match json {
            _ => panic!("unknown account event {:?}", json),
        };

        let event = serde_json::from_value::<AnyBasicEvent>(val.clone()).unwrap();
        self.account_data.push(event);
        self
    }

    /// Add an event to the room events `Vec`.
    pub fn add_room_event(&mut self, json: EventsJson) -> &mut Self {
        let val: &JsonValue = match json {
            EventsJson::Member => &test_json::MEMBER,
            EventsJson::PowerLevels => &test_json::POWER_LEVELS,
            _ => panic!("unknown room event json {:?}", json),
        };

        let event = serde_json::from_value::<AnyRoomEventStub>(val.clone()).unwrap();

        self.add_joined_event(
            &RoomId::try_from("!SVkFJHzfwvuaIEawgC:localhost").unwrap(),
            event,
        );
        self
    }

    pub fn add_custom_joined_event(
        &mut self,
        room_id: &RoomId,
        event: serde_json::Value,
    ) -> &mut Self {
        let event = serde_json::from_value::<AnyRoomEventStub>(event).unwrap();
        self.add_joined_event(room_id, event);
        self
    }

    fn add_joined_event(&mut self, room_id: &RoomId, event: AnyRoomEventStub) {
        self.joined_room_events
            .entry(room_id.clone())
            .or_insert_with(Vec::new)
            .push(event);
    }

    pub fn add_custom_invited_event(
        &mut self,
        room_id: &RoomId,
        event: serde_json::Value,
    ) -> &mut Self {
        let event = serde_json::from_value::<AnyStateEventStub>(event).unwrap();
        self.invited_room_events
            .entry(room_id.clone())
            .or_insert_with(Vec::new)
            .push(event);
        self
    }

    pub fn add_custom_left_event(
        &mut self,
        room_id: &RoomId,
        event: serde_json::Value,
    ) -> &mut Self {
        let event = serde_json::from_value::<AnyRoomEventStub>(event).unwrap();
        self.left_room_events
            .entry(room_id.clone())
            .or_insert_with(Vec::new)
            .push(event);
        self
    }

    /// Add a state event to the state events `Vec`.
    pub fn add_state_event(&mut self, json: EventsJson) -> &mut Self {
        let val: &JsonValue = match json {
            EventsJson::Alias => &test_json::ALIAS,
            EventsJson::Aliases => &test_json::ALIASES,
            EventsJson::Name => &test_json::NAME,
            EventsJson::Member => &test_json::MEMBER,
            EventsJson::PowerLevels => &test_json::POWER_LEVELS,
            _ => panic!("unknown state event {:?}", json),
        };

        let event = serde_json::from_value::<AnyStateEventStub>(val.clone()).unwrap();
        self.state_events.push(event);
        self
    }

    /// Add an presence event to the presence events `Vec`.
    pub fn add_presence_event(&mut self, json: EventsJson) -> &mut Self {
        let val: &JsonValue = match json {
            EventsJson::Presence => &test_json::PRESENCE,
            _ => panic!("unknown presence event {:?}", json),
        };

        let event = serde_json::from_value::<PresenceEvent>(val.clone()).unwrap();
        self.presence_events.push(event);
        self
    }

    /// Consumes `ResponseBuilder` and returns `SyncResponse`.
    pub fn build_sync_response(&mut self) -> SyncResponse {
        let main_room_id = RoomId::try_from("!SVkFJHzfwvuaIEawgC:localhost").unwrap();

        // First time building a sync response, so initialize the `prev_batch` to a default one.
        let prev_batch = self.generate_sync_token();
        self.batch_counter += 1;
        let next_batch = self.generate_sync_token();

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
                "prev_batch": prev_batch
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
                    "prev_batch": prev_batch
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
                    "prev_batch": prev_batch
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
                "next_batch": next_batch,
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

    fn generate_sync_token(&self) -> String {
        format!("t392-516_47314_0_7_1_1_1_11444_{}", self.batch_counter)
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
