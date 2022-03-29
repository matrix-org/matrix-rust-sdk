use std::{collections::BTreeMap, convert::TryFrom};

use ruma::{
    api::client::{
        push::get_notifications::v3::Notification,
        sync::sync_events::v3::{
            DeviceLists, Ephemeral, GlobalAccountData, InvitedRoom, Presence, RoomAccountData,
            State, ToDevice, UnreadNotificationsCount as RumaUnreadNotificationsCount,
        },
    },
    events::{
        room::member::{
            RoomMemberEvent, RoomMemberEventContent, StrippedRoomMemberEvent, SyncRoomMemberEvent,
        },
        AnyRoomEvent, AnySyncRoomEvent, StateUnsigned,
    },
    serde::Raw,
    DeviceId, DeviceKeyAlgorithm, EventId, MilliSecondsSinceUnixEpoch, RoomId, UserId,
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
    pub disambiguated_member: Option<Box<UserId>>,
    /// Has another user become ambiguous because of this event.
    pub ambiguated_member: Option<Box<UserId>>,
}

/// Collection of ambiguioty changes that room member events trigger.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct AmbiguityChanges {
    /// A map from room id to a map of an event id to the `AmbiguityChange` that
    /// the event with the given id caused.
    pub changes: BTreeMap<Box<RoomId>, BTreeMap<Box<EventId>, AmbiguityChange>>,
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
    pub sender: Box<UserId>,
    /// The device ID of the device that sent us the event, note this is
    /// untrusted data unless `verification_state` is as well trusted.
    pub sender_device: Box<DeviceId>,
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
    /// Get the event id of this `SyncRoomEvent` if the event has any valid id.
    pub fn event_id(&self) -> Option<Box<EventId>> {
        self.event.get_field::<Box<EventId>>("event_id").ok().flatten()
    }
}

impl From<Raw<AnySyncRoomEvent>> for SyncRoomEvent {
    fn from(inner: Raw<AnySyncRoomEvent>) -> Self {
        Self { encryption_info: None, event: inner }
    }
}

impl From<RoomEvent> for SyncRoomEvent {
    fn from(o: RoomEvent) -> Self {
        // This conversion is unproblematic since a SyncRoomEvent is just a
        // RoomEvent without the room_id. By converting the raw value in this
        // way, we simply cause the `room_id` field in the json to be ignored by
        // a subsequent deserialization.
        Self { encryption_info: o.encryption_info, event: o.event.cast() }
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
    pub notifications: BTreeMap<Box<RoomId>, Vec<Notification>>,
}

impl SyncResponse {
    pub fn new(next_batch: String) -> Self {
        Self { next_batch, ..Default::default() }
    }
}

#[derive(Clone, Debug)]
pub struct RoomEvent {
    /// The actual event.
    pub event: Raw<AnyRoomEvent>,
    /// The encryption info about the event. Will be `None` if the event was not
    /// encrypted.
    pub encryption_info: Option<EncryptionInfo>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Rooms {
    /// The rooms that the user has left or been banned from.
    pub leave: BTreeMap<Box<RoomId>, LeftRoom>,
    /// The rooms that the user has joined.
    pub join: BTreeMap<Box<RoomId>, JoinedRoom>,
    /// The rooms that the user has been invited to.
    pub invite: BTreeMap<Box<RoomId>, InvitedRoom>,
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
    /// If `None` this `TimelineSlice` is the beginning of the room
    pub end: Option<String>,

    /// Whether the number of events returned for this slice was limited
    /// by a `limit`-filter when requesting
    pub limited: bool,

    /// A list of events.
    pub events: Vec<SyncRoomEvent>,

    /// Whether this is a timeline slice obtained from a `SyncResponse`
    pub sync: bool,
}

impl TimelineSlice {
    pub fn new(
        events: Vec<SyncRoomEvent>,
        start: String,
        end: Option<String>,
        limited: bool,
        sync: bool,
    ) -> Self {
        Self { start, end, events, limited, sync }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(try_from = "SyncRoomMemberEvent", into = "SyncRoomMemberEvent")]
pub struct MemberEvent {
    pub content: RoomMemberEventContent,
    pub event_id: Box<EventId>,
    pub origin_server_ts: MilliSecondsSinceUnixEpoch,
    pub sender: Box<UserId>,
    pub state_key: Box<UserId>,
    pub unsigned: StateUnsigned<RoomMemberEventContent>,
}

impl TryFrom<SyncRoomMemberEvent> for MemberEvent {
    type Error = ruma::IdParseError;

    fn try_from(event: SyncRoomMemberEvent) -> Result<Self, Self::Error> {
        Ok(MemberEvent {
            content: event.content,
            event_id: event.event_id,
            origin_server_ts: event.origin_server_ts,
            sender: event.sender,
            state_key: event.state_key.try_into()?,
            unsigned: event.unsigned,
        })
    }
}

impl TryFrom<RoomMemberEvent> for MemberEvent {
    type Error = ruma::IdParseError;

    fn try_from(event: RoomMemberEvent) -> Result<Self, Self::Error> {
        Ok(MemberEvent {
            content: event.content,
            event_id: event.event_id,
            origin_server_ts: event.origin_server_ts,
            sender: event.sender,
            state_key: event.state_key.try_into()?,
            unsigned: event.unsigned,
        })
    }
}

impl From<MemberEvent> for SyncRoomMemberEvent {
    fn from(other: MemberEvent) -> Self {
        Self {
            content: other.content,
            event_id: other.event_id,
            sender: other.sender,
            origin_server_ts: other.origin_server_ts,
            state_key: other.state_key.to_string(),
            unsigned: other.unsigned,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(try_from = "StrippedRoomMemberEvent", into = "StrippedRoomMemberEvent")]
pub struct StrippedMemberEvent {
    pub content: RoomMemberEventContent,
    pub sender: Box<UserId>,
    pub state_key: Box<UserId>,
}

impl TryFrom<StrippedRoomMemberEvent> for StrippedMemberEvent {
    type Error = ruma::IdParseError;

    fn try_from(event: StrippedRoomMemberEvent) -> Result<Self, Self::Error> {
        Ok(StrippedMemberEvent {
            content: event.content,
            sender: event.sender,
            state_key: event.state_key.try_into()?,
        })
    }
}

impl From<StrippedMemberEvent> for StrippedRoomMemberEvent {
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

#[cfg(test)]
mod test {
    use ruma::{
        event_id,
        events::{
            room::message::RoomMessageEventContent, AnyMessageLikeEvent, AnySyncMessageLikeEvent,
            AnySyncRoomEvent, MessageLikeEvent, MessageLikeUnsigned,
        },
        room_id,
        serde::Raw,
        user_id, MilliSecondsSinceUnixEpoch,
    };

    use super::{RoomEvent, SyncRoomEvent};

    #[test]
    fn room_event_to_sync_room_event() {
        let content = RoomMessageEventContent::text_plain("foobar");

        let event = MessageLikeEvent {
            content,
            event_id: event_id!("$xxxxx:example.org").to_owned(),
            room_id: room_id!("!someroom:example.com").to_owned(),
            origin_server_ts: MilliSecondsSinceUnixEpoch::now(),
            sender: user_id!("@carl:example.com").to_owned(),
            unsigned: MessageLikeUnsigned::default(),
        };

        let room_event =
            RoomEvent { event: Raw::new(&event).unwrap().cast(), encryption_info: None };

        let converted_room_event: SyncRoomEvent = room_event.into();

        let converted_event: AnySyncRoomEvent = converted_room_event.event.deserialize().unwrap();

        let event: AnyMessageLikeEvent = event.into();
        let sync_event: AnySyncMessageLikeEvent = event.into();
        let sync_event: AnySyncRoomEvent = sync_event.into();

        // There is no PartialEq implementation for AnySyncRoomEvent, so we
        // just compare a couple of fields here. The important thing is that
        // the deserialization above worked.
        assert_eq!(converted_event.event_id(), sync_event.event_id());
        assert_eq!(converted_event.sender(), sync_event.sender());
    }
}
