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
    database::Database,
    error::{DomException, Error, OpenDbError},
    transaction::Transaction,
};
use thiserror::Error;

/// The current version and keys used in the database.
pub mod current {
    use super::{Version, v2};

    pub const VERSION: Version = Version::V2;
    pub use v2::keys;
}

/// Opens a connection to the IndexedDB database and takes care of upgrading it
/// if necessary.
#[allow(unused)]
pub async fn open_and_upgrade_db(name: &str) -> Result<Database, OpenDbError> {
    Database::open(name)
        .with_version(current::VERSION as u32)
        .with_on_upgrade_needed(|event, transaction| {
            let mut version = Version::try_from(event.old_version() as u32)?;
            while version < current::VERSION {
                version = match version.upgrade(transaction)? {
                    Some(next) => next,
                    None => current::VERSION, /* No more upgrades to apply, jump forward! */
                };
            }
            Ok(())
        })
        .await
}

/// Represents the version of the IndexedDB database.
#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u32)]
pub enum Version {
    /// Version 0 of the database, for details see [`v0`].
    V0 = 0,
    /// Version 1 of the database, for details see [`v1`].
    V1 = 1,
    /// Version 2 of the database, for details see [`v2`].
    V2 = 2,
}

impl Version {
    /// Upgrade the database to the next version, if one exists.
    pub fn upgrade(self, transaction: &Transaction<'_>) -> Result<Option<Self>, Error> {
        match self {
            Self::V0 => v0::upgrade(transaction).map(Some),
            Self::V1 => v1::upgrade(transaction).map(Some),
            Self::V2 => Ok(None),
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
            2 => Ok(Version::V2),
            v => Err(UnknownVersionError(v)),
        }
    }
}

impl From<UnknownVersionError> for Error {
    fn from(value: UnknownVersionError) -> Self {
        let message = format!("unknown version: {}", value.0);
        let name = "UnknownVersionError";
        match web_sys::DomException::new_with_message_and_name(&message, name) {
            Ok(inner) => Self::DomException(DomException::DataError(inner)),
            Err(err) => err.into(),
        }
    }
}

pub mod v0 {
    use super::*;

    /// Upgrade database from `v0` to `v1`
    pub fn upgrade(transaction: &Transaction<'_>) -> Result<Version, Error> {
        v1::create_object_stores(transaction.db())?;
        Ok(Version::V1)
    }
}

pub mod v1 {
    use indexed_db_futures::Build;

    use super::*;

    pub mod keys {
        pub const LEASES: &str = "leases";
        pub const LEASES_KEY_PATH: &str = "id";
        pub const ROOMS: &str = "rooms";
        pub const LINKED_CHUNK_IDS: &str = "linked_chunk_ids";
        pub const LINKED_CHUNKS: &str = "linked_chunks";
        pub const LINKED_CHUNKS_KEY_PATH: &str = "id";
        pub const LINKED_CHUNKS_NEXT: &str = "linked_chunks_next";
        pub const LINKED_CHUNKS_NEXT_KEY_PATH: &str = "next";
        pub const EVENTS: &str = "events";
        pub const EVENTS_KEY_PATH: &str = "id";
        pub const EVENTS_ROOM: &str = "events_room";
        pub const EVENTS_ROOM_KEY_PATH: &str = "room";
        pub const EVENTS_POSITION: &str = "events_position";
        pub const EVENTS_POSITION_KEY_PATH: &str = "position";
        pub const EVENTS_RELATION: &str = "events_relation";
        pub const EVENTS_RELATION_KEY_PATH: &str = "relation";
        pub const EVENTS_RELATION_RELATED_EVENTS: &str = "events_relation_related_event";
        pub const EVENTS_RELATION_RELATION_TYPES: &str = "events_relation_relation_type";
        pub const GAPS: &str = "gaps";
        pub const GAPS_KEY_PATH: &str = "id";
    }

    /// Create all object stores and indices for v1 database
    pub fn create_object_stores(db: &Database) -> Result<(), Error> {
        create_lease_object_store(db)?;
        create_linked_chunks_object_store(db)?;
        create_events_object_store(db)?;
        create_gaps_object_store(db)?;
        Ok(())
    }

    /// Create an object store tracking leases on time-based locks
    fn create_lease_object_store(db: &Database) -> Result<(), Error> {
        let _ = db
            .create_object_store(keys::LEASES)
            .with_key_path(keys::LEASES_KEY_PATH.into())
            .build()?;
        Ok(())
    }

    /// Create an object store for tracking information about linked chunks.
    ///
    /// * Primary Key - `id`
    /// * Index - `is_last` - tracks the last chunk in linked chunks
    fn create_linked_chunks_object_store(db: &Database) -> Result<(), Error> {
        let _ = db
            .create_object_store(keys::LINKED_CHUNKS)
            .with_key_path(keys::LINKED_CHUNKS_KEY_PATH.into())
            .build()?
            .create_index(keys::LINKED_CHUNKS_NEXT, keys::LINKED_CHUNKS_NEXT_KEY_PATH.into())
            .build()?;
        Ok(())
    }

    /// Create an object store for tracking information about events.
    ///
    /// * Primary Key - `id`
    /// * Index (unique) - `room` - tracks whether an event is in a given room
    /// * Index (unique) - `position` - tracks position of an event in linked
    ///   chunks
    /// * Index - `relation` - tracks any event to which the given event is
    ///   related
    fn create_events_object_store(db: &Database) -> Result<(), Error> {
        let events = db
            .create_object_store(keys::EVENTS)
            .with_key_path(keys::EVENTS_KEY_PATH.into())
            .build()?;
        let _ = events
            .create_index(keys::EVENTS_ROOM, keys::EVENTS_ROOM_KEY_PATH.into())
            .with_unique(true)
            .build()?;
        let _ = events
            .create_index(keys::EVENTS_POSITION, keys::EVENTS_POSITION_KEY_PATH.into())
            .with_unique(true)
            .build()?;
        let _ = events
            .create_index(keys::EVENTS_RELATION, keys::EVENTS_RELATION_KEY_PATH.into())
            .build()?;
        Ok(())
    }

    /// Create an object store for tracking information about gaps.
    ///
    /// * Primary Key - `id`
    fn create_gaps_object_store(db: &Database) -> Result<(), Error> {
        let _ =
            db.create_object_store(keys::GAPS).with_key_path(keys::GAPS_KEY_PATH.into()).build()?;
        Ok(())
    }

    /// Upgrade database from `v1` to `v2`
    pub fn upgrade(transaction: &Transaction<'_>) -> Result<Version, Error> {
        v2::empty_leases(transaction)?;
        Ok(Version::V2)
    }
}

mod v2 {
    // Re-use all the same keys from `v1`.
    pub use super::v1::keys;
    use super::*;

    /// The format of [`Lease`][super::super::types::Lease] is changing. Let's
    /// erase previous values.
    pub fn empty_leases(transaction: &Transaction<'_>) -> Result<(), Error> {
        let object_store = transaction.object_store(keys::LEASES)?;

        // Remove all previous leases.
        object_store.clear()?;

        Ok(())
    }
}
