use ruma::{
    events::{
        AnyRoomAccountDataEvent, AnyStrippedStateEvent, AnySyncStateEvent, presence::PresenceEvent,
    },
    serde::Raw,
};
use serde_json::{Value as JsonValue, from_value as from_json_value};

use crate::test_json;

/// Test events that can be added to the state.
pub enum StateTestEvent {
    Alias,
    Aliases,
    Create,
    Encryption,
    EncryptionWithEncryptedStateEvents,
    HistoryVisibility,
    JoinRules,
    Member,
    MemberAdditional,
    MemberBan,
    MemberInvite,
    MemberLeave,
    MemberNameChange,
    PowerLevels,
    RedactedInvalid,
    RedactedState,
    RoomAvatar,
    RoomName,
    RoomPinnedEvents,
    RoomTopic,
    Custom(JsonValue),
}

impl From<StateTestEvent> for JsonValue {
    fn from(val: StateTestEvent) -> Self {
        match val {
            StateTestEvent::Alias => test_json::sync_events::ALIAS.to_owned(),
            StateTestEvent::Aliases => test_json::sync_events::ALIASES.to_owned(),
            StateTestEvent::Create => test_json::sync_events::CREATE.to_owned(),
            StateTestEvent::Encryption => test_json::sync_events::ENCRYPTION.to_owned(),
            StateTestEvent::EncryptionWithEncryptedStateEvents => {
                test_json::sync_events::ENCRYPTION_WITH_ENCRYPTED_STATE_EVENTS.to_owned()
            }
            StateTestEvent::HistoryVisibility => {
                test_json::sync_events::HISTORY_VISIBILITY.to_owned()
            }
            StateTestEvent::JoinRules => test_json::sync_events::JOIN_RULES.to_owned(),
            StateTestEvent::Member => test_json::sync_events::MEMBER.to_owned(),
            StateTestEvent::MemberAdditional => {
                test_json::sync_events::MEMBER_ADDITIONAL.to_owned()
            }
            StateTestEvent::MemberBan => test_json::sync_events::MEMBER_BAN.to_owned(),
            StateTestEvent::MemberInvite => test_json::sync_events::MEMBER_INVITE.to_owned(),
            StateTestEvent::MemberLeave => test_json::sync_events::MEMBER_LEAVE.to_owned(),
            StateTestEvent::MemberNameChange => {
                test_json::sync_events::MEMBER_NAME_CHANGE.to_owned()
            }
            StateTestEvent::PowerLevels => test_json::sync_events::POWER_LEVELS.to_owned(),
            StateTestEvent::RedactedInvalid => test_json::sync_events::REDACTED_INVALID.to_owned(),
            StateTestEvent::RedactedState => test_json::sync_events::REDACTED_STATE.to_owned(),
            StateTestEvent::RoomAvatar => test_json::sync_events::ROOM_AVATAR.to_owned(),
            StateTestEvent::RoomName => test_json::sync_events::NAME.to_owned(),
            StateTestEvent::RoomPinnedEvents => test_json::sync_events::PINNED_EVENTS.to_owned(),
            StateTestEvent::RoomTopic => test_json::sync_events::TOPIC.to_owned(),
            StateTestEvent::Custom(json) => json,
        }
    }
}

impl From<StateTestEvent> for Raw<AnySyncStateEvent> {
    fn from(val: StateTestEvent) -> Self {
        from_json_value(val.into()).unwrap()
    }
}

/// Test events that can be added to the stripped state.
pub enum StrippedStateTestEvent {
    Member,
    RoomName,
    Custom(JsonValue),
}

impl From<StrippedStateTestEvent> for JsonValue {
    fn from(val: StrippedStateTestEvent) -> Self {
        match val {
            StrippedStateTestEvent::Member => test_json::sync_events::MEMBER_STRIPPED.to_owned(),
            StrippedStateTestEvent::RoomName => test_json::sync_events::NAME_STRIPPED.to_owned(),
            StrippedStateTestEvent::Custom(json) => json,
        }
    }
}

impl From<StrippedStateTestEvent> for Raw<AnyStrippedStateEvent> {
    fn from(val: StrippedStateTestEvent) -> Self {
        from_json_value(val.into()).unwrap()
    }
}

/// Test events that can be added to the room account data.
pub enum RoomAccountDataTestEvent {
    FullyRead,
    Tags,
    MarkedUnread,
    Custom(JsonValue),
}

impl From<RoomAccountDataTestEvent> for JsonValue {
    fn from(val: RoomAccountDataTestEvent) -> Self {
        match val {
            RoomAccountDataTestEvent::FullyRead => test_json::sync_events::FULLY_READ.to_owned(),
            RoomAccountDataTestEvent::Tags => test_json::sync_events::TAG.to_owned(),
            RoomAccountDataTestEvent::MarkedUnread => {
                test_json::sync_events::MARKED_UNREAD.to_owned()
            }
            RoomAccountDataTestEvent::Custom(json) => json,
        }
    }
}

impl From<RoomAccountDataTestEvent> for Raw<AnyRoomAccountDataEvent> {
    fn from(val: RoomAccountDataTestEvent) -> Self {
        from_json_value(val.into()).unwrap()
    }
}

/// Test events that can be added to the presence events.
pub enum PresenceTestEvent {
    Presence,
    Custom(JsonValue),
}

impl From<PresenceTestEvent> for JsonValue {
    fn from(val: PresenceTestEvent) -> Self {
        match val {
            PresenceTestEvent::Presence => test_json::sync_events::PRESENCE.to_owned(),
            PresenceTestEvent::Custom(json) => json,
        }
    }
}

impl From<PresenceTestEvent> for Raw<PresenceEvent> {
    fn from(val: PresenceTestEvent) -> Self {
        from_json_value(val.into()).unwrap()
    }
}
