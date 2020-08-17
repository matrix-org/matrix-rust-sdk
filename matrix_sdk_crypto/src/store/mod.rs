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

use std::{collections::HashSet, io::Error as IoError, sync::Arc};

use async_trait::async_trait;
use core::fmt::Debug;
use matrix_sdk_common::{
    identifiers::{DeviceId, RoomId, UserId},
    locks::Mutex,
};
use matrix_sdk_common_macros::send_sync;
use olm_rs::errors::{OlmAccountError, OlmGroupSessionError, OlmSessionError};
use serde_json::Error as SerdeError;
use thiserror::Error;
use url::ParseError;

use super::{
    device::ReadOnlyDevice,
    memory_stores::UserDevices,
    olm::{Account, InboundGroupSession, Session},
};

pub mod memorystore;

#[cfg(not(target_arch = "wasm32"))]
#[cfg(feature = "sqlite_cryptostore")]
pub mod sqlite;

#[cfg(not(target_arch = "wasm32"))]
#[cfg(feature = "sqlite_cryptostore")]
use sqlx::Error as SqlxError;

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
    #[error("can't load session timestamps")]
    SessionTimestampError,

    /// The store failed to (de)serialize a data type.
    #[error(transparent)]
    Serialization(#[from] SerdeError),

    /// An error occurred while parsing an URL.
    #[error(transparent)]
    UrlParse(#[from] ParseError),
}

pub type Result<T> = std::result::Result<T, CryptoStoreError>;

#[async_trait]
#[allow(clippy::type_complexity)]
#[cfg_attr(not(target_arch = "wasm32"), send_sync)]
/// Trait abstracting a store that the `OlmMachine` uses to store cryptographic
/// keys.
pub trait CryptoStore: Debug {
    /// Load an account that was previously stored.
    async fn load_account(&self) -> Result<Option<Account>>;

    /// Save the given account in the store.
    ///
    /// # Arguments
    ///
    /// * `account` - The account that should be stored.
    async fn save_account(&self, account: Account) -> Result<()>;

    /// Save the given sessions in the store.
    ///
    /// # Arguments
    ///
    /// * `session` - The sessions that should be stored.
    async fn save_sessions(&self, session: &[Session]) -> Result<()>;

    /// Get all the sessions that belong to the given sender key.
    ///
    /// # Arguments
    ///
    /// * `sender_key` - The sender key that was used to establish the sessions.
    async fn get_sessions(&self, sender_key: &str) -> Result<Option<Arc<Mutex<Vec<Session>>>>>;

    /// Save the given inbound group session in the store.
    ///
    /// If the session wasn't already in the store true is returned, false
    /// otherwise.
    ///
    /// # Arguments
    ///
    /// * `session` - The session that should be stored.
    async fn save_inbound_group_session(&self, session: InboundGroupSession) -> Result<bool>;

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

    /// Save the given devices in the store.
    ///
    /// # Arguments
    ///
    /// * `device` - The device that should be stored.
    async fn save_devices(&self, devices: &[ReadOnlyDevice]) -> Result<()>;

    /// Delete the given device from the store.
    ///
    /// # Arguments
    ///
    /// * `device` - The device that should be stored.
    async fn delete_device(&self, device: ReadOnlyDevice) -> Result<()>;

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
    async fn get_user_devices(&self, user_id: &UserId) -> Result<UserDevices>;
}
