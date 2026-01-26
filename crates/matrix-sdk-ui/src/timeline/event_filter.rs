// Copyright 2026 The Matrix.org Foundation C.I.C.
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

use ruma::events::{
    AnySyncStateEvent, AnySyncTimelineEvent, SyncStateEvent, TimelineEventType,
    room::member::MembershipChange,
};

/// A timeline filter that in- or excludes events based on their type or
/// content.
pub enum TimelineEventFilter {
    /// Only return items whose event matches any of the conditions in the list.
    Include(Vec<TimelineEventCondition>),
    /// Return all items except the ones whose event matches any of the
    /// conditions in the list
    Exclude(Vec<TimelineEventCondition>),
}

impl TimelineEventFilter {
    /// Filters any incoming `event` using the filter conditions.
    ///
    /// # Arguments
    ///
    /// * `event` - The event to run the filter on.
    ///
    /// # Returns
    /// `true` if the filter allows the event or `false` otherwise.
    pub fn filter(&self, event: &AnySyncTimelineEvent) -> bool {
        match self {
            Self::Include(conditions) => conditions.iter().any(|c| c.matches(event)),
            Self::Exclude(conditions) => !conditions.iter().any(|c| c.matches(event)),
        }
    }
}

/// A condition that matches on an event's type or content.
#[derive(Clone)]
pub enum TimelineEventCondition {
    /// The event has the specified event type.
    EventType(TimelineEventType),
    /// The event is an `m.room.member` event that represents a membership
    /// change (join, leave, etc.).
    MembershipChange,
    /// The event is an `m.room.member` event that represents a profile
    /// change (displayname or avatar URL).
    ProfileChange,
}

impl TimelineEventCondition {
    /// Evaluate the condition against an event.
    ///
    /// # Arguments
    ///
    /// * `event` - The event to test the condition against.
    ///
    /// # Returns
    /// `true` if the condition matches or `false` otherwise.
    fn matches(&self, event: &AnySyncTimelineEvent) -> bool {
        match self {
            Self::EventType(event_type) => event.event_type() == *event_type,
            Self::MembershipChange => match event {
                AnySyncTimelineEvent::State(AnySyncStateEvent::RoomMember(
                    SyncStateEvent::Original(ev),
                )) => {
                    let change = ev.content.membership_change(
                        ev.prev_content().as_ref().map(|c| c.details()),
                        &ev.sender,
                        &ev.state_key,
                    );
                    !matches!(change, MembershipChange::ProfileChanged { .. })
                }
                _ => false,
            },
            Self::ProfileChange => match event {
                AnySyncTimelineEvent::State(AnySyncStateEvent::RoomMember(
                    SyncStateEvent::Original(ev),
                )) => {
                    let change = ev.content.membership_change(
                        ev.prev_content().as_ref().map(|c| c.details()),
                        &ev.sender,
                        &ev.state_key,
                    );
                    matches!(change, MembershipChange::ProfileChanged { .. })
                }
                _ => false,
            },
        }
    }
}
