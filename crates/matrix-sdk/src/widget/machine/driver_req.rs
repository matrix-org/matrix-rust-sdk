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

//use ruma::{
//    api::client::account::request_openid_token::v3::Response as
// RumaOpenIdResponse,    events::AnyTimelineEvent, serde::Raw, OwnedEventId,
//};
use tracing::error;

use super::{
    actions::MatrixDriverRequestData,
    //actions::{ReadMessageLikeEventCommand, SendEventCommand},
    Event,
    MatrixDriverRequestMeta,
    WidgetMachine,
};
use crate::widget::Permissions;

/// A handle to a pending `toWidget` request.
pub(crate) struct MatrixDriverRequestHandle<'m, T> {
    request_meta: Option<&'m mut MatrixDriverRequestMeta>,
    _phantom: PhantomData<fn() -> T>,
}

impl<'m, T> MatrixDriverRequestHandle<'m, T>
where
    T: MatrixDriverResponse,
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
            request_meta.response_fn = Some(Box::new(move |event, machine| {
                if let Some(response_data) = T::from_event(event) {
                    response_handler(response_data, machine)
                }
            }));
        }
    }
}

/// Represents a request that the widget API state machine can send.
pub(crate) trait MatrixDriverRequest: Into<MatrixDriverRequestData> {
    type Response: MatrixDriverResponse;
}

pub(crate) trait MatrixDriverResponse: Sized {
    fn from_event(_: Event) -> Option<Self>;
}

/// Ask the client (permission provider) to acquire given permissions
/// from the user. The client must eventually respond with granted permissions.
#[derive(Debug)]
pub(crate) struct AcquirePermissions {
    pub(crate) desired_permissions: Permissions,
}

impl From<AcquirePermissions> for MatrixDriverRequestData {
    fn from(value: AcquirePermissions) -> Self {
        MatrixDriverRequestData::AcquirePermissions(value)
    }
}

impl MatrixDriverRequest for AcquirePermissions {
    type Response = Permissions;
}

impl MatrixDriverResponse for Permissions {
    fn from_event(ev: Event) -> Option<Self> {
        match ev {
            Event::PermissionsAcquired(response) => response.result.ok(),
            Event::MessageFromWidget(_) | Event::MatrixEventReceived(_) => {
                error!("this should be unreachable, no ID to match");
                None
            }
            Event::OpenIdReceived(_) | Event::MatrixEventSent(_) | Event::MatrixEventRead(_) => {
                error!("bug in MatrixDriver, received wrong event response");
                None
            }
        }
    }
}

/*
/// Request open ID from the Matrix client.
pub(crate) struct RequestOpenId;
impl MatrixDriverRequest for RequestOpenId {
    type Response = RumaOpenIdResponse;
}

/// Ask the client to read matrix event(s) that corresponds to the given
/// description and return a list of events as a response.
pub(crate) struct ReadMatrixEvent(pub(crate) ReadMessageLikeEventCommand);
impl MatrixDriverRequest for ReadMatrixEvent {
    type Response = Vec<Raw<AnyTimelineEvent>>;
}

/// Ask the client to send matrix event that corresponds to the given
/// description and return an event ID as a response.
pub(crate) struct SendMatrixEvent(pub(crate) SendEventCommand);
impl MatrixDriverRequest for SendMatrixEvent {
    type Response = OwnedEventId;
}
 */
