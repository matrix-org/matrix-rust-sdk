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

/// The current version and keys used in the database.
pub mod current {
    use super::{v1, Version};

    pub const VERSION: Version = Version::V1;
    pub use v1::keys;
}

/// Opens a connection to the IndexedDB database and takes care of upgrading it
/// if necessary.
#[allow(unused)]
pub async fn open_and_upgrade_db(name: &str) -> Result<IdbDatabase, DomException> {
    let mut request = IdbDatabase::open_u32(name, current::VERSION as u32)?;
    request.set_on_upgrade_needed(Some(|event: &IdbVersionChangeEvent| -> Result<(), JsValue> {
        let mut version =
            Version::try_from(event.old_version() as u32).map_err(DomException::from)?;
        while version < current::VERSION {
            version = match version.upgrade(event.db())? {
                Some(next) => next,
                None => current::VERSION, /* No more upgrades to apply, jump forward! */
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
        pub const CORE_KEY_PATH: &str = "id";
        pub const LEASES: &str = "leases";
        pub const LEASES_KEY_PATH: &str = "id";
        pub const MEDIA_RETENTION_POLICY_KEY: &str = "media_retention_policy";
        pub const MEDIA: &str = "media";
        pub const MEDIA_KEY_PATH: &str = "id";
        pub const MEDIA_SOURCE: &str = "media_source";
        pub const MEDIA_SOURCE_KEY_PATH: &str = "source";
        pub const MEDIA_CONTENT_SIZE: &str = "media_content_size";
        pub const MEDIA_CONTENT_SIZE_KEY_PATH: &str = "content_size";
        pub const MEDIA_LAST_ACCESS: &str = "media_last_access";
        pub const MEDIA_LAST_ACCESS_KEY_PATH: &str = "last_access";
        pub const MEDIA_RETENTION_METADATA: &str = "media_retention_metadata";
        pub const MEDIA_RETENTION_METADATA_KEY_PATH: &str = "retention_metadata";
    }

    /// Create all object stores and indices for v1 database
    pub fn create_object_stores(db: &IdbDatabase) -> Result<(), DomException> {
        create_core_object_store(db)?;
        create_lease_object_store(db)?;
        create_media_object_store(db)?;
        Ok(())
    }

    /// Create an object store for tracking miscellaneous information
    ///
    /// * Primary Key - `id`
    fn create_core_object_store(db: &IdbDatabase) -> Result<(), DomException> {
        let mut object_store_params = IdbObjectStoreParameters::new();
        object_store_params.key_path(Some(&keys::CORE_KEY_PATH.into()));
        let _ = db.create_object_store_with_params(keys::CORE, &object_store_params)?;
        Ok(())
    }

    /// Create an object store tracking leases on time-based locks
    fn create_lease_object_store(db: &IdbDatabase) -> Result<(), DomException> {
        let mut object_store_params = IdbObjectStoreParameters::new();
        object_store_params.key_path(Some(&keys::LEASES_KEY_PATH.into()));
        let _ = db.create_object_store_with_params(keys::LEASES, &object_store_params)?;
        Ok(())
    }

    /// Create an object store for tracking information about media.
    ///
    /// * Primary Key - `id`
    /// * Index - `source` - tracks the [`MediaSource`][1] of the associated
    ///   media
    /// * Index - `content_size` - tracks the size of the media content and
    ///   whether to ignore the [`MediaRetentionPolicy`][2]
    /// * Index - `last_access` - tracks the last time the associated media was
    ///   accessed
    /// * Index - `retention_metadata` - tracks all retention metadata - i.e.,
    ///   joins `content_size` and `last_access`
    ///
    /// [1]: ruma::events::room::MediaSource
    /// [2]: matrix_sdk_base::media::store::MediaRetentionPolicy
    fn create_media_object_store(db: &IdbDatabase) -> Result<(), DomException> {
        let mut object_store_params = IdbObjectStoreParameters::new();
        object_store_params.key_path(Some(&keys::MEDIA_KEY_PATH.into()));
        let media = db.create_object_store_with_params(keys::MEDIA, &object_store_params)?;
        media.create_index(keys::MEDIA_SOURCE, &keys::MEDIA_SOURCE_KEY_PATH.into())?;
        media.create_index(keys::MEDIA_CONTENT_SIZE, &keys::MEDIA_CONTENT_SIZE_KEY_PATH.into())?;
        media.create_index(keys::MEDIA_LAST_ACCESS, &keys::MEDIA_LAST_ACCESS_KEY_PATH.into())?;
        media.create_index(
            keys::MEDIA_RETENTION_METADATA,
            &keys::MEDIA_RETENTION_METADATA_KEY_PATH.into(),
        )?;
        Ok(())
    }
}
