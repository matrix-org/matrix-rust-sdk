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

//! The SDK's representation of the result of a `/sync` request.

use std::{collections::BTreeMap, fmt};

use matrix_sdk_common::{
    deserialized_responses::SyncTimelineEvent, DebugRawEvent, DebugRawEventNoId,
};
use ruma::{
    api::client::{
        push::get_notifications::v3::Notification,
        sync::sync_events::{
            v3::{Ephemeral, InvitedRoom, Presence, RoomAccountData, State},
            DeviceLists, UnreadNotificationsCount as RumaUnreadNotificationsCount,
        },
    },
    events::{AnyGlobalAccountDataEvent, AnyRoomAccountDataEvent, AnyToDeviceEvent},
    serde::Raw,
    DeviceKeyAlgorithm, OwnedRoomId,
};
use serde::{Deserialize, Serialize};

use crate::deserialized_responses::AmbiguityChanges;

/// Internal representation of a `/sync` response.
///
/// This type is intended to be applicable regardless of the endpoint used for
/// syncing.
#[derive(Clone, Default)]
pub struct SyncResponse {
    /// Updates to rooms.
    pub rooms: Rooms,
    /// Updates to the presence status of other users.
    pub presence: Presence,
    /// The global private data created by this user.
    pub account_data: Vec<Raw<AnyGlobalAccountDataEvent>>,
    /// Messages sent directly between devices.
    pub to_device_events: Vec<Raw<AnyToDeviceEvent>>,
    /// Information on E2E device updates.
    ///
    /// Only present on an incremental sync.
    pub device_lists: DeviceLists,
    /// For each key algorithm, the number of unclaimed one-time keys
    /// currently held on the server for a device.
    pub device_one_time_keys_count: BTreeMap<DeviceKeyAlgorithm, u64>,
    /// Collection of ambiguity changes that room member events trigger.
    pub ambiguity_changes: AmbiguityChanges,
    /// New notifications per room.
    pub notifications: BTreeMap<OwnedRoomId, Vec<Notification>>,
}

#[cfg(not(tarpaulin_include))]
impl fmt::Debug for SyncResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SyncResponse")
            .field("rooms", &self.rooms)
            .field("account_data", &DebugListOfRawEventsNoId(&self.account_data))
            .field("to_device_events", &DebugListOfRawEventsNoId(&self.to_device_events))
            .field("device_lists", &self.device_lists)
            .field("device_one_time_keys_count", &self.device_one_time_keys_count)
            .field("ambiguity_changes", &self.ambiguity_changes)
            .field("notifications", &DebugNotificationMap(&self.notifications))
            .finish_non_exhaustive()
    }
}

/// Updates to rooms in a [`SyncResponse`].
#[derive(Clone, Debug, Default)]
pub struct Rooms {
    /// The rooms that the user has left or been banned from.
    pub leave: BTreeMap<OwnedRoomId, LeftRoom>,
    /// The rooms that the user has joined.
    pub join: BTreeMap<OwnedRoomId, JoinedRoom>,
    /// The rooms that the user has been invited to.
    pub invite: BTreeMap<OwnedRoomId, InvitedRoom>,
}

/// Updates to joined rooms.
#[derive(Clone, Debug)]
pub struct JoinedRoom {
    /// Counts of unread notifications for this room.
    pub unread_notifications: UnreadNotificationsCount,
    /// The timeline of messages and state changes in the room.
    pub timeline: Timeline,
    /// Updates to the state, between the time indicated by the `since`
    /// parameter, and the start of the `timeline` (or all state up to the
    /// start of the `timeline`, if `since` is not given, or `full_state` is
    /// true).
    pub state: State,
    /// The private data that this user has attached to this room.
    pub account_data: Vec<Raw<AnyRoomAccountDataEvent>>,
    /// The ephemeral events in the room that aren't recorded in the timeline or
    /// state of the room. e.g. typing.
    pub ephemeral: Ephemeral,
}

impl JoinedRoom {
    pub(crate) fn new(
        timeline: Timeline,
        state: State,
        account_data: Vec<Raw<AnyRoomAccountDataEvent>>,
        ephemeral: Ephemeral,
        unread_notifications: UnreadNotificationsCount,
    ) -> Self {
        Self { unread_notifications, timeline, state, account_data, ephemeral }
    }
}

/// Counts of unread notifications for a room.
#[derive(Copy, Clone, Debug, Default, Deserialize, Serialize)]
pub struct UnreadNotificationsCount {
    /// The number of unread notifications for this room with the highlight flag
    /// set.
    pub highlight_count: u64,
    /// The total number of unread notifications for this room.
    pub notification_count: u64,
}

impl From<RumaUnreadNotificationsCount> for UnreadNotificationsCount {
    fn from(notifications: RumaUnreadNotificationsCount) -> Self {
        Self {
            highlight_count: notifications.highlight_count.map(|c| c.into()).unwrap_or(0),
            notification_count: notifications.notification_count.map(|c| c.into()).unwrap_or(0),
        }
    }
}

/// Updates to left rooms.
#[derive(Clone, Debug)]
pub struct LeftRoom {
    /// The timeline of messages and state changes in the room up to the point
    /// when the user left.
    pub timeline: Timeline,
    /// Updates to the state, between the time indicated by the `since`
    /// parameter, and the start of the `timeline` (or all state up to the
    /// start of the `timeline`, if `since` is not given, or `full_state` is
    /// true).
    pub state: State,
    /// The private data that this user has attached to this room.
    pub account_data: RoomAccountData,
}

impl LeftRoom {
    pub(crate) fn new(timeline: Timeline, state: State, account_data: RoomAccountData) -> Self {
        Self { timeline, state, account_data }
    }
}

/// Events in the room.
#[derive(Clone, Debug, Default)]
pub struct Timeline {
    /// True if the number of events returned was limited by the `limit` on the
    /// filter.
    pub limited: bool,

    /// A token that can be supplied to to the `from` parameter of the
    /// `/rooms/{roomId}/messages` endpoint.
    pub prev_batch: Option<String>,

    /// A list of events.
    pub events: Vec<SyncTimelineEvent>,
}

impl Timeline {
    pub(crate) fn new(limited: bool, prev_batch: Option<String>) -> Self {
        Self { limited, prev_batch, ..Default::default() }
    }
}

struct DebugListOfRawEventsNoId<'a, T>(&'a [Raw<T>]);

impl<'a, T> fmt::Debug for DebugListOfRawEventsNoId<'a, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut list = f.debug_list();
        list.entries(self.0.iter().map(DebugRawEventNoId));
        list.finish()
    }
}

struct DebugNotificationMap<'a>(&'a BTreeMap<OwnedRoomId, Vec<Notification>>);

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
        f.debug_struct("DebugNotification")
            .field("actions", &self.0.actions)
            .field("event", &DebugRawEvent(&self.0.event))
            .field("profile_tag", &self.0.profile_tag)
            .field("read", &self.0.read)
            .field("room_id", &self.0.room_id)
            .field("ts", &self.0.ts)
            .finish()
    }
}
