use std::{collections::BTreeMap, convert::TryFrom};

use ruma::{
    api::client::r0::{
        push::get_notifications::Notification,
        sync::sync_events::{
            DeviceLists, Ephemeral, GlobalAccountData, InvitedRoom, Presence, RoomAccountData,
            State, ToDevice, UnreadNotificationsCount as RumaUnreadNotificationsCount,
        },
    },
    events::{
        room::member::MemberEventContent, AnyRoomEvent, AnySyncRoomEvent, StateEvent,
        StrippedStateEvent, SyncStateEvent, Unsigned,
    },
    identifiers::{DeviceKeyAlgorithm, EventId, RoomId, UserId},
    serde::Raw,
    DeviceIdBox, MilliSecondsSinceUnixEpoch,
};
use serde::{Deserialize, Serialize};

/// A change in ambiguity of room members that an `m.room.member` event
/// triggers.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct AmbiguityChange {
    /// Is the member that is contained in the state key of the `m.room.member`
    /// event itself ambiguous because of the event.
    pub member_ambiguous: bool,
    /// Has another user been disambiguated because of this event.
    pub disambiguated_member: Option<UserId>,
    /// Has another user become ambiguous because of this event.
    pub ambiguated_member: Option<UserId>,
}

/// Collection of ambiguioty changes that room member events trigger.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct AmbiguityChanges {
    /// A map from room id to a map of an event id to the `AmbiguityChange` that
    /// the event with the given id caused.
    pub changes: BTreeMap<RoomId, BTreeMap<EventId, AmbiguityChange>>,
}

/// The verification state of the device that sent an event to us.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub enum VerificationState {
    /// The device is trusted.
    Trusted,
    /// The device is not trusted.
    Untrusted,
    /// The device is not known to us.
    UnknownDevice,
}

/// The algorithm specific information of a decrypted event.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub enum AlgorithmInfo {
    /// The info if the event was encrypted using m.megolm.v1.aes-sha2
    MegolmV1AesSha2 {
        /// The curve25519 key of the device that created the megolm decryption
        /// key originally.
        curve25519_key: String,
        /// The signing keys that have created the megolm key that was used to
        /// decrypt this session. This map will usually contain a single ed25519
        /// key.
        sender_claimed_keys: BTreeMap<DeviceKeyAlgorithm, String>,
        /// Chain of curve25519 keys through which this session was forwarded,
        /// via m.forwarded_room_key events.
        forwarding_curve25519_key_chain: Vec<String>,
    },
}

/// Struct containing information on how an event was decrypted.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct EncryptionInfo {
    /// The user ID of the event sender, note this is untrusted data unless the
    /// `verification_state` is as well trusted.
    pub sender: UserId,
    /// The device ID of the device that sent us the event, note this is
    /// untrusted data unless `verification_state` is as well trusted.
    pub sender_device: DeviceIdBox,
    /// Information about the algorithm that was used to encrypt the event.
    pub algorithm_info: AlgorithmInfo,
    /// The verification state of the device that sent us the event, note this
    /// is the state of the device at the time of decryption. It may change in
    /// the future if a device gets verified or deleted.
    pub verification_state: VerificationState,
}

/// A customized version of a room event coming from a sync that holds optional
/// encryption info.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SyncRoomEvent {
    /// The actual event.
    pub event: Raw<AnySyncRoomEvent>,
    /// The encryption info about the event. Will be `None` if the event was not
    /// encrypted.
    pub encryption_info: Option<EncryptionInfo>,
}

impl SyncRoomEvent {
    /// Get the event id of this `SyncRoomEvent`
    pub fn event_id(&self) -> EventId {
        self.event.get_field::<EventId>("event_id").unwrap().unwrap()
    }
}

impl From<Raw<AnySyncRoomEvent>> for SyncRoomEvent {
    fn from(inner: Raw<AnySyncRoomEvent>) -> Self {
        Self { encryption_info: None, event: inner }
    }
}

impl From<Raw<AnyRoomEvent>> for SyncRoomEvent {
    fn from(inner: Raw<AnyRoomEvent>) -> Self {
        // FIXME: we should strip the room id from `Raw`
        Self { encryption_info: None, event: Raw::from_json(Raw::into_json(inner)) }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct SyncResponse {
    /// The batch token to supply in the `since` param of the next `/sync`
    /// request.
    pub next_batch: String,
    /// Updates to rooms.
    pub rooms: Rooms,
    /// Updates to the presence status of other users.
    pub presence: Presence,
    /// The global private data created by this user.
    pub account_data: GlobalAccountData,
    /// Messages sent directly between devices.
    pub to_device: ToDevice,
    /// Information on E2E device updates.
    ///
    /// Only present on an incremental sync.
    pub device_lists: DeviceLists,
    /// For each key algorithm, the number of unclaimed one-time keys
    /// currently held on the server for a device.
    pub device_one_time_keys_count: BTreeMap<DeviceKeyAlgorithm, u64>,
    /// Collection of ambiguity changes that room member events trigger.
    pub ambiguity_changes: AmbiguityChanges,
    /// New notifications per room.
    pub notifications: BTreeMap<RoomId, Vec<Notification>>,
}

impl SyncResponse {
    pub fn new(next_batch: String) -> Self {
        Self { next_batch, ..Default::default() }
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
    /// Updates to the state, between the time indicated by the `since`
    /// parameter, and the start of the `timeline` (or all state up to the
    /// start of the `timeline`, if `since` is not given, or `full_state` is
    /// true).
    pub state: State,
    /// The private data that this user has attached to this room.
    pub account_data: RoomAccountData,
    /// The ephemeral events in the room that aren't recorded in the timeline or
    /// state of the room. e.g. typing.
    pub ephemeral: Ephemeral,
}

impl JoinedRoom {
    pub fn new(
        timeline: Timeline,
        state: State,
        account_data: RoomAccountData,
        ephemeral: Ephemeral,
        unread_notifications: UnreadNotificationsCount,
    ) -> Self {
        Self { unread_notifications, timeline, state, account_data, ephemeral }
    }
}

/// Counts of unread notifications for a room.
#[derive(Copy, Clone, Debug, Default, Deserialize, Serialize)]
pub struct UnreadNotificationsCount {
    /// The number of unread notifications for this room with the highlight flag
    /// set.
    pub highlight_count: u64,
    /// The total number of unread notifications for this room.
    pub notification_count: u64,
}

impl From<RumaUnreadNotificationsCount> for UnreadNotificationsCount {
    fn from(notifications: RumaUnreadNotificationsCount) -> Self {
        Self {
            highlight_count: notifications.highlight_count.map(|c| c.into()).unwrap_or(0),
            notification_count: notifications.notification_count.map(|c| c.into()).unwrap_or(0),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct LeftRoom {
    /// The timeline of messages and state changes in the room up to the point
    /// when the user left.
    pub timeline: Timeline,
    /// Updates to the state, between the time indicated by the `since`
    /// parameter, and the start of the `timeline` (or all state up to the
    /// start of the `timeline`, if `since` is not given, or `full_state` is
    /// true).
    pub state: State,
    /// The private data that this user has attached to this room.
    pub account_data: RoomAccountData,
}

impl LeftRoom {
    pub fn new(timeline: Timeline, state: State, account_data: RoomAccountData) -> Self {
        Self { timeline, state, account_data }
    }
}

/// Events in the room.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Timeline {
    /// True if the number of events returned was limited by the `limit` on the
    /// filter.
    pub limited: bool,

    /// A token that can be supplied to to the `from` parameter of the
    /// `/rooms/{roomId}/messages` endpoint.
    pub prev_batch: Option<String>,

    /// A list of events.
    pub events: Vec<SyncRoomEvent>,
}

impl Timeline {
    pub fn new(limited: bool, prev_batch: Option<String>) -> Self {
        Self { limited, prev_batch, ..Default::default() }
    }
}

/// A slice of the timeline in the room.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct TimelineSlice {
    /// The `next_batch` or `from` token used to obtain this slice
    pub start: String,

    /// The `prev_batch` or `to` token used to obtain this slice
    /// If `None` this `TimelineSlice` is the begining of the room
    pub end: Option<String>,

    /// A list of events.
    pub events: Vec<SyncRoomEvent>,
}

impl TimelineSlice {
    pub fn new(events: Vec<SyncRoomEvent>, start: String, end: Option<String>) -> Self {
        Self { start, end, events }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(
    try_from = "SyncStateEvent<MemberEventContent>",
    into = "SyncStateEvent<MemberEventContent>"
)]
pub struct MemberEvent {
    pub content: MemberEventContent,
    pub event_id: EventId,
    pub origin_server_ts: MilliSecondsSinceUnixEpoch,
    pub prev_content: Option<MemberEventContent>,
    pub sender: UserId,
    pub state_key: UserId,
    pub unsigned: Unsigned,
}

impl TryFrom<SyncStateEvent<MemberEventContent>> for MemberEvent {
    type Error = ruma::identifiers::Error;

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
    type Error = ruma::identifiers::Error;

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

impl From<MemberEvent> for SyncStateEvent<MemberEventContent> {
    fn from(other: MemberEvent) -> SyncStateEvent<MemberEventContent> {
        SyncStateEvent {
            content: other.content,
            event_id: other.event_id,
            sender: other.sender,
            origin_server_ts: other.origin_server_ts,
            state_key: other.state_key.to_string(),
            prev_content: other.prev_content,
            unsigned: other.unsigned,
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
    type Error = ruma::identifiers::Error;

    fn try_from(event: StrippedStateEvent<MemberEventContent>) -> Result<Self, Self::Error> {
        Ok(StrippedMemberEvent {
            content: event.content,
            sender: event.sender,
            state_key: UserId::try_from(event.state_key)?,
        })
    }
}

impl From<StrippedMemberEvent> for StrippedStateEvent<MemberEventContent> {
    fn from(other: StrippedMemberEvent) -> Self {
        Self {
            content: other.content,
            sender: other.sender,
            state_key: other.state_key.to_string(),
        }
    }
}

/// A deserialized response for the rooms members API call.
///
/// [GET /_matrix/client/r0/rooms/{roomId}/members](https://matrix.org/docs/spec/client_server/r0.6.0#get-matrix-client-r0-rooms-roomid-members)
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct MembersResponse {
    /// The list of members events.
    pub chunk: Vec<MemberEvent>,
    /// Collection of ambiguity changes that room member events trigger.
    pub ambiguity_changes: AmbiguityChanges,
}
