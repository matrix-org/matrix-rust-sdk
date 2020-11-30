use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use matrix_sdk_common::{
    api::r0::sync::sync_events::{self, DeviceLists},
    events::{presence::PresenceEvent, AnySyncRoomEvent, AnySyncStateEvent, AnyToDeviceEvent},
    identifiers::{DeviceKeyAlgorithm, RoomId},
};

use crate::store::StateChanges;

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct SyncResponse {
    /// The batch token to supply in the `since` param of the next `/sync` request.
    pub next_batch: String,
    /// Updates to rooms.
    pub rooms: Rooms,
    /// Updates to the presence status of other users.
    pub presence: Presence,
    ///// The global private data created by this user.
    //#[serde(default, skip_serializing_if = "AccountData::is_empty")]
    //pub account_data: AccountData,
    /// Messages sent dirrectly between devices.
    pub to_device: ToDevice,
    /// Information on E2E device updates.
    ///
    /// Only present on an incremental sync.
    pub device_lists: DeviceLists,
    /// For each key algorithm, the number of unclaimed one-time keys
    /// currently held on the server for a device.
    pub device_one_time_keys_count: BTreeMap<DeviceKeyAlgorithm, u64>,
}

impl SyncResponse {
    pub fn new(response: sync_events::Response, rooms: Rooms, changes: StateChanges) -> Self {
        Self {
            next_batch: response.next_batch,
            rooms,
            presence: Presence {
                events: changes.presence.into_iter().map(|(_, v)| v).collect(),
            },
            device_lists: response.device_lists,
            device_one_time_keys_count: response
                .device_one_time_keys_count
                .into_iter()
                .map(|(k, v)| (k, v.into()))
                .collect(),
            ..Default::default()
        }
    }

    pub fn new_empty(next_batch: String) -> Self {
        Self {
            next_batch,
            ..Default::default()
        }
    }
}

/// Updates to the presence status of other users.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Presence {
    /// A list of events.
    pub events: Vec<PresenceEvent>,
}

/// Messages sent dirrectly between devices.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct ToDevice {
    /// A list of events.
    pub events: Vec<AnyToDeviceEvent>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Rooms {
    // /// The rooms that the user has left or been banned from.
    // #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    // pub leave: BTreeMap<RoomId, LeftRoom>,
    /// The rooms that the user has joined.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub join: BTreeMap<RoomId, JoinedRoom>,
    // /// The rooms that the user has been invited to.
    // #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    // pub invite: BTreeMap<RoomId, InvitedRoom>,
}

/// Updates to joined rooms.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct JoinedRoom {
    // /// Information about the room which clients may need to correctly render it
    // /// to users.
    // #[serde(default, skip_serializing_if = "RoomSummary::is_empty")]
    // pub summary: RoomSummary,

    // /// Counts of unread notifications for this room.
    // #[serde(default, skip_serializing_if = "UnreadNotificationsCount::is_empty")]
    // pub unread_notifications: UnreadNotificationsCount,
    /// The timeline of messages and state changes in the room.
    pub timeline: Timeline,

    /// Updates to the state, between the time indicated by the `since` parameter, and the start
    /// of the `timeline` (or all state up to the start of the `timeline`, if `since` is not
    /// given, or `full_state` is true).
    pub state: State,
    // /// The private data that this user has attached to this room.
    // #[serde(default, skip_serializing_if = "AccountData::is_empty")]
    // pub account_data: AccountData,

    // /// The ephemeral events in the room that aren't recorded in the timeline or state of the
    // /// room. e.g. typing.
    // #[serde(default, skip_serializing_if = "Ephemeral::is_empty")]
    // pub ephemeral: Ephemeral,
}

impl JoinedRoom {
    pub fn new(timeline: Timeline, state: State) -> Self {
        Self { timeline, state }
    }
}

/// Events in the room.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Timeline {
    /// True if the number of events returned was limited by the `limit` on the filter.
    pub limited: bool,

    /// A token that can be supplied to to the `from` parameter of the
    /// `/rooms/{roomId}/messages` endpoint.
    pub prev_batch: Option<String>,

    /// A list of events.
    pub events: Vec<AnySyncRoomEvent>,
}

impl Timeline {
    pub fn new(limited: bool, prev_batch: Option<String>) -> Self {
        Self {
            limited,
            prev_batch,
            ..Default::default()
        }
    }
}

/// State events in the room.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct State {
    /// A list of state events.
    pub events: Vec<AnySyncStateEvent>,
}
