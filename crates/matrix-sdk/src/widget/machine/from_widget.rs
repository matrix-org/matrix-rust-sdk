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

use std::fmt;

use ruma::{
    api::client::future,
    events::{AnyTimelineEvent, MessageLikeEventType, StateEventType},
    serde::Raw,
    OwnedEventId, OwnedRoomId,
};
use serde::{Deserialize, Serialize};

use super::SendEventRequest;
use crate::widget::StateKeySelector;

#[derive(Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", content = "data")]
pub(super) enum FromWidgetRequest {
    SupportedApiVersions {},
    ContentLoaded {},
    #[serde(rename = "get_openid")]
    GetOpenId {},
    #[serde(rename = "org.matrix.msc2876.read_events")]
    ReadEvent(ReadEventRequest),
    SendEvent(SendEventRequest),
}

#[derive(Serialize)]
pub(super) struct FromWidgetErrorResponse {
    error: FromWidgetError,
}

impl FromWidgetErrorResponse {
    pub(super) fn new(e: impl fmt::Display) -> Self {
        Self { error: FromWidgetError { message: e.to_string() } }
    }
}

#[derive(Serialize)]
struct FromWidgetError {
    message: String,
}

#[derive(Serialize)]
pub(super) struct SupportedApiVersionsResponse {
    supported_versions: Vec<ApiVersion>,
}

impl SupportedApiVersionsResponse {
    pub(super) fn new() -> Self {
        Self {
            supported_versions: vec![
                ApiVersion::V0_0_1,
                ApiVersion::V0_0_2,
                ApiVersion::MSC2762,
                ApiVersion::MSC2871,
                ApiVersion::MSC3819,
            ],
        }
    }
}

#[derive(Serialize)]
#[allow(dead_code)] // not all variants used right now
pub(super) enum ApiVersion {
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

#[derive(Deserialize)]
#[serde(untagged)]
pub(super) enum ReadEventRequest {
    ReadStateEvent {
        #[serde(rename = "type")]
        event_type: StateEventType,
        state_key: StateKeySelector,
    },
    #[allow(dead_code)]
    ReadMessageLikeEvent {
        #[serde(rename = "type")]
        event_type: MessageLikeEventType,
        limit: Option<u32>,
    },
}

#[derive(Debug, Serialize)]
pub(super) struct ReadEventResponse {
    pub(super) events: Vec<Raw<AnyTimelineEvent>>,
}

#[derive(Serialize, Debug)]
pub struct SendEventResponse {
    /// The room id for the send event.
    pub room_id: Option<OwnedRoomId>,
    /// The event id of the send event. Its optional because if its a future one does not get
    /// the event_id at this point.
    pub event_id: Option<OwnedEventId>,
    /// A token to send/insert the future into the DAG.
    pub send_token: Option<String>,
    /// A token to cancel this future. It will never be send if this is called.
    pub cancel_token: Option<String>,
    /// The `future_group_id` generated for this future. Used to connect multiple futures
    /// only one of the connected futures will be sent and inserted into the DAG.
    pub future_group_id: Option<String>,
    /// A token used to refresh the timer of the future. This allows
    /// to implement heartbeat like capabilities. An event is only sent once
    /// a refresh in the timeout interval is missed.
    ///
    /// If the future does not have a timeout this will be `None`.
    pub refresh_token: Option<String>,
}

impl SendEventResponse {
    pub fn from_event_id(event_id: OwnedEventId) -> Self {
        SendEventResponse {
            room_id: None,
            event_id: Some(event_id),
            send_token: None,
            cancel_token: None,
            future_group_id: None,
            refresh_token: None,
        }
    }
    pub fn set_room_id(&mut self, room_id: OwnedRoomId) {
        self.room_id = Some(room_id);
    }
}
impl Into<SendEventResponse> for future::send_future_message_event::unstable::Response {
    fn into(self) -> SendEventResponse {
        SendEventResponse {
            room_id: None,
            event_id: None,
            send_token: Some(self.send_token),
            cancel_token: Some(self.cancel_token),
            future_group_id: Some(self.future_group_id),
            refresh_token: self.refresh_token,
        }
    }
}

impl Into<SendEventResponse> for future::send_future_state_event::unstable::Response {
    fn into(self) -> SendEventResponse {
        SendEventResponse {
            room_id: None,
            event_id: None,
            send_token: Some(self.send_token),
            cancel_token: Some(self.cancel_token),
            future_group_id: Some(self.future_group_id),
            refresh_token: self.refresh_token,
        }
    }
}
