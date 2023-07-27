use serde::{Deserialize, Serialize};

use crate::widget_api::messages::{MessageBody, OpenIdState, MatrixEvent, self};


#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "action")]
pub enum ToWidgetMessage {
    #[serde(rename = "capabilities")]
    SendMeCapabilities(MessageBody<(), SendMeCapabilitiesResponse>),
    #[serde(rename = "notify_capabilities")]
    CapabilitiesUpdated(MessageBody<CapabilitiesUpdatedRequest, ()>),
    #[serde(rename = "openid_credentials")]
    OpenIdCredentials(MessageBody<OpenIdCredentialsRequest, ()>),
    #[serde(rename = "sent_to_device")]
    SendToDevice(MessageBody<SendToDeviceRequest, ()>),
    #[serde(rename = "send_event")]
    SendEvent(MessageBody<MatrixEvent, ()>)
}

#[derive(Serialize, Deserialize, Debug)]
pub struct SendMeCapabilitiesResponse {
    pub capabilities: messages::capabilities::Options,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct OpenIdCredentialsRequest {
    state: OpenIdState, //OpenIDRequestState;
    original_request_id: String,
    access_token: Option<String>,
    expires_in: Option<i32>,
    matrix_server_name: Option<String>,
    token_type: Option<String>,
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
    requested: Vec<String>,
    approved: Vec<String>
}

