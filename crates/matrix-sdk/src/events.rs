use ruma::{
    events::{
        EventContent, EventContentFromType, MessageLikeEventContent, MessageLikeEventType,
        MessageLikeUnsigned, OriginalSyncMessageLikeEvent, OriginalSyncStateEvent,
        PossiblyRedactedStateEventContent, RedactContent, RedactedMessageLikeEventContent,
        RedactedStateEventContent, RedactedSyncMessageLikeEvent, RedactedSyncStateEvent,
        StateEventContent, StateEventType, StaticStateEventContent,
    },
    serde::from_raw_json_value,
    EventId, MilliSecondsSinceUnixEpoch, TransactionId, UserId,
};
use serde::{de, Deserialize, Serialize};
use serde_json::value::RawValue as RawJsonValue;

#[allow(clippy::large_enum_variant)]
pub(crate) enum SyncTimelineEventWithoutContent {
    OriginalMessageLike(OriginalSyncMessageLikeEvent<NoMessageLikeEventContent>),
    RedactedMessageLike(RedactedSyncMessageLikeEvent<NoMessageLikeEventContent>),
    OriginalState(OriginalSyncStateEvent<NoStateEventContent>),
    RedactedState(RedactedSyncStateEvent<NoStateEventContent>),
}

impl SyncTimelineEventWithoutContent {
    pub(crate) fn event_id(&self) -> &EventId {
        match self {
            Self::OriginalMessageLike(ev) => &ev.event_id,
            Self::RedactedMessageLike(ev) => &ev.event_id,
            Self::OriginalState(ev) => &ev.event_id,
            Self::RedactedState(ev) => &ev.event_id,
        }
    }

    pub(crate) fn origin_server_ts(&self) -> MilliSecondsSinceUnixEpoch {
        match self {
            SyncTimelineEventWithoutContent::OriginalMessageLike(ev) => ev.origin_server_ts,
            SyncTimelineEventWithoutContent::RedactedMessageLike(ev) => ev.origin_server_ts,
            SyncTimelineEventWithoutContent::OriginalState(ev) => ev.origin_server_ts,
            SyncTimelineEventWithoutContent::RedactedState(ev) => ev.origin_server_ts,
        }
    }

    pub(crate) fn sender(&self) -> &UserId {
        match self {
            Self::OriginalMessageLike(ev) => &ev.sender,
            Self::RedactedMessageLike(ev) => &ev.sender,
            Self::OriginalState(ev) => &ev.sender,
            Self::RedactedState(ev) => &ev.sender,
        }
    }

    pub(crate) fn transaction_id(&self) -> Option<&TransactionId> {
        match self {
            SyncTimelineEventWithoutContent::OriginalMessageLike(ev) => {
                ev.unsigned.transaction_id.as_deref()
            }
            SyncTimelineEventWithoutContent::OriginalState(ev) => {
                ev.unsigned.transaction_id.as_deref()
            }
            SyncTimelineEventWithoutContent::RedactedMessageLike(_)
            | SyncTimelineEventWithoutContent::RedactedState(_) => None,
        }
    }
}

#[derive(Deserialize)]
struct EventDeHelper {
    state_key: Option<de::IgnoredAny>,
    #[serde(default)]
    unsigned: UnsignedDeHelper,
}

#[derive(Deserialize, Default)]
struct UnsignedDeHelper {
    redacted_because: Option<de::IgnoredAny>,
}

impl<'de> Deserialize<'de> for SyncTimelineEventWithoutContent {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: de::Deserializer<'de>,
    {
        let json = Box::<RawJsonValue>::deserialize(deserializer)?;
        let EventDeHelper { state_key, unsigned } = from_raw_json_value(&json)?;

        Ok(match (state_key.is_some(), unsigned.redacted_because.is_some()) {
            (false, false) => Self::OriginalMessageLike(from_raw_json_value(&json)?),
            (false, true) => Self::RedactedMessageLike(from_raw_json_value(&json)?),
            (true, false) => Self::OriginalState(from_raw_json_value(&json)?),
            (true, true) => Self::RedactedState(from_raw_json_value(&json)?),
        })
    }
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct NoMessageLikeEventContent {
    #[serde(skip)]
    pub event_type: MessageLikeEventType,
}

impl EventContent for NoMessageLikeEventContent {
    type EventType = MessageLikeEventType;

    fn event_type(&self) -> Self::EventType {
        self.event_type.clone()
    }
}
impl EventContentFromType for NoMessageLikeEventContent {
    fn from_parts(event_type: &str, _content: &RawJsonValue) -> serde_json::Result<Self> {
        Ok(Self { event_type: event_type.into() })
    }
}
impl MessageLikeEventContent for NoMessageLikeEventContent {}
impl RedactedMessageLikeEventContent for NoMessageLikeEventContent {}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct NoStateEventContent {
    #[serde(skip)]
    pub event_type: StateEventType,
}

impl EventContent for NoStateEventContent {
    type EventType = StateEventType;

    fn event_type(&self) -> Self::EventType {
        self.event_type.clone()
    }
}
impl EventContentFromType for NoStateEventContent {
    fn from_parts(event_type: &str, _content: &RawJsonValue) -> serde_json::Result<Self> {
        Ok(Self { event_type: event_type.into() })
    }
}
impl RedactContent for NoStateEventContent {
    type Redacted = Self;

    fn redact(self, _version: &ruma::RoomVersionId) -> Self::Redacted {
        self
    }
}
impl StateEventContent for NoStateEventContent {
    type StateKey = String;
}
impl StaticStateEventContent for NoStateEventContent {
    // We don't care about the `prev_content` since it wont deserialize with useful
    // data. Use this type which is `StateUnsigned` minus the `prev_content`
    // field.
    type Unsigned = MessageLikeUnsigned<NoMessageLikeEventContent>;
    type PossiblyRedacted = Self;
}
impl RedactedStateEventContent for NoStateEventContent {
    type StateKey = String;
}
impl PossiblyRedactedStateEventContent for NoStateEventContent {
    type StateKey = String;
}
