// Copyright 2025 The Matrix.org Foundation C.I.C.
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
// limitations under the License

use std::sync::LazyLock;

use matrix_sdk_base::linked_chunk::ChunkIdentifier;

use crate::event_cache_store::types::Position;

/// A [`ChunkIdentifier`] constructed with `0`.
///
/// This value is useful for constructing a key range over all keys which
/// contain [`ChunkIdentifier`]s when used in conjunction with
/// [`INDEXED_KEY_UPPER_CHUNK_IDENTIFIER`].
pub static INDEXED_KEY_LOWER_CHUNK_IDENTIFIER: LazyLock<ChunkIdentifier> =
    LazyLock::new(|| ChunkIdentifier::new(0));

/// A [`ChunkIdentifier`] constructed with [`js_sys::Number::MAX_SAFE_INTEGER`].
///
/// This value is useful for constructing a key range over all keys which
/// contain [`ChunkIdentifier`]s when used in conjunction with
/// [`INDEXED_KEY_LOWER_CHUNK_IDENTIFIER`].
pub static INDEXED_KEY_UPPER_CHUNK_IDENTIFIER: LazyLock<ChunkIdentifier> =
    LazyLock::new(|| ChunkIdentifier::new(js_sys::Number::MAX_SAFE_INTEGER as u64));

/// The lowest possible index that can be used to reference an [`Event`] inside
/// a [`Chunk`] - i.e., `0`.
///
/// This value is useful for constructing a key range over all keys which
/// contain [`Position`]s when used in conjunction with
/// [`INDEXED_KEY_UPPER_EVENT_INDEX`].
pub const INDEXED_KEY_LOWER_EVENT_INDEX: usize = 0;

/// The highest possible index that can be used to reference an [`Event`] inside
/// a [`Chunk`] - i.e., [`js_sys::Number::MAX_SAFE_INTEGER`].
///
/// This value is useful for constructing a key range over all keys which
/// contain [`Position`]s when used in conjunction with
/// [`INDEXED_KEY_LOWER_EVENT_INDEX`].
pub const INDEXED_KEY_UPPER_EVENT_INDEX: usize = js_sys::Number::MAX_SAFE_INTEGER as usize;

/// The lowest possible [`Position`] that can be used to reference an [`Event`].
///
/// This value is useful for constructing a key range over all keys which
/// contain [`Position`]s when used in conjunction with
/// [`INDEXED_KEY_UPPER_EVENT_INDEX`].
pub static INDEXED_KEY_LOWER_EVENT_POSITION: LazyLock<Position> = LazyLock::new(|| Position {
    chunk_identifier: INDEXED_KEY_LOWER_CHUNK_IDENTIFIER.index(),
    index: INDEXED_KEY_LOWER_EVENT_INDEX,
});

/// The highest possible [`Position`] that can be used to reference an
/// [`Event`].
///
/// This value is useful for constructing a key range over all keys which
/// contain [`Position`]s when used in conjunction with
/// [`INDEXED_KEY_LOWER_EVENT_INDEX`].
pub static INDEXED_KEY_UPPER_EVENT_POSITION: LazyLock<Position> = LazyLock::new(|| Position {
    chunk_identifier: INDEXED_KEY_UPPER_CHUNK_IDENTIFIER.index(),
    index: INDEXED_KEY_UPPER_EVENT_INDEX,
});
