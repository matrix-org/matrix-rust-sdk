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

//! A high-level API to generate commands (requests) that we send to the widget
//! or client.

use std::borrow::Cow;

use ruma::{
    api::client::account::request_openid_token::v3::Response as RumaOpenIdResponse,
    events::AnyTimelineEvent, serde::Raw, OwnedEventId,
};

use super::{
    super::actions::{ReadEventCommand, SendEventCommand},
    messages::OpenIdResponse,
};
use crate::widget::Permissions;

pub struct CommandProxy;

impl CommandProxy {
    pub async fn send<T: Request>(&self, _request: T) -> Result<T::Response, Cow<'static, str>> {
        Err("not implemented".into())
    }
}

pub trait Request {
    type Response;
}

pub struct RequestPermissions;
impl Request for RequestPermissions {
    type Response = Permissions;
}

pub struct AcquirePermissions(pub Permissions);
impl Request for AcquirePermissions {
    type Response = Permissions;
}

pub struct UpdatePermissions(pub Permissions);
impl Request for UpdatePermissions {
    type Response = ();
}

pub struct RequestOpenId;
impl Request for RequestOpenId {
    type Response = RumaOpenIdResponse;
}

pub struct UpdateOpenId(pub OpenIdResponse);
impl Request for UpdateOpenId {
    type Response = ();
}

pub struct ReadMatrixEvent(pub ReadEventCommand);
impl Request for ReadMatrixEvent {
    type Response = Vec<Raw<AnyTimelineEvent>>;
}

pub struct SendMatrixEvent(pub SendEventCommand);
impl Request for SendMatrixEvent {
    type Response = OwnedEventId;
}
