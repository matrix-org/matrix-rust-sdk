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

use assert_matches2::assert_matches;
use matrix_sdk_test::async_test;
use ruma::to_device::DeviceIdOrAllDevices;
use serde_json::{json, value::to_raw_value};

use crate::{
    machine::{
        test_helpers::{get_machine_pair, get_machine_pair_with_session},
        tests,
    },
    types::events::ToDeviceEvent,
    utilities::json_convert,
    EncryptionSyncChanges, OlmError, ToDeviceRequest,
};

#[async_test]
async fn test_send_encrypted_to_device() {
    let (alice, bob) =
        get_machine_pair_with_session(tests::alice_id(), tests::user_id(), false).await;

    let custom_event_type = "m.new_device";

    let custom_content = json!({
            "device_id": "XYZABCDE",
            "rooms": ["!726s6s6q:example.com"]
    });

    let device = alice.get_device(bob.user_id(), bob.device_id(), None).await.unwrap().unwrap();
    let raw_encrypted = device
        .encrypt_event_raw(custom_event_type, &custom_content)
        .await
        .expect("Should have encryted the content");

    let request = ToDeviceRequest::new(
        bob.user_id(),
        DeviceIdOrAllDevices::DeviceId(tests::bob_device_id().to_owned()),
        "m.room.encrypted",
        raw_encrypted.cast(),
    );

    assert_eq!("m.room.encrypted", request.event_type.to_string());

    let messages = &request.messages;
    assert_eq!(1, messages.len());
    assert!(messages.get(bob.user_id()).is_some());
    let target_devices = messages.get(bob.user_id()).unwrap();
    assert_eq!(1, target_devices.len());
    assert!(target_devices
        .get(&DeviceIdOrAllDevices::DeviceId(tests::bob_device_id().to_owned()))
        .is_some());

    let event = ToDeviceEvent::new(
        alice.user_id().to_owned(),
        tests::to_device_requests_to_content(vec![request.clone().into()]),
    );

    let event = json_convert(&event).unwrap();

    let sync_changes = EncryptionSyncChanges {
        to_device_events: vec![event],
        changed_devices: &Default::default(),
        one_time_keys_counts: &Default::default(),
        unused_fallback_keys: None,
        next_batch_token: None,
    };

    let (decrypted, _) = bob.receive_sync_changes(sync_changes).await.unwrap();

    assert_eq!(1, decrypted.len());

    let decrypted_event = decrypted[0].deserialize().unwrap();

    assert_eq!(decrypted_event.event_type().to_string(), custom_event_type.to_owned());

    let decrypted_value = to_raw_value(&decrypted[0]).unwrap();
    let decrypted_value = serde_json::to_value(decrypted_value).unwrap();

    assert_eq!(
        decrypted_value.get("content").unwrap().get("device_id").unwrap().as_str().unwrap(),
        custom_content.get("device_id").unwrap().as_str().unwrap(),
    );

    assert_eq!(
        decrypted_value.get("content").unwrap().get("rooms").unwrap().as_array().unwrap(),
        custom_content.get("rooms").unwrap().as_array().unwrap(),
    );
}

#[async_test]
async fn test_send_encrypted_to_device_no_session() {
    let (alice, bob, _) = get_machine_pair(tests::alice_id(), tests::user_id(), false).await;

    let custom_event_type = "m.new_device";

    let custom_content = json!({
            "device_id": "XYZABCDE",
            "rooms": ["!726s6s6q:example.com"]
    });

    let encryption_result = alice
        .get_device(bob.user_id(), tests::bob_device_id(), None)
        .await
        .unwrap()
        .unwrap()
        .encrypt_event_raw(custom_event_type, &custom_content)
        .await;

    assert_matches!(encryption_result, Err(OlmError::MissingSession));
}
