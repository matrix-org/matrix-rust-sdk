use assert_matches2::assert_matches;
use futures_util::FutureExt;
use matrix_sdk::{
    Client,
    encryption::VerificationState,
    test_utils::{logged_in_client_with_server, mocks::MatrixMockServer},
};
use matrix_sdk_test::async_test;
use ruma::{owned_device_id, owned_user_id, user_id};
use serde_json::json;
use wiremock::{
    Mock, ResponseTemplate,
    matchers::{body_json, method, path},
};

async fn bootstrap_cross_signing(client: &Client) {
    client.encryption().bootstrap_cross_signing(None).await.unwrap();

    let status = client.encryption().cross_signing_status().await.unwrap();
    assert!(status.is_complete());
}

#[async_test]
async fn test_own_verification() {
    let server = MatrixMockServer::new().await;
    server.mock_crypto_endpoints_preset().await;

    let user_id = owned_user_id!("@alice:example.org");
    let device_id = owned_device_id!("4L1C3");
    let alice = server.client_builder_for_crypto_end_to_end(&user_id, &device_id).build().await;
    // Subscribe to verification state updates
    let mut verification_state_subscriber = alice.encryption().verification_state();
    assert_eq!(alice.encryption().verification_state().get(), VerificationState::Unknown);

    // Have Alice bootstrap cross-signing.
    bootstrap_cross_signing(&alice).await;

    // The local device is considered verified by default, we need a keys query to
    // run
    let own_device = alice.encryption().get_device(&user_id, &device_id).await.unwrap().unwrap();
    assert!(own_device.is_verified());
    assert!(!own_device.is_deleted());

    // The device is not considered cross signed yet
    assert_eq!(
        verification_state_subscriber.next().now_or_never().flatten().unwrap(),
        VerificationState::Unverified
    );
    assert_eq!(alice.encryption().verification_state().get(), VerificationState::Unverified);

    // Manually re-verifying doesn't change the outcome.
    own_device.verify().await.unwrap();
    assert!(own_device.is_verified());

    // Bootstrapping signed the user identity we created.
    let user_identity = alice.encryption().get_user_identity(&user_id).await.unwrap().unwrap();

    assert_eq!(user_identity.user_id(), user_id);
    assert!(user_identity.is_verified());

    let master_pub_key = user_identity.master_key();
    assert_eq!(master_pub_key.user_id(), user_id);
    assert!(!master_pub_key.keys().is_empty());
    assert_eq!(master_pub_key.keys().iter().count(), 1);

    // Manually re-verifying doesn't change the outcome.
    user_identity.verify().await.unwrap();
    assert!(user_identity.is_verified());

    // Force a keys query to pick up the cross-signing state
    server
        .mock_sync()
        .ok_and_run(&alice, |builder| {
            builder.add_change_device(&user_id);
        })
        .await;

    // The device should now be cross-signed
    assert_eq!(
        verification_state_subscriber.next().now_or_never().unwrap().unwrap(),
        VerificationState::Verified
    );
    assert_eq!(alice.encryption().verification_state().get(), VerificationState::Verified);
}

#[async_test]
async fn test_reset_cross_signing_resets_verification() {
    let server = MatrixMockServer::new().await;
    server.mock_crypto_endpoints_preset().await;

    let user_id = owned_user_id!("@alice:example.org");
    let device_id = owned_device_id!("4L1C3");
    let alice = server.client_builder_for_crypto_end_to_end(&user_id, &device_id).build().await;

    // Subscribe to verification state updates
    let mut verification_state_subscriber = alice.encryption().verification_state();
    assert_eq!(alice.encryption().verification_state().get(), VerificationState::Unknown);

    // Have Alice bootstrap cross-signing.
    bootstrap_cross_signing(&alice).await;

    // The device is not considered cross signed yet
    assert_eq!(
        verification_state_subscriber.next().await.unwrap_or(VerificationState::Unknown),
        VerificationState::Unverified
    );
    assert_eq!(alice.encryption().verification_state().get(), VerificationState::Unverified);

    // Force a keys query to pick up the cross-signing state
    server
        .mock_sync()
        .ok_and_run(&alice, |builder| {
            builder.add_change_device(&user_id);
        })
        .await;

    // The device should now be cross-signed
    assert_eq!(
        verification_state_subscriber.next().now_or_never().unwrap().unwrap(),
        VerificationState::Verified
    );
    assert_eq!(alice.encryption().verification_state().get(), VerificationState::Verified);

    let device_id = owned_device_id!("AliceDevice2");
    let alice2 = server.client_builder_for_crypto_end_to_end(&user_id, &device_id).build().await;

    // Have Alice bootstrap cross-signing again, this time on her second device.
    bootstrap_cross_signing(&alice2).await;

    server
        .mock_sync()
        .ok_and_run(&alice, |builder| {
            builder.add_change_device(&user_id);
        })
        .await;

    // The device shouldn't be cross-signed anymore.
    assert_eq!(alice.encryption().verification_state().get(), VerificationState::Unverified);
    assert_eq!(
        verification_state_subscriber.next().now_or_never().unwrap().unwrap(),
        VerificationState::Unverified
    );
}

#[async_test]
async fn test_unchecked_mutual_verification() {
    let server = MatrixMockServer::new().await;
    server.mock_crypto_endpoints_preset().await;

    let user_id = owned_user_id!("@alice:example.org");
    let device_id = owned_device_id!("4L1C3");
    let alice = server.client_builder_for_crypto_end_to_end(&user_id, &device_id).build().await;

    let bob_user_id = owned_user_id!("@bob:example.org");
    let bob_device_id = owned_device_id!("B0B0B0B0B");

    let bob =
        server.client_builder_for_crypto_end_to_end(&bob_user_id, &bob_device_id).build().await;

    let alice_verifies_bob =
        alice.encryption().get_verification(bob.user_id().unwrap(), "flow_id").await;
    assert!(alice_verifies_bob.is_none());

    let alice_verifies_bob_request =
        alice.encryption().get_verification_request(&bob_user_id, "flow_id").await;
    assert!(alice_verifies_bob_request.is_none());

    let alice_bob_device =
        alice.encryption().get_device(&bob_user_id, &bob_device_id).await.unwrap();
    assert!(alice_bob_device.is_none());

    // Have both Alice and Bob bootstrap cross-signing.
    bootstrap_cross_signing(&alice).await;
    bootstrap_cross_signing(&bob).await;

    // Have Alice and Bob upload their signed device keys.
    server.mock_sync().ok_and_run(&alice, |_builder| {}).await;
    server.mock_sync().ok_and_run(&bob, |_builder| {}).await;

    // Have Alice track Bob, so she queries his keys later.
    {
        let alice_olm = alice.olm_machine_for_testing().await;
        let alice_olm = alice_olm.as_ref().unwrap();
        alice_olm.update_tracked_users([bob_user_id.as_ref()]).await.unwrap();
    }

    // Run a sync so we do send outgoing requests, including the /keys/query for
    // getting bob's identity.
    server.mock_sync().ok_and_run(&alice, |_builder| {}).await;

    // From the point of view of Alice, Bob now has a device.
    let alice_bob_device = alice
        .encryption()
        .get_device(&bob_user_id, &bob_device_id)
        .await
        .unwrap()
        .expect("alice sees bob's device");

    assert!(!alice_bob_device.is_verified());
    assert!(!alice_bob_device.is_deleted());
    assert!(alice_bob_device.verify().await.is_err(), "can't sign the device of another user");

    let alice_bob_ident = alice
        .encryption()
        .get_user_identity(&bob_user_id)
        .await
        .unwrap()
        .expect("alice sees bob's identity");

    alice_bob_ident.verify().await.unwrap();

    // Notify Alice's devices that some identify changed, so it does another
    // /keys/query request.
    server
        .mock_sync()
        .ok_and_run(&alice, |builder| {
            builder.add_change_device(&bob_user_id);
        })
        .await;

    let alice_bob_ident = alice
        .encryption()
        .get_user_identity(&bob_user_id)
        .await
        .unwrap()
        .expect("alice sees bob's identity");
    assert_eq!(alice_bob_ident.user_id(), bob_user_id);
    assert!(alice_bob_ident.is_verified());

    let master_pub_key = alice_bob_ident.master_key();
    assert_eq!(master_pub_key.user_id(), bob_user_id);
    assert!(!master_pub_key.keys().is_empty());
    assert_eq!(master_pub_key.keys().iter().count(), 1);

    let alice_bob_device = alice
        .encryption()
        .get_device(&bob_user_id, &bob_device_id)
        .await
        .unwrap()
        .expect("alice sees bob's device");
    assert!(alice_bob_device.is_verified());
    assert!(alice_bob_device.is_verified_with_cross_signing());
}

#[async_test]
async fn test_request_user_identity() {
    let (client, server) = logged_in_client_with_server().await;
    let bob_id = user_id!("@bob:example.org");

    Mock::given(method("POST"))
        .and(path("/_matrix/client/r0/keys/query"))
        .and(body_json(json!({ "device_keys": { bob_id: []}})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "failures": {},
            "device_keys": {
                "@bob:example.org": {
                    "B0B0B0B0B": {
                        "user_id": "@bob:example.org",
                        "device_id": "B0B0B0B0B",
                        "algorithms": [
                            "m.olm.v1.curve25519-aes-sha2",
                            "m.megolm.v1.aes-sha2"
                        ],
                        "keys": {
                            "curve25519:B0B0B0B0B": "I3YsPwqMZQXHkSQbjFNEs7b529uac2xBpI83eN3LUXo",
                            "ed25519:B0B0B0B0B": "qzdW3F5IMPFl0HQgz5w/L5Oi/npKUFn8Um84acIHfPY"
                        },
                        "signatures": {
                            "@bob:example.org": {
                                "ed25519:5JpU6BNHsBZbf4Y3t0IvpIWa7kKDSGy3b+DjVjUuJmc": "MpU1mqNNWymS2mWYsBH0HFxizIVWDgTRmh+qXzXZVdD0dvhwyLKaUAZF/jrbrdyPvjikBtRQGRAk/hhj7DOjDg",
                                "ed25519:B0B0B0B0B": "jU36UPWk8rrCOUR+v1MN7CsjThmFn4doNR5rFEO2fTERku4zLpAR9oG3OLgRcs+L1Vc8Hqm5++wv9bYuJhp2Bg"
                            }
                        }
                    },
                },
            },
            "master_keys": {
                "@bob:example.org": {
                    "user_id": "@bob:example.org",
                    "usage": ["master"],
                    "keys": {
                        "ed25519:3NZwYz0VjFrhONhYT2iBWCYdEYF266jn/vmZqc6QdDU": "3NZwYz0VjFrhONhYT2iBWCYdEYF266jn/vmZqc6QdDU"
                    },
                    "signatures": {
                        "@bob:example.org": {
                            "ed25519:3NZwYz0VjFrhONhYT2iBWCYdEYF266jn/vmZqc6QdDU": "TCnq8/vy6lp56cF5J9PmsqTnLjKcvsNYN7qFpD6isYWmEFLFBfml8B2ceBzlu9NLwi0xT9jpQV7SYRQt4ZnICQ",
                            "ed25519:B0B0B0B0B": "yCQFDN+1sJQ+qhqfubOnmPOu/agHT8k17SaD886QmVDwEXCeFFDSZKY29oBDaCRJZJ2BvE2WSK+GACXv5t2FDw"
                        }
                    }
                },
            },
            "self_signing_keys": {
                "@bob:example.org": {
                    "user_id": "@bob:example.org",
                    "usage": ["self_signing"],
                    "keys": {
                        "ed25519:5JpU6BNHsBZbf4Y3t0IvpIWa7kKDSGy3b+DjVjUuJmc": "5JpU6BNHsBZbf4Y3t0IvpIWa7kKDSGy3b+DjVjUuJmc"
                    },
                    "signatures": {
                        "@bob:example.org": {
                            "ed25519:3NZwYz0VjFrhONhYT2iBWCYdEYF266jn/vmZqc6QdDU": "yre3bDdzSYNQweNRRB0BXSaaM8n9IA2puathxXSDGebyF6Bh1+Kd6Q8/tl271LwM4Wdar4vgPwrgExW7k2hLBg"
                        }
                    }
                },
            },
            "user_signing_keys": {
                "@bob:example.org": {
                    "user_id": "@bob:example.org",
                    "usage": ["user_signing"],
                    "keys": {
                        "ed25519:JIKwmV4DJMn4/OY/WuNQrJpNOT8zJSq7fWebLdBq4E8": "JIKwmV4DJMn4/OY/WuNQrJpNOT8zJSq7fWebLdBq4E8"
                    },
                    "signatures": {
                        "@bob:example.org": {
                            "ed25519:3NZwYz0VjFrhONhYT2iBWCYdEYF266jn/vmZqc6QdDU": "pd0MbRNyh9riLeA9yqEBo+Dk0TUVkdrxyEqkwExKEsP5e3LhPAd6t6f9g7fZv68rxUWjJ2lDbw2xRu3SJghaDA"
                        }
                    }
                },
            },
        })))
        .expect(1)
        .mount(&server)
        .await;

    let encryption = client.encryption();

    // We don't have Bob's identity yet.
    assert_matches!(encryption.get_user_identity(bob_id).await, Ok(None));

    assert_matches!(encryption.request_user_identity(bob_id).await, Ok(Some(_)));
    assert_matches!(encryption.get_user_identity(bob_id).await, Ok(Some(_)));
}
