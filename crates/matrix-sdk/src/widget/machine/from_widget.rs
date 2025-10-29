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

use std::collections::BTreeMap;

use as_variant::as_variant;
use ruma::{
    OwnedEventId, OwnedRoomId,
    api::client::{
        delayed_events::{delayed_message_event, delayed_state_event, update_delayed_event},
        error::{ErrorBody, StandardErrorBody},
    },
    events::AnyTimelineEvent,
    serde::Raw,
};
use serde::{Deserialize, Serialize};
use tracing::error;

use super::{
    MatrixDriverResponse, SendEventRequest, UpdateDelayedEventRequest,
    driver_req::SendToDeviceRequest,
};
use crate::{
    Error, HttpError, RumaApiError,
    widget::{StateKeySelector, machine::driver_req::FromMatrixDriverResponse},
};

#[derive(Deserialize, Debug)]
#[serde(tag = "action", rename_all = "snake_case", content = "data")]
pub(super) enum FromWidgetRequest {
    SupportedApiVersions {},
    ContentLoaded {},
    #[serde(rename = "get_openid")]
    GetOpenId {},
    #[serde(rename = "org.matrix.msc2876.read_events")]
    ReadEvent(ReadEventsRequest),
    SendEvent(SendEventRequest),
    SendToDevice(SendToDeviceRequest),
    #[serde(rename = "org.matrix.msc4157.update_delayed_event")]
    DelayedEventUpdate(UpdateDelayedEventRequest),
}

/// The full response a client sends to a [`FromWidgetRequest`] in case of an
/// error.
#[derive(Serialize)]
pub(super) struct FromWidgetErrorResponse {
    error: FromWidgetError,
}

impl FromWidgetErrorResponse {
    /// Create a error response to send to the widget from an http error.
    pub(crate) fn from_http_error(error: HttpError) -> Self {
        let message = error.to_string();
        let matrix_api_error = match error {
            HttpError::Api(error) => {
                as_variant!(*error, ruma::api::error::FromHttpResponseError::Server(RumaApiError::ClientApi(err)) => err)
            }
            _ => None,
        };

        Self {
            error: FromWidgetError {
                message,
                matrix_api_error: matrix_api_error.and_then(|api_error| match api_error.body {
                    ErrorBody::Standard(response) => Some(FromWidgetMatrixErrorBody {
                        http_status: api_error.status_code.as_u16().into(),
                        response,
                    }),
                    _ => None,
                }),
            },
        }
    }

    /// Create an error response to send to the widget from a Matrix SDK error.
    pub(crate) fn from_error(error: Error) -> Self {
        match error {
            Error::Http(e) => FromWidgetErrorResponse::from_http_error(*e),
            // For UnknownError's we do not want to have the `unknown error` bit in the message.
            // Hence we only convert the inner error to a string.
            Error::UnknownError(e) => FromWidgetErrorResponse::from_string(e.to_string()),
            _ => FromWidgetErrorResponse::from_string(error.to_string()),
        }
    }

    /// Create a error response to send to the widget from a string.
    pub(crate) fn from_string<S: Into<String>>(error: S) -> Self {
        Self { error: FromWidgetError { message: error.into(), matrix_api_error: None } }
    }
}

/// Serializable section of an error response send by the client as a
/// response to a [`FromWidgetRequest`].
#[derive(Serialize)]
struct FromWidgetError {
    /// Unspecified error message text that caused this widget action to
    /// fail.
    ///
    /// This is useful to prompt the user on an issue but cannot be used to
    /// decide on how to deal with the error.
    message: String,

    /// Optional Matrix error hinting at workarounds for specific errors.
    #[serde(skip_serializing_if = "Option::is_none")]
    matrix_api_error: Option<FromWidgetMatrixErrorBody>,
}

/// Serializable section of a widget response that represents a Matrix error.
#[derive(Serialize)]
struct FromWidgetMatrixErrorBody {
    /// Status code of the http response.
    http_status: u32,

    /// Standard error response including the `errorcode` and the `error`
    /// message as defined in the [spec](https://spec.matrix.org/v1.12/client-server-api/#standard-error-response).
    response: StandardErrorBody,
}

/// The serializable section of a widget response containing the supported
/// versions.
#[derive(Serialize)]
pub(super) struct SupportedApiVersionsResponse {
    supported_versions: Vec<ApiVersion>,
}

impl SupportedApiVersionsResponse {
    /// The currently supported widget api versions from the rust widget driver.
    pub(super) fn new() -> Self {
        Self {
            supported_versions: vec![
                ApiVersion::V0_0_1,
                ApiVersion::V0_0_2,
                ApiVersion::MSC2762,
                ApiVersion::MSC2762UpdateState,
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

    /// Supports sending and receiving events.
    #[serde(rename = "org.matrix.msc2762")]
    MSC2762,

    /// Supports receiving room state with the `update_state` action.
    #[serde(rename = "org.matrix.msc2762_update_state")]
    MSC2762UpdateState,

    /// Supports sending approved capabilities back to the widget.
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

    /// Supports sending and receiving to-device events.
    #[serde(rename = "org.matrix.msc3819")]
    MSC3819,

    /// Supports access to the TURN servers.
    #[serde(rename = "town.robin.msc3846")]
    MSC3846,
}

#[derive(Deserialize, Debug)]
pub(super) struct ReadEventsRequest {
    #[serde(rename = "type")]
    pub(super) event_type: String,
    pub(super) state_key: Option<StateKeySelector>,
    pub(super) limit: Option<u32>,
}

#[derive(Debug, Serialize)]
pub(super) struct ReadEventsResponse {
    pub(super) events: Vec<Raw<AnyTimelineEvent>>,
}

#[derive(Serialize, Debug)]
pub(crate) struct SendEventResponse {
    /// The room id for the send event.
    pub(crate) room_id: Option<OwnedRoomId>,
    /// The event id of the send event. It's optional because if it's a delayed
    /// event, it does not get the event_id at this point.
    pub(crate) event_id: Option<OwnedEventId>,
    /// The `delay_id` generated for this delayed event. Used to interact with
    /// the delayed event.
    pub(crate) delay_id: Option<String>,
}

impl SendEventResponse {
    pub(crate) fn from_event_id(event_id: OwnedEventId) -> Self {
        SendEventResponse { room_id: None, event_id: Some(event_id), delay_id: None }
    }
    pub(crate) fn set_room_id(&mut self, room_id: OwnedRoomId) {
        self.room_id = Some(room_id);
    }
}

impl From<delayed_message_event::unstable::Response> for SendEventResponse {
    fn from(val: delayed_message_event::unstable::Response) -> Self {
        SendEventResponse { room_id: None, event_id: None, delay_id: Some(val.delay_id) }
    }
}

impl From<delayed_state_event::unstable::Response> for SendEventResponse {
    fn from(val: delayed_state_event::unstable::Response) -> Self {
        SendEventResponse { room_id: None, event_id: None, delay_id: Some(val.delay_id) }
    }
}

/// A wrapper type for the empty okay response from
/// [`update_delayed_event`](update_delayed_event::unstable::Response)
/// which derives Serialize. (The response struct from Ruma does not derive
/// serialize)
/// This is intentionally an empty tuple struct (not a unit struct), so that it
/// serializes to `{}` instead of `Null` when returned to the widget as json.
#[derive(Serialize, Debug)]
pub(crate) struct UpdateDelayedEventResponse {}
impl From<update_delayed_event::unstable::Response> for UpdateDelayedEventResponse {
    fn from(_: update_delayed_event::unstable::Response) -> Self {
        Self {}
    }
}

/// Response for a send-to-device request.
/// The failure map contains recipients that didn't receive the content due to:
/// - Recipient devices not being found
/// - Encryption failures (e.g., missing one-time keys)
/// - Network/Server errors during sending
#[derive(Serialize, Debug, Default)]
pub(crate) struct SendToDeviceEventResponse {
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub failures: BTreeMap<String, Vec<String>>,
}

impl FromMatrixDriverResponse for SendToDeviceEventResponse {
    fn from_response(matrix_driver_response: MatrixDriverResponse) -> Option<Self> {
        match matrix_driver_response {
            MatrixDriverResponse::ToDeviceSent(resp) => Some(Self { failures: resp.failures }),
            _ => {
                error!("bug in MatrixDriver, received wrong event response");
                None
            }
        }
    }
}
