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

//! Helpers for creating `std::fmt::Debug` implementations.

use std::fmt;

use ruma::serde::Raw;

/// A wrapper around `Raw` that implements `Debug` in a way that only prints the
/// event ID and event type.
pub struct DebugRawEvent<'a, T>(pub &'a Raw<T>);

#[cfg(not(tarpaulin_include))]
impl<T> fmt::Debug for DebugRawEvent<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RawEvent")
            .field("event_id", &DebugStringField(self.0.get_field("event_id")))
            .field("event_type", &DebugStringField(self.0.get_field("type")))
            .finish_non_exhaustive()
    }
}

/// A wrapper around `Raw` that implements `Debug` in a way that only prints the
/// event type.
pub struct DebugRawEventNoId<'a, T>(pub &'a Raw<T>);

#[cfg(not(tarpaulin_include))]
impl<T> fmt::Debug for DebugRawEventNoId<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RawEvent")
            .field("event_type", &DebugStringField(self.0.get_field("type")))
            .finish_non_exhaustive()
    }
}

struct DebugStringField(serde_json::Result<Option<String>>);

#[cfg(not(tarpaulin_include))]
impl fmt::Debug for DebugStringField {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.0 {
            Ok(Some(id)) => id.fmt(f),
            Ok(None) => f.write_str("Missing"),
            Err(e) => f.debug_tuple("Invalid").field(&e).finish(),
        }
    }
}
