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
    api::client::sync::sync_events::DeviceLists, events::AnyToDeviceEvent, serde::Raw,
    OneTimeKeyAlgorithm, UInt,
};

use super::Context;
use crate::Result;

/// Process the to-device events and other related e2ee data. This returns a
/// list of all the to-device events that were passed in but encrypted ones
/// were replaced with their decrypted version.
pub async fn e2ee(
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
