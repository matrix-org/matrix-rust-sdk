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

#![allow(dead_code)]

//! A high-level API for requests that we send to the matrix driver.

use std::marker::PhantomData;

use ruma::{
    api::client::account::request_openid_token, events::AnyTimelineEvent, serde::Raw, OwnedEventId,
};
use tracing::error;

use super::{
    actions::{MatrixDriverRequestData, ReadMessageLikeEventCommand},
    incoming::MatrixDriverResponse,
    MatrixDriverRequestMeta, SendEventCommand, WidgetMachine,
};
use crate::widget::Capabilities;

/// A handle to a pending `toWidget` request.
pub(crate) struct MatrixDriverRequestHandle<'m, T> {
    request_meta: Option<&'m mut MatrixDriverRequestMeta>,
    _phantom: PhantomData<fn() -> T>,
}

impl<'m, T> MatrixDriverRequestHandle<'m, T>
where
    T: FromMatrixDriverResponse,
{
    pub(crate) fn new(request_meta: &'m mut MatrixDriverRequestMeta) -> Self {
        Self { request_meta: Some(request_meta), _phantom: PhantomData }
    }

    pub(crate) fn null() -> Self {
        Self { request_meta: None, _phantom: PhantomData }
    }

    pub(crate) fn then(
        self,
        response_handler: impl FnOnce(T, &mut WidgetMachine) + Send + 'static,
    ) {
        if let Some(request_meta) = self.request_meta {
            request_meta.response_fn = Some(Box::new(move |response, machine| {
                if let Some(response_data) = T::from_response(response) {
                    response_handler(response_data, machine)
                }
            }));
        }
    }
}

/// Represents a request that the widget API state machine can send.
pub(crate) trait MatrixDriverRequest: Into<MatrixDriverRequestData> {
    type Response: FromMatrixDriverResponse;
}

pub(crate) trait FromMatrixDriverResponse: Sized {
    fn from_response(_: MatrixDriverResponse) -> Option<Self>;
}

/// Ask the client (capability provider) to acquire given capabilities
/// from the user. The client must eventually respond with granted capabilities.
#[derive(Debug)]
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

/// Ask the client to read matrix event(s) that corresponds to the given
/// description and return a list of events as a response.
#[derive(Debug)]
pub(crate) struct ReadMatrixEvent(pub(crate) ReadMessageLikeEventCommand);

impl From<ReadMatrixEvent> for MatrixDriverRequestData {
    fn from(value: ReadMatrixEvent) -> Self {
        MatrixDriverRequestData::ReadMessageLikeEvent(value.0)
    }
}

impl MatrixDriverRequest for ReadMatrixEvent {
    type Response = Vec<Raw<AnyTimelineEvent>>;
}

impl FromMatrixDriverResponse for Vec<Raw<AnyTimelineEvent>> {
    fn from_response(ev: MatrixDriverResponse) -> Option<Self> {
        match ev {
            MatrixDriverResponse::MatrixEventRead(response) => Some(response),
            _ => {
                error!("bug in MatrixDriver, received wrong event response");
                None
            }
        }
    }
}

/// Ask the client to send matrix event that corresponds to the given
/// description and return an event ID as a response.
#[derive(Debug)]
pub(crate) struct SendMatrixEvent(pub(crate) SendEventCommand);

impl From<SendMatrixEvent> for MatrixDriverRequestData {
    fn from(value: SendMatrixEvent) -> Self {
        MatrixDriverRequestData::SendMatrixEvent(value.0)
    }
}

impl MatrixDriverRequest for SendMatrixEvent {
    type Response = OwnedEventId;
}

impl FromMatrixDriverResponse for OwnedEventId {
    fn from_response(ev: MatrixDriverResponse) -> Option<Self> {
        match ev {
            MatrixDriverResponse::MatrixEventSent(response) => Some(response),
            _ => {
                error!("bug in MatrixDriver, received wrong event response");
                None
            }
        }
    }
}
