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

use matrix_sdk_test::async_test;
use ruma::{
    api::client::to_device::send_event_to_device::v3::Response as ToDeviceResponse,
    events::key::verification::VerificationMethod,
};

use crate::{
    machine::{test_helpers::get_machine_pair_with_setup_sessions_test_helper, tests},
    verification::tests::{outgoing_request_to_event, request_to_event},
};

#[async_test]
async fn test_interactive_verification() {
    let (alice, bob) = get_machine_pair_with_setup_sessions_test_helper(
        tests::alice_id(),
        tests::user_id(),
        false,
    )
    .await;

    let bob_device = alice.get_device(bob.user_id(), bob.device_id(), None).await.unwrap().unwrap();

    assert!(!bob_device.is_verified());

    let (alice_sas, request) = bob_device.start_verification().await.unwrap();

    let event = request_to_event(alice.user_id(), &request.into());
    bob.handle_verification_event(&event).await;

    let bob_sas = bob
        .get_verification(alice.user_id(), alice_sas.flow_id().as_str())
        .unwrap()
        .sas_v1()
        .unwrap();

    assert!(alice_sas.emoji().is_none());
    assert!(bob_sas.emoji().is_none());

    let event = bob_sas.accept().map(|r| request_to_event(bob.user_id(), &r)).unwrap();

    alice.handle_verification_event(&event).await;

    let (event, request_id) = alice
        .inner
        .verification_machine
        .outgoing_messages()
        .first()
        .map(|r| (outgoing_request_to_event(alice.user_id(), r), r.request_id.to_owned()))
        .unwrap();
    alice.mark_request_as_sent(&request_id, &ToDeviceResponse::new()).await.unwrap();
    bob.handle_verification_event(&event).await;

    let (event, request_id) = bob
        .inner
        .verification_machine
        .outgoing_messages()
        .first()
        .map(|r| (outgoing_request_to_event(bob.user_id(), r), r.request_id.to_owned()))
        .unwrap();
    alice.handle_verification_event(&event).await;
    bob.mark_request_as_sent(&request_id, &ToDeviceResponse::new()).await.unwrap();

    assert!(alice_sas.emoji().is_some());
    assert!(bob_sas.emoji().is_some());

    assert_eq!(alice_sas.emoji(), bob_sas.emoji());
    assert_eq!(alice_sas.decimals(), bob_sas.decimals());

    let contents = bob_sas.confirm().await.unwrap().0;
    assert!(contents.len() == 1);
    let event = request_to_event(bob.user_id(), &contents[0]);
    alice.handle_verification_event(&event).await;

    assert!(!alice_sas.is_done());
    assert!(!bob_sas.is_done());

    let contents = alice_sas.confirm().await.unwrap().0;
    assert!(contents.len() == 1);
    let event = request_to_event(alice.user_id(), &contents[0]);

    assert!(alice_sas.is_done());
    assert!(bob_device.is_verified());

    let alice_device =
        bob.get_device(alice.user_id(), alice.device_id(), None).await.unwrap().unwrap();

    assert!(!alice_device.is_verified());
    bob.handle_verification_event(&event).await;
    assert!(bob_sas.is_done());
    assert!(alice_device.is_verified());
}

#[async_test]
async fn test_interactive_verification_started_from_request() {
    let (alice, bob) = get_machine_pair_with_setup_sessions_test_helper(
        tests::alice_id(),
        tests::user_id(),
        false,
    )
    .await;

    // ----------------------------------------------------------------------------
    // On Alice's device:
    let bob_device = alice.get_device(bob.user_id(), bob.device_id(), None).await.unwrap().unwrap();

    assert!(!bob_device.is_verified());

    // Alice sends a verification request with her desired methods to Bob
    let (alice_ver_req, request) =
        bob_device.request_verification_with_methods(vec![VerificationMethod::SasV1]);

    // ----------------------------------------------------------------------------
    // On Bobs's device:
    let event = request_to_event(alice.user_id(), &request);
    bob.handle_verification_event(&event).await;
    let flow_id = alice_ver_req.flow_id().as_str();

    let verification_request = bob.get_verification_request(alice.user_id(), flow_id).unwrap();

    // Bob accepts the request, sending a Ready request
    let accept_request =
        verification_request.accept_with_methods(vec![VerificationMethod::SasV1]).unwrap();
    // And also immediately sends a start request
    let (_, start_request_from_bob) = verification_request.start_sas().await.unwrap().unwrap();

    // ----------------------------------------------------------------------------
    // On Alice's device:

    // Alice receives the Ready
    let event = request_to_event(bob.user_id(), &accept_request);
    alice.handle_verification_event(&event).await;

    let verification_request = alice.get_verification_request(bob.user_id(), flow_id).unwrap();

    // And also immediately sends a start request
    let (alice_sas, start_request_from_alice) =
        verification_request.start_sas().await.unwrap().unwrap();

    // Now alice receives Bob's start:
    let event = request_to_event(bob.user_id(), &start_request_from_bob);
    alice.handle_verification_event(&event).await;

    // Since Alice's user id is lexicographically smaller than Bob's, Alice does not
    // do anything with the request, however.
    assert!(alice.user_id() < bob.user_id());

    // ----------------------------------------------------------------------------
    // On Bob's device:

    // Bob receives Alice's start:
    let event = request_to_event(alice.user_id(), &start_request_from_alice);
    bob.handle_verification_event(&event).await;

    let bob_sas = bob
        .get_verification(alice.user_id(), alice_sas.flow_id().as_str())
        .unwrap()
        .sas_v1()
        .unwrap();

    assert!(alice_sas.emoji().is_none());
    assert!(bob_sas.emoji().is_none());

    // ... and accepts it
    let event = bob_sas.accept().map(|r| request_to_event(bob.user_id(), &r)).unwrap();

    // ----------------------------------------------------------------------------
    // On Alice's device:

    // Alice receives the Accept request:
    alice.handle_verification_event(&event).await;

    // Alice sends a key
    let msgs = alice.inner.verification_machine.outgoing_messages();
    assert!(msgs.len() == 1);
    let msg = &msgs[0];
    let event = outgoing_request_to_event(alice.user_id(), msg);
    alice.inner.verification_machine.mark_request_as_sent(&msg.request_id);

    // ----------------------------------------------------------------------------
    // On Bob's device:

    // And bob receive's it:
    bob.handle_verification_event(&event).await;

    // Now bob sends a key
    let msgs = bob.inner.verification_machine.outgoing_messages();
    assert!(msgs.len() == 1);
    let msg = &msgs[0];
    let event = outgoing_request_to_event(bob.user_id(), msg);
    bob.inner.verification_machine.mark_request_as_sent(&msg.request_id);

    // ----------------------------------------------------------------------------
    // On Alice's device:

    // And alice receives it
    alice.handle_verification_event(&event).await;

    // As a result, both devices now can show emojis/decimals
    assert!(alice_sas.emoji().is_some());
    assert!(bob_sas.emoji().is_some());

    // ----------------------------------------------------------------------------
    // On Bob's device:

    assert_eq!(alice_sas.emoji(), bob_sas.emoji());
    assert_eq!(alice_sas.decimals(), bob_sas.decimals());

    // Bob first confirms that the emojis match and sends the MAC...
    let contents = bob_sas.confirm().await.unwrap().0;
    assert!(contents.len() == 1);
    let event = request_to_event(bob.user_id(), &contents[0]);

    // ----------------------------------------------------------------------------
    // On Alice's device:

    // ...which alice receives
    alice.handle_verification_event(&event).await;

    assert!(!alice_sas.is_done());
    assert!(!bob_sas.is_done());

    // Now alice confirms that the emojis match and sends...
    let contents = alice_sas.confirm().await.unwrap().0;
    assert!(contents.len() == 2);
    // ... her own MAC...
    let event_mac = request_to_event(alice.user_id(), &contents[0]);
    // ... and a Done message
    let event_done = request_to_event(alice.user_id(), &contents[1]);

    // ----------------------------------------------------------------------------
    // On Bob's device:

    // Bob receives the MAC message
    bob.handle_verification_event(&event_mac).await;

    // Bob verifies that the MAC is valid and also sends a "done" message.
    let msgs = bob.inner.verification_machine.outgoing_messages();
    eprintln!("{msgs:?}");
    assert!(msgs.len() == 1);
    let event = msgs.first().map(|r| outgoing_request_to_event(bob.user_id(), r)).unwrap();

    let alice_device =
        bob.get_device(alice.user_id(), alice.device_id(), None).await.unwrap().unwrap();

    assert!(!bob_sas.is_done());
    assert!(!alice_device.is_verified());
    // And Bob receives the Done message of alice.
    bob.handle_verification_event(&event_done).await;

    assert!(bob_sas.is_done());
    assert!(alice_device.is_verified());

    // ----------------------------------------------------------------------------
    // On Alice's device:

    assert!(!alice_sas.is_done());
    assert!(!bob_device.is_verified());
    // Alices receives the done message
    eprintln!("{event:?}");
    alice.handle_verification_event(&event).await;

    assert!(alice_sas.is_done());
    assert!(bob_device.is_verified());
}
