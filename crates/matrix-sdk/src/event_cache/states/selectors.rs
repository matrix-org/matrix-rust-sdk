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
//!
//! [`State`]: super::State

use std::collections::hash_map::Entry;

use ruma::{OwnedEventId, OwnedRoomId, RoomId};

use super::{
    super::EventCacheError, EventFocusedCacheKey, EventFocusedCacheState, PinnedEventsCacheState,
    RoomEventCacheState, StateForRoom, ThreadEventCacheState,
};

/// Trait to select a specific state of a cache inside a [`State`].
///
/// The states are stored per room: a selector designates the room with
/// [`Self::room_id`], and then the specific state inside the
/// [`StateForRoom`] of this room.
///
/// [`State`]: super::State
pub trait CacheState {
    /// The type of the specific state of cache.
    type Item;

    /// The room the selected state belongs to.
    fn room_id(&self) -> &RoomId;

    /// Immutably select a specific state of a cache inside a [`StateForRoom`].
    fn select<'state>(&self, state: &'state StateForRoom) -> Option<&'state Self::Item>;

    /// Mutably select a specific state of a cache inside a [`StateForRoom`].
    fn select_mut<'state>(&self, state: &'state mut StateForRoom)
    -> Option<&'state mut Self::Item>;

    /// Insert a new [`Self::Item`] in [`StateForRoom`].
    ///
    /// It returns `true` if it's been inserted, `false` otherwise, i.e. if the
    /// place is occupied.
    fn insert_once(&self, state: &mut StateForRoom, cache_state: Self::Item) -> bool;
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

    fn room_id(&self) -> &RoomId {
        &self.0
    }

    fn select<'state>(&self, state: &'state StateForRoom) -> Option<&'state Self::Item> {
        Some(state)
    }

    fn select_mut<'state>(
        &self,
        state: &'state mut StateForRoom,
    ) -> Option<&'state mut Self::Item> {
        Some(state)
    }

    fn insert_once(&self, _state: &mut StateForRoom, _cache_state: Self::Item) -> bool {
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

    fn room_id(&self) -> &RoomId {
        &self.0
    }

    fn select<'state>(&self, state: &'state StateForRoom) -> Option<&'state Self::Item> {
        state.room.as_ref()
    }

    fn select_mut<'state>(
        &self,
        state: &'state mut StateForRoom,
    ) -> Option<&'state mut Self::Item> {
        state.room.as_mut()
    }

    fn insert_once(&self, state: &mut StateForRoom, cache_state: Self::Item) -> bool {
        let room = &mut state.room;

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

    fn room_id(&self) -> &RoomId {
        &self.0
    }

    fn select<'state>(&self, state: &'state StateForRoom) -> Option<&'state Self::Item> {
        state.threads.get(&self.1)
    }

    fn select_mut<'state>(
        &self,
        state: &'state mut StateForRoom,
    ) -> Option<&'state mut Self::Item> {
        state.threads.get_mut(&self.1)
    }

    fn insert_once(&self, state: &mut StateForRoom, cache_state: Self::Item) -> bool {
        let threads = &mut state.threads;

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

    fn room_id(&self) -> &RoomId {
        &self.0
    }

    fn select<'state>(&self, state: &'state StateForRoom) -> Option<&'state Self::Item> {
        state.pinned_events.as_ref()
    }

    fn select_mut<'state>(
        &self,
        state: &'state mut StateForRoom,
    ) -> Option<&'state mut Self::Item> {
        state.pinned_events.as_mut()
    }

    fn insert_once(&self, state: &mut StateForRoom, cache_state: Self::Item) -> bool {
        let pinned_events = &mut state.pinned_events;

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

    fn room_id(&self) -> &RoomId {
        &self.0
    }

    fn select<'state>(&self, state: &'state StateForRoom) -> Option<&'state Self::Item> {
        state.event_focused.get(&self.1)
    }

    fn select_mut<'state>(
        &self,
        state: &'state mut StateForRoom,
    ) -> Option<&'state mut Self::Item> {
        state.event_focused.get_mut(&self.1)
    }

    fn insert_once(&self, state: &mut StateForRoom, cache_state: Self::Item) -> bool {
        let event_focused = &mut state.event_focused;

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
