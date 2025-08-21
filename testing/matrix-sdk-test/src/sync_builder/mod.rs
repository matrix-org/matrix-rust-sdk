use std::collections::HashMap;

use http::Response;
use ruma::{
    OwnedRoomId, OwnedUserId, UserId,
    api::{
        IncomingResponse,
        client::sync::sync_events::v3::{
            InvitedRoom, JoinedRoom, KnockedRoom, LeftRoom, Response as SyncResponse, State,
        },
    },
    events::{
        AnyGlobalAccountDataEvent, AnySyncStateEvent, AnyToDeviceEvent, presence::PresenceEvent,
    },
    serde::Raw,
};
use serde_json::{Value as JsonValue, from_value as from_json_value, json};

use super::test_json;

mod bulk;
mod invited_room;
mod joined_room;
mod knocked_room;
mod left_room;
mod test_event;

pub use bulk::bulk_room_members;
pub use invited_room::InvitedRoomBuilder;
pub use joined_room::JoinedRoomBuilder;
pub use knocked_room::KnockedRoomBuilder;
pub use left_room::LeftRoomBuilder;
pub use test_event::{
    PresenceTestEvent, RoomAccountDataTestEvent, StateTestEvent, StrippedStateTestEvent,
};

/// The `SyncResponseBuilder` struct can be used to easily generate valid sync
/// responses for testing. These can be then fed into either `Client` or `Room`.
///
/// It supports generated a number of canned events, such as a member entering a
/// room, his power level and display name changing and similar. It also
/// supports insertion of custom events in the form of `EventsJson` values.
#[derive(Default)]
pub struct SyncResponseBuilder {
    /// Updates to joined `Room`s.
    joined_rooms: HashMap<OwnedRoomId, JoinedRoom>,
    /// Updates to invited `Room`s.
    invited_rooms: HashMap<OwnedRoomId, InvitedRoom>,
    /// Updates to left `Room`s.
    left_rooms: HashMap<OwnedRoomId, LeftRoom>,
    /// Updates to knocked `Room`s.
    knocked_rooms: HashMap<OwnedRoomId, KnockedRoom>,
    /// Events that determine the presence state of a user.
    presence: Vec<Raw<PresenceEvent>>,
    /// Global account data events.
    account_data: Vec<Raw<AnyGlobalAccountDataEvent>>,
    /// Internal counter to enable the `prev_batch` and `next_batch` of each
    /// sync response to vary.
    batch_counter: i64,
    /// The device lists of the user.
    changed_device_lists: Vec<OwnedUserId>,
    to_device_events: Vec<Raw<AnyToDeviceEvent>>,
}

impl SyncResponseBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a joined room to the next sync response.
    ///
    /// If a room with the same room ID already exists, it is replaced by this
    /// one.
    pub fn add_joined_room(&mut self, room: JoinedRoomBuilder) -> &mut Self {
        self.invited_rooms.remove(&room.room_id);
        self.left_rooms.remove(&room.room_id);
        self.knocked_rooms.remove(&room.room_id);
        self.joined_rooms.insert(room.room_id, room.inner);
        self
    }

    /// Add an invited room to the next sync response.
    ///
    /// If a room with the same room ID already exists, it is replaced by this
    /// one.
    pub fn add_invited_room(&mut self, room: InvitedRoomBuilder) -> &mut Self {
        self.joined_rooms.remove(&room.room_id);
        self.left_rooms.remove(&room.room_id);
        self.knocked_rooms.remove(&room.room_id);
        self.invited_rooms.insert(room.room_id, room.inner);
        self
    }

    /// Add a left room to the next sync response.
    ///
    /// If a room with the same room ID already exists, it is replaced by this
    /// one.
    pub fn add_left_room(&mut self, room: LeftRoomBuilder) -> &mut Self {
        self.joined_rooms.remove(&room.room_id);
        self.invited_rooms.remove(&room.room_id);
        self.knocked_rooms.remove(&room.room_id);
        self.left_rooms.insert(room.room_id, room.inner);
        self
    }

    /// Add a knocked room to the next sync response.
    ///
    /// If a room with the same room ID already exists, it is replaced by this
    /// one.
    pub fn add_knocked_room(&mut self, room: KnockedRoomBuilder) -> &mut Self {
        self.joined_rooms.remove(&room.room_id);
        self.invited_rooms.remove(&room.room_id);
        self.left_rooms.remove(&room.room_id);
        self.knocked_rooms.insert(room.room_id, room.inner);
        self
    }

    /// Add a presence event.
    pub fn add_presence_event(&mut self, event: PresenceTestEvent) -> &mut Self {
        let val = match event {
            PresenceTestEvent::Presence => test_json::PRESENCE.to_owned(),
            PresenceTestEvent::Custom(json) => json,
        };

        self.presence.push(from_json_value(val).unwrap());
        self
    }

    /// Add presence in bulk.
    pub fn add_presence_bulk<I>(&mut self, events: I) -> &mut Self
    where
        I: IntoIterator<Item = Raw<PresenceEvent>>,
    {
        self.presence.extend(events);
        self
    }

    /// Add global account data.
    pub fn add_global_account_data(
        &mut self,
        event: impl Into<Raw<AnyGlobalAccountDataEvent>>,
    ) -> &mut Self {
        self.account_data.push(event.into());
        self
    }

    /// Add custom global account data based on a JSON value.
    pub fn add_custom_global_account_data(&mut self, event: serde_json::Value) -> &mut Self {
        self.account_data.push(Raw::new(&event).unwrap().cast_unchecked());
        self
    }

    pub fn add_change_device(&mut self, user_id: &UserId) -> &mut Self {
        self.changed_device_lists.push(user_id.to_owned());
        self
    }

    /// Add a to device event.
    pub fn add_to_device_event(&mut self, event: JsonValue) -> &mut Self {
        self.to_device_events.push(from_json_value(event).unwrap());
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
        self.batch_counter += 1;
        let next_batch = self.generate_sync_token();

        let body = json! {
            {
                "device_one_time_keys_count": {},
                "next_batch": next_batch,
                "device_lists": {
                    "changed": self.changed_device_lists,
                    "left": [],
                },
                "rooms": {
                    "invite": self.invited_rooms,
                    "join": self.joined_rooms,
                    "leave": self.left_rooms,
                    "knock": self.knocked_rooms,
                },
                "to_device": {
                    "events": self.to_device_events,
                },
                "presence": {
                    "events": self.presence,
                },
                "account_data": {
                    "events": self.account_data,
                },
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
        self.invited_rooms.clear();
        self.joined_rooms.clear();
        self.left_rooms.clear();
        self.knocked_rooms.clear();
        self.presence.clear();
    }
}

/// Helper trait to mutate the data in [`State`].
trait StateMutExt {
    /// Use the `After` variant rather than `Before`.
    fn use_state_after(&mut self);
    /// Access the inner list of state events.
    fn events_mut(&mut self) -> &mut Vec<Raw<AnySyncStateEvent>>;
}

impl StateMutExt for State {
    fn use_state_after(&mut self) {
        *self = Self::After(Default::default());
    }

    fn events_mut(&mut self) -> &mut Vec<Raw<AnySyncStateEvent>> {
        match self {
            Self::Before(state) => &mut state.events,
            Self::After(state) => &mut state.events,
            // We don't allow to construct another variant.
            _ => unreachable!(),
        }
    }
}
