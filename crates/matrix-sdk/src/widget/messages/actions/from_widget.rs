use serde::{Deserialize, Serialize};

use crate::widget::messages::{
    Empty, EventType, MatrixEvent, MessageKind, OpenIDRequest, OpenIDResponse,
};

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "action")]
pub enum Action {
    #[serde(rename = "supported_api_versions")]
    GetSupportedApiVersion(MessageKind<Empty, SupportedApiVersionsResponse>),
    #[serde(rename = "content_loaded")]
    ContentLoaded(MessageKind<Empty, Empty>),
    #[serde(rename = "get_openid")]
    GetOpenID(MessageKind<OpenIDRequest, OpenIDResponse>),
    #[serde(rename = "send_events")]
    SendEvent(MessageKind<SendEventRequest, SendEventResponse>),
    #[serde(rename = "org.matrix.msc2876.read_events")]
    ReadEvent(MessageKind<ReadEventRequest, ReadEventResponse>),
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct SupportedApiVersionsResponse {
    pub versions: Vec<ApiVersion>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum ApiVersion {
    /// First stable version.
    #[serde(rename = "0.0.1")]
    V0_0_1,
    /// Second stable version.
    #[serde(rename = "0.0.2")]
    V0_0_2,
    /// Supports sending and receiving of events.
    #[serde(rename = "org.matrix.msc2762")]
    MSC2762,
    /// Supports sending of approved capabilities back to the widget.
    #[serde(rename = "org.matrix.msc2871")]
    MSC2871,
    /// Supports navigating to a URI.
    #[serde(rename = "org.matrix.msc2931")]
    MSC2931,
    /// Supports capabilities renegotiation.
    #[serde(rename = "org.matrix.msc2974")]
    MSC2974,
    /// Supports reading eventsi in a room (deprecated).
    #[serde(rename = "org.matrix.msc2876")]
    MSC2876,
    /// Supports sending and receiving of to-device events.
    #[serde(rename = "org.matrix.msc3819")]
    MSC3819,
    /// Supports access to the TURN servers.
    #[serde(rename = "town.robin.msc3846")]
    MSC3846,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct SendEventRequest {
    #[serde(flatten)]
    pub event_type: EventType,
    pub content: serde_json::Value,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct SendEventResponse {
    pub room_id: String,
    pub event_id: String,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ReadEventRequest {
    #[serde(flatten)]
    pub event_type: EventType,
    pub limit: u32,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ReadEventResponse {
    pub events: Vec<MatrixEvent>,
}
