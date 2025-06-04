use matrix_sdk_base::{deserialized_responses::TimelineEvent, linked_chunk::ChunkIdentifier};
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
pub struct Chunk {
    pub identifier: u64,
    pub previous: Option<Box<Chunk>>,
    pub next: Option<Box<Chunk>>,
    pub chunk_type: ChunkType,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChunkType {
    Event,
    Gap,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Event {
    InBand(InBandEvent),
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

#[derive(Debug, Serialize, Deserialize)]
pub struct GenericEvent<P> {
    pub content: TimelineEvent,
    pub room_id: String,
    pub position: P,
}

pub type InBandEvent = GenericEvent<Position>;
pub type OutOfBandEvent = GenericEvent<()>;

#[derive(Debug, Default, Copy, Clone, Serialize, Deserialize)]
pub struct Position {
    pub chunk_identifier: u64,
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

#[derive(Debug, Serialize, Deserialize)]
pub struct Gap {
    pub prev_token: String,
}
