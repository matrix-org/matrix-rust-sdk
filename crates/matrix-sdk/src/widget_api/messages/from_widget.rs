use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::widget_api::messages::{
    MessageBody, MatrixEvent, OpenIdState, SupportedVersions, ReadRelationsDirection,
};

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "action")]
pub enum FromWidgetMessage {
    #[serde(rename = "supported_api_versions")]
    GetSupportedApiVersion(MessageBody<(), SupportedVersions>),
    #[serde(rename = "content_loaded")]
    ContentLoaded(MessageBody<(), ()>),
    #[serde(rename = "get_openid")]
    GetOpenId(MessageBody<(), GetOpenIdResponse>),
    #[serde(rename = "send_to_device")]
    SendToDevice(MessageBody<SendToDeviceRequest, ()>),
    #[serde(rename = "send_events")]
    SendEvent(MessageBody<SendEventRequest, SendEventResponse>),
    #[serde(rename = "org.matrix.msc2876.read_events")]
    ReadEvent(MessageBody<ReadEventRequest, ReadEventResponse>),
    #[serde(rename = "org.matrix.msc3869.read_relations")]
    ReadRelations(MessageBody<ReadEventRequest, ReadEventResponse>),
}

// MSC1960

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct GetOpenIdResponse {
    state: OpenIdState, //OpenIDRequestState;
    access_token: Option<String>,
    expires_in: Option<i32>,
    matrix_server_name: Option<String>,
    token_type: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct SendToDeviceRequest {
    #[serde(rename = "type")]
    message_type: String,
    encrypted: bool,
    content: HashMap<String, HashMap<String, serde_json::Value>>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct SendEventRequest {
    #[serde(rename = "type")]
    message_type: String,
    state_key: String,
    content: serde_json::Value,
}
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct SendEventResponse {
    room_id: String,
    event_id: String,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ReadEventRequest {
    #[serde(rename = "type")]
    message_type: String,
    state_key: String,
    limit: u32,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ReadEventResponse {
    events: Vec<MatrixEvent>,
}

// MSC3869
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ReadRelationsRequest {
    event_id: String,
    room_id: Option<String>,
    rel_type: Option<String>,
    event_type: Option<String>,
    limit: Option<u32>,
    from: Option<String>,
    to: Option<String>,
    direction: Option<ReadRelationsDirection>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ReadRelationsResponse {
    chunk: Vec<MatrixEvent>,
    next_batch: String,
    prev_batch: String,
}
