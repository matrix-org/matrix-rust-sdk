use matrix_sdk_base::{
    deserialized_responses::TimelineEvent, event_cache::store::extract_event_relation,
    linked_chunk::ChunkIdentifier,
};
use ruma::OwnedEventId;
use serde::{Deserialize, Serialize};

/// Representation of a [`Chunk`](matrix_sdk_base::linked_chunk::Chunk)
/// which can be stored in IndexedDB.
#[derive(Debug, Serialize, Deserialize)]
pub struct Chunk {
    /// The identifier of the chunk - i.e.,
    /// [`ChunkIdentifier`](matrix_sdk_base::linked_chunk::ChunkIdentifier).
    pub identifier: u64,
    /// The previous chunk in the list.
    pub previous: Option<u64>,
    /// The next chunk in the list.
    pub next: Option<u64>,
    /// The type of the chunk.
    pub chunk_type: ChunkType,
}

/// The type of a [`Chunk`](Chunk)
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChunkType {
    /// A chunk that holds events.
    Event,
    /// A chunk that represents a gap.
    Gap,
}

/// An inclusive representation of an
/// [`Event`](matrix_sdk_base::event_cache::Event) which can be stored in
/// IndexedDB.
///
/// This is useful when (de)serializing an event which may either be in-band or
/// out-of-band.
#[derive(Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Event {
    /// An in-band event, i.e., an event which is part of a chunk.
    InBand(InBandEvent),
    /// An out-of-band event, i.e., an event which is not part of a chunk.
    OutOfBand(OutOfBandEvent),
}

impl From<Event> for TimelineEvent {
    fn from(value: Event) -> Self {
        match value {
            Event::InBand(e) => e.content,
            Event::OutOfBand(e) => e.content,
        }
    }
}

impl Event {
    /// The [`OwnedEventId`] of the underlying event.
    pub fn event_id(&self) -> Option<OwnedEventId> {
        match self {
            Event::InBand(e) => e.event_id(),
            Event::OutOfBand(e) => e.event_id(),
        }
    }

    /// The [`Position`] of the underlying event, if it is in a chunk.
    pub fn position(&self) -> Option<Position> {
        match self {
            Event::InBand(e) => Some(e.position),
            Event::OutOfBand(_) => None,
        }
    }

    /// The [`OwnedEventId`] and
    /// [`RelationType`](ruma::events::relation::RelationType) of the underlying
    /// event as a [`String`].
    pub fn relation(&self) -> Option<(OwnedEventId, String)> {
        match self {
            Event::InBand(e) => e.relation(),
            Event::OutOfBand(e) => e.relation(),
        }
    }
}

/// A generic representation of an
/// [`Event`](matrix_sdk_base::event_cache::Event) which can be stored in
/// IndexedDB.
///
/// This is useful when (de)serializing an event which is required to be either
/// in-band or out-of-band.
#[derive(Debug, Serialize, Deserialize)]
pub struct GenericEvent<P> {
    /// The full content of the event.
    pub content: TimelineEvent,
    /// The position of the event, if it is in a chunk.
    pub position: P,
}

impl<P> GenericEvent<P> {
    /// The [`OwnedEventId`] of the underlying event.
    pub fn event_id(&self) -> Option<OwnedEventId> {
        self.content.event_id()
    }

    /// The event that the underlying event relates to, if any.
    ///
    /// Returns the related [`OwnedEventId`] and the
    /// [`RelationType`](ruma::events::relation::RelationType) as a [`String`].
    pub fn relation(&self) -> Option<(OwnedEventId, String)> {
        extract_event_relation(self.content.raw())
    }
}

/// A concrete instance of [`GenericEvent`] for in-band events, i.e.,
/// events which are part of a chunk and therefore have a position.
pub type InBandEvent = GenericEvent<Position>;

/// A concrete instance of [`GenericEvent`] for out-of-band events, i.e.,
/// events which are not part of a chunk and therefore have no position.
pub type OutOfBandEvent = GenericEvent<()>;

/// A representation of [`Position`](matrix_sdk_base::linked_chunk::Position)
/// which can be stored in IndexedDB.
#[derive(Debug, Default, Copy, Clone, Serialize, Deserialize)]
pub struct Position {
    /// The identifier of the chunk.
    pub chunk_identifier: u64,
    /// The index of the event within the chunk.
    pub index: usize,
}

impl From<Position> for matrix_sdk_base::linked_chunk::Position {
    fn from(value: Position) -> Self {
        Self::new(ChunkIdentifier::new(value.chunk_identifier), value.index)
    }
}

impl From<matrix_sdk_base::linked_chunk::Position> for Position {
    fn from(value: matrix_sdk_base::linked_chunk::Position) -> Self {
        Self { chunk_identifier: value.chunk_identifier().index(), index: value.index() }
    }
}

/// A representation of [`Gap`](matrix_sdk_base::linked_chunk::Gap)
/// which can be stored in IndexedDB.
#[derive(Debug, Serialize, Deserialize)]
pub struct Gap {
    /// The identifier of the chunk containing this gap.
    pub chunk_identifier: u64,
    /// The token to use in the query, extracted from a previous "from" /
    /// "end" field of a `/messages` response.
    pub prev_token: String,
}
