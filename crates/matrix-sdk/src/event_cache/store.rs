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

use std::{fmt, iter::once};

use matrix_sdk_common::deserialized_responses::SyncTimelineEvent;

use super::linked_chunk::{
    Chunk, ChunkIdentifier, Error, Iter, IterBackward, LinkedChunk, Position,
};

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

#[allow(dead_code)]
impl RoomEvents {
    pub fn new() -> Self {
        Self { chunks: LinkedChunk::new() }
    }

    /// Clear all events.
    pub fn reset(&mut self) {
        self.chunks = LinkedChunk::new();
    }

    /// Return the number of events.
    pub fn len(&self) -> usize {
        self.chunks.len()
    }

    /// Push one event after existing events.
    pub fn push_event(&mut self, event: SyncTimelineEvent) {
        self.push_events(once(event))
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

    /// Search for an item, and return its position.
    pub fn event_position<'a, P>(&'a self, predicate: P) -> Option<Position>
    where
        P: FnMut(&'a SyncTimelineEvent) -> bool,
    {
        self.chunks.item_position(predicate)
    }

    /// Iterate over the chunks, backward.
    ///
    /// The most recent chunk comes first.
    pub fn rchunks(&self) -> IterBackward<'_, DEFAULT_CHUNK_CAPACITY, SyncTimelineEvent, Gap> {
        self.chunks.rchunks()
    }

    /// Iterate over the chunks, forward.
    ///
    /// The oldest chunk comes first.
    pub fn chunks(&self) -> Iter<'_, DEFAULT_CHUNK_CAPACITY, SyncTimelineEvent, Gap> {
        self.chunks.chunks()
    }

    /// Iterate over the chunks, starting from `identifier`, backward.
    pub fn rchunks_from(
        &self,
        identifier: ChunkIdentifier,
    ) -> Result<IterBackward<'_, DEFAULT_CHUNK_CAPACITY, SyncTimelineEvent, Gap>, Error> {
        self.chunks.rchunks_from(identifier)
    }

    /// Iterate over the chunks, starting from `identifier`, forward — i.e.
    /// to the latest chunk.
    pub fn chunks_from(
        &self,
        identifier: ChunkIdentifier,
    ) -> Result<Iter<'_, DEFAULT_CHUNK_CAPACITY, SyncTimelineEvent, Gap>, Error> {
        self.chunks.chunks_from(identifier)
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

    /// Iterate over the events, starting from `position`, backward.
    pub fn revents_from(
        &self,
        position: Position,
    ) -> Result<impl Iterator<Item = (Position, &SyncTimelineEvent)>, Error> {
        self.chunks.ritems_from(position)
    }

    /// Iterate over the events, starting from `position`, forward — i.e.
    /// to the latest event.
    pub fn events_from(
        &self,
        position: Position,
    ) -> Result<impl Iterator<Item = (Position, &SyncTimelineEvent)>, Error> {
        self.chunks.items_from(position)
    }
}

impl fmt::Debug for RoomEvents {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> Result<(), fmt::Error> {
        formatter.debug_struct("RoomEvents").field("chunk", &self.chunks).finish()
    }
}
