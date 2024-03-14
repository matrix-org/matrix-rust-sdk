// Copyright 2024 The Matrix.org Foundation C.I.C.
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

use std::collections::BTreeMap;

use async_trait::async_trait;
use matrix_sdk_common::deserialized_responses::SyncTimelineEvent;
use ruma::{OwnedRoomId, RoomId};
use tokio::sync::RwLock;

use super::Result;

/// A store that can be remember information about the event cache.
///
/// It really acts as a cache, in the sense that clearing the backing data
/// should not have any irremediable effect, other than providing a lesser user
/// experience.
#[async_trait]
pub trait EventCacheStore: Send + Sync {
    /// Returns all the known events for the given room.
    async fn room_events(&self, room: &RoomId) -> Result<Vec<SyncTimelineEvent>>;

    /// Adds all the entries to the given room's timeline.
    async fn append_room_entries(&self, room: &RoomId, entries: Vec<TimelineEntry>) -> Result<()>;

    /// Returns whether the store knows about the given pagination token.
    async fn contains_gap(&self, room: &RoomId, pagination_token: &PaginationToken)
        -> Result<bool>;

    /// Replaces a given gap (identified by its pagination token) with the given
    /// entries.
    ///
    /// Note: if the gap hasn't been found, then nothing happens, and the events
    /// are lost.
    ///
    /// Returns whether the gap was found.
    async fn replace_gap(
        &self,
        room: &RoomId,
        gap_id: Option<&PaginationToken>,
        entries: Vec<TimelineEntry>,
    ) -> Result<bool>;

    /// Retrieve the oldest backpagination token for the given room.
    async fn oldest_backpagination_token(&self, room: &RoomId) -> Result<Option<PaginationToken>>;

    /// Clear all the information tied to a given room.
    ///
    /// This forgets the following:
    /// - events in the room
    /// - pagination tokens
    async fn clear_room(&self, room: &RoomId) -> Result<()>;
}

/// A newtype wrapper for a pagination token returned by a /messages response.
#[derive(Clone, Debug, PartialEq)]
pub struct PaginationToken(pub String);

#[derive(Clone)]
pub enum TimelineEntry {
    Event(SyncTimelineEvent),

    Gap {
        /// The token to use in the query, extracted from a previous "from" /
        /// "end" field of a `/messages` response.
        prev_token: PaginationToken,
    },
}

/// All the information related to a room and stored in the event cache.
#[derive(Default)]
struct RoomInfo {
    /// All the timeline entries per room, in sync order.
    entries: Vec<TimelineEntry>,
}

impl RoomInfo {
    fn clear(&mut self) {
        self.entries.clear();
    }
}

/// An [`EventCacheStore`] implementation that keeps all the information in
/// memory.
#[derive(Default)]
pub(crate) struct MemoryStore {
    by_room: RwLock<BTreeMap<OwnedRoomId, RoomInfo>>,
}

impl MemoryStore {
    /// Create a new empty [`MemoryStore`].
    pub fn new() -> Self {
        Default::default()
    }
}

#[async_trait]
impl EventCacheStore for MemoryStore {
    async fn room_events(&self, room: &RoomId) -> Result<Vec<SyncTimelineEvent>> {
        Ok(self
            .by_room
            .read()
            .await
            .get(room)
            .map(|room_info| {
                room_info
                    .entries
                    .iter()
                    .filter_map(
                        |entry| if let TimelineEntry::Event(ev) = entry { Some(ev) } else { None },
                    )
                    .cloned()
                    .collect()
            })
            .unwrap_or_default())
    }

    async fn append_room_entries(&self, room: &RoomId, entries: Vec<TimelineEntry>) -> Result<()> {
        self.by_room.write().await.entry(room.to_owned()).or_default().entries.extend(entries);
        Ok(())
    }

    async fn clear_room(&self, room: &RoomId) -> Result<()> {
        // Clear the room, so as to avoid reallocations if the room is being reused.
        // XXX: do we also want an actual way to *remove* a room? (for left rooms)
        if let Some(room) = self.by_room.write().await.get_mut(room) {
            room.clear();
        }
        Ok(())
    }

    async fn oldest_backpagination_token(&self, room: &RoomId) -> Result<Option<PaginationToken>> {
        Ok(self.by_room.read().await.get(room).and_then(|room| {
            room.entries.iter().find_map(|entry| {
                if let TimelineEntry::Gap { prev_token: backpagination_token } = entry {
                    Some(backpagination_token.clone())
                } else {
                    None
                }
            })
        }))
    }

    async fn contains_gap(&self, room: &RoomId, needle: &PaginationToken) -> Result<bool> {
        let mut by_room_guard = self.by_room.write().await;
        let room = by_room_guard.entry(room.to_owned()).or_default();

        Ok(room.entries.iter().any(|entry| {
            if let TimelineEntry::Gap { prev_token: existing } = entry {
                existing == needle
            } else {
                false
            }
        }))
    }

    async fn replace_gap(
        &self,
        room: &RoomId,
        token: Option<&PaginationToken>,
        entries: Vec<TimelineEntry>,
    ) -> Result<bool> {
        let mut by_room_guard = self.by_room.write().await;
        let room = by_room_guard.entry(room.to_owned()).or_default();

        if let Some(token) = token {
            let gap_pos = room.entries.iter().enumerate().find_map(|(i, t)| {
                if let TimelineEntry::Gap { prev_token: existing } = t {
                    if existing == token {
                        return Some(i);
                    }
                }
                None
            });

            if let Some(pos) = gap_pos {
                room.entries.splice(pos..pos + 1, entries);
                Ok(true)
            } else {
                Ok(false)
            }
        } else {
            // We had no previous token: assume we can prepend the events.
            room.entries.splice(0..0, entries);
            Ok(true)
        }
    }
}
