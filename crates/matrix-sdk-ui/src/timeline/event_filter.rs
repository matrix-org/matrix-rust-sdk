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
    room::member::{MembershipChange, MembershipState},
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
    MembershipChange(MembershipChangeFilter),
    /// The event is an `m.room.member` event that represents a profile
    /// change (displayname or avatar URL).
    ProfileChange,
}

/// The membership states that should be included/excluded from the timeline
/// item filters.
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[derive(Clone)]
pub enum MembershipChangeFilter {
    /// Include/exclude all membership state events.
    Any,
    /// Include/exclude only `join` membership state events.
    Join,
    /// Include/exclude only `leave` membership state events.
    Leave,
    /// Include/exclude only `invite` membership state events.
    Invite,
    /// Include/exclude only `ban` membership state events.
    Ban,
    /// Include/exclude only `knock` membership state events.
    Knock,
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
            Self::MembershipChange(filter) => match event {
                AnySyncTimelineEvent::State(AnySyncStateEvent::RoomMember(
                    SyncStateEvent::Original(ev),
                )) => {
                    if matches!(ev.membership_change(), MembershipChange::ProfileChanged { .. }) {
                        return false;
                    }
                    match (filter, &ev.content.membership) {
                        (MembershipChangeFilter::Any, _) => {
                            !matches!(ev.membership_change(), MembershipChange::None)
                        }
                        (MembershipChangeFilter::Join, MembershipState::Join) => true,
                        (MembershipChangeFilter::Invite, MembershipState::Invite) => true,
                        (MembershipChangeFilter::Leave, MembershipState::Leave) => true,
                        (MembershipChangeFilter::Knock, MembershipState::Knock) => true,
                        (MembershipChangeFilter::Ban, MembershipState::Ban) => true,
                        _ => false,
                    }
                }
                _ => false,
            },
            Self::ProfileChange => match event {
                AnySyncTimelineEvent::State(AnySyncStateEvent::RoomMember(
                    SyncStateEvent::Original(ev),
                )) => {
                    matches!(ev.membership_change(), MembershipChange::ProfileChanged { .. })
                }
                _ => false,
            },
        }
    }
}
