// Copyright 2020 The Matrix.org Foundation C.I.C.
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
// limitations under the License.

//! Types and traits to implement the storage layer for the [`OlmMachine`]
//!
//! The storage layer for the [`OlmMachine`] can be customized using a trait.
//! Implementing your own [`CryptoStore`]
//!
//! An in-memory only store is provided as well as a SQLite based one, depending
//! on your needs and targets a custom store may be implemented, e.g. for
//! `wasm-unknown-unknown` an indexeddb store would be needed
//!
//! ```
//! # use matrix_sdk_crypto::{
//! #     OlmMachine,
//! #     store::MemoryStore,
//! # };
//! # use matrix_sdk_common::identifiers::{user_id, DeviceIdBox};
//! # let user_id = user_id!("@example:localhost");
//! # let device_id: DeviceIdBox = "TEST".into();
//! let store = Box::new(MemoryStore::new());
//!
//! let machine = OlmMachine::new_with_store(user_id, device_id, store);
//! ```
//!
//! [`OlmMachine`]: /matrix_sdk_crypto/struct.OlmMachine.html
//! [`CryptoStore`]: trait.Cryptostore.html

pub mod caches;
mod memorystore;
mod pickle_key;
#[cfg(not(target_arch = "wasm32"))]
#[cfg(feature = "sqlite_cryptostore")]
pub(crate) mod sqlite;

use matrix_sdk_common::identifiers::DeviceIdBox;
pub use memorystore::MemoryStore;
pub use pickle_key::{EncryptedPickleKey, PickleKey};
#[cfg(not(target_arch = "wasm32"))]
#[cfg(feature = "sqlite_cryptostore")]
pub use sqlite::SqliteStore;

use std::{
    collections::{HashMap, HashSet},
    fmt::Debug,
    io::Error as IoError,
    ops::Deref,
    sync::Arc,
};

use olm_rs::errors::{OlmAccountError, OlmGroupSessionError, OlmSessionError};
use serde::{Deserialize, Serialize};
use serde_json::Error as SerdeError;
use thiserror::Error;

#[cfg_attr(feature = "docs", doc(cfg(r#sqlite_cryptostore)))]
#[cfg(not(target_arch = "wasm32"))]
#[cfg(feature = "sqlite_cryptostore")]
use sqlx::Error as SqlxError;

use matrix_sdk_common::{
    identifiers::{
        DeviceId, DeviceKeyAlgorithm, Error as IdentifierValidationError, RoomId, UserId,
    },
    locks::Mutex,
};
use matrix_sdk_common_macros::async_trait;
#[cfg(not(target_arch = "wasm32"))]
use matrix_sdk_common_macros::send_sync;

use crate::{
    error::SessionUnpicklingError,
    identities::{Device, ReadOnlyDevice, UserDevices, UserIdentities},
    olm::{InboundGroupSession, PrivateCrossSigningIdentity, ReadOnlyAccount, Session},
    verification::VerificationMachine,
};

/// A `CryptoStore` specific result type.
pub type Result<T> = std::result::Result<T, CryptoStoreError>;

/// A wrapper for our CryptoStore trait object.
///
/// This is needed because we want to have a generic interface so we can
/// store/restore objects that we can serialize. Since trait objects and
/// generics don't mix let the CryptoStore store strings and this wrapper
/// adds the generic interface on top.
#[derive(Debug, Clone)]
pub(crate) struct Store {
    user_id: Arc<UserId>,
    inner: Arc<Box<dyn CryptoStore>>,
    verification_machine: VerificationMachine,
}

#[derive(Debug, Default)]
#[allow(missing_docs)]
pub struct Changes {
    pub account: Option<ReadOnlyAccount>,
    pub sessions: Vec<Session>,
    pub inbound_group_sessions: Vec<InboundGroupSession>,
    pub identities: IdentityChanges,
    pub devices: DeviceChanges,
}

#[derive(Debug, Clone, Default)]
#[allow(missing_docs)]
pub struct IdentityChanges {
    pub new: Vec<UserIdentities>,
    pub changed: Vec<UserIdentities>,
}

#[derive(Debug, Clone, Default)]
#[allow(missing_docs)]
pub struct DeviceChanges {
    pub new: Vec<ReadOnlyDevice>,
    pub changed: Vec<ReadOnlyDevice>,
    pub deleted: Vec<ReadOnlyDevice>,
}

impl Store {
    pub fn new(
        user_id: Arc<UserId>,
        store: Arc<Box<dyn CryptoStore>>,
        verification_machine: VerificationMachine,
    ) -> Self {
        Self {
            user_id,
            inner: store,
            verification_machine,
        }
    }

    pub async fn get_readonly_device(
        &self,
        user_id: &UserId,
        device_id: &DeviceId,
    ) -> Result<Option<ReadOnlyDevice>> {
        self.inner.get_device(user_id, device_id).await
    }

    pub async fn save_sessions(&self, sessions: &[Session]) -> Result<()> {
        let changes = Changes {
            sessions: sessions.to_vec(),
            ..Default::default()
        };

        self.save_changes(changes).await
    }

    #[cfg(test)]
    pub async fn save_devices(&self, devices: &[ReadOnlyDevice]) -> Result<()> {
        let changes = Changes {
            devices: DeviceChanges {
                changed: devices.to_vec(),
                ..Default::default()
            },
            ..Default::default()
        };

        self.save_changes(changes).await
    }

    #[cfg(test)]
    pub async fn save_inbound_group_sessions(
        &self,
        sessions: &[InboundGroupSession],
    ) -> Result<()> {
        let changes = Changes {
            inbound_group_sessions: sessions.to_vec(),
            ..Default::default()
        };

        self.save_changes(changes).await
    }

    pub async fn get_readonly_devices(
        &self,
        user_id: &UserId,
    ) -> Result<HashMap<DeviceIdBox, ReadOnlyDevice>> {
        self.inner.get_user_devices(user_id).await
    }

    pub async fn get_device_from_curve_key(
        &self,
        user_id: &UserId,
        curve_key: &str,
    ) -> Result<Option<Device>> {
        self.get_user_devices(user_id).await.map(|d| {
            d.devices().find(|d| {
                d.get_key(DeviceKeyAlgorithm::Curve25519)
                    .map_or(false, |k| k == curve_key)
            })
        })
    }

    pub async fn get_user_devices(&self, user_id: &UserId) -> Result<UserDevices> {
        let devices = self.inner.get_user_devices(user_id).await?;

        let own_identity = self
            .inner
            .get_user_identity(&self.user_id)
            .await?
            .map(|i| i.own().cloned())
            .flatten();
        let device_owner_identity = self.inner.get_user_identity(user_id).await.ok().flatten();

        Ok(UserDevices {
            inner: devices,
            verification_machine: self.verification_machine.clone(),
            own_identity,
            device_owner_identity,
        })
    }

    pub async fn get_device(
        &self,
        user_id: &UserId,
        device_id: &DeviceId,
    ) -> Result<Option<Device>> {
        let own_identity = self
            .get_user_identity(&self.user_id)
            .await?
            .map(|i| i.own().cloned())
            .flatten();
        let device_owner_identity = self.get_user_identity(user_id).await?;

        Ok(self
            .inner
            .get_device(user_id, device_id)
            .await?
            .map(|d| Device {
                inner: d,
                verification_machine: self.verification_machine.clone(),
                own_identity,
                device_owner_identity,
            }))
    }

    pub async fn get_object<V: for<'b> Deserialize<'b>>(&self, key: &str) -> Result<Option<V>> {
        if let Some(value) = self.get_value(key).await? {
            Ok(Some(serde_json::from_str(&value)?))
        } else {
            Ok(None)
        }
    }

    pub async fn save_object(&self, key: &str, value: &impl Serialize) -> Result<()> {
        let value = serde_json::to_string(value)?;
        self.save_value(key.to_owned(), value).await
    }

    pub async fn delete_object(&self, key: &str) -> Result<()> {
        self.inner.remove_value(key).await?;
        Ok(())
    }
}

impl Deref for Store {
    type Target = dyn CryptoStore;

    fn deref(&self) -> &Self::Target {
        &**self.inner
    }
}

#[derive(Error, Debug)]
/// The crypto store's error type.
pub enum CryptoStoreError {
    /// The account that owns the sessions, group sessions, and devices wasn't
    /// found.
    #[error("can't save/load sessions or group sessions in the store before an account is stored")]
    AccountUnset,

    /// SQL error occurred.
    // TODO flatten the SqlxError to make it easier for other store
    // implementations.
    #[cfg(feature = "sqlite_cryptostore")]
    #[error(transparent)]
    DatabaseError(#[from] SqlxError),

    /// An IO error occurred.
    #[error(transparent)]
    Io(#[from] IoError),

    /// The underlying Olm Account operation returned an error.
    #[error(transparent)]
    OlmAccount(#[from] OlmAccountError),

    /// The underlying Olm session operation returned an error.
    #[error(transparent)]
    OlmSession(#[from] OlmSessionError),

    /// The underlying Olm group session operation returned an error.
    #[error(transparent)]
    OlmGroupSession(#[from] OlmGroupSessionError),

    /// A session time-stamp couldn't be loaded.
    #[error(transparent)]
    SessionUnpickling(#[from] SessionUnpicklingError),

    /// Failed to decrypt an pickled object.
    #[error("An object failed to be decrypted while unpickling")]
    UnpicklingError,

    /// A Matirx identifier failed to be validated.
    #[error(transparent)]
    IdentifierValidation(#[from] IdentifierValidationError),

    /// The store failed to (de)serialize a data type.
    #[error(transparent)]
    Serialization(#[from] SerdeError),
}

/// Trait abstracting a store that the `OlmMachine` uses to store cryptographic
/// keys.
#[async_trait]
#[allow(clippy::type_complexity)]
#[cfg_attr(not(target_arch = "wasm32"), send_sync)]
pub trait CryptoStore: Debug {
    /// Load an account that was previously stored.
    async fn load_account(&self) -> Result<Option<ReadOnlyAccount>>;

    /// Save the given account in the store.
    ///
    /// # Arguments
    ///
    /// * `account` - The account that should be stored.
    async fn save_account(&self, account: ReadOnlyAccount) -> Result<()>;

    /// TODO
    async fn save_identity(&self, identity: PrivateCrossSigningIdentity) -> Result<()>;

    /// TODO
    async fn load_identity(&self) -> Result<Option<PrivateCrossSigningIdentity>>;

    /// TODO
    async fn save_changes(&self, changes: Changes) -> Result<()>;

    /// Get all the sessions that belong to the given sender key.
    ///
    /// # Arguments
    ///
    /// * `sender_key` - The sender key that was used to establish the sessions.
    async fn get_sessions(&self, sender_key: &str) -> Result<Option<Arc<Mutex<Vec<Session>>>>>;

    /// Get the inbound group session from our store.
    ///
    /// # Arguments
    /// * `room_id` - The room id of the room that the session belongs to.
    ///
    /// * `sender_key` - The sender key that sent us the session.
    ///
    /// * `session_id` - The unique id of the session.
    async fn get_inbound_group_session(
        &self,
        room_id: &RoomId,
        sender_key: &str,
        session_id: &str,
    ) -> Result<Option<InboundGroupSession>>;

    /// Get all the inbound group sessions we have stored.
    async fn get_inbound_group_sessions(&self) -> Result<Vec<InboundGroupSession>>;

    /// Is the given user already tracked.
    fn is_user_tracked(&self, user_id: &UserId) -> bool;

    /// Are there any tracked users that are marked as dirty.
    fn has_users_for_key_query(&self) -> bool;

    /// Set of users that we need to query keys for. This is a subset of
    /// the tracked users.
    fn users_for_key_query(&self) -> HashSet<UserId>;

    /// Add an user for tracking.
    ///
    /// Returns true if the user wasn't already tracked, false otherwise.
    ///
    /// # Arguments
    ///
    /// * `user` - The user that should be marked as tracked.
    ///
    /// * `dirty` - Should the user be also marked for a key query.
    async fn update_tracked_user(&self, user: &UserId, dirty: bool) -> Result<bool>;

    /// Get the device for the given user with the given device id.
    ///
    /// # Arguments
    ///
    /// * `user_id` - The user that the device belongs to.
    ///
    /// * `device_id` - The unique id of the device.
    async fn get_device(
        &self,
        user_id: &UserId,
        device_id: &DeviceId,
    ) -> Result<Option<ReadOnlyDevice>>;

    /// Get all the devices of the given user.
    ///
    /// # Arguments
    ///
    /// * `user_id` - The user for which we should get all the devices.
    async fn get_user_devices(
        &self,
        user_id: &UserId,
    ) -> Result<HashMap<DeviceIdBox, ReadOnlyDevice>>;

    /// Get the user identity that is attached to the given user id.
    ///
    /// # Arguments
    ///
    /// * `user_id` - The user for which we should get the identity.
    async fn get_user_identity(&self, user_id: &UserId) -> Result<Option<UserIdentities>>;

    /// Save a serializeable object in the store.
    async fn save_value(&self, key: String, value: String) -> Result<()>;

    /// Remove a value from the store.
    async fn remove_value(&self, key: &str) -> Result<()>;

    /// Load a serializeable object from the store.
    async fn get_value(&self, key: &str) -> Result<Option<String>>;
}
