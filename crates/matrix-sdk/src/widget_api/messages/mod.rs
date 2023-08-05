use self::{from_widget::FromWidgetMessage, to_widget::ToWidgetMessage};
use serde::{Deserialize, Serialize};

pub mod capabilities;
pub mod from_widget;
pub mod openid;
pub mod to_widget;

#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "api")]
pub enum Message {
    #[serde(rename = "fromWidget")]
    FromWidget(FromWidgetMessage),
    #[serde(rename = "toWidget")]
    ToWidget(ToWidgetMessage),
}

impl From<FromWidgetMessage> for Message {
    fn from(msg: FromWidgetMessage) -> Self {
        Message::FromWidget(msg)
    }
}

impl From<ToWidgetMessage> for Message {
    fn from(msg: ToWidgetMessage) -> Self {
        Message::ToWidget(msg)
    }
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Header {
    pub request_id: String,
    pub widget_id: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct MessageBody<Req, Resp> {
    pub header: Header,
    #[serde(rename = "data")]
    pub request: Req,
    pub response: Option<Response<Resp>>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(untagged)]
pub enum Response<Resp> {
    Error(WidgetError),
    Response(Resp),
}

impl<T> Response<T> {
    pub fn error(message: String) -> Self {
        Response::Error(WidgetError { error: WidgetErrorMessage { message } })
    }
}

impl<Resp> Into<Result<Resp, WidgetError>> for Response<Resp> {
    fn into(self) -> Result<Resp, WidgetError> {
        match self {
            Response::Error(err) => Err(err),
            Response::Response(resp) => Ok(resp),
        }
    }
}

// Shared types for the message body
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct WidgetError {
    pub error: WidgetErrorMessage,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct WidgetErrorMessage {
    pub message: String,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct SupportedVersions {
    pub versions: Vec<ApiVersion>,
}

pub static SUPPORTED_API_VERSIONS: [ApiVersion; 6] = [
    ApiVersion::V0_0_1,
    ApiVersion::V0_0_2,
    ApiVersion::MSC2762,
    ApiVersion::MSC2871,
    ApiVersion::MSC3819,
    ApiVersion::MSC3869,
];

#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum ApiVersion {
    #[serde(rename = "0.0.1")]
    V0_0_1,
    #[serde(rename = "0.0.2")]
    V0_0_2,
    #[serde(rename = "org.matrix.msc2762")] // Allowing widgets to send/receive events
    MSC2762,
    #[serde(rename = "org.matrix.msc2871")] // Sending approved capabilities back to the widget
    MSC2871,
    #[serde(rename = "org.matrix.msc2931")] // Allow widgets to navigate with matrix.to URIs
    MSC2931,
    #[serde(rename = "org.matrix.msc2974")] // Widgets: Capabilities re-exchange
    MSC2974,
    #[serde(rename = "org.matrix.msc2876")]
    // Allowing widgets to read events in a room (Closed/Deprecated)
    MSC2876,
    #[serde(rename = "org.matrix.msc3819")] // Allowing widgets to send/receive to-device messages
    MSC3819,
    #[serde(rename = "town.robin.msc3846")] // Allowing widgets to access TURN servers
    MSC3846,
    #[serde(rename = "org.matrix.msc3869")] // Read event relations with the Widget API
    MSC3869,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct MatrixEvent {
    #[serde(rename = "type")]
    event_type: String,
    sender: String,
    event_id: String,
    room_id: String,
    state_key: Option<String>,
    origin_server_ts: u32,
    content: serde_json::Value,
    unsigned: Unsigned,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
struct Unsigned {
    age: u32,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum ReadRelationsDirection {
    #[serde(rename = "f")]
    Forwards,
    #[serde(rename = "b")]
    Backwards,
}
