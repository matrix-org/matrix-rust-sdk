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
        key::verification::start::{StartEventContent, StartToDeviceEventContent},
        AnyMessageEventContent, AnyToDeviceEventContent, MessageEvent, ToDeviceEvent,
    },
    CanonicalJsonValue,
};

#[derive(Clone, Debug)]
pub enum StartContent {
    ToDevice(StartToDeviceEventContent),
    Room(StartEventContent),
}

impl StartContent {
    pub fn to_canonical_json(self) -> CanonicalJsonValue {
        let content = match self {
            StartContent::Room(c) => serde_json::to_value(c),
            StartContent::ToDevice(c) => serde_json::to_value(c),
        };

        content
            .expect("Can't serialize content")
            .try_into()
            .expect("Can't canonicalize content")
    }
}

impl From<StartEventContent> for StartContent {
    fn from(content: StartEventContent) -> Self {
        StartContent::Room(content)
    }
}

impl From<StartToDeviceEventContent> for StartContent {
    fn from(content: StartToDeviceEventContent) -> Self {
        StartContent::ToDevice(content)
    }
}

#[derive(Clone, Debug)]
pub enum OutgoingContent {
    Room(AnyMessageEventContent),
    ToDevice(AnyToDeviceEventContent),
}

impl From<StartContent> for OutgoingContent {
    fn from(content: StartContent) -> Self {
        match content {
            StartContent::Room(c) => {
                OutgoingContent::Room(AnyMessageEventContent::KeyVerificationStart(c))
            }
            StartContent::ToDevice(c) => {
                OutgoingContent::ToDevice(AnyToDeviceEventContent::KeyVerificationStart(c))
            }
        }
    }
}
