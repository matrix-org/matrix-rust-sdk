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

use matrix_sdk_common::deserialized_responses::{
    ProcessedToDeviceEvent, ToDeviceUnableToDecryptInfo, ToDeviceUnableToDecryptReason,
};
use matrix_sdk_crypto::{DecryptionSettings, EncryptionSyncChanges, OlmMachine};
use ruma::{
    OneTimeKeyAlgorithm, UInt,
    api::client::sync::sync_events::{DeviceLists, v3, v5},
    events::AnyToDeviceEvent,
    serde::Raw,
};

use crate::Result;

/// Process the to-device events and other related e2ee data based on a response
/// from a [MSC4186 request][`v5`].
///
/// This returns a list of all the to-device events that were passed in but
/// encrypted ones were replaced with their decrypted version.
pub async fn from_msc4186(
    to_device: Option<&v5::response::ToDevice>,
    e2ee: &v5::response::E2EE,
    olm_machine: Option<&OlmMachine>,
    decryption_settings: &DecryptionSettings,
) -> Result<Output> {
    process(
        olm_machine,
        to_device.as_ref().map(|to_device| to_device.events.clone()).unwrap_or_default(),
        &e2ee.device_lists,
        &e2ee.device_one_time_keys_count,
        e2ee.device_unused_fallback_key_types.as_deref(),
        to_device.as_ref().map(|to_device| to_device.next_batch.clone()),
        decryption_settings,
    )
    .await
}

/// Process the to-device events and other related e2ee data based on a response
/// from a [`/v3/sync` request][`v3`].
///
/// This returns a list of all the to-device events that were passed in but
/// encrypted ones were replaced with their decrypted version.
pub async fn from_sync_v2(
    response: &v3::Response,
    olm_machine: Option<&OlmMachine>,
    decryption_settings: &DecryptionSettings,
) -> Result<Output> {
    process(
        olm_machine,
        response.to_device.events.clone(),
        &response.device_lists,
        &response.device_one_time_keys_count,
        response.device_unused_fallback_key_types.as_deref(),
        Some(response.next_batch.clone()),
        decryption_settings,
    )
    .await
}

/// Process the to-device events and other related e2ee data.
///
/// This returns a list of all the to-device events that were passed in but
/// encrypted ones were replaced with their decrypted version.
async fn process(
    olm_machine: Option<&OlmMachine>,
    to_device_events: Vec<Raw<AnyToDeviceEvent>>,
    device_lists: &DeviceLists,
    one_time_keys_counts: &BTreeMap<OneTimeKeyAlgorithm, UInt>,
    unused_fallback_keys: Option<&[OneTimeKeyAlgorithm]>,
    next_batch_token: Option<String>,
    decryption_settings: &DecryptionSettings,
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
        let (events, _room_key_updates) =
            olm_machine.receive_sync_changes(encryption_sync_changes, decryption_settings).await?;

        Output { processed_to_device_events: events }
    } else {
        // If we have no `OlmMachine`, just return the clear events that were passed in.
        // The encrypted ones are dropped as they are un-usable.
        // This should not happen unless we forget to set things up by calling
        // `Self::activate()`.
        Output {
            processed_to_device_events: encryption_sync_changes
                .to_device_events
                .into_iter()
                .map(|raw| {
                    if let Ok(Some(event_type)) = raw.get_field::<String>("type") {
                        if event_type == "m.room.encrypted" {
                            ProcessedToDeviceEvent::UnableToDecrypt {
                                encrypted_event: raw,
                                utd_info: ToDeviceUnableToDecryptInfo {
                                    reason: ToDeviceUnableToDecryptReason::NoOlmMachine,
                                },
                            }
                        } else {
                            ProcessedToDeviceEvent::PlainText(raw)
                        }
                    } else {
                        // Exclude events with no type
                        ProcessedToDeviceEvent::Invalid(raw)
                    }
                })
                .collect(),
        }
    })
}

pub struct Output {
    pub processed_to_device_events: Vec<ProcessedToDeviceEvent>,
}
