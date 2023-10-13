use serde::{Deserialize, Serialize};

use crate::widget::{
    messages::{openid::OpenIdResponse, Empty, MessageKind},
    Permissions as Capabilities,
};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "action")]
pub enum Action {
    #[serde(rename = "capabilities")]
    CapabilitiesRequest(MessageKind<Empty, CapabilitiesResponse>),
    #[serde(rename = "notify_capabilities")]
    CapabilitiesUpdate(MessageKind<CapabilitiesUpdatedRequest, Empty>),
    #[serde(rename = "openid_credentials")]
    OpenIdCredentialsUpdate(MessageKind<OpenIdResponse, Empty>),
    #[serde(rename = "send_event")]
    SendEvent(MessageKind<serde_json::Value, Empty>),
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct CapabilitiesResponse {
    pub capabilities: Capabilities,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct CapabilitiesUpdatedRequest {
    pub requested: Capabilities,
    pub approved: Capabilities,
}
