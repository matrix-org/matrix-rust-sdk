// Copyright 2026 The Matrix.org Foundation C.I.C.
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

//! Support for "impersonatable" devices as defined by [MSC4350].
//!
//! A bridge bot sends messages on behalf of its ghost users using the bot's
//! own device. To make that verifiable, the ghost publishes a device with no
//! keys of its own, which names the bot's device in an `impersonator` field and
//! is signed by that device. Such devices can't be stored as ordinary
//! [`DeviceData`], so they are kept separately here and consulted when
//! computing the verification state of a message whose sender doesn't match
//! the owner of the Megolm session.
//!
//! [MSC4350]: https://github.com/matrix-org/matrix-spec-proposals/pull/4350

use std::{
    collections::BTreeMap,
    sync::{Arc, RwLock as StdRwLock},
};

use matrix_sdk_common::BoxFuture;
use ruma::{DeviceId, DeviceKeyAlgorithm, DeviceKeyId, OwnedRoomId, OwnedUserId, UserId};
use serde_json::{Map, Value};

use super::DeviceData;
use crate::{
    olm::VerifyJson,
    store::{Result as StoreResult, Store},
    types::DeviceKeys,
};

/// The name of the unstable field which marks a device as impersonatable.
pub(crate) const IMPERSONATOR_FIELD: &str = "fi.mau.msc4350.impersonator";

/// A function that tells whether a user is a member of a room.
///
/// MSC4350 only allows a message to be sent on behalf of a ghost user if the
/// impersonating user (the bridge bot) is a member of the room. The crypto
/// crate doesn't track room membership, so an embedding SDK that does has to
/// provide this check with [`OlmMachine::set_impersonator_membership_check`].
/// Without one, no impersonation is accepted.
///
/// [`OlmMachine::set_impersonator_membership_check`]: crate::OlmMachine::set_impersonator_membership_check
pub type MembershipCheck =
    Arc<dyn Fn(OwnedRoomId, OwnedUserId) -> BoxFuture<'static, bool> + Send + Sync>;

/// Does the given device keys object declare an impersonator?
pub(crate) fn is_impersonatable(device_keys: &DeviceKeys) -> bool {
    device_keys.other_field(IMPERSONATOR_FIELD).is_some()
}

fn store_key(user_id: &UserId) -> String {
    format!("experimental-msc4350.impersonatable-devices.{user_id}")
}

/// The impersonatable devices we have seen in `/keys/query` responses, by
/// user.
///
/// They can't be stored as regular [`DeviceData`], because they have no keys of
/// their own, so they are kept in a small cache that is backed by the crypto
/// store's custom values (and therefore persisted by every store
/// implementation without needing a schema change). They are validated lazily,
/// when a message is received, because the impersonator's own device may not be
/// known yet when the response arrives.
#[derive(Clone)]
pub(crate) struct ImpersonatableDevices {
    store: Store,
    cache: Arc<StdRwLock<BTreeMap<OwnedUserId, Arc<Vec<DeviceKeys>>>>>,
    membership_check: Arc<StdRwLock<Option<MembershipCheck>>>,
}

impl std::fmt::Debug for ImpersonatableDevices {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ImpersonatableDevices")
            .field("cache", &self.cache)
            .field("membership_check", &self.membership_check.read().unwrap().is_some())
            .finish_non_exhaustive()
    }
}

impl ImpersonatableDevices {
    pub(crate) fn new(store: Store) -> Self {
        Self { store, cache: Default::default(), membership_check: Default::default() }
    }

    /// Replace the known impersonatable devices of a user. Passing an empty
    /// list will remove all existing devices, akin to how a standard
    /// `/keys/query` response is handled.
    pub(crate) async fn update_user_devices(
        &self,
        user_id: &UserId,
        devices: Vec<DeviceKeys>,
    ) -> StoreResult<()> {
        let key = store_key(user_id);
        if devices.is_empty() {
            // Always check the store, since the cache could be behind.
            if self.store.get_custom_value(&key).await?.is_some() {
                self.store.remove_custom_value(&key).await?;
            }
        } else {
            self.store.set_custom_value(&key, serde_json::to_vec(&devices)?).await?;
        }
        // Cache empty entries too.
        self.cache.write().unwrap().insert(user_id.to_owned(), Arc::new(devices));
        Ok(())
    }

    /// The impersonatable devices we know of for the given user, loading them
    /// from the store the first time they are needed.
    pub(crate) async fn get_user_devices(
        &self,
        user_id: &UserId,
    ) -> StoreResult<Arc<Vec<DeviceKeys>>> {
        if let Some(devices) = self.cache.read().unwrap().get(user_id) {
            return Ok(devices.clone());
        }
        let devices: Vec<DeviceKeys> =
            match self.store.get_custom_value(&store_key(user_id)).await? {
                // A value we can't parse is treated as "nothing known"; the next key query replaces
                // it.
                Some(bytes) => serde_json::from_slice(&bytes).unwrap_or_default(),
                None => Vec::new(),
            };
        // Someone may have written to the store (and hence the cache) since we
        // read it. Use what the cache holds now, or insert the result
        // we read from the store.
        Ok(self
            .cache
            .write()
            .unwrap()
            .entry(user_id.to_owned())
            .or_insert_with(|| Arc::new(devices))
            .clone())
    }

    pub(crate) fn set_membership_check(&self, check: Option<MembershipCheck>) {
        *self.membership_check.write().unwrap() = check;
    }

    pub(crate) fn membership_check(&self) -> Option<MembershipCheck> {
        self.membership_check.read().unwrap().clone()
    }
}

/// Check that `ghost_device` is a well-formed impersonatable device, signed by
/// the already known and validated `impersonator` device.
///
/// This checks that:
/// * `algorithms` and `keys` are empty, as MSC4350 requires,
/// * the embedded impersonator object names `impersonator` and carries exactly
///   its real keys (so the object can't be used to smuggle in other keys), and
/// * `impersonator` signed the device keys object, including the embedded
///   impersonator.
pub(crate) fn is_validly_impersonated_by(
    ghost_device: &DeviceKeys,
    impersonator: &DeviceData,
) -> bool {
    let Some(Value::Object(embedded)) = ghost_device.other_field(IMPERSONATOR_FIELD) else {
        return false;
    };

    if !ghost_device.algorithms.is_empty() || !ghost_device.keys.is_empty() {
        return false;
    }

    let device_id = impersonator.device_id();
    if embedded.get("user_id").and_then(Value::as_str) != Some(impersonator.user_id().as_str())
        || embedded.get("device_id").and_then(Value::as_str) != Some(device_id.as_str())
    {
        return false;
    }

    let (Some(ed25519), Some(curve25519)) =
        (impersonator.ed25519_key(), impersonator.curve25519_key())
    else {
        return false;
    };
    let Some(Value::Object(embedded_keys)) = embedded.get("keys") else {
        return false;
    };
    if embedded_key(embedded_keys, DeviceKeyAlgorithm::Ed25519, device_id)
        != Some(ed25519.to_base64())
        || embedded_key(embedded_keys, DeviceKeyAlgorithm::Curve25519, device_id)
            != Some(curve25519.to_base64())
    {
        return false;
    }

    let key_id = DeviceKeyId::from_parts(DeviceKeyAlgorithm::Ed25519, device_id);
    ed25519.verify_json(impersonator.user_id(), &key_id, ghost_device).is_ok()
}

/// Get the key of the given algorithm for `device_id` out of the `keys` of the
/// impersonator object embedded in an impersonatable device.
fn embedded_key(
    embedded_keys: &Map<String, Value>,
    algorithm: DeviceKeyAlgorithm,
    device_id: &DeviceId,
) -> Option<String> {
    embedded_keys
        .get(DeviceKeyId::from_parts(algorithm, device_id).as_str())
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
}
