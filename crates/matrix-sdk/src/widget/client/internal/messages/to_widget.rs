// Copyright 2023 The Matrix.org Foundation C.I.C.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use serde::{Deserialize, Serialize};

use super::{openid::OpenIdResponse, Empty, Request, Response};
use crate::widget::Permissions;

#[allow(dead_code)]
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "action")]
pub enum RequestType {
    #[serde(rename = "capabilities")]
    CapabilitiesRequest(Request<Empty>),
    #[serde(rename = "notify_capabilities")]
    CapabilitiesUpdate(Request<CapabilitiesUpdatedRequest>),
    #[serde(rename = "openid_credentials")]
    OpenIdCredentialsUpdate(Request<OpenIdResponse>),
    #[serde(rename = "send_event")]
    SendEvent(Request<serde_json::Value>),
}

#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "action")]
pub enum ResponseType {
    #[serde(rename = "capabilities")]
    CapabilitiesRequest(Response<Empty, CapabilitiesResponse>),
    #[serde(rename = "notify_capabilities")]
    CapabilitiesUpdate(Response<CapabilitiesUpdatedRequest, Empty>),
    #[serde(rename = "openid_credentials")]
    OpenIdCredentialsUpdate(Response<OpenIdResponse, Empty>),
    #[serde(rename = "send_event")]
    SendEvent(Response<serde_json::Value, Empty>),
}

#[derive(Clone, Debug, Deserialize)]
pub struct CapabilitiesResponse {
    pub capabilities: Permissions,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct CapabilitiesUpdatedRequest {
    pub requested: Permissions,
    pub approved: Permissions,
}
