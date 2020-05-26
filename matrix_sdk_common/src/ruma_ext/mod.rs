use std::collections::BTreeMap;
use std::time::SystemTime;

use serde_json::Value as JsonValue;

use crate::events::FromRaw;
use crate::identifiers::{EventId, RoomId, UserId};

pub mod message;
pub mod reaction;

pub use message::ExtraMessageEventContent;
pub use reaction::ExtraReactionEventContent;

pub type RumaUnsupportedEvent = RumaUnsupportedRoomEvent<ExtraRoomEventContent>;

#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(tag = "type")]
pub enum ExtraRoomEventContent {
    #[serde(rename = "m.room.message")]
    Message { content: ExtraMessageEventContent },
    #[serde(rename = "m.reaction")]
    Reaction { content: ExtraReactionEventContent },
}

#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(bound = "C: serde::de::DeserializeOwned + serde::Serialize")]
pub struct RumaUnsupportedRoomEvent<C: serde::de::DeserializeOwned + serde::Serialize> {
    /// The event's content.
    #[serde(flatten)]
    pub content: C,

    /// The unique identifier for the event.
    pub event_id: EventId,

    /// Time on originating homeserver when this event was sent.
    #[serde(with = "ruma_serde::time::ms_since_unix_epoch")]
    pub origin_server_ts: SystemTime,

    /// The unique identifier for the room associated with this event.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub room_id: Option<RoomId>,

    /// The unique identifier for the user who sent this event.
    pub sender: UserId,

    /// Additional key-value pairs not signed by the homeserver.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub unsigned: BTreeMap<String, JsonValue>,
}

impl<C: Sized + serde::de::DeserializeOwned + serde::Serialize> FromRaw
    for RumaUnsupportedRoomEvent<C>
{
    type Raw = Self;
    fn from_raw(raw: Self) -> Self {
        raw
    }
}

#[test]
fn test_message_edit_event() {
    use crate::events::EventJson;

    let ev = serde_json::from_str::<EventJson<RumaUnsupportedEvent>>(include_str!(
        "../../../test_data/events/message_edit.json"
    ))
    .unwrap()
    .deserialize()
    .unwrap();

    let json = serde_json::to_string_pretty(&ev).unwrap();
    assert_eq!(
        ev,
        serde_json::from_str::<EventJson<RumaUnsupportedEvent>>(&json)
            .unwrap()
            .deserialize()
            .unwrap()
    )
}

#[test]
fn test_reaction_event() {
    use crate::events::EventJson;

    let ev = serde_json::from_str::<EventJson<RumaUnsupportedEvent>>(include_str!(
        "../../../test_data/events/reaction.json"
    ))
    .unwrap()
    .deserialize()
    .unwrap();

    let json = serde_json::to_string_pretty(&ev).unwrap();
    assert_eq!(
        ev,
        serde_json::from_str::<EventJson<RumaUnsupportedEvent>>(&json)
            .unwrap()
            .deserialize()
            .unwrap()
    )
}
