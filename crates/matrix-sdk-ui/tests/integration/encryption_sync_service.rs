use std::sync::{Arc, Mutex};

use futures_util::{pin_mut, StreamExt as _};
use matrix_sdk::test_utils::logged_in_client_with_server;
use matrix_sdk_test::async_test;
use matrix_sdk_ui::encryption_sync_service::{
    EncryptionSyncPermit, EncryptionSyncService, WithLocking,
};
use serde_json::json;
use tokio::sync::Mutex as AsyncMutex;
use wiremock::{Mock, MockGuard, MockServer, Request, ResponseTemplate};

use crate::{
    sliding_sync::{check_requests, PartialSlidingSyncRequest, SlidingSyncMatcher},
    sliding_sync_then_assert_request_and_fake_response,
};

#[async_test]
async fn test_smoke_encryption_sync_works() -> anyhow::Result<()> {
    let (client, server) = logged_in_client_with_server().await;

    let sync_permit = Arc::new(AsyncMutex::new(EncryptionSyncPermit::new_for_testing()));
    let sync_permit_guard = sync_permit.clone().lock_owned().await;
    let encryption_sync =
        EncryptionSyncService::new("tests".to_owned(), client, None, WithLocking::Yes).await?;

    let stream = encryption_sync.sync(sync_permit_guard);
    pin_mut!(stream);

    // Requests enable the e2ee and to_device extensions on the first run.
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
            "pos": "0"
        },
    };

    // The request then passes the `pos`ition marker to the next request, as usual
    // in sliding sync. The extensions haven't changed, so they're not updated
    // (sticky parameters ftw).
    sliding_sync_then_assert_request_and_fake_response! {
        [server, stream]
        assert request = {
            "conn_id": "encryption",
        },
        respond with = {
            "pos": "1",
            "extensions": {
                "to_device": {
                    "next_batch": "nb0"
                }
            }
        },
    };

    // The to-device since token is passed from the previous request.
    // The extensions haven't changed, so they're not updated (sticky parameters
    // ftw).
    sliding_sync_then_assert_request_and_fake_response! {
        [server, stream]
        assert request = {
            "conn_id": "encryption",
            "extensions": {
                "to_device": {
                    "since": "nb0"
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
    // The extensions haven't changed, so they're not updated (sticky parameters
    // ftw)... in the first request. Then, the sliding sync instance will retry
    // those requests, so it will include them again; as a matter of fact, the
    // last request that we assert against will contain those.
    sliding_sync_then_assert_request_and_fake_response! {
        [server, stream]
        sync matches Some(Err(_)),
        assert request = {
            "conn_id": "encryption",
            "extensions": {
                "to_device": {
                    "since": "nb1"
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

    // The next request will contain sticky parameters again.
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
    let encryption_sync =
        EncryptionSyncService::new("tests".to_owned(), client, None, WithLocking::Yes).await?;

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
    let encryption_sync =
        EncryptionSyncService::new("tests".to_owned(), client, None, WithLocking::Yes).await?;

    encryption_sync.run_fixed_iterations(2, sync_permit_guard).await?;

    // First iteration fills the whole request.
    // Second iteration only sends non-sticky parameters.
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
        EncryptionSyncService::new("tests".to_owned(), client.clone(), None, WithLocking::Yes)
            .await?;

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

    // Second iteration only sends non-sticky parameters, plus the to-device token
    // from the previous request.
    sliding_sync_then_assert_request_and_fake_response! {
        [server, stream]
        assert request = {
            "conn_id": "encryption",
            "extensions": {
                "to_device": {
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
    #[cfg(feature = "e2e-encryption")]
    {
        use matrix_sdk_base::crypto::store::Changes;
        if let Some(olm_machine) = &*client.olm_machine_for_testing().await {
            olm_machine
                .store()
                .save_changes(Changes {
                    next_batch_token: Some("nb2".to_owned()),
                    ..Default::default()
                })
                .await?;
        }
    }

    // Next iteration must have reloaded the latest to-device token.
    sliding_sync_then_assert_request_and_fake_response! {
        [server, stream]
        assert request = {
            "conn_id": "encryption",
            "extensions": {
                "to_device": {
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
