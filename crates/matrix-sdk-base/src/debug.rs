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

pub use matrix_sdk_common::debug::*;
use ruma::{api::client::sync::sync_events::v3::InvitedRoom, serde::Raw};

/// A wrapper around a slice of `Raw` events that implements `Debug` in a way
/// that only prints the event type of each item.
pub struct DebugListOfRawEventsNoId<'a, T>(pub &'a [Raw<T>]);

#[cfg(not(tarpaulin_include))]
impl<'a, T> fmt::Debug for DebugListOfRawEventsNoId<'a, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut list = f.debug_list();
        list.entries(self.0.iter().map(DebugRawEventNoId));
        list.finish()
    }
}

/// A wrapper around an invited room as found in `/sync` responses that
/// implements `Debug` in a way that only prints the event ID and event type for
/// the raw events contained in `invite_state`.
pub struct DebugInvitedRoom<'a>(pub &'a InvitedRoom);

#[cfg(not(tarpaulin_include))]
impl<'a> fmt::Debug for DebugInvitedRoom<'a> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("InvitedRoom")
            .field("invite_state", &DebugListOfRawEvents(&self.0.invite_state.events))
            .finish()
    }
}

pub(crate) struct DebugListOfRawEvents<'a, T>(pub &'a [Raw<T>]);

#[cfg(not(tarpaulin_include))]
impl<'a, T> fmt::Debug for DebugListOfRawEvents<'a, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut list = f.debug_list();
        list.entries(self.0.iter().map(DebugRawEvent));
        list.finish()
    }
}
