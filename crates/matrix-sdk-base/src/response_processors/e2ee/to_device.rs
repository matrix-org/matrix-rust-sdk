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
// limitations under the License.

use std::collections::BTreeMap;

use matrix_sdk_crypto::{store::RoomKeyInfo, EncryptionSyncChanges, OlmMachine};
use ruma::{
    api::client::sync::sync_events::{v3, v5, DeviceLists},
    events::AnyToDeviceEvent,
    serde::Raw,
    OneTimeKeyAlgorithm, UInt,
};

use super::super::Context;
use crate::Result;

/// Process the to-device events and other related e2ee data based on a response
/// from a [MSC4186 request][`v5`].
///
/// This returns a list of all the to-device events that were passed in but
/// encrypted ones were replaced with their decrypted version.
pub async fn from_msc4186(
    context: &mut Context,
    to_device: Option<&v5::response::ToDevice>,
    e2ee: &v5::response::E2EE,
    olm_machine: Option<&OlmMachine>,
) -> Result<Output> {
    process(
        context,
        olm_machine,
        to_device.as_ref().map(|to_device| to_device.events.clone()).unwrap_or_default(),
        &e2ee.device_lists,
        &e2ee.device_one_time_keys_count,
        e2ee.device_unused_fallback_key_types.as_deref(),
        to_device.as_ref().map(|to_device| to_device.next_batch.clone()),
    )
    .await
}

/// Process the to-device events and other related e2ee data based on a response
/// from a [`/v3/sync` request][`v3`].
///
/// This returns a list of all the to-device events that were passed in but
/// encrypted ones were replaced with their decrypted version.
pub async fn from_sync_v2(
    context: &mut Context,
    response: &v3::Response,
    olm_machine: Option<&OlmMachine>,
) -> Result<Output> {
    process(
        context,
        olm_machine,
        response.to_device.events.clone(),
        &response.device_lists,
        &response.device_one_time_keys_count,
        response.device_unused_fallback_key_types.as_deref(),
        Some(response.next_batch.clone()),
    )
    .await
}

/// Process the to-device events and other related e2ee data.
///
/// This returns a list of all the to-device events that were passed in but
/// encrypted ones were replaced with their decrypted version.
async fn process(
    _context: &mut Context,
    olm_machine: Option<&OlmMachine>,
    to_device_events: Vec<Raw<AnyToDeviceEvent>>,
    device_lists: &DeviceLists,
    one_time_keys_counts: &BTreeMap<OneTimeKeyAlgorithm, UInt>,
    unused_fallback_keys: Option<&[OneTimeKeyAlgorithm]>,
    next_batch_token: Option<String>,
) -> Result<Output> {
    let encryption_sync_changes = EncryptionSyncChanges {
        to_device_events,
        changed_devices: device_lists,
        one_time_keys_counts,
        unused_fallback_keys,
        next_batch_token,
    };

    Ok(if let Some(olm_machine) = olm_machine {
        // Let the crypto machine handle the sync response, this
        // decrypts to-device events, but leaves room events alone.
        // This makes sure that we have the decryption keys for the room
        // events at hand.
        let (events, room_key_updates) =
            olm_machine.receive_sync_changes(encryption_sync_changes).await?;

        let events = events
            .iter()
            // TODO: There is loss of information here, after calling `to_raw` it is not
            // possible to make the difference between a successfully decrypted event and a plain
            // text event. This information needs to be propagated to top layer at some point if
            // clients relies on custom encrypted to device events.
            .map(|p| p.to_raw())
            .collect();

        Output { decrypted_to_device_events: events, room_key_updates: Some(room_key_updates) }
    } else {
        // If we have no `OlmMachine`, just return the events that were passed in.
        // This should not happen unless we forget to set things up by calling
        // `Self::activate()`.
        Output {
            decrypted_to_device_events: encryption_sync_changes.to_device_events,
            room_key_updates: None,
        }
    })
}

pub struct Output {
    pub decrypted_to_device_events: Vec<Raw<AnyToDeviceEvent>>,
    pub room_key_updates: Option<Vec<RoomKeyInfo>>,
}
