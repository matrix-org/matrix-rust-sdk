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
    debug::DebugRawEvent,
    deserialized_responses::{ProcessedToDeviceEvent, TimelineEvent},
};
pub use ruma::api::client::sync::sync_events::v3::{
    InvitedRoom as InvitedRoomUpdate, KnockedRoom as KnockedRoomUpdate,
};
use ruma::{
    OwnedEventId, OwnedRoomId,
    api::client::sync::sync_events::UnreadNotificationsCount as RumaUnreadNotificationsCount,
    events::{
        AnyGlobalAccountDataEvent, AnyRoomAccountDataEvent, AnySyncEphemeralRoomEvent,
        AnySyncStateEvent, presence::PresenceEvent,
    },
    push::Action,
    serde::Raw,
};
use serde::{Deserialize, Serialize};

use crate::{
    debug::{
        DebugInvitedRoom, DebugKnockedRoom, DebugListOfProcessedToDeviceEvents,
        DebugListOfRawEvents, DebugListOfRawEventsNoId,
    },
    deserialized_responses::{AmbiguityChange, RawAnySyncOrStrippedTimelineEvent},
};

/// Generalized representation of a `/sync` response.
///
/// This type is intended to be applicable regardless of the endpoint used for
/// syncing.
#[derive(Clone, Default)]
pub struct SyncResponse {
    /// Updates to rooms.
    pub rooms: RoomUpdates,
    /// Updates to the presence status of other users.
    pub presence: Vec<Raw<PresenceEvent>>,
    /// The global private data created by this user.
    pub account_data: Vec<Raw<AnyGlobalAccountDataEvent>>,
    /// Messages sent directly between devices.
    pub to_device: Vec<ProcessedToDeviceEvent>,
    /// New notifications per room.
    pub notifications: BTreeMap<OwnedRoomId, Vec<Notification>>,
}

#[cfg(not(tarpaulin_include))]
impl fmt::Debug for SyncResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SyncResponse")
            .field("rooms", &self.rooms)
            .field("account_data", &DebugListOfRawEventsNoId(&self.account_data))
            .field("to_device", &DebugListOfProcessedToDeviceEvents(&self.to_device))
            .field("notifications", &self.notifications)
            .finish_non_exhaustive()
    }
}

/// Updates to rooms in a [`SyncResponse`].
#[derive(Clone, Default)]
pub struct RoomUpdates {
    /// The rooms that the user has left or been banned from.
    pub left: BTreeMap<OwnedRoomId, LeftRoomUpdate>,
    /// The rooms that the user has joined.
    pub joined: BTreeMap<OwnedRoomId, JoinedRoomUpdate>,
    /// The rooms that the user has been invited to.
    pub invited: BTreeMap<OwnedRoomId, InvitedRoomUpdate>,
    /// The rooms that the user has knocked on.
    pub knocked: BTreeMap<OwnedRoomId, KnockedRoomUpdate>,
}

impl RoomUpdates {
    /// Iterate over all room IDs, from [`RoomUpdates::left`],
    /// [`RoomUpdates::joined`], [`RoomUpdates::invited`] and
    /// [`RoomUpdates::knocked`].
    pub fn iter_all_room_ids(&self) -> impl Iterator<Item = &OwnedRoomId> {
        self.left
            .keys()
            .chain(self.joined.keys())
            .chain(self.invited.keys())
            .chain(self.knocked.keys())
    }

    /// Returns whether or not this update contains any changes to the list
    /// of invited, joined, knocked or left rooms.
    pub fn is_empty(&self) -> bool {
        self.invited.is_empty()
            && self.joined.is_empty()
            && self.knocked.is_empty()
            && self.left.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use assert_matches::assert_matches;
    use ruma::room_id;

    use super::{
        InvitedRoomUpdate, JoinedRoomUpdate, KnockedRoomUpdate, LeftRoomUpdate, RoomUpdates,
    };

    #[test]
    fn test_room_updates_iter_all_room_ids() {
        let room_id_0 = room_id!("!r0");
        let room_id_1 = room_id!("!r1");
        let room_id_2 = room_id!("!r2");
        let room_id_3 = room_id!("!r3");
        let room_id_4 = room_id!("!r4");
        let room_id_5 = room_id!("!r5");
        let room_id_6 = room_id!("!r6");
        let room_id_7 = room_id!("!r7");
        let room_updates = RoomUpdates {
            left: {
                let mut left = BTreeMap::new();
                left.insert(room_id_0.to_owned(), LeftRoomUpdate::default());
                left.insert(room_id_1.to_owned(), LeftRoomUpdate::default());
                left
            },
            joined: {
                let mut joined = BTreeMap::new();
                joined.insert(room_id_2.to_owned(), JoinedRoomUpdate::default());
                joined.insert(room_id_3.to_owned(), JoinedRoomUpdate::default());
                joined
            },
            invited: {
                let mut invited = BTreeMap::new();
                invited.insert(room_id_4.to_owned(), InvitedRoomUpdate::default());
                invited.insert(room_id_5.to_owned(), InvitedRoomUpdate::default());
                invited
            },
            knocked: {
                let mut knocked = BTreeMap::new();
                knocked.insert(room_id_6.to_owned(), KnockedRoomUpdate::default());
                knocked.insert(room_id_7.to_owned(), KnockedRoomUpdate::default());
                knocked
            },
        };

        let mut iter = room_updates.iter_all_room_ids();
        assert_matches!(iter.next(), Some(room_id) => assert_eq!(room_id, room_id_0));
        assert_matches!(iter.next(), Some(room_id) => assert_eq!(room_id, room_id_1));
        assert_matches!(iter.next(), Some(room_id) => assert_eq!(room_id, room_id_2));
        assert_matches!(iter.next(), Some(room_id) => assert_eq!(room_id, room_id_3));
        assert_matches!(iter.next(), Some(room_id) => assert_eq!(room_id, room_id_4));
        assert_matches!(iter.next(), Some(room_id) => assert_eq!(room_id, room_id_5));
        assert_matches!(iter.next(), Some(room_id) => assert_eq!(room_id, room_id_6));
        assert_matches!(iter.next(), Some(room_id) => assert_eq!(room_id, room_id_7));
        assert!(iter.next().is_none());
    }

    #[test]
    fn test_empty_room_updates() {
        let room_updates = RoomUpdates::default();

        let mut iter = room_updates.iter_all_room_ids();
        assert!(iter.next().is_none());

        assert!(room_updates.is_empty());
    }
}

#[cfg(not(tarpaulin_include))]
impl fmt::Debug for RoomUpdates {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RoomUpdates")
            .field("left", &self.left)
            .field("joined", &self.joined)
            .field("invited", &DebugInvitedRoomUpdates(&self.invited))
            .field("knocked", &DebugKnockedRoomUpdates(&self.knocked))
            .finish()
    }
}

/// Updates to joined rooms.
#[derive(Clone, Default)]
pub struct JoinedRoomUpdate {
    /// Counts of unread notifications for this room.
    pub unread_notifications: UnreadNotificationsCount,
    /// The timeline of messages and state changes in the room.
    pub timeline: Timeline,
    /// Updates to the state.
    ///
    /// If `since` is missing or `full_state` is true, the start point of the
    /// update is the beginning of the timeline. Otherwise, the start point
    /// is the time specified in `since`.
    ///
    /// If `state_after` was used, the end point of the update is the end of the
    /// `timeline`. Otherwise, the end point of these updates is the start of
    /// the `timeline`, and to calculate room state we must scan the `timeline`
    /// for state events as well as using this information in this property.
    pub state: State,
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
impl fmt::Debug for JoinedRoomUpdate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("JoinedRoomUpdate")
            .field("unread_notifications", &self.unread_notifications)
            .field("timeline", &self.timeline)
            .field("state", &self.state)
            .field("account_data", &DebugListOfRawEventsNoId(&self.account_data))
            .field("ephemeral", &self.ephemeral)
            .field("ambiguity_changes", &self.ambiguity_changes)
            .finish()
    }
}

impl JoinedRoomUpdate {
    pub(crate) fn new(
        timeline: Timeline,
        state: State,
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
pub struct LeftRoomUpdate {
    /// The timeline of messages and state changes in the room up to the point
    /// when the user left.
    pub timeline: Timeline,
    /// Updates to the state.
    ///
    /// If `since` is missing or `full_state` is true, the start point of the
    /// update is the beginning of the timeline. Otherwise, the start point
    /// is the time specified in `since`.
    ///
    /// If `state_after` was used, the end point of the update is the end of the
    /// `timeline`. Otherwise, the end point of these updates is the start of
    /// the `timeline`, and to calculate room state we must scan the `timeline`
    /// for state events as well as using this information in this property.
    pub state: State,
    /// The private data that this user has attached to this room.
    pub account_data: Vec<Raw<AnyRoomAccountDataEvent>>,
    /// Collection of ambiguity changes that room member events trigger.
    ///
    /// This is a map of event ID of the `m.room.member` event to the
    /// details of the ambiguity change.
    pub ambiguity_changes: BTreeMap<OwnedEventId, AmbiguityChange>,
}

impl LeftRoomUpdate {
    pub(crate) fn new(
        timeline: Timeline,
        state: State,
        account_data: Vec<Raw<AnyRoomAccountDataEvent>>,
        ambiguity_changes: BTreeMap<OwnedEventId, AmbiguityChange>,
    ) -> Self {
        Self { timeline, state, account_data, ambiguity_changes }
    }
}

#[cfg(not(tarpaulin_include))]
impl fmt::Debug for LeftRoomUpdate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LeftRoomUpdate")
            .field("timeline", &self.timeline)
            .field("state", &self.state)
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
    pub events: Vec<TimelineEvent>,
}

impl Timeline {
    pub(crate) fn new(limited: bool, prev_batch: Option<String>) -> Self {
        Self { limited, prev_batch, ..Default::default() }
    }
}

/// State changes in the room.
#[derive(Clone)]
pub enum State {
    /// The state changes between the previous sync and the start of the
    /// timeline.
    ///
    /// To get the full list of state changes since the previous sync, the state
    /// events in [`Timeline`] must be added to these events to update the local
    /// state.
    Before(Vec<Raw<AnySyncStateEvent>>),

    /// The state changes between the previous sync and the end of the timeline.
    ///
    /// This contains the full list of state changes since the previous sync.
    /// State events in [`Timeline`] must be ignored to update the local state.
    After(Vec<Raw<AnySyncStateEvent>>),
}

impl Default for State {
    fn default() -> Self {
        Self::Before(vec![])
    }
}

#[cfg(not(tarpaulin_include))]
impl fmt::Debug for State {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Before(events) => {
                f.debug_tuple("Before").field(&DebugListOfRawEvents(events)).finish()
            }
            Self::After(events) => {
                f.debug_tuple("After").field(&DebugListOfRawEvents(events)).finish()
            }
        }
    }
}

struct DebugInvitedRoomUpdates<'a>(&'a BTreeMap<OwnedRoomId, InvitedRoomUpdate>);

#[cfg(not(tarpaulin_include))]
impl fmt::Debug for DebugInvitedRoomUpdates<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_map().entries(self.0.iter().map(|(k, v)| (k, DebugInvitedRoom(v)))).finish()
    }
}

struct DebugKnockedRoomUpdates<'a>(&'a BTreeMap<OwnedRoomId, KnockedRoomUpdate>);

#[cfg(not(tarpaulin_include))]
impl fmt::Debug for DebugKnockedRoomUpdates<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_map().entries(self.0.iter().map(|(k, v)| (k, DebugKnockedRoom(v)))).finish()
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
            RawAnySyncOrStrippedTimelineEvent::Stripped(ev) => {
                DebugRawEvent(ev.cast_ref_unchecked())
            }
        };

        f.debug_struct("Notification")
            .field("actions", &self.actions)
            .field("event", &event_debug)
            .finish()
    }
}
