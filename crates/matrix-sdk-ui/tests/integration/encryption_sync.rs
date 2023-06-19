use futures_util::{pin_mut, StreamExt as _};
use matrix_sdk_test::async_test;
use matrix_sdk_ui::encryption_sync::{EncryptionSync, EncryptionSyncMode};

use crate::{logged_in_client, sliding_sync_then_assert_request_and_fake_response};

#[async_test]
async fn test_smoke_encryption_sync_works() -> anyhow::Result<()> {
    for with_lock in [true, false] {
        let (client, server) = logged_in_client().await;

        let notification_api = EncryptionSync::new(
            "notifs".to_owned(),
            client,
            EncryptionSyncMode::NeverStop,
            with_lock,
        )
        .await?;

        let notification_stream = notification_api.sync();
        pin_mut!(notification_stream);

        // Requests enable the e2ee and to_device extensions on the first run.
        sliding_sync_then_assert_request_and_fake_response! {
            [server, notification_stream]
            assert request = {
                "conn_id": "notifs",
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
            [server, notification_stream]
            assert request = {
                "conn_id": "notifs",
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
            [server, notification_stream]
            assert request = {
                "conn_id": "notifs",
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
            [server, notification_stream]
            sync matches Some(Err(_)),
            assert request = {
                "conn_id": "notifs",
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

        // The notification stream will stop, as it ran into an error.
        assert!(notification_stream.next().await.is_none());

        // Start a new sync.
        let notification_stream = notification_api.sync();
        pin_mut!(notification_stream);

        // The next request will contain sticky parameters again.
        sliding_sync_then_assert_request_and_fake_response! {
            [server, notification_stream]
            assert request = {
                "conn_id": "notifs",
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
    }

    Ok(())
}
