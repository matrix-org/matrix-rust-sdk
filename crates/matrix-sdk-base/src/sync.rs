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

use matrix_sdk_common::{debug::DebugRawEvent, deserialized_responses::SyncTimelineEvent};
use ruma::{
    api::client::sync::sync_events::{
        v3::InvitedRoom, UnreadNotificationsCount as RumaUnreadNotificationsCount,
    },
    events::{
        presence::PresenceEvent, AnyGlobalAccountDataEvent, AnyRoomAccountDataEvent,
        AnySyncEphemeralRoomEvent, AnySyncStateEvent, AnyToDeviceEvent,
    },
    push::Action,
    serde::Raw,
    OwnedEventId, OwnedRoomId,
};
use serde::{Deserialize, Serialize};

use crate::{
    debug::{DebugInvitedRoom, DebugListOfRawEvents, DebugListOfRawEventsNoId},
    deserialized_responses::{AmbiguityChange, RawAnySyncOrStrippedTimelineEvent},
};

/// Internal representation of a `/sync` response.
///
/// This type is intended to be applicable regardless of the endpoint used for
/// syncing.
#[derive(Clone, Default)]
pub struct SyncResponse {
    /// Updates to rooms.
    pub rooms: Rooms,
    /// Updates to the presence status of other users.
    pub presence: Vec<Raw<PresenceEvent>>,
    /// The global private data created by this user.
    pub account_data: Vec<Raw<AnyGlobalAccountDataEvent>>,
    /// Messages sent directly between devices.
    pub to_device: Vec<Raw<AnyToDeviceEvent>>,
    /// New notifications per room.
    pub notifications: BTreeMap<OwnedRoomId, Vec<Notification>>,
}

#[cfg(not(tarpaulin_include))]
impl fmt::Debug for SyncResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SyncResponse")
            .field("rooms", &self.rooms)
            .field("account_data", &DebugListOfRawEventsNoId(&self.account_data))
            .field("to_device", &DebugListOfRawEventsNoId(&self.to_device))
            .field("notifications", &self.notifications)
            .finish_non_exhaustive()
    }
}

/// Updates to rooms in a [`SyncResponse`].
#[derive(Clone, Default)]
pub struct Rooms {
    /// The rooms that the user has left or been banned from.
    pub leave: BTreeMap<OwnedRoomId, LeftRoom>,
    /// The rooms that the user has joined.
    pub join: BTreeMap<OwnedRoomId, JoinedRoom>,
    /// The rooms that the user has been invited to.
    pub invite: BTreeMap<OwnedRoomId, InvitedRoom>,
}

#[cfg(not(tarpaulin_include))]
impl fmt::Debug for Rooms {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Rooms")
            .field("leave", &self.leave)
            .field("join", &self.join)
            .field("invite", &DebugInvitedRooms(&self.invite))
            .finish()
    }
}

/// Updates to joined rooms.
#[derive(Clone, Default)]
pub struct JoinedRoom {
    /// Counts of unread notifications for this room.
    pub unread_notifications: UnreadNotificationsCount,
    /// The timeline of messages and state changes in the room.
    pub timeline: Timeline,
    /// Updates to the state, between the time indicated by the `since`
    /// parameter, and the start of the `timeline` (or all state up to the
    /// start of the `timeline`, if `since` is not given, or `full_state` is
    /// true).
    pub state: Vec<Raw<AnySyncStateEvent>>,
    /// The private data that this user has attached to this room.
    pub account_data: Vec<Raw<AnyRoomAccountDataEvent>>,
    /// The ephemeral events in the room that aren't recorded in the timeline or
    /// state of the room. e.g. typing.
    pub ephemeral: Vec<Raw<AnySyncEphemeralRoomEvent>>,
    /// Collection of ambiguity changes that room member events trigger.
    ///
    /// This is a map of event ID of the `m.room.member` event to the
    /// details of the ambiguity change.
    pub ambiguity_changes: BTreeMap<OwnedEventId, AmbiguityChange>,
}

#[cfg(not(tarpaulin_include))]
impl fmt::Debug for JoinedRoom {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("JoinedRoom")
            .field("unread_notifications", &self.unread_notifications)
            .field("timeline", &self.timeline)
            .field("state", &DebugListOfRawEvents(&self.state))
            .field("account_data", &DebugListOfRawEventsNoId(&self.account_data))
            .field("ephemeral", &self.ephemeral)
            .field("ambiguity_changes", &self.ambiguity_changes)
            .finish()
    }
}

impl JoinedRoom {
    pub(crate) fn new(
        timeline: Timeline,
        state: Vec<Raw<AnySyncStateEvent>>,
        account_data: Vec<Raw<AnyRoomAccountDataEvent>>,
        ephemeral: Vec<Raw<AnySyncEphemeralRoomEvent>>,
        unread_notifications: UnreadNotificationsCount,
        ambiguity_changes: BTreeMap<OwnedEventId, AmbiguityChange>,
    ) -> Self {
        Self { unread_notifications, timeline, state, account_data, ephemeral, ambiguity_changes }
    }
}

/// Counts of unread notifications for a room.
#[derive(Copy, Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
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
#[derive(Clone, Default)]
pub struct LeftRoom {
    /// The timeline of messages and state changes in the room up to the point
    /// when the user left.
    pub timeline: Timeline,
    /// Updates to the state, between the time indicated by the `since`
    /// parameter, and the start of the `timeline` (or all state up to the
    /// start of the `timeline`, if `since` is not given, or `full_state` is
    /// true).
    pub state: Vec<Raw<AnySyncStateEvent>>,
    /// The private data that this user has attached to this room.
    pub account_data: Vec<Raw<AnyRoomAccountDataEvent>>,
    /// Collection of ambiguity changes that room member events trigger.
    ///
    /// This is a map of event ID of the `m.room.member` event to the
    /// details of the ambiguity change.
    pub ambiguity_changes: BTreeMap<OwnedEventId, AmbiguityChange>,
}

impl LeftRoom {
    pub(crate) fn new(
        timeline: Timeline,
        state: Vec<Raw<AnySyncStateEvent>>,
        account_data: Vec<Raw<AnyRoomAccountDataEvent>>,
        ambiguity_changes: BTreeMap<OwnedEventId, AmbiguityChange>,
    ) -> Self {
        Self { timeline, state, account_data, ambiguity_changes }
    }
}

#[cfg(not(tarpaulin_include))]
impl fmt::Debug for LeftRoom {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("JoinedRoom")
            .field("timeline", &self.timeline)
            .field("state", &DebugListOfRawEvents(&self.state))
            .field("account_data", &DebugListOfRawEventsNoId(&self.account_data))
            .field("ambiguity_changes", &self.ambiguity_changes)
            .finish()
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

struct DebugInvitedRooms<'a>(&'a BTreeMap<OwnedRoomId, InvitedRoom>);

#[cfg(not(tarpaulin_include))]
impl<'a> fmt::Debug for DebugInvitedRooms<'a> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_map().entries(self.0.iter().map(|(k, v)| (k, DebugInvitedRoom(v)))).finish()
    }
}

/// A notification triggered by a sync response.
#[derive(Clone)]
pub struct Notification {
    /// The actions to perform when the conditions for this rule are met.
    pub actions: Vec<Action>,

    /// The event that triggered the notification.
    pub event: RawAnySyncOrStrippedTimelineEvent,
}

#[cfg(not(tarpaulin_include))]
impl fmt::Debug for Notification {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let event_debug = match &self.event {
            RawAnySyncOrStrippedTimelineEvent::Sync(ev) => DebugRawEvent(ev),
            RawAnySyncOrStrippedTimelineEvent::Stripped(ev) => DebugRawEvent(ev.cast_ref()),
        };

        f.debug_struct("Notification")
            .field("actions", &self.actions)
            .field("event", &event_debug)
            .finish()
    }
}
