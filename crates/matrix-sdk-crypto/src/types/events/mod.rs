// Copyright 2022 The Matrix.org Foundation C.I.C.
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

//! Types modeling end-to-end encryption related Matrix events
//!
//! These types aim to provide a more strict variant of the equivalent Ruma
//! types. Once deserialized they aim to zeroize all the secret material once
//! the type is dropped.

pub mod dummy;
pub mod forwarded_room_key;
pub mod olm_v1;
pub mod room;
pub mod room_key;
pub mod room_key_bundle;
pub mod room_key_request;
pub mod room_key_withheld;
pub mod secret_send;
mod to_device;
mod utd_cause;

use ruma::serde::Raw;
pub use to_device::{ToDeviceCustomEvent, ToDeviceEvent, ToDeviceEvents};
pub use utd_cause::{CryptoContextInfo, UtdCause};

/// A trait for event contents to define their event type.
pub trait EventType {
    /// The event type of the event content.
    const EVENT_TYPE: &'static str;

    /// Get the event type of the event content.
    ///
    /// **Note**: This usually doesn't need to be implemented. The default
    /// implementation will take the event type from the
    /// [`EventType::EVENT_TYPE`] constant.
    fn event_type(&self) -> &str {
        Self::EVENT_TYPE
    }
}

impl<T: EventType> EventType for Raw<T> {
    const EVENT_TYPE: &'static str = T::EVENT_TYPE;
}

fn from_str<'a, T, E>(string: &'a str) -> Result<T, E>
where
    T: serde::Deserialize<'a>,
    E: serde::de::Error,
{
    serde_json::from_str(string).map_err(serde::de::Error::custom)
}
