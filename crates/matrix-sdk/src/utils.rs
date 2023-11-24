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

#[cfg(feature = "e2e-encryption")]
use std::sync::{Arc, RwLock};

#[cfg(feature = "e2e-encryption")]
use futures_core::Stream;
#[cfg(feature = "e2e-encryption")]
use futures_util::StreamExt;
use ruma::{
    events::{AnyMessageLikeEventContent, AnyStateEventContent},
    serde::Raw,
};
use serde_json::value::{RawValue as RawJsonValue, Value as JsonValue};
#[cfg(feature = "e2e-encryption")]
use tokio::sync::broadcast;
#[cfg(feature = "e2e-encryption")]
use tokio_stream::wrappers::{errors::BroadcastStreamRecvError, BroadcastStream};

#[cfg(doc)]
use crate::Room;

/// An observable with channel semantics.
///
/// Channel semantics means that each update to the shared mutable value will be
/// sent out to subscribers. That is, intermediate updates to the value will not
/// be skipped like they would be in an observable without channel semantics.
#[cfg(feature = "e2e-encryption")]
#[derive(Clone, Debug)]
pub(crate) struct ChannelObservable<T: Clone + Send> {
    value: Arc<RwLock<T>>,
    channel: broadcast::Sender<T>,
}

#[cfg(feature = "e2e-encryption")]
impl<T: Default + Clone + Send + 'static> Default for ChannelObservable<T> {
    fn default() -> Self {
        let value = Default::default();
        Self::new(value)
    }
}

#[cfg(feature = "e2e-encryption")]
impl<T: 'static + Send + Clone> ChannelObservable<T> {
    /// Create a new [`ChannelObservable`] with the given value for the
    /// underlying data.
    pub(crate) fn new(value: T) -> Self {
        let channel = broadcast::Sender::new(100);
        Self { value: RwLock::new(value).into(), channel }
    }

    /// Subscribe to updates to the observable value.
    ///
    /// The current value will always be emitted as the first item in the
    /// stream.
    pub(crate) fn subscribe(&self) -> impl Stream<Item = Result<T, BroadcastStreamRecvError>> {
        let current_value = self.value.read().unwrap().to_owned();
        let initial_stream = tokio_stream::once(Ok(current_value));
        let broadcast_stream = BroadcastStream::new(self.channel.subscribe());

        initial_stream.chain(broadcast_stream)
    }

    /// Set the underlying data to the new value.
    pub(crate) fn set(&self, new_value: T) {
        *self.value.write().unwrap() = new_value.to_owned();
        // We're ignoring the error case where no receivers exist.
        let _ = self.channel.send(new_value);
    }

    /// Get the current value of the underlying data.
    pub(crate) fn get(&self) -> T {
        self.value.read().unwrap().to_owned()
    }
}

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
