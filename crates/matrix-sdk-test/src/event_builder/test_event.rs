use ruma::{
    events::{
        presence::PresenceEvent, AnyGlobalAccountDataEvent, AnyRoomAccountDataEvent,
        AnyStrippedStateEvent, AnySyncEphemeralRoomEvent, AnySyncRoomEvent, AnySyncStateEvent,
    },
    serde::Raw,
};
use serde_json::{from_value as from_json_value, Value as JsonValue};

use crate::test_json;

/// Test events that can be added to the timeline.
pub enum TimelineTestEvent {
    Alias,
    Aliases,
    Create,
    Encryption,
    HistoryVisibility,
    JoinRules,
    Member,
    MemberInvite,
    MemberNameChange,
    MessageEdit,
    MessageEmote,
    MessageNotice,
    MessageText,
    PowerLevels,
    Reaction,
    RedactedInvalid,
    RedactedMessage,
    RedactedState,
    Redaction,
    RoomAvatar,
    RoomName,
    RoomTopic,
    Custom(JsonValue),
}

impl TimelineTestEvent {
    /// Get the JSON representation of this test event.
    pub fn into_json_value(self) -> JsonValue {
        match self {
            Self::Alias => test_json::events::ALIAS.to_owned(),
            Self::Aliases => test_json::events::ALIASES.to_owned(),
            Self::Create => test_json::events::CREATE.to_owned(),
            Self::Encryption => test_json::events::ENCRYPTION.to_owned(),
            Self::HistoryVisibility => test_json::events::HISTORY_VISIBILITY.to_owned(),
            Self::JoinRules => test_json::events::JOIN_RULES.to_owned(),
            Self::Member => test_json::events::MEMBER.to_owned(),
            Self::MemberInvite => test_json::events::MEMBER_INVITE.to_owned(),
            Self::MemberNameChange => test_json::events::MEMBER_NAME_CHANGE.to_owned(),
            Self::MessageEdit => test_json::events::MESSAGE_EDIT.to_owned(),
            Self::MessageEmote => test_json::events::MESSAGE_EMOTE.to_owned(),
            Self::MessageNotice => test_json::events::MESSAGE_NOTICE.to_owned(),
            Self::MessageText => test_json::events::MESSAGE_TEXT.to_owned(),
            Self::PowerLevels => test_json::events::POWER_LEVELS.to_owned(),
            Self::Reaction => test_json::events::REACTION.to_owned(),
            Self::RedactedInvalid => test_json::events::REDACTED_INVALID.to_owned(),
            Self::RedactedMessage => test_json::events::REDACTED.to_owned(),
            Self::RedactedState => test_json::events::REDACTED_STATE.to_owned(),
            Self::Redaction => test_json::events::REDACTION.to_owned(),
            Self::RoomAvatar => test_json::events::ROOM_AVATAR.to_owned(),
            Self::RoomName => test_json::events::NAME.to_owned(),
            Self::RoomTopic => test_json::events::TOPIC.to_owned(),
            Self::Custom(json) => json,
        }
    }

    /// Get the typed JSON representation of this test event.
    pub fn into_raw_event(self) -> Raw<AnySyncRoomEvent> {
        from_json_value(self.into_json_value()).unwrap()
    }
}

/// Test events that can be added to the state.
pub enum StateTestEvent {
    Alias,
    Aliases,
    Create,
    Encryption,
    HistoryVisibility,
    JoinRules,
    Member,
    MemberInvite,
    MemberNameChange,
    PowerLevels,
    RedactedInvalid,
    RedactedState,
    RoomAvatar,
    RoomName,
    RoomTopic,
    Custom(JsonValue),
}

impl StateTestEvent {
    /// Get the JSON representation of this test event.
    pub fn into_json_value(self) -> JsonValue {
        match self {
            Self::Alias => test_json::events::ALIAS.to_owned(),
            Self::Aliases => test_json::events::ALIASES.to_owned(),
            Self::Create => test_json::events::CREATE.to_owned(),
            Self::Encryption => test_json::events::ENCRYPTION.to_owned(),
            Self::HistoryVisibility => test_json::events::HISTORY_VISIBILITY.to_owned(),
            Self::JoinRules => test_json::events::JOIN_RULES.to_owned(),
            Self::Member => test_json::events::MEMBER.to_owned(),
            Self::MemberInvite => test_json::events::MEMBER_INVITE.to_owned(),
            Self::MemberNameChange => test_json::events::MEMBER_NAME_CHANGE.to_owned(),
            Self::PowerLevels => test_json::events::POWER_LEVELS.to_owned(),
            Self::RedactedInvalid => test_json::events::REDACTED_INVALID.to_owned(),
            Self::RedactedState => test_json::events::REDACTED_STATE.to_owned(),
            Self::RoomAvatar => test_json::events::ROOM_AVATAR.to_owned(),
            Self::RoomName => test_json::events::NAME.to_owned(),
            Self::RoomTopic => test_json::events::TOPIC.to_owned(),
            Self::Custom(json) => json,
        }
    }

    /// Get the typed JSON representation of this test event.
    pub fn into_raw_event(self) -> Raw<AnySyncStateEvent> {
        from_json_value(self.into_json_value()).unwrap()
    }
}

/// Test events that can be added to the stripped state.
pub enum StrippedStateTestEvent {
    Member,
    RoomName,
    Custom(JsonValue),
}

impl StrippedStateTestEvent {
    /// Get the JSON representation of this test event.
    pub fn into_json_value(self) -> JsonValue {
        match self {
            Self::Member => test_json::events::MEMBER_STRIPPED.to_owned(),
            Self::RoomName => test_json::events::NAME_STRIPPED.to_owned(),
            Self::Custom(json) => json,
        }
    }

    /// Get the typed JSON representation of this test event.
    pub fn into_raw_event(self) -> Raw<AnyStrippedStateEvent> {
        from_json_value(self.into_json_value()).unwrap()
    }
}

/// Test events that can be added to the room account data.
pub enum RoomAccountDataTestEvent {
    FullyRead,
    Custom(JsonValue),
}

impl RoomAccountDataTestEvent {
    /// Get the JSON representation of this test event.
    pub fn into_json_value(self) -> JsonValue {
        match self {
            Self::FullyRead => test_json::events::FULLY_READ.to_owned(),
            Self::Custom(json) => json,
        }
    }

    /// Get the typed JSON representation of this test event.
    pub fn into_raw_event(self) -> Raw<AnyRoomAccountDataEvent> {
        from_json_value(self.into_json_value()).unwrap()
    }
}

/// Test events that can be added to the ephemeral events.
pub enum EphemeralTestEvent {
    ReadReceipt,
    ReadReceiptOther,
    Typing,
    Custom(JsonValue),
}

impl EphemeralTestEvent {
    /// Get the JSON representation of this test event.
    pub fn into_json_value(self) -> JsonValue {
        match self {
            Self::ReadReceipt => test_json::events::READ_RECEIPT.to_owned(),
            Self::ReadReceiptOther => test_json::events::READ_RECEIPT_OTHER.to_owned(),
            Self::Typing => test_json::events::TYPING.to_owned(),
            Self::Custom(json) => json,
        }
    }

    /// Get the typed JSON representation of this test event.
    pub fn into_raw_event(self) -> Raw<AnySyncEphemeralRoomEvent> {
        from_json_value(self.into_json_value()).unwrap()
    }
}

/// Test events that can be added to the presence events.
pub enum PresenceTestEvent {
    Presence,
    Custom(JsonValue),
}

impl PresenceTestEvent {
    /// Get the JSON representation of this test event.
    pub fn into_json_value(self) -> JsonValue {
        match self {
            Self::Presence => test_json::events::PRESENCE.to_owned(),
            Self::Custom(json) => json,
        }
    }

    /// Get the typed JSON representation of this test event.
    pub fn into_raw_event(self) -> Raw<PresenceEvent> {
        from_json_value(self.into_json_value()).unwrap()
    }
}

/// Test events that can be added to the global account data.
pub enum GlobalAccountDataTestEvent {
    PushRules,
    Tags,
    Custom(JsonValue),
}

impl GlobalAccountDataTestEvent {
    /// Get the JSON representation of this test event.
    pub fn into_json_value(self) -> JsonValue {
        match self {
            Self::PushRules => test_json::events::PUSH_RULES.to_owned(),
            Self::Tags => test_json::events::TAG.to_owned(),
            Self::Custom(json) => json,
        }
    }

    /// Get the typed JSON representation of this test event.
    pub fn into_raw_event(self) -> Raw<AnyGlobalAccountDataEvent> {
        from_json_value(self.into_json_value()).unwrap()
    }
}
