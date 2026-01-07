use std::{
    collections::{BTreeMap, HashSet},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

use futures_util::{StreamExt as _, pin_mut};
use matrix_sdk::{
    config::RequestConfig,
    test_utils::{
        client::mock_matrix_session, logged_in_client_with_server, test_client_builder_with_server,
    },
};
use matrix_sdk_base::crypto::store::types::Changes;
use matrix_sdk_test::async_test;
use matrix_sdk_ui::encryption_sync_service::{
    EncryptionSyncPermit, EncryptionSyncService, WithLocking,
};
use serde::Deserialize;
use serde_json::json;
use tokio::sync::Mutex as AsyncMutex;
use tracing::{error, info, trace, warn};
use wiremock::{
    Mock, MockGuard, MockServer, Request, ResponseTemplate,
    matchers::{method, path},
};

use crate::{
    mock_sync,
    sliding_sync::{PartialSlidingSyncRequest, SlidingSyncMatcher, check_requests},
    sliding_sync_then_assert_request_and_fake_response,
};

#[async_test]
async fn test_smoke_encryption_sync_works() -> anyhow::Result<()> {
    let (client, server) = logged_in_client_with_server().await;

    let sync_permit = Arc::new(AsyncMutex::new(EncryptionSyncPermit::new_for_testing()));
    let sync_permit_guard = sync_permit.clone().lock_owned().await;
    let encryption_sync = EncryptionSyncService::new(client, None, WithLocking::Yes).await?;

    let stream = encryption_sync.sync(sync_permit_guard);
    pin_mut!(stream);

    // Requests enable the e2ee and to_device extensions on the first run.
    sliding_sync_then_assert_request_and_fake_response! {
        [server, stream]
        assert request >= {
            "conn_id": "encryption",
            "extensions": {
                "e2ee": {
                    "enabled": true,
                },
                "to_device": {
                    "enabled": true,
                }
            }
        },
        respond with = {
            "pos": "0"
        },
    };

    // The request then passes the `pos`ition marker to the next request, as usual
    // in sliding sync.
    sliding_sync_then_assert_request_and_fake_response! {
        [server, stream]
        assert request >= {
            "conn_id": "encryption",
            "extensions": {
                "e2ee": {
                    "enabled": true,
                },
                "to_device": {
                    "enabled": true,
                }
            }
        },
        respond with = {
            "pos": "1",
            "extensions": {
                "to_device": {
                    "next_batch": "nb0",
                }
            }
        },
    };

    // The to-device since token is passed from the previous request.
    sliding_sync_then_assert_request_and_fake_response! {
        [server, stream]
        assert request >= {
            "conn_id": "encryption",
            "extensions": {
                "to_device": {
                    "enabled": true,
                    "since": "nb0",
                }
            }
        },
        respond with = {
            "pos": "2",
            "extensions": {
                "to_device": {
                    "next_batch": "nb1"
                }
            }
        },
    };

    // The to-device since token is passed from the previous request.
    sliding_sync_then_assert_request_and_fake_response! {
        [server, stream]
        sync matches Some(Err(_)),
        assert request >= {
            "conn_id": "encryption",
            "extensions": {
                "to_device": {
                    "enabled": true,
                    "since": "nb1",
                }
            }
        },
        respond with = (code 400) {
            "error": "foo",
            "errcode": "M_UNKNOWN_POS",
        },
    };

    // The stream will stop, as it ran into an error.
    assert!(stream.next().await.is_none());

    // Start a new sync.
    let sync_permit_guard = sync_permit.clone().lock_owned().await;
    let stream = encryption_sync.sync(sync_permit_guard);
    pin_mut!(stream);

    // The next request will contain extensions again.
    sliding_sync_then_assert_request_and_fake_response! {
        [server, stream]
        assert request >= {
            "conn_id": "encryption",
            "extensions": {
                "to_device": {
                    "enabled": true,
                    "since": "nb1"
                }
            }
        },
        respond with = {
            "pos": "a"
        },
    };

    Ok(())
}

async fn setup_mocking_sliding_sync_server(server: &MockServer) -> MockGuard {
    let pos = Mutex::new(0);

    Mock::given(SlidingSyncMatcher)
        .respond_with(move |request: &Request| {
            let partial_request: PartialSlidingSyncRequest = request.body_json().unwrap();
            // Repeat the transaction id in the response, to validate sticky parameters.
            let mut pos = pos.lock().unwrap();
            *pos += 1;
            let pos_as_str = (*pos).to_string();
            ResponseTemplate::new(200).set_body_json(json!({
                "txn_id": partial_request.txn_id,
                "pos": pos_as_str
            }))
        })
        .mount_as_scoped(server)
        .await
}

#[async_test]
async fn test_encryption_sync_one_fixed_iteration() -> anyhow::Result<()> {
    let (client, server) = logged_in_client_with_server().await;

    let _guard = setup_mocking_sliding_sync_server(&server).await;

    let sync_permit = Arc::new(AsyncMutex::new(EncryptionSyncPermit::new_for_testing()));
    let sync_permit_guard = sync_permit.lock_owned().await;
    let encryption_sync = EncryptionSyncService::new(client, None, WithLocking::Yes).await?;

    // Run all the iterations.
    encryption_sync.run_fixed_iterations(1, sync_permit_guard).await?;

    // Check the requests are the ones we've expected.
    let expected_requests = [json!({
        "conn_id": "encryption",
        "extensions": {
            "e2ee": {
                "enabled": true
            },
            "to_device": {
                "enabled": true
            }
        }
    })];

    check_requests(server, &expected_requests).await;

    Ok(())
}

#[async_test]
async fn test_encryption_sync_two_fixed_iterations() -> anyhow::Result<()> {
    let (client, server) = logged_in_client_with_server().await;

    let _guard = setup_mocking_sliding_sync_server(&server).await;

    let sync_permit = Arc::new(AsyncMutex::new(EncryptionSyncPermit::new_for_testing()));
    let sync_permit_guard = sync_permit.lock_owned().await;
    let encryption_sync = EncryptionSyncService::new(client, None, WithLocking::Yes).await?;

    encryption_sync.run_fixed_iterations(2, sync_permit_guard).await?;

    let expected_requests = [
        json!({
            "conn_id": "encryption",
            "extensions": {
                "e2ee": {
                    "enabled": true
                },
                "to_device": {
                    "enabled": true
                }
            }
        }),
        json!({
            "conn_id": "encryption",
            "extensions": {
                "e2ee": {
                    "enabled": true
                },
                "to_device": {
                    "enabled": true
                }
            }
        }),
    ];

    check_requests(server, &expected_requests).await;

    Ok(())
}

#[async_test]
async fn test_encryption_sync_always_reloads_todevice_token() -> anyhow::Result<()> {
    let (client, server) = logged_in_client_with_server().await;

    let sync_permit = Arc::new(AsyncMutex::new(EncryptionSyncPermit::new_for_testing()));
    let sync_permit_guard = sync_permit.lock_owned().await;
    let encryption_sync =
        EncryptionSyncService::new(client.clone(), None, WithLocking::Yes).await?;

    let stream = encryption_sync.sync(sync_permit_guard);
    pin_mut!(stream);

    // First iteration fills the whole request; server responds with the to-device
    // token that should remembered.
    sliding_sync_then_assert_request_and_fake_response! {
        [server, stream]
        assert request = {
            "conn_id": "encryption",
            "extensions": {
                "e2ee": {
                    "enabled": true
                },
                "to_device": {
                    "enabled": true
                }
            }
        },
        respond with = {
            "pos": "0",
            "extensions": {
                "to_device": {
                    "next_batch": "nb0"
                }
            }
        },
    };

    // Second iteration contains the to-device token from the previous request.
    sliding_sync_then_assert_request_and_fake_response! {
        [server, stream]
        assert request = {
            "conn_id": "encryption",
            "extensions": {
                "e2ee": {
                    "enabled": true
                },
                "to_device": {
                    "enabled": true,
                    "since": "nb0",
                },
            }
        },
        respond with = {
            "pos": "1",
            "extensions": {
                "to_device": {
                    "next_batch": "nb1"
                }
            }
        },
    };

    // This encryption sync now conceptually goes to sleep, and another encryption
    // sync starts in another process, runs a sync and changes the to-device
    // token cached on disk.
    if let Some(olm_machine) = &*client.olm_machine_for_testing().await {
        olm_machine
            .store()
            .save_changes(Changes {
                next_batch_token: Some("nb2".to_owned()),
                ..Default::default()
            })
            .await?;
    }

    // Next iteration must have reloaded the latest to-device token.
    sliding_sync_then_assert_request_and_fake_response! {
        [server, stream]
        assert request = {
            "conn_id": "encryption",
            "extensions": {
                "e2ee": {
                    "enabled": true
                },
                "to_device": {
                    "enabled": true,
                    "since": "nb2",
                },
            }
        },
        respond with = {
            "pos": "2",
        },
    };

    Ok(())
}

#[async_test]
async fn test_notification_client_does_not_upload_duplicate_one_time_keys() -> anyhow::Result<()> {
    use tempfile::tempdir;

    let dir = tempdir().unwrap();

    let (builder, server) = test_client_builder_with_server().await;
    let client = builder
        .request_config(RequestConfig::new().disable_retry())
        .sqlite_store(dir.path(), None)
        .build()
        .await
        .unwrap();

    let session = mock_matrix_session();

    client.restore_session(session.to_owned()).await.unwrap();

    info!("Creating the notification client");
    let notification_client = client
        .notification_client("tests".to_owned())
        .await
        .expect("We should be able to build a notification client");

    let sync_permit = Arc::new(AsyncMutex::new(EncryptionSyncPermit::new_for_testing()));
    let sync_permit_guard = sync_permit.lock_owned().await;
    let encryption_sync =
        EncryptionSyncService::new(client.clone(), None, WithLocking::Yes).await?;

    let stream = encryption_sync.sync(sync_permit_guard);
    pin_mut!(stream);

    Mock::given(method("POST"))
        .and(path("/_matrix/client/r0/keys/query"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
        .mount(&server)
        .await;

    info!("First sync, uploading 50 one-time keys");

    sliding_sync_then_assert_request_and_fake_response! {
        [server, stream]
        assert request = {
            "conn_id": "encryption",
            "extensions": {
                "e2ee": {
                    "enabled": true
                },
                "to_device": {
                    "enabled": true
                }
            }
        },
        respond with = {
            "pos": "0",
            "extensions": {
                "to_device": {
                    "next_batch": "nb0"
                },
            }
        },
    };

    #[derive(Debug, Deserialize)]
    struct UploadRequest {
        one_time_keys: BTreeMap<String, serde_json::Value>,
    }

    let found_duplicate = Arc::new(AtomicBool::new(false));
    let uploaded_key_ids = Arc::new(Mutex::new(HashSet::new()));

    Mock::given(method("POST"))
        .and(path("/_matrix/client/r0/keys/upload"))
        .respond_with({
            let found_duplicate = found_duplicate.clone();
            let uploaded_key_ids = uploaded_key_ids.clone();

            move |request: &Request| {
                let request: UploadRequest = request
                    .body_json()
                    .expect("The /keys/upload request should contain one-time keys");

                let mut uploaded_key_ids = uploaded_key_ids.lock().unwrap();

                let new_key_ids: HashSet<String> = request.one_time_keys.into_keys().collect();

                warn!(?new_key_ids, "Got a new /keys/upload request");

                let duplicates: HashSet<_> = uploaded_key_ids.intersection(&new_key_ids).collect();

                if let Some(duplicate) = duplicates.into_iter().next() {
                    error!("Duplicate one-time keys were uploaded.");

                    found_duplicate.store(true, Ordering::SeqCst);

                    ResponseTemplate::new(400).set_body_json(json!({
                        "errcode": "M_WAT",
                        "error:": format!("One time key {duplicate} already exists!")
                    }))
                } else {
                    trace!("No duplicate one-time keys found.");
                    uploaded_key_ids.extend(new_key_ids);

                    ResponseTemplate::new(200).set_body_json(json!({
                        "one_time_key_counts": {
                            "signed_curve25519": 50
                        }
                    }))
                }
            }
        })
        .expect(4)
        .mount(&server)
        .await;

    info!("Main sync now gets told that a one-time key has been used up.");

    sliding_sync_then_assert_request_and_fake_response! {
        [server, stream]
        assert request >= {
            "conn_id": "encryption",
            "extensions": {
                "to_device": {
                    "since": "nb0",
                },
            }
        },
        respond with = {
            "pos": "2",
            "extensions": {
                "to_device": {
                    "next_batch": "nb2"
                },
                "e2ee": {
                    "device_one_time_keys_count": {
                        "signed_curve25519": 49
                    }
                }
            }
        },
    };

    assert!(
        !found_duplicate.load(Ordering::SeqCst),
        "The main sync should not have caused a duplicate one-time key"
    );

    mock_sync(
        &server,
        json!({
            "next_batch": "foo",
            "device_one_time_keys_count": {
                "signed_curve25519": 49
            }
        }),
        None,
    )
    .await;

    info!("The notification client now syncs and tries to upload some one-time keys");

    notification_client
        .sync_once(Default::default())
        .await
        .expect("The notification client should be able to sync successfully");

    info!("Back to the main sync");

    sliding_sync_then_assert_request_and_fake_response! {
        [server, stream]
        assert request >= {
            "conn_id": "encryption",
            "extensions": {
                "to_device": {
                    "since": "foo",
                },
            }
        },
        respond with = {
            "pos": "2",
            "extensions": {
                "to_device": {
                    "next_batch": "nb4"
                },
                "e2ee": {
                    "device_one_time_keys_count": {
                        "signed_curve25519": 49
                    }
                }
            }
        },
    };

    sliding_sync_then_assert_request_and_fake_response! {
        [server, stream]
        assert request >= {
            "conn_id": "encryption",
            "extensions": {
                "to_device": {
                    "since": "nb4",
                },
            }
        },
        respond with = {
            "pos": "2",
            "extensions": {
                "to_device": {
                    "next_batch": "nb5"
                },
            }
        },
    };

    assert!(
        !found_duplicate.load(Ordering::SeqCst),
        "Duplicate one-time keys should not have been created"
    );

    server.verify().await;

    Ok(())
}
