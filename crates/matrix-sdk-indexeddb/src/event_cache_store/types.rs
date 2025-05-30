use matrix_sdk_base::{
    deserialized_responses::TimelineEvent,
    linked_chunk::{ChunkIdentifier, Position},
};
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
pub struct ChunkForCache {
    pub identifier: u64,
    pub previous: Option<Box<ChunkForCache>>,
    pub next: Option<Box<ChunkForCache>>,
    pub chunk_type: ChunkTypeForCache,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChunkTypeForCache {
    Event,
    Gap,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum EventForCache {
    InBand(InBandEventForCache),
    OutOfBand(OutOfBandEventForCache),
}

impl From<EventForCache> for TimelineEvent {
    fn from(value: EventForCache) -> Self {
        match value {
            EventForCache::InBand(e) => e.content,
            EventForCache::OutOfBand(e) => e.content,
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct GenericEventForCache<P> {
    pub content: TimelineEvent,
    pub room_id: String,
    pub position: P,
}

pub type InBandEventForCache = GenericEventForCache<PositionForCache>;
pub type OutOfBandEventForCache = GenericEventForCache<()>;

#[derive(Debug, Default, Copy, Clone, Serialize, Deserialize)]
pub struct PositionForCache {
    pub chunk_identifier: u64,
    pub index: usize,
}

impl From<PositionForCache> for Position {
    fn from(value: PositionForCache) -> Self {
        Self::new(ChunkIdentifier::new(value.chunk_identifier), value.index)
    }
}

impl From<Position> for PositionForCache {
    fn from(value: Position) -> Self {
        Self { chunk_identifier: value.chunk_identifier().index(), index: value.index() }
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct GapForCache {
    pub prev_token: String,
}
