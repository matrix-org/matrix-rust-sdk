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

use ruma::{
    events::{AnyTimelineEvent, TimelineEventType},
    serde::Raw,
};
use serde::{Deserialize, Serialize};

use super::{openid::OpenIdResponse, Empty, Request, Response};
use crate::widget::client::actions::SendEventCommand;

#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "action")]
pub(crate) enum RequestType {
    #[serde(rename = "supported_api_versions")]
    GetSupportedApiVersion(Request<Empty>),
    #[serde(rename = "content_loaded")]
    ContentLoaded(Request<Empty>),
    #[serde(rename = "get_openid")]
    GetOpenId(Request<Empty>),
    #[serde(rename = "send_event")]
    SendEvent(Request<SendEventBody>),
    #[serde(rename = "org.matrix.msc2876.read_events")]
    ReadEvent(Request<ReadEventBody>),
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct WithStateKey<T> {
    #[serde(flatten)]
    pub inner: T,
    pub state_key: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ReadEventBody {
    #[serde(rename = "type")]
    pub event_type: TimelineEventType,
    pub state_key: Option<String>,
    pub limit: Option<u32>,
}

pub type SendEventBody = SendEventCommand;

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "action")]
pub(crate) enum ResponseType {
    #[serde(rename = "supported_api_versions")]
    GetSupportedApiVersion(Response<Empty, SupportedApiVersionsResponse>),
    #[serde(rename = "content_loaded")]
    ContentLoaded(Response<Empty, Empty>),
    #[serde(rename = "get_openid")]
    GetOpenId(Response<Empty, OpenIdResponse>),
    #[serde(rename = "send_event")]
    SendEvent(Response<SendEventBody, SendEventResponseBody>),
    #[serde(rename = "org.matrix.msc2876.read_events")]
    ReadEvent(Response<ReadEventBody, ReadEventResponseBody>),
}

#[derive(Debug, Clone, Serialize)]
pub struct SupportedApiVersionsResponse {
    #[serde(rename = "supported_versions")]
    pub versions: Vec<ApiVersion>,
}

impl SupportedApiVersionsResponse {
    pub fn new() -> Self {
        Self {
            versions: vec![
                ApiVersion::V0_0_1,
                ApiVersion::V0_0_2,
                ApiVersion::MSC2762,
                ApiVersion::MSC2871,
                ApiVersion::MSC3819,
            ],
        }
    }
}

#[allow(dead_code)]
#[derive(Debug, Clone, Serialize)]
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
    /// Supports reading events in a room (deprecated).
    #[serde(rename = "org.matrix.msc2876")]
    MSC2876,
    /// Supports sending and receiving of to-device events.
    #[serde(rename = "org.matrix.msc3819")]
    MSC3819,
    /// Supports access to the TURN servers.
    #[serde(rename = "town.robin.msc3846")]
    MSC3846,
}

#[derive(Debug, Clone, Serialize)]
pub struct SendEventResponseBody {
    pub room_id: String,
    pub event_id: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ReadEventResponseBody {
    pub events: Vec<Raw<AnyTimelineEvent>>,
}

impl From<Vec<Raw<AnyTimelineEvent>>> for ReadEventResponseBody {
    fn from(events: Vec<Raw<AnyTimelineEvent>>) -> Self {
        Self { events }
    }
}
