use std::{collections::HashMap, panic};

use http::Response;
pub use matrix_sdk_test_macros::async_test;
use ruma::{
    api::{client::sync::sync_events::v3::Response as SyncResponse, IncomingResponse},
    events::{
        presence::PresenceEvent, AnyGlobalAccountDataEvent, AnySyncEphemeralRoomEvent,
        AnySyncRoomEvent, AnySyncStateEvent,
    },
    room_id,
    serde::Raw,
    RoomId,
};
use serde_json::Value as JsonValue;

#[cfg(feature = "appservice")]
pub mod appservice;
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
    MemberInvite,
    MemberNameChange,
    MessageEmote,
    MessageNotice,
    MessageText,
    Name,
    PowerLevels,
    PushRules,
    Presence,
    ReadReceipt,
    ReadReceiptOther,
    RedactedInvalid,
    RedactedState,
    Redacted,
    Redaction,
    RoomAvatar,
    Tag,
    Topic,
    Typing,
}

/// The `EventBuilder` struct can be used to easily generate valid sync
/// responses for testing. These can be then fed into either `Client` or `Room`.
///
/// It supports generated a number of canned events, such as a member entering a
/// room, his power level and display name changing and similar. It also
/// supports insertion of custom events in the form of `EventsJson` values.
///
/// **Important** You *must* use the *same* builder when sending multiple sync
/// responses to a single client. Otherwise, the subsequent responses will be
/// *ignored* by the client because the `next_batch` sync token will not be
/// rotated properly.
///
/// # Example usage
///
/// ```rust
/// use matrix_sdk_test::{EventBuilder, EventsJson};
///
/// let mut builder = EventBuilder::new();
///
/// // response1 now contains events that add an example member to the room and change their power
/// // level
/// let response1 = builder
///     .add_room_event(EventsJson::Member)
///     .add_room_event(EventsJson::PowerLevels)
///     .build_sync_response();
///
/// // response2 is now empty (nothing changed)
/// let response2 = builder.build_sync_response();
///
/// // response3 contains a display name change for member example
/// let response3 = builder
///     .add_room_event(EventsJson::MemberNameChange)
///     .build_sync_response();
/// ```

#[derive(Default)]
pub struct EventBuilder {
    /// The events that determine the state of a `Room`.
    joined_room_events: HashMap<Box<RoomId>, Vec<Raw<AnySyncRoomEvent>>>,
    /// The events that determine the state of a `Room`.
    invited_room_events: HashMap<Box<RoomId>, Vec<Raw<AnySyncStateEvent>>>,
    /// The events that determine the state of a `Room`.
    left_room_events: HashMap<Box<RoomId>, Vec<Raw<AnySyncRoomEvent>>>,
    /// The presence events that determine the presence state of a `RoomMember`.
    presence_events: Vec<PresenceEvent>,
    /// The state events that determine the state of a `Room`.
    state_events: Vec<Raw<AnySyncStateEvent>>,
    /// The ephemeral room events that determine the state of a `Room`.
    ephemeral: Vec<Raw<AnySyncEphemeralRoomEvent>>,
    /// The account data events that determine the state of a `Room`.
    account_data: Vec<Raw<AnyGlobalAccountDataEvent>>,
    /// Internal counter to enable the `prev_batch` and `next_batch` of each
    /// sync response to vary.
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
            EventsJson::ReadReceipt => &test_json::READ_RECEIPT,
            EventsJson::ReadReceiptOther => &test_json::READ_RECEIPT_OTHER,
            EventsJson::Typing => &test_json::TYPING,
            _ => panic!("unknown ephemeral event {:?}", json),
        };

        let event = serde_json::from_value(val.clone()).unwrap();
        self.ephemeral.push(event);
        self
    }

    /// Add an event to the room events `Vec`.
    #[allow(unused)]
    pub fn add_account(&mut self, json: EventsJson) -> &mut Self {
        let val: &JsonValue = match json {
            EventsJson::PushRules => &test_json::PUSH_RULES,
            _ => panic!("unknown account event {:?}", json),
        };

        let event = serde_json::from_value(val.clone()).unwrap();
        self.account_data.push(event);
        self
    }

    /// Add an event to the room events `Vec`.
    pub fn add_room_event(&mut self, json: EventsJson) -> &mut Self {
        let val: &JsonValue = match json {
            EventsJson::Member => &test_json::MEMBER,
            EventsJson::MemberInvite => &test_json::MEMBER_INVITE,
            EventsJson::MemberNameChange => &test_json::MEMBER_NAME_CHANGE,
            EventsJson::PowerLevels => &test_json::POWER_LEVELS,
            _ => panic!("unknown room event json {:?}", json),
        };

        let event = serde_json::from_value(val.clone()).unwrap();

        self.add_joined_event(room_id!("!SVkFJHzfwvuaIEawgC:localhost"), event);
        self
    }

    pub fn add_custom_joined_event(
        &mut self,
        room_id: &RoomId,
        event: serde_json::Value,
    ) -> &mut Self {
        let event = serde_json::from_value(event).unwrap();
        self.add_joined_event(room_id, event);
        self
    }

    fn add_joined_event(&mut self, room_id: &RoomId, event: Raw<AnySyncRoomEvent>) {
        self.joined_room_events.entry(room_id.to_owned()).or_insert_with(Vec::new).push(event);
    }

    pub fn add_custom_invited_event(
        &mut self,
        room_id: &RoomId,
        event: serde_json::Value,
    ) -> &mut Self {
        let event = serde_json::from_value(event).unwrap();
        self.invited_room_events.entry(room_id.to_owned()).or_insert_with(Vec::new).push(event);
        self
    }

    pub fn add_custom_left_event(
        &mut self,
        room_id: &RoomId,
        event: serde_json::Value,
    ) -> &mut Self {
        let event = serde_json::from_value(event).unwrap();
        self.left_room_events.entry(room_id.to_owned()).or_insert_with(Vec::new).push(event);
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

        let event = serde_json::from_value(val.clone()).unwrap();
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

    /// Builds a sync response as a JSON Value containing the events we queued
    /// so far.
    ///
    /// The next response returned by `build_sync_response` will then be empty
    /// if no further events were queued.
    ///
    /// This method is raw JSON equivalent to
    /// [build_sync_response()](#method.build_sync_response), use
    /// [build_sync_response()](#method.build_sync_response) if you need a typed
    /// response.
    pub fn build_json_sync_response(&mut self) -> JsonValue {
        let main_room_id = room_id!("!SVkFJHzfwvuaIEawgC:localhost");

        // First time building a sync response, so initialize the `prev_batch` to a
        // default one.
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
                "events": self.joined_room_events.remove(main_room_id).unwrap_or_default(),
                "limited": true,
                "prev_batch": prev_batch
            },
            "unread_notifications": {
                "highlight_count": 0,
                "notification_count": 11
            }
        });

        let mut joined_rooms: HashMap<Box<RoomId>, serde_json::Value> = HashMap::new();

        joined_rooms.insert(main_room_id.to_owned(), joined_room);

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

        let mut left_rooms: HashMap<Box<RoomId>, serde_json::Value> = HashMap::new();

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

        let mut invited_rooms: HashMap<Box<RoomId>, serde_json::Value> = HashMap::new();

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

        // Clear state so that the next sync response will be empty if nothing
        // was added.
        self.clear();

        body
    }

    /// Builds a `SyncResponse` containing the events we queued so far.
    ///
    /// The next response returned by `build_sync_response` will then be empty
    /// if no further events were queued.
    ///
    /// This method is high level and typed equivalent to
    /// [build_json_sync_response()](#method.build_json_sync_response), use
    /// [build_json_sync_response()](#method.build_json_sync_response) if you
    /// need an untyped response.
    pub fn build_sync_response(&mut self) -> SyncResponse {
        let body = self.build_json_sync_response();

        let response = Response::builder().body(serde_json::to_vec(&body).unwrap()).unwrap();

        SyncResponse::try_from_http_response(response).unwrap()
    }

    fn generate_sync_token(&self) -> String {
        format!("t392-516_47314_0_7_1_1_1_11444_{}", self.batch_counter)
    }

    pub fn clear(&mut self) {
        self.account_data.clear();
        self.ephemeral.clear();
        self.invited_room_events.clear();
        self.joined_room_events.clear();
        self.left_room_events.clear();
        self.presence_events.clear();
        self.state_events.clear();
    }
}

/// Embedded sync response files
pub enum SyncResponseFile {
    All,
    Default,
    DefaultWithSummary,
    Invite,
    Leave,
    Voip,
}

/// Get specific API responses for testing
pub fn sync_response(kind: SyncResponseFile) -> SyncResponse {
    let data: &JsonValue = match kind {
        SyncResponseFile::All => &test_json::MORE_SYNC,
        SyncResponseFile::Default => &test_json::SYNC,
        SyncResponseFile::DefaultWithSummary => &test_json::DEFAULT_SYNC_SUMMARY,
        SyncResponseFile::Invite => &test_json::INVITE_SYNC,
        SyncResponseFile::Leave => &test_json::LEAVE_SYNC,
        SyncResponseFile::Voip => &test_json::VOIP_SYNC,
    };

    let response = Response::builder().body(data.to_string().as_bytes().to_vec()).unwrap();
    SyncResponse::try_from_http_response(response).unwrap()
}

pub fn response_from_file(json: &serde_json::Value) -> Response<Vec<u8>> {
    Response::builder().status(200).body(json.to_string().as_bytes().to_vec()).unwrap()
}
