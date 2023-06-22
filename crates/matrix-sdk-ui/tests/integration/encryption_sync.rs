use futures_util::{pin_mut, StreamExt as _};
use matrix_sdk::SlidingSync;
use matrix_sdk_test::async_test;
use matrix_sdk_ui::encryption_sync::{EncryptionSync, EncryptionSyncMode};

use crate::{logged_in_client, sliding_sync_then_assert_request_and_fake_response};

#[async_test]
async fn test_smoke_encryption_sync_works() -> anyhow::Result<()> {
    let (client, server) = logged_in_client().await;

    let encryption_sync =
        EncryptionSync::new("tests".to_owned(), client, EncryptionSyncMode::NeverStop).await?;

    let stream = encryption_sync.sync();
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
    let stream = encryption_sync.sync();
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

#[async_test]
async fn test_encryption_sync_one_fixed_iteration() -> anyhow::Result<()> {
    let (client, server) = logged_in_client().await;

    let encryption_sync =
        EncryptionSync::new("tests".to_owned(), client, EncryptionSyncMode::RunFixedIterations(1))
            .await?;

    let stream = encryption_sync.sync();
    pin_mut!(stream);

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

    // Only run once, so the next iteration on the stream should stop it.
    assert!(stream.next().await.is_none());
    assert!(stream.next().await.is_none());

    Ok(())
}

#[async_test]
async fn test_encryption_sync_two_fixed_iterations() -> anyhow::Result<()> {
    let (client, server) = logged_in_client().await;

    let encryption_sync =
        EncryptionSync::new("tests".to_owned(), client, EncryptionSyncMode::RunFixedIterations(2))
            .await?;

    let stream = encryption_sync.sync();
    pin_mut!(stream);

    // First iteration fills the whole request.
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

    // Second iteration only sends non-sticky parameters.
    sliding_sync_then_assert_request_and_fake_response! {
        [server, stream]
        assert request = {
            "conn_id": "encryption",
        },
        respond with = {
            "pos": "1",
        },
    };

    // This was the last iteration, there should be no next one.
    assert!(stream.next().await.is_none());
    assert!(stream.next().await.is_none());

    Ok(())
}

#[async_test]
async fn test_encryption_sync_always_reloads_todevice_token() -> anyhow::Result<()> {
    let (client, server) = logged_in_client().await;

    let encryption_sync =
        EncryptionSync::new("tests".to_owned(), client.clone(), EncryptionSyncMode::NeverStop)
            .await?;

    let stream = encryption_sync.sync();
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
    {
        let sliding_sync = SlidingSync::builder("encryption".to_owned(), client)?.build().await?;
        sliding_sync.set_to_device_token("nb2".to_owned());
        sliding_sync.force_cache_to_storage().await?;
    }

    encryption_sync.reload_caches().await?;

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
