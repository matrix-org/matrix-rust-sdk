// Copyright 2021 Jonas Platte
// Copyright 2022 Famedly GmbH
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

use std::ops::Deref;

use matrix_sdk_base::deserialized_responses::EncryptionInfo;
use ruma::push::Action;
use serde_json::value::RawValue as RawJsonValue;

use super::{EventHandlerData, EventHandlerHandle};
use crate::{room, Client};

/// Context for an event handler.
///
/// This trait defines the set of types that may be used as additional arguments
/// in event handler functions after the event itself.
pub trait EventHandlerContext: Sized {
    #[doc(hidden)]
    fn from_data(_: &EventHandlerData<'_>) -> Option<Self>;
}

impl EventHandlerContext for Client {
    fn from_data(data: &EventHandlerData<'_>) -> Option<Self> {
        Some(data.client.clone())
    }
}

impl EventHandlerContext for EventHandlerHandle {
    fn from_data(data: &EventHandlerData<'_>) -> Option<Self> {
        Some(data.handle.clone())
    }
}

/// This event handler context argument is only applicable to room-specific
/// events.
///
/// Trying to use it in the event handler for another event, for example a
/// global account data or presence event, will result in the event handler
/// being skipped and an error getting logged.
impl EventHandlerContext for room::Room {
    fn from_data(data: &EventHandlerData<'_>) -> Option<Self> {
        data.room.clone()
    }
}

/// The raw JSON form of an event.
///
/// Used as a context argument for event handlers (see
/// [`Client::add_event_handler`]).
#[derive(Clone, Debug)]
pub struct RawEvent(Box<RawJsonValue>);

impl Deref for RawEvent {
    type Target = RawJsonValue;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl EventHandlerContext for RawEvent {
    fn from_data(data: &EventHandlerData<'_>) -> Option<Self> {
        Some(Self(data.raw.to_owned()))
    }
}

impl EventHandlerContext for Option<EncryptionInfo> {
    fn from_data(data: &EventHandlerData<'_>) -> Option<Self> {
        Some(data.encryption_info.cloned())
    }
}

impl EventHandlerContext for Vec<Action> {
    fn from_data(data: &EventHandlerData<'_>) -> Option<Self> {
        Some(data.push_actions.to_owned())
    }
}

/// A custom value registered with
/// [`.add_event_handler_context`][Client::add_event_handler_context].
#[derive(Debug)]
pub struct Ctx<T>(pub T);

impl<T: Clone + Send + Sync + 'static> EventHandlerContext for Ctx<T> {
    fn from_data(data: &EventHandlerData<'_>) -> Option<Self> {
        let map = data.client.inner.event_handlers.context.read().unwrap();
        map.get::<T>().cloned().map(Ctx)
    }
}

impl<T> Deref for Ctx<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
