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

use std::fmt;

use matrix_sdk_common::deserialized_responses::SyncTimelineEvent;

use super::linked_chunk::{Chunk, ChunkIdentifier, Error, Iter, LinkedChunk, Position};

#[derive(Clone, Debug)]
pub struct Gap {
    /// The token to use in the query, extracted from a previous "from" /
    /// "end" field of a `/messages` response.
    pub prev_token: String,
}

const DEFAULT_CHUNK_CAPACITY: usize = 128;

pub struct RoomEvents {
    chunks: LinkedChunk<DEFAULT_CHUNK_CAPACITY, SyncTimelineEvent, Gap>,
}

impl Default for RoomEvents {
    fn default() -> Self {
        Self::new()
    }
}

impl RoomEvents {
    pub fn new() -> Self {
        Self { chunks: LinkedChunk::new() }
    }

    /// Clear all events.
    pub fn reset(&mut self) {
        self.chunks = LinkedChunk::new();
    }

    /// Push events after all events or gaps.
    ///
    /// The last event in `events` is the most recent one.
    pub fn push_events<I>(&mut self, events: I)
    where
        I: IntoIterator<Item = SyncTimelineEvent>,
        I::IntoIter: ExactSizeIterator,
    {
        self.chunks.push_items_back(events)
    }

    /// Push a gap after all events or gaps.
    pub fn push_gap(&mut self, gap: Gap) {
        self.chunks.push_gap_back(gap)
    }

    /// Insert events at a specified position.
    pub fn insert_events_at<I>(&mut self, events: I, position: Position) -> Result<(), Error>
    where
        I: IntoIterator<Item = SyncTimelineEvent>,
        I::IntoIter: ExactSizeIterator,
    {
        self.chunks.insert_items_at(events, position)
    }

    /// Insert a gap at a specified position.
    pub fn insert_gap_at(&mut self, gap: Gap, position: Position) -> Result<(), Error> {
        self.chunks.insert_gap_at(gap, position)
    }

    /// Replace the gap identified by `gap_identifier`, by events.
    ///
    /// Because the `gap_identifier` can represent non-gap chunk, this method
    /// returns a `Result`.
    ///
    /// This method returns a reference to the (first if many) newly created
    /// `Chunk` that contains the `items`.
    pub fn replace_gap_at<I>(
        &mut self,
        events: I,
        gap_identifier: ChunkIdentifier,
    ) -> Result<&Chunk<DEFAULT_CHUNK_CAPACITY, SyncTimelineEvent, Gap>, Error>
    where
        I: IntoIterator<Item = SyncTimelineEvent>,
        I::IntoIter: ExactSizeIterator,
    {
        self.chunks.replace_gap_at(events, gap_identifier)
    }

    /// Search for a chunk, and return its identifier.
    pub fn chunk_identifier<'a, P>(&'a self, predicate: P) -> Option<ChunkIdentifier>
    where
        P: FnMut(&'a Chunk<DEFAULT_CHUNK_CAPACITY, SyncTimelineEvent, Gap>) -> bool,
    {
        self.chunks.chunk_identifier(predicate)
    }

    /// Iterate over the chunks, forward.
    ///
    /// The oldest chunk comes first.
    pub fn chunks(&self) -> Iter<'_, DEFAULT_CHUNK_CAPACITY, SyncTimelineEvent, Gap> {
        self.chunks.chunks()
    }

    /// Iterate over the events, backward.
    ///
    /// The most recent event comes first.
    pub fn revents(&self) -> impl Iterator<Item = (Position, &SyncTimelineEvent)> {
        self.chunks.ritems()
    }

    /// Iterate over the events, forward.
    ///
    /// The oldest event comes first.
    pub fn events(&self) -> impl Iterator<Item = (Position, &SyncTimelineEvent)> {
        self.chunks.items()
    }
}

impl fmt::Debug for RoomEvents {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> Result<(), fmt::Error> {
        formatter.debug_struct("RoomEvents").field("chunk", &self.chunks).finish()
    }
}
