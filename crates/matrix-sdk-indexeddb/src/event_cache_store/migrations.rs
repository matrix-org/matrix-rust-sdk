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

use indexed_db_futures::{
    idb_object_store::IdbObjectStoreParameters, request::IdbOpenDbRequestLike, IdbDatabase,
    IdbVersionChangeEvent,
};
use thiserror::Error;
use wasm_bindgen::JsValue;
use web_sys::{DomException, IdbIndexParameters};

const CURRENT_DB_VERSION: Version = Version::V1;

/// Opens a connection to the IndexedDB database and takes care of upgrading it
/// if necessary.
#[allow(unused)]
pub async fn open_and_upgrade_db(name: &str) -> Result<IdbDatabase, DomException> {
    let mut request = IdbDatabase::open_u32(name, CURRENT_DB_VERSION as u32)?;
    request.set_on_upgrade_needed(Some(|event: &IdbVersionChangeEvent| -> Result<(), JsValue> {
        let mut version =
            Version::try_from(event.old_version() as u32).map_err(DomException::from)?;
        while version < CURRENT_DB_VERSION {
            version = match version.upgrade(event.db())? {
                Some(next) => next,
                None => CURRENT_DB_VERSION, /* No more upgrades to apply, jump forward! */
            };
        }
        Ok(())
    }));
    request.await
}

/// Represents the version of the IndexedDB database.
#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u32)]
pub enum Version {
    /// Version 0 of the database, for details see [`v0`]
    V0 = 0,
    /// Version 1 of the database, for details see [`v1`]
    V1 = 1,
}

impl Version {
    /// Upgrade the database to the next version, if one exists.
    pub fn upgrade(self, db: &IdbDatabase) -> Result<Option<Self>, DomException> {
        match self {
            Self::V0 => v0::upgrade(db).map(Some),
            Self::V1 => Ok(None),
        }
    }
}

#[derive(Debug, Error)]
#[error("unknown version: {0}")]
pub struct UnknownVersionError(u32);

impl TryFrom<u32> for Version {
    type Error = UnknownVersionError;

    fn try_from(value: u32) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Version::V0),
            1 => Ok(Version::V1),
            v => Err(UnknownVersionError(v)),
        }
    }
}

impl From<UnknownVersionError> for DomException {
    fn from(value: UnknownVersionError) -> Self {
        let message = format!("unknown version: {}", value.0);
        let name = "UnknownVersionError";
        match DomException::new_with_message_and_name(&message, name) {
            Ok(inner) => inner,
            Err(err) => err.into(),
        }
    }
}

pub mod v0 {
    use super::*;

    /// Upgrade database from `v0` to `v1`
    pub fn upgrade(db: &IdbDatabase) -> Result<Version, DomException> {
        v1::create_object_stores(db)?;
        Ok(Version::V1)
    }
}

pub mod v1 {
    use super::*;

    pub mod keys {
        pub const CORE: &str = "core";
        pub const LINKED_CHUNKS: &str = "linked_chunks";
        pub const LINKED_CHUNKS_KEY_PATH: &str = "id";
        pub const LINKED_CHUNKS_NEXT: &str = "linked_chunks_next";
        pub const LINKED_CHUNKS_NEXT_KEY_PATH: &str = "next";
        pub const EVENTS: &str = "events";
        pub const EVENTS_KEY_PATH: &str = "id";
        pub const EVENTS_POSITION: &str = "events_position";
        pub const EVENTS_POSITION_KEY_PATH: &str = "position";
        pub const EVENTS_RELATION: &str = "events_relation";
        pub const EVENTS_RELATION_KEY_PATH: &str = "relation";
        pub const GAPS: &str = "gaps";
        pub const GAPS_KEY_PATH: &str = "id";
    }

    /// Create all object stores and indices for v1 database
    pub fn create_object_stores(db: &IdbDatabase) -> Result<(), DomException> {
        create_core_object_store(db)?;
        create_linked_chunks_object_store(db)?;
        create_events_object_store(db)?;
        create_gaps_object_store(db)?;
        Ok(())
    }

    /// Create an object store for tracking miscellaneous information, e.g.,
    /// leases locks
    fn create_core_object_store(db: &IdbDatabase) -> Result<(), DomException> {
        let _ = db.create_object_store(keys::CORE)?;
        Ok(())
    }

    /// Create an object store for tracking information about linked chunks.
    ///
    /// * Primary Key - `id`
    /// * Index - `is_last` - tracks the last chunk in linked chunks
    fn create_linked_chunks_object_store(db: &IdbDatabase) -> Result<(), DomException> {
        let mut object_store_params = IdbObjectStoreParameters::new();
        object_store_params.key_path(Some(&keys::LINKED_CHUNKS_KEY_PATH.into()));
        let linked_chunks =
            db.create_object_store_with_params(keys::LINKED_CHUNKS, &object_store_params)?;
        linked_chunks
            .create_index(keys::LINKED_CHUNKS_NEXT, &keys::LINKED_CHUNKS_NEXT_KEY_PATH.into())?;
        Ok(())
    }

    /// Create an object store for tracking information about events.
    ///
    /// * Primary Key - `id`
    /// * Index (unique) - `position` - tracks position of an event in linked
    ///   chunks
    /// * Index - `relation` - tracks any event to which the given event is
    ///   related
    fn create_events_object_store(db: &IdbDatabase) -> Result<(), DomException> {
        let mut object_store_params = IdbObjectStoreParameters::new();
        object_store_params.key_path(Some(&keys::EVENTS_KEY_PATH.into()));
        let events = db.create_object_store_with_params(keys::EVENTS, &object_store_params)?;

        let events_position_params = IdbIndexParameters::new();
        events_position_params.set_unique(true);
        events.create_index_with_params(
            keys::EVENTS_POSITION,
            &keys::EVENTS_POSITION_KEY_PATH.into(),
            &events_position_params,
        )?;

        events.create_index(keys::EVENTS_RELATION, &keys::EVENTS_RELATION_KEY_PATH.into())?;

        Ok(())
    }

    /// Create an object store for tracking information about gaps.
    ///
    /// * Primary Key - `id`
    fn create_gaps_object_store(db: &IdbDatabase) -> Result<(), DomException> {
        let mut object_store_params = IdbObjectStoreParameters::new();
        object_store_params.key_path(Some(&keys::GAPS_KEY_PATH.into()));
        let _ = db.create_object_store_with_params(keys::GAPS, &object_store_params)?;
        Ok(())
    }
}
