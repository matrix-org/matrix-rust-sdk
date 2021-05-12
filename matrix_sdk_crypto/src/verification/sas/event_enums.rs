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

use std::{collections::BTreeMap, convert::TryInto};

use matrix_sdk_common::{
    events::{
        key::verification::{
            accept::{AcceptEventContent, AcceptMethod, AcceptToDeviceEventContent},
            cancel::{CancelEventContent, CancelToDeviceEventContent},
            done::DoneEventContent,
            key::{KeyEventContent, KeyToDeviceEventContent},
            mac::{MacEventContent, MacToDeviceEventContent},
            start::{StartEventContent, StartMethod, StartToDeviceEventContent},
        },
        AnyMessageEventContent, AnyToDeviceEventContent,
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

    pub fn canonical_json(self) -> CanonicalJsonValue {
        let content = match self {
            StartContent::ToDevice(c) => serde_json::to_value(c),
            StartContent::Room(_, c) => serde_json::to_value(c),
        };

        content.expect("Can't serialize content").try_into().expect("Can't canonicalize content")
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

impl AcceptContent {
    pub fn flow_id(&self) -> FlowId {
        match self {
            AcceptContent::ToDevice(c) => FlowId::ToDevice(c.transaction_id.clone()),
            AcceptContent::Room(r, c) => FlowId::InRoom(r.clone(), c.relation.event_id.clone()),
        }
    }

    pub fn method(&self) -> &AcceptMethod {
        match self {
            AcceptContent::ToDevice(c) => &c.method,
            AcceptContent::Room(_, c) => &c.method,
        }
    }
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

pub enum KeyContent {
    ToDevice(KeyToDeviceEventContent),
    Room(RoomId, KeyEventContent),
}

impl KeyContent {
    pub fn flow_id(&self) -> FlowId {
        match self {
            KeyContent::ToDevice(c) => FlowId::ToDevice(c.transaction_id.clone()),
            KeyContent::Room(r, c) => FlowId::InRoom(r.clone(), c.relation.event_id.clone()),
        }
    }

    pub fn public_key(&self) -> &str {
        match self {
            KeyContent::ToDevice(c) => &c.key,
            KeyContent::Room(_, c) => &c.key,
        }
    }
}

impl From<KeyToDeviceEventContent> for KeyContent {
    fn from(content: KeyToDeviceEventContent) -> Self {
        KeyContent::ToDevice(content)
    }
}

impl From<(RoomId, KeyEventContent)> for KeyContent {
    fn from(content: (RoomId, KeyEventContent)) -> Self {
        KeyContent::Room(content.0, content.1)
    }
}

pub enum MacContent {
    ToDevice(MacToDeviceEventContent),
    Room(RoomId, MacEventContent),
}

impl MacContent {
    pub fn flow_id(&self) -> FlowId {
        match self {
            MacContent::ToDevice(c) => FlowId::ToDevice(c.transaction_id.clone()),
            MacContent::Room(r, c) => FlowId::InRoom(r.clone(), c.relation.event_id.clone()),
        }
    }

    pub fn mac(&self) -> &BTreeMap<String, String> {
        match self {
            MacContent::ToDevice(c) => &c.mac,
            MacContent::Room(_, c) => &c.mac,
        }
    }

    pub fn keys(&self) -> &str {
        match self {
            MacContent::ToDevice(c) => &c.keys,
            MacContent::Room(_, c) => &c.keys,
        }
    }
}

impl From<MacToDeviceEventContent> for MacContent {
    fn from(content: MacToDeviceEventContent) -> Self {
        MacContent::ToDevice(content)
    }
}

impl From<(RoomId, MacEventContent)> for MacContent {
    fn from(content: (RoomId, MacEventContent)) -> Self {
        MacContent::Room(content.0, content.1)
    }
}

pub enum CancelContent {
    ToDevice(CancelToDeviceEventContent),
    Room(RoomId, CancelEventContent),
}

impl From<(RoomId, CancelEventContent)> for CancelContent {
    fn from(content: (RoomId, CancelEventContent)) -> Self {
        CancelContent::Room(content.0, content.1)
    }
}

impl From<CancelToDeviceEventContent> for CancelContent {
    fn from(content: CancelToDeviceEventContent) -> Self {
        CancelContent::ToDevice(content)
    }
}

pub enum DoneContent {
    Room(RoomId, DoneEventContent),
}

impl DoneContent {
    pub fn flow_id(&self) -> FlowId {
        match self {
            DoneContent::Room(r, c) => FlowId::InRoom(r.clone(), c.relation.event_id.clone()),
        }
    }
}

impl From<(RoomId, DoneEventContent)> for DoneContent {
    fn from(content: (RoomId, DoneEventContent)) -> Self {
        DoneContent::Room(content.0, content.1)
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

impl From<CancelContent> for OutgoingContent {
    fn from(content: CancelContent) -> Self {
        match content {
            CancelContent::Room(r, c) => {
                (r, AnyMessageEventContent::KeyVerificationCancel(c)).into()
            }
            CancelContent::ToDevice(c) => AnyToDeviceEventContent::KeyVerificationCancel(c).into(),
        }
    }
}

impl From<KeyContent> for OutgoingContent {
    fn from(content: KeyContent) -> Self {
        match content {
            KeyContent::Room(r, c) => (r, AnyMessageEventContent::KeyVerificationKey(c)).into(),
            KeyContent::ToDevice(c) => AnyToDeviceEventContent::KeyVerificationKey(c).into(),
        }
    }
}

impl From<DoneContent> for OutgoingContent {
    fn from(content: DoneContent) -> Self {
        match content {
            DoneContent::Room(r, c) => (r, AnyMessageEventContent::KeyVerificationDone(c)).into(),
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
