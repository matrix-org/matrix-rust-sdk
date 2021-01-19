use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, convert::TryFrom, time::SystemTime};

use super::{
    api::r0::sync::sync_events::{
        DeviceLists, UnreadNotificationsCount as RumaUnreadNotificationsCount,
    },
    events::{
        presence::PresenceEvent, room::member::MemberEventContent, AnyBasicEvent,
        AnyStrippedStateEvent, AnySyncEphemeralRoomEvent, AnySyncRoomEvent, AnySyncStateEvent,
        AnyToDeviceEvent, StateEvent, StrippedStateEvent, SyncStateEvent, Unsigned,
    },
    identifiers::{DeviceKeyAlgorithm, EventId, RoomId, UserId},
};

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct SyncResponse {
    /// The batch token to supply in the `since` param of the next `/sync` request.
    pub next_batch: String,
    /// Updates to rooms.
    pub rooms: Rooms,
    /// Updates to the presence status of other users.
    pub presence: Presence,
    /// The global private data created by this user.
    pub account_data: AccountData,
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
    pub fn new(next_batch: String) -> Self {
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

/// Data that the user has attached to either the account or a specific room.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct AccountData {
    /// The list of account data events.
    pub events: Vec<AnyBasicEvent>,
}

/// Messages sent dirrectly between devices.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct ToDevice {
    /// A list of events.
    pub events: Vec<AnyToDeviceEvent>,
}

impl From<Vec<AnyToDeviceEvent>> for ToDevice {
    fn from(events: Vec<AnyToDeviceEvent>) -> Self {
        Self { events }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Rooms {
    /// The rooms that the user has left or been banned from.
    pub leave: BTreeMap<RoomId, LeftRoom>,
    /// The rooms that the user has joined.
    pub join: BTreeMap<RoomId, JoinedRoom>,
    /// The rooms that the user has been invited to.
    pub invite: BTreeMap<RoomId, InvitedRoom>,
}

/// Updates to joined rooms.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct JoinedRoom {
    /// Counts of unread notifications for this room.
    pub unread_notifications: UnreadNotificationsCount,
    /// The timeline of messages and state changes in the room.
    pub timeline: Timeline,
    /// Updates to the state, between the time indicated by the `since` parameter, and the start
    /// of the `timeline` (or all state up to the start of the `timeline`, if `since` is not
    /// given, or `full_state` is true).
    pub state: State,
    /// The private data that this user has attached to this room.
    pub account_data: AccountData,
    /// The ephemeral events in the room that aren't recorded in the timeline or state of the
    /// room. e.g. typing.
    pub ephemeral: Ephemeral,
}

impl JoinedRoom {
    pub fn new(
        timeline: Timeline,
        state: State,
        account_data: AccountData,
        ephemeral: Ephemeral,
        unread_notifications: UnreadNotificationsCount,
    ) -> Self {
        Self {
            timeline,
            state,
            account_data,
            ephemeral,
            unread_notifications,
        }
    }
}

/// Updates to the rooms that the user has been invited to.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct InvitedRoom {
    /// The state of a room that the user has been invited to.
    pub invite_state: InviteState,
}

/// The state of a room that the user has been invited to.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct InviteState {
    /// A list of state events.
    pub events: Vec<AnyStrippedStateEvent>,
}

/// Counts of unread notifications for a room.
#[derive(Copy, Clone, Debug, Default, Deserialize, Serialize)]
pub struct UnreadNotificationsCount {
    /// The number of unread notifications for this room with the highlight flag set.
    highlight_count: u64,
    /// The total number of unread notifications for this room.
    notification_count: u64,
}

impl From<RumaUnreadNotificationsCount> for UnreadNotificationsCount {
    fn from(notifications: RumaUnreadNotificationsCount) -> Self {
        Self {
            highlight_count: notifications.highlight_count.map(|c| c.into()).unwrap_or(0),
            notification_count: notifications
                .notification_count
                .map(|c| c.into())
                .unwrap_or(0),
        }
    }
}

/// The ephemeral events in the room that aren't recorded in the timeline or
/// state of the room. e.g. typing.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Ephemeral {
    pub events: Vec<AnySyncEphemeralRoomEvent>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct LeftRoom {
    /// The timeline of messages and state changes in the room up to the point
    /// when the user left.
    pub timeline: Timeline,
    /// Updates to the state, between the time indicated by the `since` parameter, and the start
    /// of the `timeline` (or all state up to the start of the `timeline`, if `since` is not
    /// given, or `full_state` is true).
    pub state: State,
    /// The private data that this user has attached to this room.
    pub account_data: AccountData,
}

impl LeftRoom {
    pub fn new(timeline: Timeline, state: State, account_data: AccountData) -> Self {
        Self {
            timeline,
            state,
            account_data,
        }
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

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(
    try_from = "SyncStateEvent<MemberEventContent>",
    into = "SyncStateEvent<MemberEventContent>"
)]
pub struct MemberEvent {
    pub content: MemberEventContent,
    pub event_id: EventId,
    pub origin_server_ts: SystemTime,
    pub prev_content: Option<MemberEventContent>,
    pub sender: UserId,
    pub state_key: UserId,
    pub unsigned: Unsigned,
}

impl TryFrom<SyncStateEvent<MemberEventContent>> for MemberEvent {
    type Error = super::identifiers::Error;

    fn try_from(event: SyncStateEvent<MemberEventContent>) -> Result<Self, Self::Error> {
        Ok(MemberEvent {
            content: event.content,
            event_id: event.event_id,
            origin_server_ts: event.origin_server_ts,
            prev_content: event.prev_content,
            sender: event.sender,
            state_key: UserId::try_from(event.state_key)?,
            unsigned: event.unsigned,
        })
    }
}

impl TryFrom<StateEvent<MemberEventContent>> for MemberEvent {
    type Error = super::identifiers::Error;

    fn try_from(event: StateEvent<MemberEventContent>) -> Result<Self, Self::Error> {
        Ok(MemberEvent {
            content: event.content,
            event_id: event.event_id,
            origin_server_ts: event.origin_server_ts,
            prev_content: event.prev_content,
            sender: event.sender,
            state_key: UserId::try_from(event.state_key)?,
            unsigned: event.unsigned,
        })
    }
}

impl Into<SyncStateEvent<MemberEventContent>> for MemberEvent {
    fn into(self) -> SyncStateEvent<MemberEventContent> {
        SyncStateEvent {
            content: self.content,
            event_id: self.event_id,
            sender: self.sender,
            origin_server_ts: self.origin_server_ts,
            state_key: self.state_key.to_string(),
            prev_content: self.prev_content,
            unsigned: self.unsigned,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(
    try_from = "StrippedStateEvent<MemberEventContent>",
    into = "StrippedStateEvent<MemberEventContent>"
)]
pub struct StrippedMemberEvent {
    pub content: MemberEventContent,
    pub sender: UserId,
    pub state_key: UserId,
}

impl TryFrom<StrippedStateEvent<MemberEventContent>> for StrippedMemberEvent {
    type Error = super::identifiers::Error;

    fn try_from(event: StrippedStateEvent<MemberEventContent>) -> Result<Self, Self::Error> {
        Ok(StrippedMemberEvent {
            content: event.content,
            sender: event.sender,
            state_key: UserId::try_from(event.state_key)?,
        })
    }
}

impl Into<StrippedStateEvent<MemberEventContent>> for StrippedMemberEvent {
    fn into(self) -> StrippedStateEvent<MemberEventContent> {
        StrippedStateEvent {
            content: self.content,
            sender: self.sender,
            state_key: self.state_key.to_string(),
        }
    }
}
