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

use matrix_sdk_common::{
    events::{AnyMessageEventContent, AnyToDeviceEventContent},
    identifiers::RoomId,
};

#[derive(Clone, Debug)]
pub enum OutgoingContent {
    Room(RoomId, AnyMessageEventContent),
    ToDevice(AnyToDeviceEventContent),
}

impl From<OwnedStartContent> for OutgoingContent {
    fn from(content: OwnedStartContent) -> Self {
        match content {
            OwnedStartContent::Room(r, c) => {
                (r, AnyMessageEventContent::KeyVerificationStart(c)).into()
            }
            OwnedStartContent::ToDevice(c) => {
                AnyToDeviceEventContent::KeyVerificationStart(c).into()
            }
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

#[cfg(test)]
use std::convert::TryFrom;

use crate::verification::event_enums::OwnedStartContent;
#[cfg(test)]
use crate::{OutgoingRequest, OutgoingVerificationRequest, RoomMessageRequest, ToDeviceRequest};

#[cfg(test)]
impl From<OutgoingVerificationRequest> for OutgoingContent {
    fn from(request: OutgoingVerificationRequest) -> Self {
        match request {
            OutgoingVerificationRequest::ToDevice(r) => Self::try_from(r).unwrap(),
            OutgoingVerificationRequest::InRoom(r) => Self::from(r),
        }
    }
}

#[cfg(test)]
impl From<RoomMessageRequest> for OutgoingContent {
    fn from(value: RoomMessageRequest) -> Self {
        (value.room_id, value.content).into()
    }
}

#[cfg(test)]
impl TryFrom<ToDeviceRequest> for OutgoingContent {
    type Error = ();

    fn try_from(value: ToDeviceRequest) -> Result<Self, Self::Error> {
        use matrix_sdk_common::events::EventType;
        use serde_json::Value;

        let json: Value = serde_json::from_str(
            value
                .messages
                .values()
                .next()
                .and_then(|m| m.values().next().map(|j| j.get()))
                .ok_or(())?,
        )
        .map_err(|_| ())?;

        match value.event_type {
            EventType::KeyVerificationRequest => {
                Ok(AnyToDeviceEventContent::KeyVerificationRequest(
                    serde_json::from_value(json).map_err(|_| ())?,
                )
                .into())
            }
            EventType::KeyVerificationReady => Ok(AnyToDeviceEventContent::KeyVerificationReady(
                serde_json::from_value(json).map_err(|_| ())?,
            )
            .into()),
            EventType::KeyVerificationDone => Ok(AnyToDeviceEventContent::KeyVerificationDone(
                serde_json::from_value(json).map_err(|_| ())?,
            )
            .into()),
            EventType::KeyVerificationStart => Ok(AnyToDeviceEventContent::KeyVerificationStart(
                serde_json::from_value(json).map_err(|_| ())?,
            )
            .into()),
            EventType::KeyVerificationKey => Ok(AnyToDeviceEventContent::KeyVerificationKey(
                serde_json::from_value(json).map_err(|_| ())?,
            )
            .into()),
            EventType::KeyVerificationAccept => Ok(AnyToDeviceEventContent::KeyVerificationAccept(
                serde_json::from_value(json).map_err(|_| ())?,
            )
            .into()),
            EventType::KeyVerificationMac => Ok(AnyToDeviceEventContent::KeyVerificationMac(
                serde_json::from_value(json).map_err(|_| ())?,
            )
            .into()),
            EventType::KeyVerificationCancel => Ok(AnyToDeviceEventContent::KeyVerificationCancel(
                serde_json::from_value(json).map_err(|_| ())?,
            )
            .into()),
            _ => Err(()),
        }
    }
}

#[cfg(test)]
impl TryFrom<OutgoingRequest> for OutgoingContent {
    type Error = ();

    fn try_from(value: OutgoingRequest) -> Result<Self, ()> {
        match value.request() {
            crate::OutgoingRequests::KeysUpload(_) => Err(()),
            crate::OutgoingRequests::KeysQuery(_) => Err(()),
            crate::OutgoingRequests::ToDeviceRequest(r) => Self::try_from(r.clone()),
            crate::OutgoingRequests::SignatureUpload(_) => Err(()),
            crate::OutgoingRequests::RoomMessage(r) => Ok(Self::from(r.clone())),
        }
    }
}
