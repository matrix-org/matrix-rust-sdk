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
};
use thiserror::Error;

/// The current version and keys used in the database.
pub mod current {
    use super::{v1, Version};

    pub const VERSION: Version = Version::V1;
    pub use v1::keys;
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
                version = match version.upgrade(transaction.db())? {
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
    /// Version 0 of the database, for details see [`v0`]
    V0 = 0,
    /// Version 1 of the database, for details see [`v1`]
    V1 = 1,
}

impl Version {
    /// Upgrade the database to the next version, if one exists.
    pub fn upgrade(self, db: &Database) -> Result<Option<Self>, Error> {
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
    pub fn upgrade(db: &Database) -> Result<Version, Error> {
        v1::create_object_stores(db)?;
        Ok(Version::V1)
    }
}

pub mod v1 {
    use indexed_db_futures::Build;

    use super::*;

    pub mod keys {
        pub const CORE: &str = "core";
        pub const CORE_KEY_PATH: &str = "id";
        pub const LEASES: &str = "leases";
        pub const LEASES_KEY_PATH: &str = "id";
        pub const MEDIA_RETENTION_POLICY_KEY: &str = "media_retention_policy";
        pub const MEDIA_CLEANUP_TIME_KEY: &str = "media_cleanup_time";
        pub const MEDIA: &str = "media";
        pub const MEDIA_KEY_PATH: &str = "id";
        pub const MEDIA_URI: &str = "media_uri";
        pub const MEDIA_URI_KEY_PATH: &str = "uri";
        pub const MEDIA_CONTENT_SIZE: &str = "media_content_size";
        pub const MEDIA_CONTENT_SIZE_KEY_PATH: &str = "content_size";
        pub const MEDIA_LAST_ACCESS: &str = "media_last_access";
        pub const MEDIA_LAST_ACCESS_KEY_PATH: &str = "last_access";
        pub const MEDIA_RETENTION_METADATA: &str = "media_retention_metadata";
        pub const MEDIA_RETENTION_METADATA_KEY_PATH: &str = "retention_metadata";
        pub const MEDIA_METADATA: &str = "media_metadata";
        pub const MEDIA_METADATA_KEY_PATH: &str = "id";
        pub const MEDIA_METADATA_URI: &str = "media_metadata_uri";
        pub const MEDIA_METADATA_URI_KEY_PATH: &str = "uri";
        pub const MEDIA_METADATA_CONTENT_SIZE: &str = "media_metadata_content_size";
        pub const MEDIA_METADATA_CONTENT_SIZE_KEY_PATH: &str = "content_size";
        pub const MEDIA_METADATA_LAST_ACCESS: &str = "media_metadata_last_access";
        pub const MEDIA_METADATA_LAST_ACCESS_KEY_PATH: &str = "last_access";
        pub const MEDIA_METADATA_RETENTION: &str = "media_metadata_retention";
        pub const MEDIA_METADATA_RETENTION_KEY_PATH: &str = "retention";
        pub const MEDIA_CONTENT: &str = "media_content";
        pub const MEDIA_CONTENT_KEY_PATH: &str = "id";
    }

    /// Create all object stores and indices for v1 database
    pub fn create_object_stores(db: &Database) -> Result<(), Error> {
        create_core_object_store(db)?;
        create_lease_object_store(db)?;
        create_media_object_store(db)?;
        create_media_metadata_object_store(db)?;
        create_media_content_object_store(db)?;
        Ok(())
    }

    /// Create an object store for tracking miscellaneous information
    ///
    /// * Primary Key - `id`
    fn create_core_object_store(db: &Database) -> Result<(), Error> {
        let _ =
            db.create_object_store(keys::CORE).with_key_path(keys::CORE_KEY_PATH.into()).build()?;
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

    /// Create an object store for tracking information about media.
    ///
    /// * Primary Key - `id`
    /// * Index - `uri` - tracks the [`MxcUri`][1] of the associated media
    /// * Index - `content_size` - tracks the size of the media content and
    ///   whether to ignore the [`MediaRetentionPolicy`][2]
    /// * Index - `last_access` - tracks the last time the associated media was
    ///   accessed
    /// * Index - `retention_metadata` - tracks all retention metadata - i.e.,
    ///   joins `content_size` and `last_access`
    ///
    /// [1]: ruma::MxcUri
    /// [2]: matrix_sdk_base::media::store::MediaRetentionPolicy
    fn create_media_object_store(db: &Database) -> Result<(), Error> {
        let media = db
            .create_object_store(keys::MEDIA)
            .with_key_path(keys::MEDIA_KEY_PATH.into())
            .build()?;
        let _ = media.create_index(keys::MEDIA_URI, keys::MEDIA_URI_KEY_PATH.into()).build()?;
        let _ = media
            .create_index(keys::MEDIA_CONTENT_SIZE, keys::MEDIA_CONTENT_SIZE_KEY_PATH.into())
            .build()?;
        let _ = media
            .create_index(keys::MEDIA_LAST_ACCESS, keys::MEDIA_LAST_ACCESS_KEY_PATH.into())
            .build()?;
        let _ = media
            .create_index(
                keys::MEDIA_RETENTION_METADATA,
                keys::MEDIA_RETENTION_METADATA_KEY_PATH.into(),
            )
            .build()?;
        Ok(())
    }

    /// Create an object store for tracking information about media metadata.
    ///
    /// * Primary Key - `id` - unique key derived from
    ///   [`MediaRequestParameters`] of the associated media
    /// * Index - `uri` - tracks the [`MxcUri`][1] of the associated media
    /// * Index - `content_size` - tracks the size of the media content and
    ///   whether to ignore the [`MediaRetentionPolicy`][2]
    /// * Index - `last_access` - tracks the last time the associated media was
    ///   accessed
    /// * Index - `retention` - tracks all retention metadata - i.e., joins
    ///   `content_size` and `last_access`
    ///
    /// [1]: ruma::MxcUri
    /// [2]: matrix_sdk_base::media::store::MediaRetentionPolicy
    fn create_media_metadata_object_store(db: &Database) -> Result<(), Error> {
        let media = db
            .create_object_store(keys::MEDIA_METADATA)
            .with_key_path(keys::MEDIA_METADATA_KEY_PATH.into())
            .build()?;
        let _ = media
            .create_index(keys::MEDIA_METADATA_URI, keys::MEDIA_METADATA_URI_KEY_PATH.into())
            .build()?;
        let _ = media
            .create_index(
                keys::MEDIA_METADATA_CONTENT_SIZE,
                keys::MEDIA_METADATA_CONTENT_SIZE_KEY_PATH.into(),
            )
            .build()?;
        let _ = media
            .create_index(
                keys::MEDIA_METADATA_LAST_ACCESS,
                keys::MEDIA_METADATA_LAST_ACCESS_KEY_PATH.into(),
            )
            .build()?;
        let _ = media
            .create_index(
                keys::MEDIA_METADATA_RETENTION,
                keys::MEDIA_METADATA_RETENTION_KEY_PATH.into(),
            )
            .build()?;
        Ok(())
    }

    /// Create an object store for tracking information about media.
    ///
    /// * Primary Key - `id` - number tracking the ID of the media content.
    fn create_media_content_object_store(db: &Database) -> Result<(), Error> {
        let _ = db
            .create_object_store(keys::MEDIA_CONTENT)
            .with_key_path(keys::MEDIA_CONTENT_KEY_PATH.into())
            .build()?;
        Ok(())
    }
}
