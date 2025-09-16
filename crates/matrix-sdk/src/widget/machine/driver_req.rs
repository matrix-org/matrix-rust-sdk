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

//! A high-level API for requests that we send to the Matrix driver.

use std::{collections::BTreeMap, marker::PhantomData};

use ruma::{
    OwnedUserId,
    api::client::{account::request_openid_token, delayed_events::update_delayed_event},
    events::{AnyStateEvent, AnyTimelineEvent, AnyToDeviceEventContent},
    serde::Raw,
    to_device::DeviceIdOrAllDevices,
};
use serde::Deserialize;
use serde_json::value::RawValue as RawJsonValue;
use tracing::error;

use super::{
    Action, MatrixDriverRequestMeta, SendToDeviceEventResponse, WidgetMachine,
    from_widget::SendEventResponse, incoming::MatrixDriverResponse,
};
use crate::widget::{Capabilities, StateKeySelector};

#[derive(Clone, Debug)]
pub(crate) enum MatrixDriverRequestData {
    /// Acquire capabilities from the user given the set of desired
    /// capabilities.
    ///
    /// Must eventually be answered with
    /// [`MatrixDriverResponse::CapabilitiesAcquired`].
    AcquireCapabilities(AcquireCapabilities),

    /// Get OpenId token for a given request ID.
    GetOpenId,

    /// Read events from the timeline.
    ReadEvents(ReadEventsRequest),

    /// Read room state entries.
    ReadState(ReadStateRequest),

    /// Send Matrix event that corresponds to the given description.
    SendEvent(SendEventRequest),

    /// Send a to-device message over the Matrix homeserver.
    SendToDeviceEvent(SendToDeviceRequest),

    /// Data for sending a UpdateDelayedEvent client server api request.
    UpdateDelayedEvent(UpdateDelayedEventRequest),
}

/// A handle to a pending `toWidget` request.
pub(crate) struct MatrixDriverRequestHandle<'m, T> {
    request_meta: &'m mut MatrixDriverRequestMeta,
    _phantom: PhantomData<fn() -> T>,
}

impl<'m, T> MatrixDriverRequestHandle<'m, T>
where
    T: FromMatrixDriverResponse,
{
    pub(crate) fn new(request_meta: &'m mut MatrixDriverRequestMeta) -> Self {
        Self { request_meta, _phantom: PhantomData }
    }

    /// Setup a callback function that will be called once the Matrix driver has
    /// processed the request.
    pub(crate) fn add_response_handler(
        self,
        response_handler: impl FnOnce(Result<T, crate::Error>, &mut WidgetMachine) -> Vec<Action>
        + Send
        + 'static,
    ) {
        self.request_meta.response_fn = Some(Box::new(move |response, machine| {
            if let Some(response_data) = response.map(T::from_response).transpose() {
                response_handler(response_data, machine)
            } else {
                Vec::new()
            }
        }));
    }
}

/// Represents a request that the widget API state machine can send.
pub(crate) trait MatrixDriverRequest: Into<MatrixDriverRequestData> {
    type Response: FromMatrixDriverResponse;
}

pub(crate) trait FromMatrixDriverResponse: Sized {
    fn from_response(_: MatrixDriverResponse) -> Option<Self>;
}

impl<T> FromMatrixDriverResponse for T
where
    MatrixDriverResponse: TryInto<T>,
{
    fn from_response(response: MatrixDriverResponse) -> Option<Self> {
        // Delegates to the existing TryInto implementation
        response.try_into().ok()
    }
}

/// Ask the client (capability provider) to acquire given capabilities
/// from the user. The client must eventually respond with granted capabilities.
#[derive(Clone, Debug)]
pub(crate) struct AcquireCapabilities {
    pub(crate) desired_capabilities: Capabilities,
}

impl From<AcquireCapabilities> for MatrixDriverRequestData {
    fn from(value: AcquireCapabilities) -> Self {
        MatrixDriverRequestData::AcquireCapabilities(value)
    }
}

impl MatrixDriverRequest for AcquireCapabilities {
    type Response = Capabilities;
}

impl FromMatrixDriverResponse for Capabilities {
    fn from_response(ev: MatrixDriverResponse) -> Option<Self> {
        match ev {
            MatrixDriverResponse::CapabilitiesAcquired(response) => Some(response),
            _ => {
                error!("bug in MatrixDriver, received wrong event response");
                None
            }
        }
    }
}

/// Request open ID from the Matrix client.
#[derive(Debug)]
pub(crate) struct RequestOpenId;

impl From<RequestOpenId> for MatrixDriverRequestData {
    fn from(_: RequestOpenId) -> Self {
        MatrixDriverRequestData::GetOpenId
    }
}

impl MatrixDriverRequest for RequestOpenId {
    type Response = request_openid_token::v3::Response;
}

impl FromMatrixDriverResponse for request_openid_token::v3::Response {
    fn from_response(ev: MatrixDriverResponse) -> Option<Self> {
        match ev {
            MatrixDriverResponse::OpenIdReceived(response) => Some(response),
            _ => {
                error!("bug in MatrixDriver, received wrong event response");
                None
            }
        }
    }
}

/// Ask the client to read Matrix events that correspond to the given
/// description and return a list of events as a response.
#[derive(Clone, Debug)]
pub(crate) struct ReadEventsRequest {
    /// The event type to read.
    // TODO: This wants to be `MessageLikeEventType`` but we need a type which supports `as_str()`
    // as soon as ruma supports `as_str()` on `MessageLikeEventType` we can use it here.
    pub(crate) event_type: String,

    /// The `state_key` to read. If None, this will read events regardless of
    /// whether they are state events. If `Some(Any)`, this will only read state
    /// events of the given type. If set to a specific state key, this will only
    /// read state events of the given type matching that state key.
    pub(crate) state_key: Option<StateKeySelector>,

    /// The maximum number of events to return.
    pub(crate) limit: u32,
}

impl From<ReadEventsRequest> for MatrixDriverRequestData {
    fn from(value: ReadEventsRequest) -> Self {
        MatrixDriverRequestData::ReadEvents(value)
    }
}

impl MatrixDriverRequest for ReadEventsRequest {
    type Response = Vec<Raw<AnyTimelineEvent>>;
}

impl FromMatrixDriverResponse for Vec<Raw<AnyTimelineEvent>> {
    fn from_response(ev: MatrixDriverResponse) -> Option<Self> {
        match ev {
            MatrixDriverResponse::EventsRead(response) => Some(response),
            _ => {
                error!("bug in MatrixDriver, received wrong event response");
                None
            }
        }
    }
}

/// Ask the client to read Matrix room state entries corresponding to the given
/// description and return a list of state events as a response.
#[derive(Clone, Debug)]
pub(crate) struct ReadStateRequest {
    /// The event type to read.
    // TODO: This wants to be `TimelineEventType` but we need a type which supports `as_str()`
    // as soon as ruma supports `as_str()` on `TimelineEventType` we can use it here.
    pub(crate) event_type: String,

    /// The `state_key` to read, or `Any` to receive any/all room state entries
    /// of the given type, regardless of their `state_key`.
    pub(crate) state_key: StateKeySelector,
}

impl From<ReadStateRequest> for MatrixDriverRequestData {
    fn from(value: ReadStateRequest) -> Self {
        MatrixDriverRequestData::ReadState(value)
    }
}

impl MatrixDriverRequest for ReadStateRequest {
    type Response = Vec<Raw<AnyStateEvent>>;
}

impl FromMatrixDriverResponse for Vec<Raw<AnyStateEvent>> {
    fn from_response(ev: MatrixDriverResponse) -> Option<Self> {
        match ev {
            MatrixDriverResponse::StateRead(response) => Some(response),
            _ => {
                error!("bug in MatrixDriver, received wrong event response");
                None
            }
        }
    }
}

/// Ask the client to send Matrix event that corresponds to the given
/// description and returns an event ID (or a delay ID,
/// see [MSC4140](https://github.com/matrix-org/matrix-spec-proposals/pull/4140)) as a response.
#[derive(Clone, Debug, Deserialize)]
pub(crate) struct SendEventRequest {
    /// The type of the event.
    // TODO: This wants to be `TimelineEventType` but we need a type which supports `as_str()`
    // as soon as ruma supports `as_str()` on `TimelineEventType` we can use it here.
    #[serde(rename = "type")]
    pub(crate) event_type: String,
    /// State key of an event (if it's a state event).
    pub(crate) state_key: Option<String>,
    /// Raw content of an event.
    pub(crate) content: Box<RawJsonValue>,
    /// The optional delay (in ms) to send the event at.
    /// If provided, the response will contain a delay_id instead of a event_id.
    /// Defined by [MSC4157](https://github.com/matrix-org/matrix-spec-proposals/pull/4157)
    pub(crate) delay: Option<u64>,
}

impl From<SendEventRequest> for MatrixDriverRequestData {
    fn from(value: SendEventRequest) -> Self {
        MatrixDriverRequestData::SendEvent(value)
    }
}

impl MatrixDriverRequest for SendEventRequest {
    type Response = SendEventResponse;
}

impl FromMatrixDriverResponse for SendEventResponse {
    fn from_response(ev: MatrixDriverResponse) -> Option<Self> {
        match ev {
            MatrixDriverResponse::EventSent(response) => Some(response),
            _ => {
                error!("bug in MatrixDriver, received wrong event response");
                None
            }
        }
    }
}

/// Ask the client to send a to-device message that corresponds to the given
/// description.
#[derive(Clone, Debug, Deserialize)]
pub(crate) struct SendToDeviceRequest {
    /// The type of the to-device message.
    #[serde(rename = "type")]
    pub(crate) event_type: String,
    /// The messages to be sent.
    /// They are organized in a map of user ID -> device ID -> content like the
    /// cs api request.
    pub(crate) messages:
        BTreeMap<OwnedUserId, BTreeMap<DeviceIdOrAllDevices, Raw<AnyToDeviceEventContent>>>,
}

impl From<SendToDeviceRequest> for MatrixDriverRequestData {
    fn from(value: SendToDeviceRequest) -> Self {
        MatrixDriverRequestData::SendToDeviceEvent(value)
    }
}

impl MatrixDriverRequest for SendToDeviceRequest {
    type Response = SendToDeviceEventResponse;
}

/// Ask the client to send a UpdateDelayedEventRequest with the given `delay_id`
/// and `action`. Defined by [MSC4157](https://github.com/matrix-org/matrix-spec-proposals/pull/4157)
#[derive(Deserialize, Debug, Clone)]
pub(crate) struct UpdateDelayedEventRequest {
    pub(crate) action: update_delayed_event::unstable::UpdateAction,
    pub(crate) delay_id: String,
}

impl From<UpdateDelayedEventRequest> for MatrixDriverRequestData {
    fn from(value: UpdateDelayedEventRequest) -> Self {
        MatrixDriverRequestData::UpdateDelayedEvent(value)
    }
}

impl MatrixDriverRequest for UpdateDelayedEventRequest {
    type Response = update_delayed_event::unstable::Response;
}

impl FromMatrixDriverResponse for update_delayed_event::unstable::Response {
    fn from_response(ev: MatrixDriverResponse) -> Option<Self> {
        match ev {
            MatrixDriverResponse::DelayedEventUpdated(response) => Some(response),
            _ => {
                error!("bug in MatrixDriver, received wrong event response");
                None
            }
        }
    }
}
