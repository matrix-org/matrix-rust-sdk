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

use ruma::{DeviceKeyAlgorithm, DeviceKeyId, OwnedUserId, UserId};
use serde_json::Value;

use super::DeviceData;
use crate::{olm::VerifyJson, types::DeviceKeys};

/// The name of the unstable field which marks a device as impersonatable.
pub(crate) const IMPERSONATOR_FIELD: &str = "fi.mau.msc4350.impersonator";

/// Does the given device keys object declare an impersonator?
pub(crate) fn is_impersonatable(device_keys: &DeviceKeys) -> bool {
    device_keys.other_field(IMPERSONATOR_FIELD).is_some()
}

/// The impersonatable devices we have seen in `/keys/query` responses, by
/// user. They are validated lazily, when a message is received, because the
/// impersonator's own device may not be known yet when the response arrives.
///
/// The set is kept in memory only: it is rebuilt by the next key query after a
/// restart.
#[derive(Clone, Debug, Default)]
pub(crate) struct ImpersonatableDevices {
    inner: Arc<StdRwLock<BTreeMap<OwnedUserId, Vec<DeviceKeys>>>>,
}

impl ImpersonatableDevices {
    /// Replace the known impersonatable devices of a user. A key query
    /// response always lists all of a user's devices, so an empty list means
    /// the user has none any more.
    pub(crate) fn replace_for_user(&self, user_id: &UserId, devices: Vec<DeviceKeys>) {
        let mut inner = self.inner.write().unwrap();
        if devices.is_empty() {
            inner.remove(user_id);
        } else {
            inner.insert(user_id.to_owned(), devices);
        }
    }

    /// The impersonatable devices we know of for the given user.
    pub(crate) fn for_user(&self, user_id: &UserId) -> Vec<DeviceKeys> {
        self.inner.read().unwrap().get(user_id).cloned().unwrap_or_default()
    }
}

/// Check that `ghost_device` is a well-formed impersonatable device, signed by
/// `impersonator`, which is a device we already know and validated.
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
    let embedded_key = |algorithm: DeviceKeyAlgorithm| {
        embedded_keys
            .get(DeviceKeyId::from_parts(algorithm, device_id).as_str())
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)
    };
    if embedded_key(DeviceKeyAlgorithm::Ed25519) != Some(ed25519.to_base64())
        || embedded_key(DeviceKeyAlgorithm::Curve25519) != Some(curve25519.to_base64())
    {
        return false;
    }

    let key_id = DeviceKeyId::from_parts(DeviceKeyAlgorithm::Ed25519, device_id);
    ed25519.verify_json(impersonator.user_id(), &key_id, ghost_device).is_ok()
}
