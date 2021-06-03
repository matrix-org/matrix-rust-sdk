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

use std::{
    collections::BTreeMap,
    convert::{TryFrom, TryInto},
};

use matrix_sdk_common::{
    events::{
        key::verification::{
            accept::{AcceptEventContent, AcceptMethod, AcceptToDeviceEventContent},
            cancel::{CancelCode, CancelEventContent, CancelToDeviceEventContent},
            done::{DoneEventContent, DoneToDeviceEventContent},
            key::{KeyEventContent, KeyToDeviceEventContent},
            mac::{MacEventContent, MacToDeviceEventContent},
            ready::{ReadyEventContent, ReadyToDeviceEventContent},
            request::RequestToDeviceEventContent,
            start::{StartEventContent, StartMethod, StartToDeviceEventContent},
            VerificationMethod,
        },
        room::message::{KeyVerificationRequestEventContent, MessageType},
        AnyMessageEvent, AnyMessageEventContent, AnyToDeviceEvent, AnyToDeviceEventContent,
    },
    identifiers::{DeviceId, RoomId, UserId},
    CanonicalJsonValue,
};

use super::{sas::OutgoingContent, FlowId};

#[derive(Debug)]
pub enum AnyEvent<'a> {
    Room(&'a AnyMessageEvent),
    ToDevice(&'a AnyToDeviceEvent),
}

impl AnyEvent<'_> {
    pub fn sender(&self) -> &UserId {
        match self {
            Self::Room(e) => &e.sender(),
            Self::ToDevice(e) => &e.sender(),
        }
    }

    pub fn verification_content(&self) -> Option<AnyVerificationContent> {
        match self {
            AnyEvent::Room(e) => match e {
                AnyMessageEvent::CallAnswer(_)
                | AnyMessageEvent::CallInvite(_)
                | AnyMessageEvent::CallHangup(_)
                | AnyMessageEvent::CallCandidates(_)
                | AnyMessageEvent::Reaction(_)
                | AnyMessageEvent::RoomEncrypted(_)
                | AnyMessageEvent::RoomMessageFeedback(_)
                | AnyMessageEvent::RoomRedaction(_)
                | AnyMessageEvent::Sticker(_)
                | AnyMessageEvent::Custom(_) => None,
                AnyMessageEvent::RoomMessage(m) => {
                    if let MessageType::VerificationRequest(v) = &m.content.msgtype {
                        Some(RequestContent::from(v).into())
                    } else {
                        None
                    }
                }
                AnyMessageEvent::KeyVerificationReady(e) => {
                    Some(ReadyContent::from(&e.content).into())
                }
                AnyMessageEvent::KeyVerificationStart(e) => {
                    Some(StartContent::from(&e.content).into())
                }
                AnyMessageEvent::KeyVerificationCancel(e) => {
                    Some(CancelContent::from(&e.content).into())
                }
                AnyMessageEvent::KeyVerificationAccept(e) => {
                    Some(AcceptContent::from(&e.content).into())
                }
                AnyMessageEvent::KeyVerificationKey(e) => Some(KeyContent::from(&e.content).into()),
                AnyMessageEvent::KeyVerificationMac(e) => Some(MacContent::from(&e.content).into()),
                AnyMessageEvent::KeyVerificationDone(e) => {
                    Some(DoneContent::from(&e.content).into())
                }
            },
            AnyEvent::ToDevice(e) => match e {
                AnyToDeviceEvent::Dummy(_)
                | AnyToDeviceEvent::RoomKey(_)
                | AnyToDeviceEvent::RoomKeyRequest(_)
                | AnyToDeviceEvent::ForwardedRoomKey(_)
                | AnyToDeviceEvent::RoomEncrypted(_)
                | AnyToDeviceEvent::Custom(_) => None,
                AnyToDeviceEvent::KeyVerificationRequest(e) => {
                    Some(RequestContent::from(&e.content).into())
                }
                AnyToDeviceEvent::KeyVerificationReady(e) => {
                    Some(ReadyContent::from(&e.content).into())
                }
                AnyToDeviceEvent::KeyVerificationStart(e) => {
                    Some(StartContent::from(&e.content).into())
                }
                AnyToDeviceEvent::KeyVerificationCancel(e) => {
                    Some(CancelContent::from(&e.content).into())
                }
                AnyToDeviceEvent::KeyVerificationAccept(e) => {
                    Some(AcceptContent::from(&e.content).into())
                }
                AnyToDeviceEvent::KeyVerificationKey(e) => {
                    Some(KeyContent::from(&e.content).into())
                }
                AnyToDeviceEvent::KeyVerificationMac(e) => {
                    Some(MacContent::from(&e.content).into())
                }
                AnyToDeviceEvent::KeyVerificationDone(e) => {
                    Some(DoneContent::from(&e.content).into())
                }
            },
        }
    }
}

impl<'a> From<&'a AnyMessageEvent> for AnyEvent<'a> {
    fn from(e: &'a AnyMessageEvent) -> Self {
        Self::Room(e)
    }
}

impl<'a> From<&'a AnyToDeviceEvent> for AnyEvent<'a> {
    fn from(e: &'a AnyToDeviceEvent) -> Self {
        Self::ToDevice(e)
    }
}

impl TryFrom<&AnyEvent<'_>> for FlowId {
    type Error = ();

    fn try_from(value: &AnyEvent<'_>) -> Result<Self, Self::Error> {
        match value {
            AnyEvent::Room(e) => FlowId::try_from(*e),
            AnyEvent::ToDevice(e) => FlowId::try_from(*e),
        }
    }
}

impl TryFrom<&AnyMessageEvent> for FlowId {
    type Error = ();

    fn try_from(value: &AnyMessageEvent) -> Result<Self, Self::Error> {
        match value {
            AnyMessageEvent::CallAnswer(_)
            | AnyMessageEvent::CallInvite(_)
            | AnyMessageEvent::CallHangup(_)
            | AnyMessageEvent::CallCandidates(_)
            | AnyMessageEvent::Reaction(_)
            | AnyMessageEvent::RoomEncrypted(_)
            | AnyMessageEvent::RoomMessageFeedback(_)
            | AnyMessageEvent::RoomRedaction(_)
            | AnyMessageEvent::Sticker(_)
            | AnyMessageEvent::Custom(_) => Err(()),
            AnyMessageEvent::KeyVerificationReady(e) => {
                Ok(FlowId::from((&e.room_id, &e.content.relation.event_id)))
            }
            AnyMessageEvent::RoomMessage(e) => Ok(FlowId::from((&e.room_id, &e.event_id))),
            AnyMessageEvent::KeyVerificationStart(e) => {
                Ok(FlowId::from((&e.room_id, &e.content.relation.event_id)))
            }
            AnyMessageEvent::KeyVerificationCancel(e) => {
                Ok(FlowId::from((&e.room_id, &e.content.relation.event_id)))
            }
            AnyMessageEvent::KeyVerificationAccept(e) => {
                Ok(FlowId::from((&e.room_id, &e.content.relation.event_id)))
            }
            AnyMessageEvent::KeyVerificationKey(e) => {
                Ok(FlowId::from((&e.room_id, &e.content.relation.event_id)))
            }
            AnyMessageEvent::KeyVerificationMac(e) => {
                Ok(FlowId::from((&e.room_id, &e.content.relation.event_id)))
            }
            AnyMessageEvent::KeyVerificationDone(e) => {
                Ok(FlowId::from((&e.room_id, &e.content.relation.event_id)))
            }
        }
    }
}

impl TryFrom<&AnyToDeviceEvent> for FlowId {
    type Error = ();

    fn try_from(value: &AnyToDeviceEvent) -> Result<Self, Self::Error> {
        match value {
            AnyToDeviceEvent::Dummy(_)
            | AnyToDeviceEvent::RoomKey(_)
            | AnyToDeviceEvent::RoomKeyRequest(_)
            | AnyToDeviceEvent::ForwardedRoomKey(_)
            | AnyToDeviceEvent::RoomEncrypted(_)
            | AnyToDeviceEvent::Custom(_) => Err(()),
            AnyToDeviceEvent::KeyVerificationRequest(e) => {
                Ok(FlowId::from(e.content.transaction_id.to_owned()))
            }
            AnyToDeviceEvent::KeyVerificationReady(e) => {
                Ok(FlowId::from(e.content.transaction_id.to_owned()))
            }
            AnyToDeviceEvent::KeyVerificationStart(e) => {
                Ok(FlowId::from(e.content.transaction_id.to_owned()))
            }
            AnyToDeviceEvent::KeyVerificationCancel(e) => {
                Ok(FlowId::from(e.content.transaction_id.to_owned()))
            }
            AnyToDeviceEvent::KeyVerificationAccept(e) => {
                Ok(FlowId::from(e.content.transaction_id.to_owned()))
            }
            AnyToDeviceEvent::KeyVerificationKey(e) => {
                Ok(FlowId::from(e.content.transaction_id.to_owned()))
            }
            AnyToDeviceEvent::KeyVerificationMac(e) => {
                Ok(FlowId::from(e.content.transaction_id.to_owned()))
            }
            AnyToDeviceEvent::KeyVerificationDone(e) => {
                Ok(FlowId::from(e.content.transaction_id.to_owned()))
            }
        }
    }
}

#[derive(Debug)]
pub enum AnyVerificationContent<'a> {
    Request(RequestContent<'a>),
    Ready(ReadyContent<'a>),
    Cancel(CancelContent<'a>),
    Start(StartContent<'a>),
    Done(DoneContent<'a>),
    Accept(AcceptContent<'a>),
    Key(KeyContent<'a>),
    Mac(MacContent<'a>),
}

impl<'a> From<ReadyContent<'a>> for AnyVerificationContent<'a> {
    fn from(c: ReadyContent<'a>) -> Self {
        Self::Ready(c)
    }
}

impl<'a> From<RequestContent<'a>> for AnyVerificationContent<'a> {
    fn from(c: RequestContent<'a>) -> Self {
        Self::Request(c)
    }
}

impl<'a> From<StartContent<'a>> for AnyVerificationContent<'a> {
    fn from(c: StartContent<'a>) -> Self {
        Self::Start(c)
    }
}

impl<'a> From<DoneContent<'a>> for AnyVerificationContent<'a> {
    fn from(c: DoneContent<'a>) -> Self {
        Self::Done(c)
    }
}

impl<'a> From<CancelContent<'a>> for AnyVerificationContent<'a> {
    fn from(c: CancelContent<'a>) -> Self {
        Self::Cancel(c)
    }
}

impl<'a> From<AcceptContent<'a>> for AnyVerificationContent<'a> {
    fn from(c: AcceptContent<'a>) -> Self {
        Self::Accept(c)
    }
}

impl<'a> From<KeyContent<'a>> for AnyVerificationContent<'a> {
    fn from(c: KeyContent<'a>) -> Self {
        Self::Key(c)
    }
}

impl<'a> From<MacContent<'a>> for AnyVerificationContent<'a> {
    fn from(c: MacContent<'a>) -> Self {
        Self::Mac(c)
    }
}

#[derive(Debug)]
pub enum RequestContent<'a> {
    ToDevice(&'a RequestToDeviceEventContent),
    Room(&'a KeyVerificationRequestEventContent),
}

impl RequestContent<'_> {
    pub fn from_device(&self) -> &DeviceId {
        match self {
            Self::ToDevice(t) => &t.from_device,
            Self::Room(r) => &r.from_device,
        }
    }

    pub fn methods(&self) -> &[VerificationMethod] {
        match self {
            Self::ToDevice(t) => &t.methods,
            Self::Room(r) => &r.methods,
        }
    }
}

impl<'a> From<&'a KeyVerificationRequestEventContent> for RequestContent<'a> {
    fn from(c: &'a KeyVerificationRequestEventContent) -> Self {
        Self::Room(c)
    }
}

impl<'a> From<&'a RequestToDeviceEventContent> for RequestContent<'a> {
    fn from(c: &'a RequestToDeviceEventContent) -> Self {
        Self::ToDevice(c)
    }
}

#[derive(Debug)]
pub enum ReadyContent<'a> {
    ToDevice(&'a ReadyToDeviceEventContent),
    Room(&'a ReadyEventContent),
}

impl ReadyContent<'_> {
    pub fn from_device(&self) -> &DeviceId {
        match self {
            Self::ToDevice(t) => &t.from_device,
            Self::Room(r) => &r.from_device,
        }
    }

    pub fn methods(&self) -> &[VerificationMethod] {
        match self {
            Self::ToDevice(t) => &t.methods,
            Self::Room(r) => &r.methods,
        }
    }
}

impl<'a> From<&'a ReadyEventContent> for ReadyContent<'a> {
    fn from(c: &'a ReadyEventContent) -> Self {
        Self::Room(c)
    }
}

impl<'a> From<&'a ReadyToDeviceEventContent> for ReadyContent<'a> {
    fn from(c: &'a ReadyToDeviceEventContent) -> Self {
        Self::ToDevice(c)
    }
}

macro_rules! try_from_outgoing_content {
    ($type: ident, $enum_variant: ident) => {
        impl<'a> TryFrom<&'a OutgoingContent> for $type<'a> {
            type Error = ();

            fn try_from(value: &'a OutgoingContent) -> Result<Self, Self::Error> {
                match value {
                    OutgoingContent::Room(_, c) => {
                        if let AnyMessageEventContent::$enum_variant(c) = c {
                            Ok(Self::Room(c))
                        } else {
                            Err(())
                        }
                    }
                    OutgoingContent::ToDevice(c) => {
                        if let AnyToDeviceEventContent::$enum_variant(c) = c {
                            Ok(Self::ToDevice(c))
                        } else {
                            Err(())
                        }
                    }
                }
            }
        }
    };
}

try_from_outgoing_content!(ReadyContent, KeyVerificationReady);
try_from_outgoing_content!(StartContent, KeyVerificationStart);
try_from_outgoing_content!(AcceptContent, KeyVerificationAccept);
try_from_outgoing_content!(KeyContent, KeyVerificationKey);
try_from_outgoing_content!(MacContent, KeyVerificationMac);
try_from_outgoing_content!(CancelContent, KeyVerificationCancel);
try_from_outgoing_content!(DoneContent, KeyVerificationDone);

#[derive(Debug)]
pub enum StartContent<'a> {
    ToDevice(&'a StartToDeviceEventContent),
    Room(&'a StartEventContent),
}

impl<'a> StartContent<'a> {
    pub fn from_device(&self) -> &DeviceId {
        match self {
            Self::ToDevice(c) => &c.from_device,
            Self::Room(c) => &c.from_device,
        }
    }

    pub fn flow_id(&self) -> &str {
        match self {
            Self::ToDevice(c) => &c.transaction_id,
            Self::Room(c) => &c.relation.event_id.as_str(),
        }
    }

    pub fn method(&self) -> &StartMethod {
        match self {
            Self::ToDevice(c) => &c.method,
            Self::Room(c) => &c.method,
        }
    }

    pub fn canonical_json(&self) -> CanonicalJsonValue {
        let content = match self {
            Self::ToDevice(c) => serde_json::to_value(c),
            Self::Room(c) => serde_json::to_value(c),
        };

        content.expect("Can't serialize content").try_into().expect("Can't canonicalize content")
    }
}

impl<'a> From<&'a StartEventContent> for StartContent<'a> {
    fn from(c: &'a StartEventContent) -> Self {
        Self::Room(c)
    }
}

impl<'a> From<&'a StartToDeviceEventContent> for StartContent<'a> {
    fn from(c: &'a StartToDeviceEventContent) -> Self {
        Self::ToDevice(c)
    }
}

impl<'a> From<&'a OwnedStartContent> for StartContent<'a> {
    fn from(c: &'a OwnedStartContent) -> Self {
        match c {
            OwnedStartContent::ToDevice(c) => Self::ToDevice(c),
            OwnedStartContent::Room(_, c) => Self::Room(c),
        }
    }
}

#[derive(Debug)]
pub enum DoneContent<'a> {
    ToDevice(&'a DoneToDeviceEventContent),
    Room(&'a DoneEventContent),
}

impl<'a> From<&'a DoneEventContent> for DoneContent<'a> {
    fn from(c: &'a DoneEventContent) -> Self {
        Self::Room(c)
    }
}

impl<'a> From<&'a DoneToDeviceEventContent> for DoneContent<'a> {
    fn from(c: &'a DoneToDeviceEventContent) -> Self {
        Self::ToDevice(c)
    }
}

impl<'a> DoneContent<'a> {
    pub fn flow_id(&self) -> &str {
        match self {
            Self::ToDevice(c) => &c.transaction_id,
            Self::Room(c) => &c.relation.event_id.as_str(),
        }
    }
}

#[derive(Clone, Debug)]
pub enum AcceptContent<'a> {
    ToDevice(&'a AcceptToDeviceEventContent),
    Room(&'a AcceptEventContent),
}

impl AcceptContent<'_> {
    pub fn flow_id(&self) -> &str {
        match self {
            Self::ToDevice(c) => &c.transaction_id,
            Self::Room(c) => c.relation.event_id.as_str(),
        }
    }

    pub fn method(&self) -> &AcceptMethod {
        match self {
            Self::ToDevice(c) => &c.method,
            Self::Room(c) => &c.method,
        }
    }
}

impl<'a> From<&'a AcceptToDeviceEventContent> for AcceptContent<'a> {
    fn from(content: &'a AcceptToDeviceEventContent) -> Self {
        Self::ToDevice(content)
    }
}

impl<'a> From<&'a AcceptEventContent> for AcceptContent<'a> {
    fn from(content: &'a AcceptEventContent) -> Self {
        Self::Room(content)
    }
}

impl<'a> From<&'a OwnedAcceptContent> for AcceptContent<'a> {
    fn from(content: &'a OwnedAcceptContent) -> Self {
        match content {
            OwnedAcceptContent::ToDevice(c) => Self::ToDevice(c),
            OwnedAcceptContent::Room(_, c) => Self::Room(c),
        }
    }
}

#[derive(Clone, Debug)]
pub enum KeyContent<'a> {
    ToDevice(&'a KeyToDeviceEventContent),
    Room(&'a KeyEventContent),
}

impl KeyContent<'_> {
    pub fn flow_id(&self) -> &str {
        match self {
            Self::ToDevice(c) => &c.transaction_id,
            Self::Room(c) => c.relation.event_id.as_str(),
        }
    }

    pub fn public_key(&self) -> &str {
        match self {
            Self::ToDevice(c) => &c.key,
            Self::Room(c) => &c.key,
        }
    }
}

impl<'a> From<&'a KeyToDeviceEventContent> for KeyContent<'a> {
    fn from(content: &'a KeyToDeviceEventContent) -> Self {
        Self::ToDevice(content)
    }
}

impl<'a> From<&'a KeyEventContent> for KeyContent<'a> {
    fn from(content: &'a KeyEventContent) -> Self {
        Self::Room(content)
    }
}

#[derive(Clone, Debug)]
pub enum MacContent<'a> {
    ToDevice(&'a MacToDeviceEventContent),
    Room(&'a MacEventContent),
}

impl MacContent<'_> {
    pub fn flow_id(&self) -> &str {
        match self {
            Self::ToDevice(c) => &c.transaction_id,
            Self::Room(c) => c.relation.event_id.as_str(),
        }
    }

    pub fn mac(&self) -> &BTreeMap<String, String> {
        match self {
            Self::ToDevice(c) => &c.mac,
            Self::Room(c) => &c.mac,
        }
    }

    pub fn keys(&self) -> &str {
        match self {
            Self::ToDevice(c) => &c.keys,
            Self::Room(c) => &c.keys,
        }
    }
}

impl<'a> From<&'a MacToDeviceEventContent> for MacContent<'a> {
    fn from(content: &'a MacToDeviceEventContent) -> Self {
        Self::ToDevice(content)
    }
}

impl<'a> From<&'a MacEventContent> for MacContent<'a> {
    fn from(content: &'a MacEventContent) -> Self {
        Self::Room(content)
    }
}

#[derive(Clone, Debug)]
pub enum CancelContent<'a> {
    ToDevice(&'a CancelToDeviceEventContent),
    Room(&'a CancelEventContent),
}

impl CancelContent<'_> {
    pub fn cancel_code(&self) -> &CancelCode {
        match self {
            Self::ToDevice(c) => &c.code,
            Self::Room(c) => &c.code,
        }
    }
}

impl<'a> From<&'a CancelEventContent> for CancelContent<'a> {
    fn from(content: &'a CancelEventContent) -> Self {
        Self::Room(content)
    }
}

impl<'a> From<&'a CancelToDeviceEventContent> for CancelContent<'a> {
    fn from(content: &'a CancelToDeviceEventContent) -> Self {
        Self::ToDevice(content)
    }
}

#[derive(Clone, Debug)]
pub enum OwnedStartContent {
    ToDevice(StartToDeviceEventContent),
    Room(RoomId, StartEventContent),
}

impl OwnedStartContent {
    pub fn method(&self) -> &StartMethod {
        match self {
            Self::ToDevice(c) => &c.method,
            Self::Room(_, c) => &c.method,
        }
    }

    pub fn method_mut(&mut self) -> &mut StartMethod {
        match self {
            Self::ToDevice(c) => &mut c.method,
            Self::Room(_, c) => &mut c.method,
        }
    }

    pub fn as_start_content(&self) -> StartContent<'_> {
        match self {
            Self::ToDevice(c) => StartContent::ToDevice(c),
            Self::Room(_, c) => StartContent::Room(c),
        }
    }

    pub fn flow_id(&self) -> FlowId {
        match self {
            Self::ToDevice(c) => FlowId::ToDevice(c.transaction_id.clone()),
            Self::Room(r, c) => FlowId::InRoom(r.clone(), c.relation.event_id.clone()),
        }
    }

    pub fn canonical_json(self) -> CanonicalJsonValue {
        let content = match self {
            Self::ToDevice(c) => serde_json::to_value(c),
            Self::Room(_, c) => serde_json::to_value(c),
        };

        content.expect("Can't serialize content").try_into().expect("Can't canonicalize content")
    }
}

impl From<(RoomId, StartEventContent)> for OwnedStartContent {
    fn from(tuple: (RoomId, StartEventContent)) -> Self {
        Self::Room(tuple.0, tuple.1)
    }
}

impl From<StartToDeviceEventContent> for OwnedStartContent {
    fn from(content: StartToDeviceEventContent) -> Self {
        Self::ToDevice(content)
    }
}

#[derive(Clone, Debug)]
pub enum OwnedAcceptContent {
    ToDevice(AcceptToDeviceEventContent),
    Room(RoomId, AcceptEventContent),
}

impl From<AcceptToDeviceEventContent> for OwnedAcceptContent {
    fn from(content: AcceptToDeviceEventContent) -> Self {
        Self::ToDevice(content)
    }
}

impl From<(RoomId, AcceptEventContent)> for OwnedAcceptContent {
    fn from(content: (RoomId, AcceptEventContent)) -> Self {
        Self::Room(content.0, content.1)
    }
}

impl OwnedAcceptContent {
    pub fn method_mut(&mut self) -> &mut AcceptMethod {
        match self {
            Self::ToDevice(c) => &mut c.method,
            Self::Room(_, c) => &mut c.method,
        }
    }
}
