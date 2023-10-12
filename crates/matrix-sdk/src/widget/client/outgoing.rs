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

//! A high-level API to generate commands (requests) that we send either
//! directly to the widget, or to the matrix driver/client.

use ruma::{
    api::client::account::request_openid_token::v3::Response as RumaOpenIdResponse,
    events::AnyTimelineEvent, serde::Raw, OwnedEventId,
};

use super::{
    actions::{ReadEventCommand, SendEventCommand},
    openid::OpenIdResponse,
};
use crate::widget::Permissions;

/// Represents a request that the widget API state machine can send.
pub(crate) trait Request {
    type Response;
}

/// Send a request to a widget asking it to respond with the list of
/// permissinos (capabilities) that the widget wants to have.
pub(crate) struct RequestPermissions;
impl Request for RequestPermissions {
    type Response = Permissions;
}

/// Ask the client (permission provider) to acquire given permissions
/// from the user. The client must eventually respond with granted permissions.
pub(crate) struct AcquirePermissions(pub(crate) Permissions);
impl Request for AcquirePermissions {
    type Response = Permissions;
}

/// Send a request to the widget asking it to update its permissions.
pub(crate) struct UpdatePermissions(pub(crate) Permissions);
impl Request for UpdatePermissions {
    type Response = ();
}

/// Request open ID from the Matrix client.
pub(crate) struct RequestOpenId;
impl Request for RequestOpenId {
    type Response = RumaOpenIdResponse;
}

/// Send a request to the widget asking it to update its open ID state.
pub(crate) struct UpdateOpenId(pub(crate) OpenIdResponse);
impl Request for UpdateOpenId {
    type Response = ();
}

/// Ask the client to read matrix event(s) that corresponds to the given
/// description and return a list of events as a response.
pub(crate) struct ReadMatrixEvent(pub(crate) ReadEventCommand);
impl Request for ReadMatrixEvent {
    type Response = Vec<Raw<AnyTimelineEvent>>;
}

/// Ask the client to send matrix event that corresponds to the given
/// description and return an event ID as a response.
pub(crate) struct SendMatrixEvent(pub(crate) SendEventCommand);
impl Request for SendMatrixEvent {
    type Response = OwnedEventId;
}
