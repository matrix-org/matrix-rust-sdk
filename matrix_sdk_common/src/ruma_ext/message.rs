use crate::identifiers::EventId;

#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct RelatesTo {
    /// The unique identifier for the event.
    pub event_id: EventId,

    /// RelatesTo is not represented as an enum so we store the type here.
    pub rel_type: String,
}

#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct MessageReplacement {
    /// The plain text body of the new message.
    pub body: String,

    /// The format used in the `formatted_body`. Currently only `org.matrix.custom.html` is
    /// supported.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub format: Option<String>,

    /// The formatted version of the `body`. This is required if `format` is specified.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub formatted_body: Option<String>,

    /// Since this type is not an enum we just hold the message type directly.
    pub msgtype: String,
}

#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct EditEventContent {
    /// The plain text body of the new message.
    pub body: String,

    /// The event's new content.
    #[serde(rename = "m.new_content")]
    pub new_content: MessageReplacement,

    /// The RelatesTo struct, holds an EventId and relates_to type.
    #[serde(rename = "m.relates_to")]
    pub relates_to: RelatesTo,
}

#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(tag = "msgtype")]
pub enum ExtraMessageEventContent {
    #[serde(rename = "m.text")]
    EditEvent(EditEventContent),
}
