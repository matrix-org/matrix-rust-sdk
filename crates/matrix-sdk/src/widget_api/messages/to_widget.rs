use serde::{Deserialize, Serialize};

use super::{openid, capabilities::Options, MatrixEvent, MessageBody};

#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "action")]
pub enum ToWidgetMessage {
    #[serde(rename = "capabilities")]
    SendMeCapabilities(MessageBody<(), SendMeCapabilitiesResponse>),
    #[serde(rename = "notify_capabilities")]
    CapabilitiesUpdated(MessageBody<CapabilitiesUpdatedRequest, ()>),
    #[serde(rename = "openid_credentials")]
    OpenIdCredentials(MessageBody<openid::State, ()>),
    #[serde(rename = "sent_to_device")]
    SendToDevice(MessageBody<SendToDeviceRequest, ()>),
    #[serde(rename = "send_event")]
    SendEvent(MessageBody<MatrixEvent, ()>),
}

impl ToWidgetMessage {
    pub fn id(&self) -> &str {
        match self {
            ToWidgetMessage::SendMeCapabilities(MessageBody { header, .. })
            | ToWidgetMessage::CapabilitiesUpdated(MessageBody { header, .. })
            | ToWidgetMessage::OpenIdCredentials(MessageBody { header, .. })
            | ToWidgetMessage::SendToDevice(MessageBody { header, .. })
            | ToWidgetMessage::SendEvent(MessageBody { header, .. }) => &header.request_id,
        }
    }
}

#[derive(Serialize, Deserialize, Debug)]
pub struct SendMeCapabilitiesResponse {
    pub capabilities: Options,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct SendToDeviceRequest {
    #[serde(rename = "type")]
    event_type: String,
    sender: String,
    encrypted: bool,
    messages: serde_json::Value,
}
#[derive(Serialize, Deserialize, Debug)]
pub struct CapabilitiesUpdatedRequest {
    pub requested: Options,
    pub approved: Options,
}
