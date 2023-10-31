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

//! Utility types and traits.

use ruma::{
    events::{AnyMessageLikeEventContent, AnyStateEventContent},
    serde::Raw,
};
use serde_json::value::{RawValue as RawJsonValue, Value as JsonValue};

#[cfg(doc)]
use crate::Room;

/// The set of types that can be used with [`Room::send_raw`].
pub trait IntoRawMessageLikeEventContent {
    #[doc(hidden)]
    fn into_raw_message_like_event_content(self) -> Raw<AnyMessageLikeEventContent>;
}

impl IntoRawMessageLikeEventContent for Raw<AnyMessageLikeEventContent> {
    fn into_raw_message_like_event_content(self) -> Raw<AnyMessageLikeEventContent> {
        self
    }
}

impl IntoRawMessageLikeEventContent for &Raw<AnyMessageLikeEventContent> {
    fn into_raw_message_like_event_content(self) -> Raw<AnyMessageLikeEventContent> {
        self.clone()
    }
}

impl IntoRawMessageLikeEventContent for JsonValue {
    fn into_raw_message_like_event_content(self) -> Raw<AnyMessageLikeEventContent> {
        (&self).into_raw_message_like_event_content()
    }
}

impl IntoRawMessageLikeEventContent for &JsonValue {
    fn into_raw_message_like_event_content(self) -> Raw<AnyMessageLikeEventContent> {
        Raw::new(self).expect("serde_json::Value never fails to serialize").cast()
    }
}

impl IntoRawMessageLikeEventContent for Box<RawJsonValue> {
    fn into_raw_message_like_event_content(self) -> Raw<AnyMessageLikeEventContent> {
        Raw::from_json(self)
    }
}

impl IntoRawMessageLikeEventContent for &RawJsonValue {
    fn into_raw_message_like_event_content(self) -> Raw<AnyMessageLikeEventContent> {
        self.to_owned().into_raw_message_like_event_content()
    }
}

impl IntoRawMessageLikeEventContent for &Box<RawJsonValue> {
    fn into_raw_message_like_event_content(self) -> Raw<AnyMessageLikeEventContent> {
        self.clone().into_raw_message_like_event_content()
    }
}

/// The set of types that can be used with [`Room::send_state_event_raw`].
pub trait IntoRawStateEventContent {
    #[doc(hidden)]
    fn into_raw_state_event_content(self) -> Raw<AnyStateEventContent>;
}

impl IntoRawStateEventContent for Raw<AnyStateEventContent> {
    fn into_raw_state_event_content(self) -> Raw<AnyStateEventContent> {
        self
    }
}

impl IntoRawStateEventContent for &Raw<AnyStateEventContent> {
    fn into_raw_state_event_content(self) -> Raw<AnyStateEventContent> {
        self.clone()
    }
}

impl IntoRawStateEventContent for JsonValue {
    fn into_raw_state_event_content(self) -> Raw<AnyStateEventContent> {
        (&self).into_raw_state_event_content()
    }
}

impl IntoRawStateEventContent for &JsonValue {
    fn into_raw_state_event_content(self) -> Raw<AnyStateEventContent> {
        Raw::new(self).expect("serde_json::Value never fails to serialize").cast()
    }
}

impl IntoRawStateEventContent for Box<RawJsonValue> {
    fn into_raw_state_event_content(self) -> Raw<AnyStateEventContent> {
        Raw::from_json(self)
    }
}

impl IntoRawStateEventContent for &RawJsonValue {
    fn into_raw_state_event_content(self) -> Raw<AnyStateEventContent> {
        self.to_owned().into_raw_state_event_content()
    }
}

impl IntoRawStateEventContent for &Box<RawJsonValue> {
    fn into_raw_state_event_content(self) -> Raw<AnyStateEventContent> {
        self.clone().into_raw_state_event_content()
    }
}
