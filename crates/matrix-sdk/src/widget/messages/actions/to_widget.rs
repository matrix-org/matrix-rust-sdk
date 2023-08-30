use ruma::{events::AnySyncTimelineEvent, serde::Raw};
use serde::{Deserialize, Serialize};

use crate::widget::{
    messages::{Empty, MessageKind, OpenIdResponse},
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
    SendEvent(MessageKind<Raw<AnySyncTimelineEvent>, Empty>),
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
