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

use std::{collections::BTreeMap, fmt};

pub use matrix_sdk_common::debug::*;
use ruma::{api::client::push::get_notifications::v3::Notification, serde::Raw, OwnedRoomId};

/// A wrapper around a slice of `Raw` events that implements `Debug` in a way
/// that only prints the event type of each item.
pub struct DebugListOfRawEventsNoId<'a, T>(pub &'a [Raw<T>]);

impl<'a, T> fmt::Debug for DebugListOfRawEventsNoId<'a, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut list = f.debug_list();
        list.entries(self.0.iter().map(DebugRawEventNoId));
        list.finish()
    }
}

/// A wrapper around a notification map as found in `/sync` responses that
/// implements `Debug` in a way that only prints the event ID and event type
/// for the raw events contained in each notification.
pub struct DebugNotificationMap<'a>(pub &'a BTreeMap<OwnedRoomId, Vec<Notification>>);

#[cfg(not(tarpaulin_include))]
impl<'a> fmt::Debug for DebugNotificationMap<'a> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut map = f.debug_map();
        map.entries(self.0.iter().map(|(room_id, raw)| (room_id, DebugNotificationList(raw))));
        map.finish()
    }
}

struct DebugNotificationList<'a>(&'a [Notification]);

#[cfg(not(tarpaulin_include))]
impl<'a> fmt::Debug for DebugNotificationList<'a> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut list = f.debug_list();
        list.entries(self.0.iter().map(DebugNotification));
        list.finish()
    }
}

struct DebugNotification<'a>(&'a Notification);

#[cfg(not(tarpaulin_include))]
impl<'a> fmt::Debug for DebugNotification<'a> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Notification")
            .field("actions", &self.0.actions)
            .field("event", &DebugRawEvent(&self.0.event))
            .field("profile_tag", &self.0.profile_tag)
            .field("read", &self.0.read)
            .field("room_id", &self.0.room_id)
            .field("ts", &self.0.ts)
            .finish()
    }
}
