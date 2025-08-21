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

use std::sync::Arc;

use matrix_sdk_base::event_cache::store::{MemoryMediaStore, MemoryStore};
use matrix_sdk_store_encryption::StoreCipher;
use web_sys::DomException;

use crate::{
    media_store::{error::IndexeddbMediaStoreError, IndexeddbMediaStore},
    serializer::IndexeddbSerializer,
};

/// A type for conveniently building an [`IndexeddbMediaStore`]
#[derive(Default)]
pub struct IndexeddbMediaStoreBuilder {}

impl IndexeddbMediaStoreBuilder {
    /// Opens the IndexedDB database with the provided name. If successfully
    /// opened, builds the [`IndexeddbMediaStore`] with that database
    /// and the provided store cipher.
    pub fn build(self) -> Result<IndexeddbMediaStore, IndexeddbMediaStoreError> {
        Ok(IndexeddbMediaStore { memory_store: MemoryMediaStore::new() })
    }
}
