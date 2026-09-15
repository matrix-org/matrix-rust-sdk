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

//! This module contains the [`CacheState`] trait to select a specific cache
//! state in the full [`State`].

use std::collections::hash_map::Entry;

use ruma::{OwnedEventId, OwnedRoomId};

use super::{
    super::EventCacheError, EventFocusedCacheKey, EventFocusedCacheState, PinnedEventsCacheState,
    RoomEventCacheState, SpecificEventsCacheState, State, StateForRoom, ThreadEventCacheState,
};

/// Trait to select a specific state of a cache inside a [`State`].
pub trait CacheState {
    /// The type of the specific state of cache.
    type Item;

    /// Immutably select a specific state of a cache inside a [`State`].
    fn select<'state>(&self, state: &'state State) -> Option<&'state Self::Item>;

    /// Mutably select a specific state of a cache inside a [`State`].
    fn select_mut<'state>(&self, state: &'state mut State) -> Option<&'state mut Self::Item>;

    /// Insert a new [`Self::Item`] in [`State`].
    ///
    /// It returns `true` if it's been inserted, `false` otherwise, i.e. if the
    /// place is occupied.
    fn insert_once(&self, state: &mut State, cache_state: Self::Item) -> bool;
}

/// Select all states ([`RoomEventCacheState`], [`ThreadEventCacheState`],
/// [`PinnedEventsCacheState`] and [`EventFocusedCacheState`]) for a particular
/// room in [`State`].
#[derive(Debug)]
pub(in super::super) struct AllStatesSelector(OwnedRoomId);

impl AllStatesSelector {
    pub fn new(room_id: OwnedRoomId) -> Self {
        Self(room_id)
    }
}

impl CacheState for AllStatesSelector {
    type Item = StateForRoom;

    fn select<'state>(&self, state: &'state State) -> Option<&'state Self::Item> {
        state.by_room.get(&self.0)
    }

    fn select_mut<'state>(&self, state: &'state mut State) -> Option<&'state mut Self::Item> {
        state.by_room.get_mut(&self.0)
    }

    fn insert_once(&self, _state: &mut State, _cache_state: Self::Item) -> bool {
        false
    }
}

impl From<&AllStatesSelector> for EventCacheError {
    fn from(value: &AllStatesSelector) -> Self {
        Self::RoomNotFound { room_id: value.0.clone() }
    }
}

/// Select a [`RoomEventCacheState`] in [`State`].
#[derive(Debug)]
pub struct RoomStateSelector(OwnedRoomId);

impl RoomStateSelector {
    pub fn new(room_id: OwnedRoomId) -> Self {
        Self(room_id)
    }
}

impl CacheState for RoomStateSelector {
    type Item = RoomEventCacheState;

    fn select<'state>(&self, state: &'state State) -> Option<&'state Self::Item> {
        state.by_room.get(&self.0).and_then(|state_for_room| state_for_room.room.as_ref())
    }

    fn select_mut<'state>(&self, state: &'state mut State) -> Option<&'state mut Self::Item> {
        state.by_room.get_mut(&self.0).and_then(|state_for_room| state_for_room.room.as_mut())
    }

    fn insert_once(&self, state: &mut State, cache_state: Self::Item) -> bool {
        let room = &mut state.by_room.entry(self.0.clone()).or_default().room;

        match room {
            Some(_) => false,
            None => {
                room.replace(cache_state);

                true
            }
        }
    }
}

impl From<&RoomStateSelector> for EventCacheError {
    fn from(value: &RoomStateSelector) -> Self {
        Self::RoomNotFound { room_id: value.0.clone() }
    }
}

/// Select a [`ThreadEventCacheState`] in [`State`].
#[derive(Debug)]
pub struct ThreadStateSelector(OwnedRoomId, OwnedEventId);

impl ThreadStateSelector {
    pub fn new(room_id: OwnedRoomId, thread_id: OwnedEventId) -> Self {
        Self(room_id, thread_id)
    }
}

impl CacheState for ThreadStateSelector {
    type Item = ThreadEventCacheState;

    fn select<'state>(&self, state: &'state State) -> Option<&'state Self::Item> {
        state.by_room.get(&self.0).and_then(|state_for_room| state_for_room.threads.get(&self.1))
    }

    fn select_mut<'state>(&self, state: &'state mut State) -> Option<&'state mut Self::Item> {
        state
            .by_room
            .get_mut(&self.0)
            .and_then(|state_for_room| state_for_room.threads.get_mut(&self.1))
    }

    fn insert_once(&self, state: &mut State, cache_state: Self::Item) -> bool {
        let threads = &mut state.by_room.entry(self.0.clone()).or_default().threads;

        match threads.entry(self.1.clone()) {
            Entry::Occupied(_) => false,
            Entry::Vacant(entry) => {
                entry.insert(cache_state);

                true
            }
        }
    }
}

impl From<&ThreadStateSelector> for EventCacheError {
    fn from(value: &ThreadStateSelector) -> Self {
        Self::ThreadNotFound { room_id: value.0.clone(), thread_id: value.1.clone() }
    }
}

/// Select a [`PinnedEventCacheState`] in [`State`].
#[derive(Debug)]
pub struct PinnedEventsStateSelector(OwnedRoomId);

impl PinnedEventsStateSelector {
    pub fn new(room_id: OwnedRoomId) -> Self {
        Self(room_id)
    }
}

impl CacheState for PinnedEventsStateSelector {
    type Item = PinnedEventsCacheState;

    fn select<'state>(&self, state: &'state State) -> Option<&'state Self::Item> {
        state.by_room.get(&self.0).and_then(|state_for_room| state_for_room.pinned_events.as_ref())
    }

    fn select_mut<'state>(&self, state: &'state mut State) -> Option<&'state mut Self::Item> {
        state
            .by_room
            .get_mut(&self.0)
            .and_then(|state_for_room| state_for_room.pinned_events.as_mut())
    }

    fn insert_once(&self, state: &mut State, cache_state: Self::Item) -> bool {
        let pinned_events = &mut state.by_room.entry(self.0.clone()).or_default().pinned_events;

        match pinned_events {
            Some(_) => false,
            None => {
                pinned_events.replace(cache_state);

                true
            }
        }
    }
}

impl From<&PinnedEventsStateSelector> for EventCacheError {
    fn from(value: &PinnedEventsStateSelector) -> Self {
        Self::PinnedEventsNotFound { room_id: value.0.clone() }
    }
}

/// Select a [`EventFocusedCacheState`] in [`State`].
#[derive(Debug)]
pub struct EventFocusedStateSelector(OwnedRoomId, EventFocusedCacheKey);

impl EventFocusedStateSelector {
    pub fn new(room_id: OwnedRoomId, key: EventFocusedCacheKey) -> Self {
        Self(room_id, key)
    }
}

impl CacheState for EventFocusedStateSelector {
    type Item = EventFocusedCacheState;

    fn select<'state>(&self, state: &'state State) -> Option<&'state Self::Item> {
        state
            .by_room
            .get(&self.0)
            .and_then(|state_for_room| state_for_room.event_focused.get(&self.1))
    }

    fn select_mut<'state>(&self, state: &'state mut State) -> Option<&'state mut Self::Item> {
        state
            .by_room
            .get_mut(&self.0)
            .and_then(|state_for_room| state_for_room.event_focused.get_mut(&self.1))
    }

    fn insert_once(&self, state: &mut State, cache_state: Self::Item) -> bool {
        let event_focused = &mut state.by_room.entry(self.0.clone()).or_default().event_focused;

        match event_focused.entry(self.1.clone()) {
            Entry::Occupied(_) => false,
            Entry::Vacant(entry) => {
                entry.insert(cache_state);

                true
            }
        }
    }
}

impl From<&EventFocusedStateSelector> for EventCacheError {
    fn from(value: &EventFocusedStateSelector) -> Self {
        Self::EventFocusedNotFound { room_id: value.0.clone(), event_focused_id: value.1.clone() }
    }
}

/// Select a [`SpecificEventsCacheState`] in [`State`].
#[derive(Debug)]
pub struct SpecificEventsStateSelector(OwnedRoomId, u64);

impl SpecificEventsStateSelector {
    pub fn new(room_id: OwnedRoomId, instance_id: u64) -> Self {
        Self(room_id, instance_id)
    }
}

impl CacheState for SpecificEventsStateSelector {
    type Item = SpecificEventsCacheState;

    fn select<'state>(&self, state: &'state State) -> Option<&'state Self::Item> {
        state
            .by_room
            .get(&self.0)
            .and_then(|state_for_room| state_for_room.specific_events.get(&self.1))
    }

    fn select_mut<'state>(&self, state: &'state mut State) -> Option<&'state mut Self::Item> {
        state
            .by_room
            .get_mut(&self.0)
            .and_then(|state_for_room| state_for_room.specific_events.get_mut(&self.1))
    }

    fn insert_once(&self, state: &mut State, cache_state: Self::Item) -> bool {
        let specific_events = &mut state.by_room.entry(self.0.clone()).or_default().specific_events;

        match specific_events.entry(self.1) {
            Entry::Occupied(_) => false,
            Entry::Vacant(entry) => {
                entry.insert(cache_state);

                true
            }
        }
    }
}

impl From<&SpecificEventsStateSelector> for EventCacheError {
    fn from(value: &SpecificEventsStateSelector) -> Self {
        Self::SpecificEventsNotFound { room_id: value.0.clone(), instance_id: value.1 }
    }
}
