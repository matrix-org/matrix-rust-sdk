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

#[derive(Deserialize, Debug)]
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

#[derive(Deserialize, Debug)]
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
pub(crate) struct SendEventResponse {
    /// The room id for the send event.
    pub(crate) room_id: Option<OwnedRoomId>,
    /// The event id of the send event. It's optional because if it's a future
    /// event, it does not get the event_id at this point.
    pub(crate) event_id: Option<OwnedEventId>,
    /// A token to send/insert the future event into the DAG.
    pub(crate) send_token: Option<String>,
    /// A token to cancel this future event. It will never be sent if this is
    /// called.
    pub(crate) cancel_token: Option<String>,
    /// The `future_group_id` generated for this future event. Used to connect
    /// multiple future events. Only one of the connected future events will be
    /// sent and inserted into the DAG.
    pub(crate) future_group_id: Option<String>,
    /// A token used to refresh the timer of the future event. This allows
    /// to implement heartbeat-like capabilities. An event is only sent once
    /// a refresh in the timeout interval is missed.
    ///
    /// If the future event does not have a timeout this will be `None`.
    pub(crate) refresh_token: Option<String>,
}

impl SendEventResponse {
    pub(crate) fn from_event_id(event_id: OwnedEventId) -> Self {
        SendEventResponse {
            room_id: None,
            event_id: Some(event_id),
            send_token: None,
            cancel_token: None,
            future_group_id: None,
            refresh_token: None,
        }
    }
    pub(crate) fn set_room_id(&mut self, room_id: OwnedRoomId) {
        self.room_id = Some(room_id);
    }
}

impl From<future::send_future_message_event::unstable::Response> for SendEventResponse {
    fn from(val: future::send_future_message_event::unstable::Response) -> Self {
        SendEventResponse {
            room_id: None,
            event_id: None,
            send_token: Some(val.send_token),
            cancel_token: Some(val.cancel_token),
            future_group_id: Some(val.future_group_id),
            refresh_token: val.refresh_token,
        }
    }
}

impl From<future::send_future_state_event::unstable::Response> for SendEventResponse {
    fn from(val: future::send_future_state_event::unstable::Response) -> Self {
        SendEventResponse {
            room_id: None,
            event_id: None,
            send_token: Some(val.send_token),
            cancel_token: Some(val.cancel_token),
            future_group_id: Some(val.future_group_id),
            refresh_token: val.refresh_token,
        }
    }
}
