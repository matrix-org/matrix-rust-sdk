use serde::{Deserialize, Serialize};

use crate::widget::{
    messages::{Empty, MatrixEvent, MessageKind, OpenIDResponse},
    Permissions as Capabilities,
};

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "action")]
pub enum Action {
    #[serde(rename = "capabilities")]
    CapabilitiesRequest(MessageKind<Empty, CapabilitiesResponse>),
    #[serde(rename = "notify_capabilities")]
    CapabilitiesUpdate(MessageKind<CapabilitiesUpdatedRequest, Empty>),
    #[serde(rename = "openid_credentials")]
    OpenIdCredentialsUpdate(MessageKind<OpenIDResponse, Empty>),
    #[serde(rename = "send_event")]
    SendEvent(MessageKind<MatrixEvent, Empty>),
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct CapabilitiesResponse {
    pub capabilities: Capabilities,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct CapabilitiesUpdatedRequest {
    pub requested: Capabilities,
    pub approved: Capabilities,
}
