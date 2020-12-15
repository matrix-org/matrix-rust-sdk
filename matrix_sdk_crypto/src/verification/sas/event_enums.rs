// Copyright 2020 The Matrix.org Foundation C.I.C.
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

use std::convert::TryInto;

use matrix_sdk_common::{
    events::{
        key::verification::{
            accept::{AcceptEventContent, AcceptToDeviceEventContent},
            start::{StartEventContent, StartMethod, StartToDeviceEventContent},
            KeyAgreementProtocol,
        },
        AnyMessageEventContent, AnyToDeviceEventContent, MessageEvent, ToDeviceEvent,
    },
    identifiers::RoomId,
    CanonicalJsonValue,
};

use super::FlowId;

#[derive(Clone, Debug)]
pub enum StartContent {
    ToDevice(StartToDeviceEventContent),
    Room(RoomId, StartEventContent),
}

impl StartContent {
    pub fn method(&self) -> &StartMethod {
        match self {
            StartContent::ToDevice(c) => &c.method,
            StartContent::Room(_, c) => &c.method,
        }
    }

    pub fn flow_id(&self) -> FlowId {
        match self {
            StartContent::ToDevice(c) => FlowId::ToDevice(c.transaction_id.clone()),
            StartContent::Room(r, c) => FlowId::InRoom(r.clone(), c.relation.event_id.clone()),
        }
    }

    pub fn to_canonical_json(self) -> CanonicalJsonValue {
        let content = match self {
            StartContent::ToDevice(c) => serde_json::to_value(c),
            StartContent::Room(_, c) => serde_json::to_value(c),
        };

        content
            .expect("Can't serialize content")
            .try_into()
            .expect("Can't canonicalize content")
    }
}

impl From<(RoomId, StartEventContent)> for StartContent {
    fn from(tuple: (RoomId, StartEventContent)) -> Self {
        StartContent::Room(tuple.0, tuple.1)
    }
}

impl From<StartToDeviceEventContent> for StartContent {
    fn from(content: StartToDeviceEventContent) -> Self {
        StartContent::ToDevice(content)
    }
}

#[derive(Clone, Debug)]
pub enum AcceptContent {
    ToDevice(AcceptToDeviceEventContent),
    Room(RoomId, AcceptEventContent),
}

impl From<AcceptToDeviceEventContent> for AcceptContent {
    fn from(content: AcceptToDeviceEventContent) -> Self {
        AcceptContent::ToDevice(content)
    }
}

impl From<(RoomId, AcceptEventContent)> for AcceptContent {
    fn from(content: (RoomId, AcceptEventContent)) -> Self {
        AcceptContent::Room(content.0, content.1)
    }
}

#[derive(Clone, Debug)]
pub enum OutgoingContent {
    Room(RoomId, AnyMessageEventContent),
    ToDevice(AnyToDeviceEventContent),
}

impl From<StartContent> for OutgoingContent {
    fn from(content: StartContent) -> Self {
        match content {
            StartContent::Room(r, c) => (r, AnyMessageEventContent::KeyVerificationStart(c)).into(),
            StartContent::ToDevice(c) => AnyToDeviceEventContent::KeyVerificationStart(c).into(),
        }
    }
}

impl From<AnyToDeviceEventContent> for OutgoingContent {
    fn from(content: AnyToDeviceEventContent) -> Self {
        OutgoingContent::ToDevice(content)
    }
}

impl From<(RoomId, AnyMessageEventContent)> for OutgoingContent {
    fn from(content: (RoomId, AnyMessageEventContent)) -> Self {
        OutgoingContent::Room(content.0, content.1)
    }
}
