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

pub mod forwarded_room_key;
pub mod olm_v1;
pub mod room;
pub mod room_key;
pub mod secret_send;
mod to_device;

use ruma::serde::{Raw, StringEnum};
pub use to_device::{ToDeviceCustomEvent, ToDeviceEvent, ToDeviceEvents};

/// A trait for event contents to define their event type.
pub trait EventType {
    /// The event type of the event content.
    const EVENT_TYPE: &'static str;

    /// Get the event type of the event content.
    ///
    /// **Note**: This should never be implemented manually, this takes the
    /// event type from the constant.
    fn event_type(&self) -> &'static str {
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

// Wrapper around `Box<str>` that cannot be used in a meaningful way outside of
// this crate. Used for string enums because their `_Custom` variant can't be
// truly private (only `#[doc(hidden)]`).
#[doc(hidden)]
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PrivOwnedStr(Box<str>);

impl std::fmt::Debug for PrivOwnedStr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// An encryption algorithm to be used to encrypt messages sent to a room.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, StringEnum)]
#[non_exhaustive]
pub enum EventEncryptionAlgorithm {
    /// Olm version 1 using Curve25519, AES-256, and SHA-256.
    #[ruma_enum(rename = "m.olm.v1.curve25519-aes-sha2")]
    OlmV1Curve25519AesSha2,

    /// Olm version 2 using Curve25519, AES-256, and SHA-256.
    #[ruma_enum(rename = "m.olm.v2.curve25519-aes-sha2")]
    OlmV2Curve25519AesSha2,

    /// Megolm version 1 using AES-256 and SHA-256.
    #[ruma_enum(rename = "m.megolm.v1.aes-sha2")]
    MegolmV1AesSha2,

    /// Megolm version 2 using AES-256 and SHA-256.
    #[ruma_enum(rename = "m.megolm.v2.aes-sha2")]
    MegolmV2AesSha2,

    #[doc(hidden)]
    _Custom(PrivOwnedStr),
}
