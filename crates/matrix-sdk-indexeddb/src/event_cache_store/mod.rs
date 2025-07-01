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

#![allow(unused)]

use indexed_db_futures::IdbDatabase;
use matrix_sdk_base::event_cache::store::MemoryStore;
use web_sys::IdbTransactionMode;

use crate::event_cache_store::serializer::IndexeddbEventCacheStoreSerializer;

mod builder;
mod error;
mod migrations;
mod serializer;
mod transaction;
mod types;

pub use builder::IndexeddbEventCacheStoreBuilder;
pub use error::IndexeddbEventCacheStoreError;

/// A type for providing an IndexedDB implementation of [`EventCacheStore`][1].
/// This is meant to be used as a backend to [`EventCacheStore`][1] in browser
/// contexts.
///
/// [1]: matrix_sdk_base::event_cache::store::EventCacheStore
pub struct IndexeddbEventCacheStore {
    // A handle to the IndexedDB database
    inner: IdbDatabase,
    // A serializer with functionality tailored to `IndexeddbEventCacheStore`
    serializer: IndexeddbEventCacheStoreSerializer,
    // An in-memory store for providing temporary implementations for
    // functions of `EventCacheStore`.
    //
    // NOTE: This will be removed once we have IndexedDB-backed implementations for all
    // functions in `EventCacheStore`.
    memory_store: MemoryStore,
}

impl IndexeddbEventCacheStore {
    /// Provides a type with which to conveniently build an
    /// [`IndexeddbEventCacheStore`]
    pub fn builder() -> IndexeddbEventCacheStoreBuilder {
        IndexeddbEventCacheStoreBuilder::default()
    }
}
