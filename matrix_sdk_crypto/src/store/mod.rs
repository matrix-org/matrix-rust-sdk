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

use core::fmt::Debug;
use std::{collections::HashSet, io::Error as IoError, sync::Arc};
use url::ParseError;

use async_trait::async_trait;
use matrix_sdk_common::locks::Mutex;
use serde_json::Error as SerdeError;
use thiserror::Error;

use super::{
    device::Device,
    memory_stores::UserDevices,
    olm::{Account, InboundGroupSession, Session},
};
use matrix_sdk_common::identifiers::{DeviceId, RoomId, UserId};
use matrix_sdk_common_macros::send_sync;
use olm_rs::errors::{OlmAccountError, OlmGroupSessionError, OlmSessionError};

pub mod memorystore;

#[cfg(not(target_arch = "wasm32"))]
#[cfg(feature = "sqlite-cryptostore")]
pub mod sqlite;

#[cfg(not(target_arch = "wasm32"))]
#[cfg(feature = "sqlite-cryptostore")]
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
    #[cfg(feature = "sqlite-cryptostore")]
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
    async fn load_account(&mut self) -> Result<Option<Account>>;

    /// Save the given account in the store.
    ///
    /// # Arguments
    ///
    /// * `account` - The account that should be stored.
    async fn save_account(&mut self, account: Account) -> Result<()>;

    /// Save the given sessions in the store.
    ///
    /// # Arguments
    ///
    /// * `session` - The sessions that should be stored.
    async fn save_sessions(&mut self, session: &[Session]) -> Result<()>;

    /// Get all the sessions that belong to the given sender key.
    ///
    /// # Arguments
    ///
    /// * `sender_key` - The sender key that was used to establish the sessions.
    async fn get_sessions(&mut self, sender_key: &str) -> Result<Option<Arc<Mutex<Vec<Session>>>>>;

    /// Save the given inbound group session in the store.
    ///
    /// If the session wasn't already in the store true is returned, false
    /// otherwise.
    ///
    /// # Arguments
    ///
    /// * `session` - The session that should be stored.
    async fn save_inbound_group_session(&mut self, session: InboundGroupSession) -> Result<bool>;

    /// Get the inbound group session from our store.
    ///
    /// # Arguments
    /// * `room_id` - The room id of the room that the session belongs to.
    ///
    /// * `sender_key` - The sender key that sent us the session.
    ///
    /// * `session_id` - The unique id of the session.
    async fn get_inbound_group_session(
        &mut self,
        room_id: &RoomId,
        sender_key: &str,
        session_id: &str,
    ) -> Result<Option<InboundGroupSession>>;

    /// Get the set of tracked users.
    fn tracked_users(&self) -> &HashSet<UserId>;

    /// Set of users that we need to query keys for. This is a subset of
    /// the tracked users.
    fn users_for_key_query(&self) -> &HashSet<UserId>;

    /// Add an user for tracking.
    ///
    /// Returns true if the user wasn't already tracked, false otherwise.
    ///
    /// # Arguments
    ///
    /// * `user` - The user that should be marked as tracked.
    ///
    /// * `dirty` - Should the user be also marked for a key query.
    async fn update_tracked_user(&mut self, user: &UserId, dirty: bool) -> Result<bool>;

    /// Save the given devices in the store.
    ///
    /// # Arguments
    ///
    /// * `device` - The device that should be stored.
    async fn save_devices(&self, devices: &[Device]) -> Result<()>;

    /// Delete the given device from the store.
    ///
    /// # Arguments
    ///
    /// * `device` - The device that should be stored.
    async fn delete_device(&self, device: Device) -> Result<()>;

    /// Get the device for the given user with the given device id.
    ///
    /// # Arguments
    ///
    /// * `user_id` - The user that the device belongs to.
    ///
    /// * `device_id` - The unique id of the device.
    #[allow(clippy::ptr_arg)]
    async fn get_device(&self, user_id: &UserId, device_id: &DeviceId) -> Result<Option<Device>>;

    /// Get all the devices of the given user.
    ///
    /// # Arguments
    ///
    /// * `user_id` - The user for which we should get all the devices.
    async fn get_user_devices(&self, user_id: &UserId) -> Result<UserDevices>;
}
