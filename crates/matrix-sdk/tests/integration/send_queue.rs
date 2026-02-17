use std::{ops::Not as _, sync::Arc, time::Duration};

use as_variant::as_variant;
use assert_matches2::{assert_let, assert_matches};
#[cfg(feature = "unstable-msc4274")]
use matrix_sdk::attachment::{GalleryConfig, GalleryItemInfo};
use matrix_sdk::{
    Client, MemoryStore, ThreadingSupport, assert_let_timeout,
    attachment::{AttachmentConfig, AttachmentInfo, BaseImageInfo, Thumbnail},
    config::StoreConfig,
    media::{MediaFormat, MediaRequestParameters, MediaThumbnailSettings},
    room::reply::Reply,
    send_queue::{
        AbstractProgress, LocalEcho, LocalEchoContent, RoomSendQueue, RoomSendQueueError,
        RoomSendQueueStorageError, RoomSendQueueUpdate, SendHandle, SendQueueUpdate,
    },
    test_utils::mocks::{MatrixMock, MatrixMockServer},
};
use matrix_sdk_test::{
    ALICE, InvitedRoomBuilder, KnockedRoomBuilder, LeftRoomBuilder, async_test,
    event_factory::EventFactory,
};
#[cfg(feature = "unstable-msc4274")]
use ruma::events::room::message::GalleryItemType;
use ruma::{
    MxcUri, OwnedEventId, OwnedTransactionId, TransactionId, event_id,
    events::{
        AnyMessageLikeEventContent, Mentions, MessageLikeEventContent as _,
        poll::unstable_start::{
            NewUnstablePollStartEventContent, UnstablePollAnswer, UnstablePollAnswers,
            UnstablePollStartContentBlock, UnstablePollStartEventContent,
        },
        relation::Thread,
        room::{
            MediaSource,
            message::{
                ImageMessageEventContent, MessageType, Relation, ReplyWithinThread,
                RoomMessageEventContent, TextMessageEventContent,
            },
        },
    },
    mxc_uri, owned_mxc_uri, owned_user_id, room_id,
    serde::Raw,
    uint,
};
use serde_json::json;
use tokio::{
    sync::{Mutex, broadcast::Receiver},
    task::yield_now,
    time::{sleep, timeout},
};
use wiremock::{Request, ResponseTemplate};

/// Queues an attachment whenever the actual data/mime type etc. don't matter.
///
/// Returns the filename, for sanity check purposes.
async fn queue_attachment_no_thumbnail(q: &RoomSendQueue) -> (SendHandle, &'static str) {
    let filename = "surprise.jpeg.exe";
    let content_type = mime::IMAGE_JPEG;
    let data = b"hello world".to_vec();
    let config = AttachmentConfig::new().info(AttachmentInfo::Image(BaseImageInfo {
        height: Some(uint!(13)),
        width: Some(uint!(37)),
        size: Some(uint!(42)),
        ..Default::default()
    }));
    let handle = q
        .send_attachment(filename, content_type, data, config)
        .await
        .expect("queuing the attachment works");
    (handle, filename)
}

/// Queues an attachment whenever the actual data/mime type etc. don't matter,
/// along with a thumbnail.
///
/// Returns the filename, for sanity check purposes.
async fn queue_attachment_with_thumbnail(q: &RoomSendQueue) -> (SendHandle, &'static str) {
    let filename = "surprise.jpeg.exe";
    let content_type = mime::IMAGE_JPEG;
    let data = b"hello world".to_vec();

    let thumbnail = Thumbnail {
        data: b"thumbnail".to_vec(),
        content_type: content_type.clone(),
        height: uint!(13),
        width: uint!(37),
        size: uint!(42),
    };

    let config = AttachmentConfig::new().thumbnail(Some(thumbnail)).info(AttachmentInfo::Image(
        BaseImageInfo {
            height: Some(uint!(13)),
            width: Some(uint!(37)),
            size: Some(uint!(42)),
            ..Default::default()
        },
    ));

    let handle = q
        .send_attachment(filename, content_type, data, config)
        .await
        .expect("queuing the attachment works");

    // Let the background task pick up the request.
    yield_now().await;

    (handle, filename)
}

fn mock_jpeg_upload<'a>(
    mock: &'a MatrixMockServer,
    mxc: &MxcUri,
    lock: Arc<Mutex<()>>,
) -> MatrixMock<'a> {
    let mxc = mxc.to_owned();
    mock.mock_upload().expect_mime_type("image/jpeg").respond_with(move |_req: &Request| {
        // Wait for the signal from the main task that we can process this query.
        let mock_lock = lock.clone();
        std::thread::spawn(move || {
            tokio::runtime::Runtime::new().unwrap().block_on(async {
                drop(mock_lock.lock().await);
            });
        })
        .join()
        .unwrap();
        ResponseTemplate::new(200).set_body_json(json!({
          "content_uri": mxc
        }))
    })
}

// A macro to assert on a stream of `RoomSendQueueUpdate`s.
macro_rules! assert_update {
    // Check the next stream event is a local echo for an uploaded media.
    // Returns a tuple of (transaction_id, send_handle, room_message).
    (($global_watch:ident, $watch:ident) => local echo event) => {{
        assert_let!(
            Ok(Ok(RoomSendQueueUpdate::NewLocalEvent(LocalEcho {
                content: LocalEchoContent::Event {
                    serialized_event,
                    send_handle,
                    // New local echoes should always start as not wedged.
                    send_error: None,
                },
                transaction_id: txn,
            }))) = timeout(Duration::from_secs(1), $watch.recv()).await
        );
        assert_matches!($global_watch.recv().await, Ok(SendQueueUpdate { update: RoomSendQueueUpdate::NewLocalEvent(_), .. }));

        let content = serialized_event.deserialize().unwrap();
        assert_let!(AnyMessageLikeEventContent::RoomMessage(room_message) = content);

        (txn, send_handle, room_message)
    }};

    // Check the next stream event is a local echo for a message with the content $body.
    // Returns a tuple of (transaction_id, send_handle).
    (($global_watch:ident, $watch:ident) => local echo { body = $body:expr }) => {{
        let (txn, send_handle, room_message) = assert_update!(($global_watch, $watch) => local echo event);
        assert_eq!(room_message.body(), $body);
        (txn, send_handle)
    }};

    // Check the next stream event is a notification about an uploaded media.
    (($global_watch:ident, $watch:ident) => uploaded { related_to = $related_to:expr, mxc = $mxc:expr }) => {{
        assert_let!(
            Ok(Ok(RoomSendQueueUpdate::MediaUpload {
                related_to,
                file,
                ..
            })) = timeout(Duration::from_secs(1), $watch.recv()).await
        );

        assert_eq!(related_to, $related_to);
        assert_let!(Some(MediaSource::Plain(mxc)) = file);
        assert_eq!(mxc, $mxc);
    }};

    // Check the next stream events communicate upload progress and finally the uploaded media.
    (($global_watch:ident, $watch:ident) => uploaded_with_progress {
        related_to = $related_to:expr,
        mxc = $mxc:expr,
        index = $index:expr,
        progress_start = $progress_start:expr,
        progress_end = $progress_end:expr,
        progress_total = $progress_total:expr
    }) => {{
        let mut prev_progress: Option<AbstractProgress> = None;

         loop {
            assert_let!(
                Ok(Ok(RoomSendQueueUpdate::MediaUpload {
                    related_to,
                    file,
                    index,
                    progress, ..
                })) = timeout(Duration::from_secs(1), $watch.recv()).await
            );

            assert_eq!(related_to, $related_to, "related transaction doesn't match");
            assert_eq!(index, $index, "related index doesn't match");

            if let Some(progress_start) = $progress_start {
                assert!(progress.current >= progress_start, "current progress is more than the progress start");
            }
            assert!(progress.current <= $progress_end, "current progress is more than the progress end");
            assert_eq!(progress.total, $progress_total, "total progress doesn't match");
            if let Some(prev_progress) = prev_progress {
                assert!(progress.current >= prev_progress.current, "progress is not increasing");
            }
            prev_progress = Some(progress);

            if let Some(MediaSource::Plain(mxc)) = file {
                assert_eq!(progress.current, $progress_end, "final progress isn't the progress end");
                assert_eq!(mxc, $mxc, "mxc URI doesn't match");
                break;
            }
        }
    }};

    // Check the next stream event is a local echo for a reaction with the content $key which
    // applies to the local echo with transaction id $parent.
    (($global_watch:ident, $watch:ident) => local reaction { key = $key:expr, parent = $parent_txn_id:expr }) => {{
        assert_let!(
            Ok(Ok(RoomSendQueueUpdate::NewLocalEvent(LocalEcho {
                content: LocalEchoContent::React {
                    key,
                    applies_to,
                    send_handle: _,
                },
                transaction_id: txn,
            }))) = timeout(Duration::from_secs(1), $watch.recv()).await
        );
        assert_matches!($global_watch.recv().await, Ok(SendQueueUpdate { update: RoomSendQueueUpdate::NewLocalEvent { .. }, .. }));

        assert_eq!(key, $key);
        assert_eq!(applies_to, $parent_txn_id);

        txn
    }};

    // Check the next stream event is an edit event, and that the
    // transaction id is the one we expect.
    (($global_watch:ident, $watch:ident) => edit local echo { txn = $transaction_id:expr }) => {{
        assert_let!(
            Ok(Ok(RoomSendQueueUpdate::ReplacedLocalEvent {
                transaction_id: txn,
                new_content: serialized_event,
            })) = timeout(Duration::from_secs(1), $watch.recv()).await
        );
        assert_matches!($global_watch.recv().await, Ok(SendQueueUpdate { update: RoomSendQueueUpdate::ReplacedLocalEvent { .. }, .. }));

        assert_eq!(txn, $transaction_id);

        let content = serialized_event.deserialize().unwrap();
        assert_let!(AnyMessageLikeEventContent::RoomMessage(_msg) = content);

        _msg
    }};

    // Check the next stream event is an edit for a local echo with the content $body, and that the
    // transaction id is the one we expect.
    (($global_watch:ident, $watch:ident) => edit { body = $body:expr, txn = $transaction_id:expr }) => {{
        let msg = assert_update!(($global_watch, $watch) => edit local echo { txn = $transaction_id });
        assert_eq!(msg.body(), $body);
    }};

    // Check the next stream event is a retry event, with optional checks on txn=$txn
    (($global_watch:ident, $watch:ident) => retry { $(txn=$txn:expr)? }) => {
        assert_let!(
            Ok(Ok(RoomSendQueueUpdate::RetryEvent { transaction_id: _txn })) =
                timeout(Duration::from_secs(1), $watch.recv()).await
        );
        assert_matches!($global_watch.recv().await, Ok(SendQueueUpdate { update: RoomSendQueueUpdate::RetryEvent { .. }, .. }));

        $(assert_eq!(_txn, $txn);)?
    };

    // Check the next stream event is a sent event, with optional checks on txn=$txn and
    // event_id=$event_id.
    (($global_watch:ident, $watch:ident) => sent { $(txn=$txn:expr,)? $(event_id=$event_id:expr)? }) => {
        assert_let!(
            Ok(Ok(RoomSendQueueUpdate::SentEvent { event_id: _event_id, transaction_id: _txn })) =
                timeout(Duration::from_secs(1), $watch.recv()).await
        );
        assert_matches!($global_watch.recv().await, Ok(SendQueueUpdate { update: RoomSendQueueUpdate::SentEvent { .. }, .. }));

        $(assert_eq!(_event_id, $event_id);)?
        $(assert_eq!(_txn, $txn);)?
    };

    // Check the next stream event is a send error, with optional assertions on the recoverable
    // status and transaction id.
    //
    // Returns the error for additional checks.
    (($global_watch:ident, $watch:ident) => error { $(recoverable=$recoverable:expr,)? $(txn=$txn:expr)? }) => {{
        assert_let!(
            Ok(Ok(RoomSendQueueUpdate::SendError { transaction_id: _txn, error, is_recoverable: _is_recoverable })) =
                timeout(Duration::from_secs(10), $watch.recv()).await
        );
        assert_matches!($global_watch.recv().await, Ok(SendQueueUpdate { update: RoomSendQueueUpdate::SendError { .. }, .. }));

        $(assert_eq!(_txn, $txn);)?
        $(assert_eq!(_is_recoverable, $recoverable);)?

        error
    }};

    // Check the next stream event is a cancelled local echo for the given transaction id.
    (($global_watch:ident, $watch:ident) => cancelled { txn = $txn:expr }) => {{
        assert_let!(
            Ok(Ok(RoomSendQueueUpdate::CancelledLocalEvent { transaction_id: txn })) =
                timeout(Duration::from_secs(10), $watch.recv()).await
        );
        assert_matches!($global_watch.recv().await, Ok(SendQueueUpdate { update: RoomSendQueueUpdate::CancelledLocalEvent { .. }, .. }));

        assert_eq!(txn, $txn);
    }};
}

#[async_test]
async fn test_cant_send_invited_room() {
    let mock = MatrixMockServer::new().await;

    // When I'm invited to a room,
    let room_id = room_id!("!a:b.c");
    let client = mock.client_builder().build().await;
    let room = mock.sync_room(&client, InvitedRoomBuilder::new(room_id)).await;

    // I can't send message to it with the send queue.
    assert_matches!(
        room.send_queue().send(RoomMessageEventContent::text_plain("Hello, World!").into()).await,
        Err(RoomSendQueueError::RoomNotJoined)
    );
}

#[async_test]
async fn test_cant_send_left_room() {
    let mock = MatrixMockServer::new().await;

    // When I've left a room,
    let room_id = room_id!("!a:b.c");
    let client = mock.client_builder().build().await;
    let room = mock.sync_room(&client, LeftRoomBuilder::new(room_id)).await;

    // I can't send message to it with the send queue.
    assert_matches!(
        room.send_queue()
            .send(RoomMessageEventContent::text_plain("Farewell, World!").into())
            .await,
        Err(RoomSendQueueError::RoomNotJoined)
    );
}

#[async_test]
async fn test_cant_send_knocked_room() {
    let mock = MatrixMockServer::new().await;

    // When I've knocked into a room,
    let room_id = room_id!("!a:b.c");
    let client = mock.client_builder().build().await;
    let room = mock.sync_room(&client, KnockedRoomBuilder::new(room_id)).await;

    // I can't send message to it with the send queue.
    assert_matches!(
        room.send_queue()
            .send(RoomMessageEventContent::text_plain("Farewell, World!").into())
            .await,
        Err(RoomSendQueueError::RoomNotJoined)
    );
}

#[async_test]
async fn test_nothing_sent_when_disabled() {
    let mock = MatrixMockServer::new().await;

    // Mark the room as joined.
    let room_id = room_id!("!a:b.c");
    let client = mock.client_builder().build().await;
    let room = mock.sync_joined_room(&client, room_id).await;

    // When I disable the send queue,
    let event_id = event_id!("$1");
    mock.mock_room_send().ok(event_id).expect(0).mount().await;

    client.send_queue().set_enabled(false).await;

    // A message is queued, but never sent.
    room.send_queue()
        .send(RoomMessageEventContent::text_plain("Hello, World!").into())
        .await
        .unwrap();

    // But I can still send it with room.send().
    mock.verify_and_reset().await;

    mock.mock_room_state_encryption().plain().mount().await;
    mock.mock_room_send().ok(event_id).expect(1).mount().await;

    let result = room.send(RoomMessageEventContent::text_plain("Hello, World!")).await.unwrap();
    assert_eq!(result.response.event_id, event_id);
}

#[async_test]
async fn test_smoke() {
    let mock = MatrixMockServer::new().await;

    // Mark the room as joined.
    let room_id = room_id!("!a:b.c");

    let client = mock.client_builder().build().await;
    let room = mock.sync_joined_room(&client, room_id).await;

    let q = room.send_queue();
    let mut global_watch = client.send_queue().subscribe();

    let (local_echoes, mut watch) = q.subscribe().await.unwrap();
    assert!(local_echoes.is_empty());
    assert!(watch.is_empty());

    // When the queue is enabled and I send message in some order, it does send it.
    let event_id = event_id!("$1");

    let lock = Arc::new(Mutex::new(()));
    let lock_guard = lock.lock().await;

    let mock_lock = lock.clone();

    mock.mock_room_state_encryption().plain().mount().await;

    mock.mock_room_send()
        .respond_with(move |_req: &Request| {
            // Wait for the signal from the main thread that we can process this query.
            let mock_lock = mock_lock.clone();
            std::thread::spawn(move || {
                tokio::runtime::Runtime::new().unwrap().block_on(async {
                    drop(mock_lock.lock().await);
                });
            })
            .join()
            .unwrap();

            ResponseTemplate::new(200).set_body_json(json!({
                "event_id": "$1",
            }))
        })
        .expect(1)
        .mount()
        .await;

    room.send_queue().send(RoomMessageEventContent::text_plain("1").into()).await.unwrap();

    let (txn1, _) = assert_update!((global_watch, watch) => local echo { body = "1" });

    {
        let (local_echoes, _) = q.subscribe().await.unwrap();

        assert_eq!(local_echoes.len(), 1);
        assert_eq!(local_echoes[0].transaction_id, txn1);
    }

    assert!(watch.is_empty());

    drop(lock_guard);

    assert_update!((global_watch, watch) => sent { txn = txn1, event_id = event_id });

    assert!(watch.is_empty());
}

#[async_test]
async fn test_smoke_raw() {
    let mock = MatrixMockServer::new().await;

    // Mark the room as joined.
    let room_id = room_id!("!a:b.c");
    let client = mock.client_builder().build().await;
    let room = mock.sync_joined_room(&client, room_id).await;

    let q = room.send_queue();
    let mut global_watch = client.send_queue().subscribe();

    let (local_echoes, mut watch) = q.subscribe().await.unwrap();
    assert!(local_echoes.is_empty());
    assert!(watch.is_empty());

    // When the queue is enabled and I send message in some order, it does send it.
    let event_id = event_id!("$1");

    mock.mock_room_state_encryption().plain().mount().await;
    mock.mock_room_send().ok(event_id).mount().await;

    let json_content = r#"{"baguette": 42}"#.to_owned();
    let event = Raw::from_json_string(json_content.clone()).unwrap();
    room.send_queue().send_raw(event, "m.room.frenchie".to_owned()).await.unwrap();

    assert_let!(
        Ok(Ok(RoomSendQueueUpdate::NewLocalEvent(LocalEcho {
            content: LocalEchoContent::Event { serialized_event, .. },
            transaction_id: txn1,
        }))) = timeout(Duration::from_secs(1), watch.recv()).await
    );
    assert_matches!(
        global_watch.recv().await,
        Ok(SendQueueUpdate { update: RoomSendQueueUpdate::NewLocalEvent(_), .. })
    );

    let content = serialized_event.deserialize().unwrap();
    assert_matches!(&content, AnyMessageLikeEventContent::_Custom { .. });
    assert_eq!(content.event_type().to_string(), "m.room.frenchie");

    let (raw, event_type) = serialized_event.raw();
    assert_eq!(event_type, "m.room.frenchie");
    assert_eq!(raw.json().to_string(), json_content);

    assert_update!((global_watch, watch) => sent { txn = txn1, event_id = event_id });

    assert!(watch.is_empty());
}

#[async_test]
async fn test_error_then_locally_reenabling() {
    let mock = MatrixMockServer::new().await;

    let client = mock.client_builder().build().await;
    let mut errors = client.send_queue().subscribe_errors();

    // Starting with a globally enabled queue.
    assert!(client.send_queue().is_enabled());
    assert!(errors.is_empty());

    // Mark the room as joined.
    let room_id = room_id!("!a:b.c");
    let room = mock.sync_joined_room(&client, room_id).await;

    let q = room.send_queue();
    let mut global_watch = client.send_queue().subscribe();

    let (local_echoes, mut watch) = q.subscribe().await.unwrap();
    assert!(local_echoes.is_empty());
    assert!(watch.is_empty());

    let lock = Arc::new(Mutex::new(()));
    let lock_guard = lock.lock().await;

    let mock_lock = lock.clone();

    mock.mock_room_state_encryption().plain().mount().await;

    let scoped_send = mock
        .mock_room_send()
        .respond_with(move |_req: &Request| {
            // Wait for the signal from the main thread that we can process this query.
            let mock_lock = mock_lock.clone();
            std::thread::spawn(move || {
                tokio::runtime::Runtime::new().unwrap().block_on(async {
                    drop(mock_lock.lock().await);
                });
            })
            .join()
            .unwrap();

            ResponseTemplate::new(500)
        })
        .expect(3)
        .mount_as_scoped()
        .await;

    q.send(RoomMessageEventContent::text_plain("1").into()).await.unwrap();

    let (txn1, _) = assert_update!((global_watch, watch) => local echo { body = "1" });

    {
        let (local_echoes, _) = q.subscribe().await.unwrap();
        assert_eq!(local_echoes.len(), 1);
        assert_eq!(local_echoes[0].transaction_id, txn1);
    }

    assert!(watch.is_empty());

    // No new update on the global error reporter.
    assert!(errors.is_empty());

    drop(lock_guard);

    // The exponential backoff used when retrying a request introduces a bit of
    // non-determinism, so let it fail after a large amount of time (10
    // seconds).
    // It's the same transaction id that's used to signal the send error.
    let error = assert_update!((global_watch, watch) => error { recoverable=true, txn=txn1 });
    let error = error.as_client_api_error().unwrap();
    assert_eq!(error.status_code, 500);

    sleep(Duration::from_millis(50)).await;

    assert!(watch.is_empty());

    let report = errors.recv().await.unwrap();
    assert_eq!(report.room_id, room.room_id());
    assert!(report.is_recoverable);

    // The send queue is still globally enabled,
    assert!(client.send_queue().is_enabled());
    // But the room send queue is disabled.
    assert!(!room.send_queue().is_enabled());

    drop(scoped_send);
    mock.mock_room_send().ok(event_id!("$42")).expect(1).mount().await;

    // Re-enabling the *room* queue will re-send the same message in that room.
    room.send_queue().set_enabled(true);

    assert!(errors.is_empty());

    assert!(client.send_queue().is_enabled());
    assert!(room.send_queue().is_enabled());

    assert_update!((global_watch, watch) => sent { txn=txn1, event_id=event_id!("$42") });

    assert!(errors.is_empty());
    assert!(watch.is_empty());
}

#[async_test]
async fn test_error_then_globally_reenabling() {
    let mock = MatrixMockServer::new().await;

    let client = mock.client_builder().build().await;
    let mut errors = client.send_queue().subscribe_errors();

    // Starting with a globally enabled queue.
    assert!(client.send_queue().is_enabled());
    assert!(errors.is_empty());

    // Mark the room as joined.
    let room_id = room_id!("!a:b.c");
    let room = mock.sync_joined_room(&client, room_id).await;

    let q = room.send_queue();
    let mut global_watch = client.send_queue().subscribe();

    let (local_echoes, mut watch) = q.subscribe().await.unwrap();
    assert!(local_echoes.is_empty());
    assert!(watch.is_empty());

    mock.verify_and_reset().await;
    mock.mock_room_state_encryption().plain().mount().await;
    mock.mock_room_send().error500().expect(3).mount().await;

    q.send(RoomMessageEventContent::text_plain("1").into()).await.unwrap();

    let (txn1, _) = assert_update!((global_watch, watch) => local echo { body = "1" });

    assert!(watch.is_empty());

    // We receive an error.
    let report = errors.recv().await.unwrap();
    assert_eq!(report.room_id, room.room_id());
    assert!(report.is_recoverable);

    // The exponential backoff used when retrying a request introduces a bit of
    // non-determinism, so let it fail after a large amount of time (10
    // seconds).
    // It's the same transaction id that's used to signal the send error.
    assert_update!((global_watch, watch) => error { txn=txn1 });

    // The send queue is still globally enabled,
    assert!(client.send_queue().is_enabled());
    // But the room send queue is disabled.
    assert!(!room.send_queue().is_enabled());

    assert!(watch.is_empty());

    mock.verify_and_reset().await;
    mock.mock_room_state_encryption().plain().mount().await;
    mock.mock_room_send().ok(event_id!("$42")).expect(1).mount().await;

    // Re-enabling the global queue will cause the event to be sent.
    client.send_queue().set_enabled(true).await;

    assert!(client.send_queue().is_enabled());
    assert!(room.send_queue().is_enabled());

    assert_update!((global_watch, watch) => sent { txn=txn1, event_id=event_id!("$42") });

    assert!(errors.is_empty());
    assert!(watch.is_empty());
}

#[async_test]
async fn test_reenabling_queue() {
    let mock = MatrixMockServer::new().await;

    // Mark the room as joined.
    let room_id = room_id!("!a:b.c");
    let client = mock.client_builder().build().await;
    let room = mock.sync_joined_room(&client, room_id).await;

    let errors = client.send_queue().subscribe_errors();

    assert!(errors.is_empty());

    // When I start with a disabled send queue,
    client.send_queue().set_enabled(false).await;

    assert!(!client.send_queue().is_enabled());
    assert!(!room.send_queue().is_enabled());
    assert!(errors.is_empty());

    let q = room.send_queue();
    let mut global_watch = client.send_queue().subscribe();

    let (local_echoes, mut watch) = q.subscribe().await.unwrap();

    assert!(local_echoes.is_empty());
    assert!(watch.is_empty());

    // Three messages are queued.
    q.send(RoomMessageEventContent::text_plain("msg1").into()).await.unwrap();
    q.send(RoomMessageEventContent::text_plain("msg2").into()).await.unwrap();
    q.send(RoomMessageEventContent::text_plain("msg3").into()).await.unwrap();

    for i in 1..=3 {
        assert_update!((global_watch, watch) => local echo { body = format!("msg{i}") });
    }

    {
        let (local_echoes, _) = q.subscribe().await.unwrap();
        assert_eq!(local_echoes.len(), 3);
    }

    assert!(watch.is_empty());

    // Messages aren't sent immediately.
    sleep(Duration::from_millis(500)).await;

    assert!(watch.is_empty());

    mock.mock_room_state_encryption().plain().mount().await;

    mock.mock_room_send().ok(event_id!("$1")).mock_once().mount().await;
    mock.mock_room_send().ok(event_id!("$2")).mock_once().mount().await;
    mock.mock_room_send().ok(event_id!("$3")).mock_once().mount().await;

    // But when reenabling the queue globally,
    client.send_queue().set_enabled(true).await;

    assert!(client.send_queue().is_enabled());
    assert!(room.send_queue().is_enabled());
    assert!(errors.is_empty());

    // They're sent, in the same order.
    for i in 1..=3 {
        let event_id = OwnedEventId::try_from(format!("${i}").as_str()).unwrap();
        assert_update!((global_watch, watch) => sent { event_id = event_id });
    }

    assert!(errors.is_empty());
    assert!(watch.is_empty());
}

#[async_test]
async fn test_disjoint_enabled_status() {
    let mock = MatrixMockServer::new().await;

    // Mark the room as joined.
    let room_id1 = room_id!("!a:b.c");
    let room_id2 = room_id!("!b:b.c");

    let client = mock.client_builder().build().await;
    let room1 = mock.sync_joined_room(&client, room_id1).await;
    let room2 = mock.sync_joined_room(&client, room_id2).await;

    // When I start with a disabled send queue,
    client.send_queue().set_enabled(false).await;

    // All queues are marked as disabled.
    assert!(!client.send_queue().is_enabled());
    assert!(!room1.send_queue().is_enabled());
    assert!(!room2.send_queue().is_enabled());

    // When I enable globally,
    client.send_queue().set_enabled(true).await;

    // This enables globally and locally.
    assert!(client.send_queue().is_enabled());
    assert!(room1.send_queue().is_enabled());
    assert!(room2.send_queue().is_enabled());

    // I can disable one locally,
    room1.send_queue().set_enabled(false);

    // And it doesn't touch the state of other rooms.
    assert!(client.send_queue().is_enabled());
    assert!(!room1.send_queue().is_enabled());
    assert!(room2.send_queue().is_enabled());
}

#[async_test]
async fn test_cancellation() {
    let mock = MatrixMockServer::new().await;

    // Mark the room as joined.
    let room_id = room_id!("!a:b.c");
    let client = mock.client_builder().build().await;
    let room = mock.sync_joined_room(&client, room_id).await;

    let q = room.send_queue();
    let mut global_watch = client.send_queue().subscribe();

    let (local_echoes, mut watch) = q.subscribe().await.unwrap();

    assert!(local_echoes.is_empty());
    assert!(watch.is_empty());

    let lock = Arc::new(Mutex::new(()));
    let lock_guard = lock.lock().await;

    let mock_lock = lock.clone();

    mock.mock_room_state_encryption().plain().mount().await;

    let num_request = std::sync::Mutex::new(1);
    mock.mock_room_send()
        .respond_with(move |_req: &Request| {
            // Wait for the signal from the main thread that we can process this query.
            let mock_lock = mock_lock.clone();
            std::thread::spawn(move || {
                tokio::runtime::Runtime::new().unwrap().block_on(async {
                    drop(mock_lock.lock().await);
                });
            })
            .join()
            .unwrap();

            let mut num_request = num_request.lock().unwrap();

            let event_id = format!("${}", *num_request);
            *num_request += 1;

            ResponseTemplate::new(200).set_body_json(json!({
                "event_id": event_id,
            }))
        })
        .expect(2)
        .mount()
        .await;

    // The redact of txn1 will happen because we asked for it previously.
    mock.mock_room_redact().ok(event_id!("$1")).mount().await;

    let handle1 = q.send(RoomMessageEventContent::text_plain("msg1").into()).await.unwrap();
    let handle2 = q.send(RoomMessageEventContent::text_plain("msg2").into()).await.unwrap();
    q.send(RoomMessageEventContent::text_plain("msg3").into()).await.unwrap();
    q.send(RoomMessageEventContent::text_plain("msg4").into()).await.unwrap();
    q.send(RoomMessageEventContent::text_plain("msg5").into()).await.unwrap();

    // Receiving updates for local echoes.
    let (txn1, _) = assert_update!((global_watch, watch) => local echo { body = "msg1" });
    let (txn2, _) = assert_update!((global_watch, watch) => local echo { body = "msg2" });
    let (txn3, handle3) = assert_update!((global_watch, watch) => local echo { body = "msg3" });
    let (txn4, _) = assert_update!((global_watch, watch) => local echo { body = "msg4" });
    let (txn5, _) = assert_update!((global_watch, watch) => local echo { body = "msg5" });
    assert!(watch.is_empty());

    // Let the background task start now.
    yield_now().await;

    // While the first item is being sent, the system records the intent to abort
    // it.
    assert!(handle1.abort().await.unwrap());
    assert_update!((global_watch, watch) => cancelled { txn = txn1 });
    assert!(watch.is_empty());

    // The second item is pending, so we can abort it, using the handle returned by
    // `send()`.
    assert!(handle2.abort().await.unwrap());
    assert_update!((global_watch, watch) => cancelled { txn = txn2 });
    assert!(watch.is_empty());

    // The third item is pending, so we can abort it, using the handle received from
    // the update.
    assert!(handle3.abort().await.unwrap());
    assert_update!((global_watch, watch) => cancelled { txn = txn3 });
    assert!(watch.is_empty());

    // The fourth item is pending, so we can abort it, using an handle provided by
    // the initial array of values.
    let (mut local_echoes, _) = q.subscribe().await.unwrap();

    // At this point, local echoes = txn1, txn4, txn5.
    assert_eq!(local_echoes.len(), 3);

    let local_echo4 = local_echoes.remove(1);
    assert_eq!(local_echo4.transaction_id, txn4, "local echoes: {local_echoes:?}");

    let LocalEchoContent::Event { send_handle: handle4, .. } = local_echo4.content else {
        panic!("unexpected local echo content");
    };
    assert!(handle4.abort().await.unwrap());
    assert_update!((global_watch, watch) => cancelled { txn = txn4 });
    assert!(watch.is_empty());

    // Let the server process the responses.
    drop(lock_guard);

    // Now the server will process msg1 and msg5.
    assert_update!((global_watch, watch) => sent { txn = txn1, });
    assert_update!((global_watch, watch) => sent { txn = txn5, });
    assert!(watch.is_empty());
}

#[async_test]
async fn test_edit() {
    // Simplified version of test_cancellation: we don't test for *every single way*
    // to edit a local echo, since if the cancellation test passes, all ways
    // would work here too similarly.

    let mock = MatrixMockServer::new().await;

    // Mark the room as joined.
    let room_id = room_id!("!a:b.c");
    let client = mock.client_builder().build().await;
    let room = mock.sync_joined_room(&client, room_id).await;

    let q = room.send_queue();
    let mut global_watch = client.send_queue().subscribe();

    let (local_echoes, mut watch) = q.subscribe().await.unwrap();

    assert!(local_echoes.is_empty());
    assert!(watch.is_empty());

    let lock = Arc::new(Mutex::new(()));
    let lock_guard = lock.lock().await;

    let mock_lock = lock.clone();

    mock.mock_room_state_encryption().plain().mount().await;

    let num_request = std::sync::Mutex::new(1);
    mock.mock_room_send()
        .respond_with(move |_req: &Request| {
            // Wait for the signal from the main thread that we can process this query.
            let mock_lock = mock_lock.clone();
            std::thread::spawn(move || {
                tokio::runtime::Runtime::new().unwrap().block_on(async {
                    drop(mock_lock.lock().await);
                });
            })
            .join()
            .unwrap();

            let mut num_request = num_request.lock().unwrap();

            let event_id = format!("${}", *num_request);
            *num_request += 1;

            ResponseTemplate::new(200).set_body_json(json!({
                "event_id": event_id,
            }))
        })
        .expect(3)
        .mount()
        .await;

    // The /event endpoint is used to retrieve the original event, during creation
    // of the edit event.
    mock.mock_room_event()
        .room(room_id)
        .ok(EventFactory::new()
            .text_msg("msg1")
            .sender(client.user_id().unwrap())
            .room(room_id)
            .into_event())
        .expect(1)
        .named("room_event")
        .mount()
        .await;

    let handle1 = q.send(RoomMessageEventContent::text_plain("msg1").into()).await.unwrap();
    let handle2 = q.send(RoomMessageEventContent::text_plain("msg2").into()).await.unwrap();

    // Receiving updates for local echoes.
    let (txn1, _) = assert_update!((global_watch, watch) => local echo { body = "msg1" });
    let (txn2, _) = assert_update!((global_watch, watch) => local echo { body = "msg2" });
    assert!(watch.is_empty());

    // Let the background task start now.
    yield_now().await;

    // While the first item is being sent, the system remembers the intent to edit
    // it, and will send it later.
    assert!(
        handle1
            .edit(RoomMessageEventContent::text_plain("it's never too late!").into())
            .await
            .unwrap()
    );
    assert_update!((global_watch, watch) => edit { body = "it's never too late!", txn = txn1 });

    // The second item is pending, so we can edit it, using the handle returned by
    // `send()`.
    assert!(
        handle2
            .edit(RoomMessageEventContent::text_plain("new content, who diz").into())
            .await
            .unwrap()
    );
    assert_update!((global_watch, watch) => edit { body = "new content, who diz", txn = txn2 });
    assert!(watch.is_empty());

    // Let the server process the responses.
    drop(lock_guard);

    // The queue sends the first event, without the edit.
    assert_update!((global_watch, watch) => sent { txn = txn1, });

    // The queue sends the edit; we can't check the transaction id because it's
    // unknown.
    assert_update!((global_watch, watch) => sent {});

    // The queue sends the second event.
    assert_update!((global_watch, watch) => sent { txn = txn2, });

    assert!(watch.is_empty());
}

#[async_test]
async fn test_edit_with_poll_start() {
    let mock = MatrixMockServer::new().await;

    // Mark the room as joined.
    let room_id = room_id!("!a:b.c");
    let client = mock.client_builder().build().await;
    let room = mock.sync_joined_room(&client, room_id).await;

    let q = room.send_queue();
    let mut global_watch = client.send_queue().subscribe();

    let (local_echoes, mut watch) = q.subscribe().await.unwrap();

    assert!(local_echoes.is_empty());
    assert!(watch.is_empty());

    let lock = Arc::new(Mutex::new(()));
    let lock_guard = lock.lock().await;

    let mock_lock = lock.clone();

    mock.mock_room_state_encryption().plain().mount().await;

    let num_request = std::sync::Mutex::new(1);
    mock.mock_room_send()
        .respond_with(move |_req: &Request| {
            // Wait for the signal from the main thread that we can process this query.
            let mock_lock = mock_lock.clone();
            std::thread::spawn(move || {
                tokio::runtime::Runtime::new().unwrap().block_on(async {
                    drop(mock_lock.lock().await);
                });
            })
            .join()
            .unwrap();

            let mut num_request = num_request.lock().unwrap();

            let event_id = format!("${}", *num_request);
            *num_request += 1;

            ResponseTemplate::new(200).set_body_json(json!({
                "event_id": event_id,
            }))
        })
        .named("send_event")
        .expect(2)
        .mount()
        .await;

    // The /event endpoint is used to retrieve the original event, during creation
    // of the edit event.
    mock.mock_room_event()
        .ok(EventFactory::new()
            .poll_start("poll_start", "question", vec!["Answer A"])
            .sender(client.user_id().unwrap())
            .room(room_id)
            .into_event())
        .expect(1)
        .named("get_event")
        .mount()
        .await;

    let poll_answers: UnstablePollAnswers =
        vec![UnstablePollAnswer::new("A", "Answer A")].try_into().unwrap();
    let poll_start_block = UnstablePollStartContentBlock::new("question", poll_answers);
    let poll_start_content = UnstablePollStartEventContent::New(
        NewUnstablePollStartEventContent::plain_text("poll_start", poll_start_block),
    );
    let handle = q.send(poll_start_content.into()).await.unwrap();

    // Receiving updates for local echoes.
    assert_let!(
        Ok(Ok(RoomSendQueueUpdate::NewLocalEvent(LocalEcho {
            content: LocalEchoContent::Event {
                serialized_event,
                // New local echoes should always start as not wedged.
                send_error: None,
                ..
            },
            transaction_id: txn1,
        }))) = timeout(Duration::from_secs(1), watch.recv()).await
    );
    assert_matches!(
        global_watch.recv().await,
        Ok(SendQueueUpdate { update: RoomSendQueueUpdate::NewLocalEvent(_), .. })
    );

    let content = serialized_event.deserialize().unwrap();
    assert_let!(AnyMessageLikeEventContent::UnstablePollStart(_) = content);
    assert!(watch.is_empty());

    // Let the background task start now.
    yield_now().await;

    // Edit the poll start event
    let poll_answers: UnstablePollAnswers =
        vec![UnstablePollAnswer::new("B", "Answer B")].try_into().unwrap();
    let poll_start_block = UnstablePollStartContentBlock::new("edited question", poll_answers);
    let poll_start_content = UnstablePollStartEventContent::New(
        NewUnstablePollStartEventContent::plain_text("poll_start (edited)", poll_start_block),
    );
    assert!(handle.edit(poll_start_content.into()).await.unwrap());

    assert_let!(
        Ok(Ok(RoomSendQueueUpdate::ReplacedLocalEvent {
            transaction_id: new_txn1,
            new_content: serialized_event,
        })) = timeout(Duration::from_secs(1), watch.recv()).await
    );
    assert_matches!(
        global_watch.recv().await,
        Ok(SendQueueUpdate { update: RoomSendQueueUpdate::ReplacedLocalEvent { .. }, .. })
    );
    let content = serialized_event.deserialize().unwrap();
    assert_let!(
        AnyMessageLikeEventContent::UnstablePollStart(UnstablePollStartEventContent::New(
            poll_start
        )) = content
    );
    assert_eq!(poll_start.text.unwrap(), "poll_start (edited)");
    assert_eq!(txn1, new_txn1);
    assert!(watch.is_empty());

    // Let the server process the responses.
    drop(lock_guard);

    // Now the server will process the events in order.
    assert_update!((global_watch, watch) => sent { txn = txn1, });

    // Let a bit of time to process the edit event sent to the server for txn1.
    assert_update!((global_watch, watch) => sent {});

    assert!(watch.is_empty());
}

#[async_test]
async fn test_edit_while_being_sent_and_fails() {
    let mock = MatrixMockServer::new().await;

    // Mark the room as joined.
    let room_id = room_id!("!a:b.c");
    let client = mock.client_builder().build().await;
    let room = mock.sync_joined_room(&client, room_id).await;

    let q = room.send_queue();
    let mut global_watch = client.send_queue().subscribe();

    let (local_echoes, mut watch) = q.subscribe().await.unwrap();

    assert!(local_echoes.is_empty());
    assert!(watch.is_empty());

    let lock = Arc::new(Mutex::new(()));
    let lock_guard = lock.lock().await;

    let mock_lock = lock.clone();

    mock.mock_room_state_encryption().plain().mount().await;

    mock.mock_room_send()
        .respond_with(move |_req: &Request| {
            // Wait for the signal from the main thread that we can process this query.
            let mock_lock = mock_lock.clone();
            std::thread::spawn(move || {
                tokio::runtime::Runtime::new().unwrap().block_on(async {
                    drop(mock_lock.lock().await);
                });
            })
            .join()
            .unwrap();

            ResponseTemplate::new(500)
        })
        .expect(3) // reattempts, because of short_retry()
        .mount()
        .await;

    let handle = q.send(RoomMessageEventContent::text_plain("yo").into()).await.unwrap();

    // Receiving updates for local echoes.
    let (txn1, _) = assert_update!((global_watch, watch) => local echo { body = "yo" });
    assert!(watch.is_empty());

    // Let the background task start now.
    yield_now().await;

    // While the first item is being sent, the system remembers the intent to edit
    // it, and will send it later.
    assert!(
        handle
            .edit(RoomMessageEventContent::text_plain("it's never too late!").into())
            .await
            .unwrap()
    );
    assert_update!((global_watch, watch) => edit { body = "it's never too late!", txn = txn1 });

    // Let the server process the responses.
    drop(lock_guard);

    // Now the server will process the messages in order.
    assert_update!((global_watch, watch) => error { recoverable = true, txn = txn1 });

    assert!(watch.is_empty());

    // Looking back at the local echoes will indicate a local echo for `it's never
    // too late`.
    let (local_echoes, _) = q.subscribe().await.unwrap();
    assert_eq!(local_echoes.len(), 1);
    assert_eq!(local_echoes[0].transaction_id, txn1);

    let LocalEchoContent::Event { serialized_event, .. } = &local_echoes[0].content else {
        panic!("unexpected local echo content")
    };
    let event = serialized_event.deserialize().unwrap();

    assert_let!(AnyMessageLikeEventContent::RoomMessage(msg) = event);
    assert_eq!(msg.body(), "it's never too late!");
}

#[async_test]
async fn test_edit_wakes_the_sending_task() {
    let mock = MatrixMockServer::new().await;

    // Mark the room as joined.
    let room_id = room_id!("!a:b.c");
    let client = mock.client_builder().build().await;
    let room = mock.sync_joined_room(&client, room_id).await;

    let q = room.send_queue();
    let mut global_watch = client.send_queue().subscribe();

    let (local_echoes, mut watch) = q.subscribe().await.unwrap();

    assert!(local_echoes.is_empty());
    assert!(watch.is_empty());

    mock.mock_room_state_encryption().plain().mount().await;

    let send_mock_scope = mock.mock_room_send().error_too_large().expect(1).mount_as_scoped().await;

    let handle =
        q.send(RoomMessageEventContent::text_plain("welcome to my ted talk").into()).await.unwrap();

    // Receiving an update for the local echo.
    let (txn, _) =
        assert_update!((global_watch, watch) => local echo { body = "welcome to my ted talk" });
    assert!(watch.is_empty());

    // Let the background task start now.
    yield_now().await;

    assert_update!((global_watch, watch) => error { recoverable = false, txn = txn });
    assert!(watch.is_empty());

    // Now edit the event's content (imagine we make it "shorter").
    drop(send_mock_scope);

    // And re-enable the send queue (it was disabled since the edit caused a
    // permanent error).
    q.set_enabled(true);

    mock.mock_room_send().ok(event_id!("$1")).mount().await;

    let edited = handle
        .edit(RoomMessageEventContent::text_plain("here's the summary of my ted talk").into())
        .await
        .unwrap();
    assert!(edited);

    // Let the server process the message.
    assert_update!((global_watch, watch) => edit { body = "here's the summary of my ted talk", txn = txn });
    assert_update!((global_watch, watch) => sent { txn = txn, });

    assert!(watch.is_empty());
}

#[async_test]
async fn test_abort_after_disable() {
    let mock = MatrixMockServer::new().await;

    // Mark the room as joined.
    let room_id = room_id!("!a:b.c");
    let client = mock.client_builder().build().await;
    let room = mock.sync_joined_room(&client, room_id).await;

    let mut errors = client.send_queue().subscribe_errors();

    assert!(errors.is_empty());

    // Start with an enabled sending queue.
    client.send_queue().set_enabled(true).await;

    assert!(client.send_queue().is_enabled());

    let q = room.send_queue();
    let mut global_watch = client.send_queue().subscribe();

    let (local_echoes, mut watch) = q.subscribe().await.unwrap();

    assert!(local_echoes.is_empty());
    assert!(watch.is_empty());

    mock.verify_and_reset().await;

    mock.mock_room_state_encryption().plain().mount().await;

    // Respond to /send with a transient 500 error.
    mock.mock_room_send().error500().expect(3).mount().await;

    // One message is queued.
    let handle = q.send(RoomMessageEventContent::text_plain("hey there").into()).await.unwrap();

    // It is first seen as a local echo,
    let (txn, _) = assert_update!((global_watch, watch) => local echo { body = "hey there" });

    // Waiting for the global status to report the queue is getting disabled.
    let report = errors.recv().await.unwrap();
    assert_eq!(report.room_id, room.room_id());

    // The room updates will report the error, then the cancelled event, eventually.
    assert_update!((global_watch, watch) => error { recoverable=true, });

    // The room queue has been disabled, but not the client wide one.
    assert!(!room.send_queue().is_enabled());
    assert!(client.send_queue().is_enabled());

    // Aborting the sending should work.
    assert!(handle.abort().await.unwrap());

    assert_update!((global_watch, watch) => cancelled { txn = txn });

    assert!(watch.is_empty());
    assert!(errors.is_empty());
}

#[async_test]
async fn test_abort_or_edit_after_send() {
    let mock = MatrixMockServer::new().await;

    // Mark the room as joined.
    let room_id = room_id!("!a:b.c");
    let client = mock.client_builder().build().await;
    let room = mock.sync_joined_room(&client, room_id).await;

    // Start with an enabled sending queue.
    client.send_queue().set_enabled(true).await;

    let q = room.send_queue();
    let mut global_watch = client.send_queue().subscribe();

    let (local_echoes, mut watch) = q.subscribe().await.unwrap();
    assert!(local_echoes.is_empty());
    assert!(watch.is_empty());

    mock.verify_and_reset().await;
    mock.mock_room_state_encryption().plain().mount().await;
    mock.mock_room_send().ok(event_id!("$1")).mount().await;

    let handle = q.send(RoomMessageEventContent::text_plain("hey there").into()).await.unwrap();

    // It is first seen as a local echo,
    let (txn, _) = assert_update!((global_watch, watch) => local echo { body = "hey there" });
    // Then sent.
    assert_update!((global_watch, watch) => sent { txn = txn, });

    // Editing shouldn't work anymore.
    assert!(
        handle
            .edit(
                RoomMessageEventContent::text_plain("i meant something completely different")
                    .into()
            )
            .await
            .unwrap()
            .not()
    );
    // Neither will aborting.
    assert!(handle.abort().await.unwrap().not());
    // Or sending a reaction.
    assert!(handle.react("😊".to_owned()).await.unwrap().is_none());

    assert!(watch.is_empty());
}

#[async_test]
async fn test_abort_while_being_sent_and_fails() {
    let mock = MatrixMockServer::new().await;

    // Mark the room as joined.
    let room_id = room_id!("!a:b.c");
    let client = mock.client_builder().build().await;
    let room = mock.sync_joined_room(&client, room_id).await;

    let q = room.send_queue();
    let mut global_watch = client.send_queue().subscribe();

    let (local_echoes, mut watch) = q.subscribe().await.unwrap();

    assert!(local_echoes.is_empty());
    assert!(watch.is_empty());

    let lock = Arc::new(Mutex::new(()));
    let lock_guard = lock.lock().await;

    let mock_lock = lock.clone();

    mock.mock_room_state_encryption().plain().mount().await;

    mock.mock_room_send()
        .respond_with(move |_req: &Request| {
            // Wait for the signal from the main thread that we can process this query.
            let mock_lock = mock_lock.clone();
            std::thread::spawn(move || {
                tokio::runtime::Runtime::new().unwrap().block_on(async {
                    drop(mock_lock.lock().await);
                });
            })
            .join()
            .unwrap();

            ResponseTemplate::new(500)
        })
        .expect(3) // reattempts, because of short_retry()
        .mount()
        .await;

    let handle = q.send(RoomMessageEventContent::text_plain("yo").into()).await.unwrap();

    // Receiving updates for local echoes.
    let (txn1, _) = assert_update!((global_watch, watch) => local echo { body = "yo" });
    assert!(watch.is_empty());

    // Let the background task start now.
    yield_now().await;

    // While the item is being sent, the system remembers the intent to redact it
    // later.
    assert!(handle.abort().await.unwrap());
    assert_update!((global_watch, watch) => cancelled { txn = txn1 });

    // Let the server process the responses.
    drop(lock_guard);

    // Now the server will process the messages in order.
    assert_update!((global_watch, watch) => error { recoverable = true, txn = txn1 });

    assert!(watch.is_empty());

    // Looking back at the local echoes will indicate a local echo for `it's never
    // too late`.
    let (local_echoes, _) = q.subscribe().await.unwrap();
    assert!(local_echoes.is_empty());
}

#[async_test]
async fn test_unrecoverable_errors() {
    let mock = MatrixMockServer::new().await;

    // Mark the room as joined.
    let room_id = room_id!("!a:b.c");
    let client = mock.client_builder().build().await;
    let room = mock.sync_joined_room(&client, room_id).await;

    let mut errors = client.send_queue().subscribe_errors();

    assert!(errors.is_empty());

    // Start with an enabled sending queue.
    client.send_queue().set_enabled(true).await;

    assert!(client.send_queue().is_enabled());

    let q = room.send_queue();
    let mut global_watch = client.send_queue().subscribe();

    let (local_echoes, mut watch) = q.subscribe().await.unwrap();

    assert!(local_echoes.is_empty());
    assert!(watch.is_empty());

    mock.verify_and_reset().await;

    mock.mock_room_state_encryption().plain().mount().await;

    // Respond to the first /send with an unrecoverable error.
    mock.mock_room_send().error_too_large().mock_once().mount().await;
    // Respond to the second /send with an OK response.
    mock.mock_room_send().ok(event_id!("$42")).mock_once().mount().await;

    // Queue two messages.
    q.send(RoomMessageEventContent::text_plain("i'm too big for ya").into()).await.unwrap();
    q.send(RoomMessageEventContent::text_plain("aloha").into()).await.unwrap();

    // First message is seen as a local echo.
    let (txn1, _) =
        assert_update!((global_watch, watch) => local echo { body = "i'm too big for ya" });

    // Second message is seen as a local echo.
    let (txn2, _) = assert_update!((global_watch, watch) => local echo { body = "aloha" });

    // There will be an error report for the first message, indicating that the
    // error is unrecoverable.
    let report = errors.recv().await.unwrap();
    assert_eq!(report.room_id, room.room_id());
    assert!(!report.is_recoverable);

    // The room updates will report the error for the first message as unrecoverable
    // too.
    assert_update!((global_watch, watch) => error { recoverable=false, txn=txn1 });

    // The permanent error disables the room send queue.
    assert!(!room.send_queue().is_enabled());
    room.send_queue().set_enabled(true);

    // The second message will be properly sent.
    assert_update!((global_watch, watch) => sent { txn=txn2, event_id=event_id!("$42") });

    assert!(room.send_queue().is_enabled());
    assert!(client.send_queue().is_enabled());
}

#[async_test]
async fn test_unwedge_unrecoverable_errors() {
    let mock = MatrixMockServer::new().await;

    // Mark the room as joined.
    let room_id = room_id!("!a:b.c");
    let client = mock.client_builder().build().await;
    let room = mock.sync_joined_room(&client, room_id).await;

    let mut errors = client.send_queue().subscribe_errors();

    assert!(errors.is_empty());

    // Start with an enabled sending queue.
    client.send_queue().set_enabled(true).await;

    assert!(client.send_queue().is_enabled());

    let q = room.send_queue();
    let mut global_watch = client.send_queue().subscribe();

    let (local_echoes, mut watch) = q.subscribe().await.unwrap();

    assert!(local_echoes.is_empty());
    assert!(watch.is_empty());

    mock.verify_and_reset().await;

    mock.mock_room_state_encryption().plain().mount().await;

    // Respond to the first /send with an unrecoverable error.
    mock.mock_room_send().error_too_large().mock_once().mount().await;
    // Respond to the second /send with an OK response.
    mock.mock_room_send().ok(event_id!("$42")).mock_once().mount().await;

    // Queue the unrecoverable message.
    let send_handle =
        q.send(RoomMessageEventContent::text_plain("i'm too big for ya").into()).await.unwrap();

    // Message is seen as a local echo.
    let (txn1, _) =
        assert_update!((global_watch, watch) => local echo { body = "i'm too big for ya" });

    // There will be an error report for the first message, indicating that the
    // error is unrecoverable.
    let report = errors.recv().await.unwrap();
    assert_eq!(report.room_id, room.room_id());
    assert!(!report.is_recoverable);

    // The room updates will report the error for the first message as unrecoverable
    // too.
    assert_update!((global_watch, watch) => error { recoverable=false, txn=txn1 });

    // The queue is disabled, because it ran into an error.
    assert!(!room.send_queue().is_enabled());
    // Not *all* rooms' queues are disabled, though.
    assert!(client.send_queue().is_enabled());

    // Re-enable the room queue.
    room.send_queue().set_enabled(true);
    assert!(watch.is_empty());

    // Unwedge the previously failed message and try sending it again
    send_handle.unwedge().await.unwrap();

    // The message should be retried
    assert_update!((global_watch, watch) => retry { txn=txn1 });

    // Then eventually sent and a remote echo received
    assert_update!((global_watch, watch) => sent { txn=txn1, event_id=event_id!("$42") });
}

#[async_test]
async fn test_no_network_access_error_is_recoverable() {
    // This is subtle, but for the `drop(server)` below to be effectful, it needs to
    // not be a pooled wiremock server (the default), which will keep the dropped
    // server in a static. Using the line below will create a "bare" server,
    // which is effectively dropped upon `drop()`.
    let server = wiremock::MockServer::builder().start().await;
    let mock = MatrixMockServer::from_server(server);
    let client = mock.client_builder().build().await;

    // Mark the room as joined.
    let room_id = room_id!("!a:b.c");
    let room = mock.sync_joined_room(&client, room_id).await;

    // Dropping the server: any subsequent attempt to connect mimics an unreachable
    // server, which might be caused by missing network.
    drop(mock);

    let mut errors = client.send_queue().subscribe_errors();
    assert!(errors.is_empty());

    // Start with an enabled sending queue.
    client.send_queue().set_enabled(true).await;
    assert!(client.send_queue().is_enabled());

    let q = room.send_queue();
    let mut global_watch = client.send_queue().subscribe();

    let (local_echoes, mut watch) = q.subscribe().await.unwrap();

    assert!(local_echoes.is_empty());
    assert!(watch.is_empty());

    // Queue two messages.
    q.send(RoomMessageEventContent::text_plain("is there anyone around here").into())
        .await
        .unwrap();

    // First message is seen as a local echo.
    let (txn1, _) = assert_update!((global_watch, watch) => local echo { body = "is there anyone around here" });

    // There will be an error report for the first message, indicating that the
    // error is recoverable: because network is unreachable.
    let report = errors.recv().await.unwrap();
    assert_eq!(report.room_id, room.room_id());
    assert!(report.is_recoverable);

    // The room updates will report the error for the first message as recoverable
    // too.
    assert_update!((global_watch, watch) => error { recoverable=true, txn=txn1});

    // The room queue is disabled, because the error was recoverable.
    assert!(!room.send_queue().is_enabled());
    assert!(client.send_queue().is_enabled());
}

#[async_test]
async fn test_reloading_rooms_with_unsent_events() {
    let store = Arc::new(MemoryStore::new());

    let room_id = room_id!("!a:b.c");
    let room_id2 = room_id!("!d:e.f");

    let server = wiremock::MockServer::start().await;
    let mock = MatrixMockServer::from_server(server);

    let client = mock
        .client_builder()
        .on_builder(|builder| {
            builder.store_config(
                StoreConfig::new("cross-process-store-locks-holder-name".to_owned())
                    .state_store(store.clone()),
            )
        })
        .build()
        .await;

    // Mark two rooms as joined.
    let room = mock.sync_joined_room(&client, room_id).await;
    let room2 = mock.sync_joined_room(&client, room_id2).await;

    // Globally disable the send queue.
    let q = client.send_queue();
    q.set_enabled(false).await;
    let watch = q.subscribe_errors();

    // Send one message in each room.
    room.send_queue()
        .send(RoomMessageEventContent::text_plain("Hello, World!").into())
        .await
        .unwrap();

    room2
        .send_queue()
        .send(RoomMessageEventContent::text_plain("Hello there too, World!").into())
        .await
        .unwrap();

    // No errors, because the queue has been disabled.
    sleep(Duration::from_millis(300)).await;
    assert!(watch.is_empty());

    mock.verify_and_reset().await;

    {
        // Kill the client, let it close background tasks.
        drop(watch);
        drop(q);
        drop(client);
        sleep(Duration::from_secs(1)).await;
    }

    // Create a new client with the same memory backend. As the send queues are
    // enabled by default, it will respawn tasks for sending events to those two
    // rooms in the background.
    mock.mock_room_state_encryption().plain().mount().await;

    mock.mock_room_send().ok(event_id!("$1")).mock_once().mount().await;
    mock.mock_room_send().ok(event_id!("$2")).mock_once().mount().await;

    let new_client = mock
        .client_builder()
        .on_builder(|builder| {
            builder.store_config(
                StoreConfig::new("cross-process-store-locks-holder-name".to_owned())
                    .state_store(store),
            )
        })
        .build()
        .await;

    new_client.send_queue().respawn_tasks_for_rooms_with_unsent_requests().await;

    // Let the sending queues process events.
    sleep(Duration::from_secs(1)).await;

    // The real assertion is on the expect(2) on the above Mock.
    mock.verify_and_reset().await;
}

#[async_test]
async fn test_reactions() {
    let mock = MatrixMockServer::new().await;

    // Mark the room as joined.
    let room_id = room_id!("!a:b.c");
    let client = mock.client_builder().build().await;
    let room = mock.sync_joined_room(&client, room_id).await;

    let q = room.send_queue();
    let mut global_watch = client.send_queue().subscribe();

    let (local_echoes, mut watch) = q.subscribe().await.unwrap();
    assert!(local_echoes.is_empty());
    assert!(watch.is_empty());

    let lock = Arc::new(Mutex::new(0));
    let lock_guard = lock.lock().await;

    let mock_lock = lock.clone();

    mock.mock_room_state_encryption().plain().mount().await;

    mock.mock_room_send()
        .respond_with(move |_req: &Request| {
            // Wait for the signal from the main thread that we can process this query.
            let mock_lock = mock_lock.clone();
            let event_id = std::thread::spawn(move || {
                tokio::runtime::Runtime::new().unwrap().block_on(async {
                    let mut event_id = mock_lock.lock().await;
                    let ret = *event_id;
                    *event_id += 1;
                    ret
                })
            })
            .join()
            .unwrap();

            ResponseTemplate::new(200).set_body_json(json!({
                "event_id": format!("${event_id}"),
            }))
        })
        .expect(3)
        .mount()
        .await;

    // Sending of the second emoji has started; abort it, it will result in a redact
    // request.
    mock.mock_room_redact().ok(event_id!("$3")).expect(1).mount().await;

    // Send a message.
    let msg_handle =
        room.send_queue().send(RoomMessageEventContent::text_plain("1").into()).await.unwrap();

    // React to it a few times.
    let emoji_handle =
        msg_handle.react("💯".to_owned()).await.unwrap().expect("first emoji was queued");
    let emoji_handle2 =
        msg_handle.react("🍭".to_owned()).await.unwrap().expect("second emoji was queued");
    let emoji_handle3 =
        msg_handle.react("👍".to_owned()).await.unwrap().expect("fourth emoji was queued");

    let (txn1, _) = assert_update!((global_watch, watch) => local echo { body = "1" });
    let emoji1_txn =
        assert_update!((global_watch, watch) => local reaction { key = "💯", parent = txn1 });
    let emoji2_txn =
        assert_update!((global_watch, watch) => local reaction { key = "🍭", parent = txn1 });
    let emoji3_txn =
        assert_update!((global_watch, watch) => local reaction { key = "👍", parent = txn1 });

    {
        let (local_echoes, _) = q.subscribe().await.unwrap();

        assert_eq!(local_echoes.len(), 4);
        assert_eq!(local_echoes[0].transaction_id, txn1);

        assert_eq!(local_echoes[1].transaction_id, emoji1_txn);
        assert_let!(LocalEchoContent::React { key, applies_to, .. } = &local_echoes[1].content);
        assert_eq!(key, "💯");
        assert_eq!(*applies_to, txn1);

        assert_eq!(local_echoes[2].transaction_id, emoji2_txn);
        assert_let!(LocalEchoContent::React { key, applies_to, .. } = &local_echoes[2].content);
        assert_eq!(key, "🍭");
        assert_eq!(*applies_to, txn1);

        assert_eq!(local_echoes[3].transaction_id, emoji3_txn);
        assert_let!(LocalEchoContent::React { key, applies_to, .. } = &local_echoes[3].content);
        assert_eq!(key, "👍");
        assert_eq!(*applies_to, txn1);
    }

    // Cancel the first reaction before the original event is sent.
    let aborted = emoji_handle.abort().await.unwrap();
    assert!(aborted);
    assert_update!((global_watch, watch) => cancelled { txn = emoji1_txn });
    assert!(watch.is_empty());

    // Let the original event be sent, and re-take the lock immediately so no
    // reactions aren't sent (since the lock is fair).
    drop(lock_guard);
    assert_update!((global_watch, watch) => sent { txn = txn1, event_id = event_id!("$0") });
    let lock_guard = lock.lock().await;
    assert!(watch.is_empty());

    // Abort sending of the second emoji. It was being sent, so it's first cancelled
    // *then* sent and redacted.
    let aborted = emoji_handle2.abort().await.unwrap();
    assert!(aborted);
    assert_update!((global_watch, watch) => cancelled { txn = emoji2_txn });
    assert!(watch.is_empty());

    // Drop the guard to let the mock server process events.
    drop(lock_guard);

    // Previous emoji has been sent; it will be redacted later.
    assert_update!((global_watch, watch) => sent { txn = emoji2_txn, event_id = event_id!("$1") });

    // The final emoji is sent.
    assert_update!((global_watch, watch) => sent { txn = emoji3_txn, event_id = event_id!("$2") });

    // Cancelling sending of the third emoji fails because it's been sent already.
    assert!(emoji_handle3.abort().await.unwrap().not());

    assert!(watch.is_empty());
}

#[async_test]
async fn test_media_uploads() {
    let mock = MatrixMockServer::new().await;

    // Mark the room as joined.
    let room_id = room_id!("!a:b.c");
    let client = mock.client_builder().build().await;
    client.send_queue().enable_upload_progress(true);
    let room = mock.sync_joined_room(&client, room_id).await;

    let q = room.send_queue();
    let mut global_watch = client.send_queue().subscribe();

    let (local_echoes, mut watch) = q.subscribe().await.unwrap();
    assert!(local_echoes.is_empty());

    // ----------------------
    // Create the media to send, with a thumbnail.
    let filename = "surprise.jpeg.exe";
    let content_type = mime::IMAGE_JPEG;
    let data = b"hello world".to_vec();
    let replied_to_event_id = event_id!("$foo:bar.com");

    let thumbnail = Thumbnail {
        data: b"thumbnail".to_vec(),
        content_type: content_type.clone(),
        height: uint!(13),
        width: uint!(37),
        size: uint!(42),
    };

    let attachment_info = AttachmentInfo::Image(BaseImageInfo {
        height: Some(uint!(14)),
        width: Some(uint!(38)),
        size: Some(uint!(43)),
        is_animated: Some(false),
        ..Default::default()
    });

    let size_data = data.len();
    let size_thumbnail = thumbnail.data.len();

    let transaction_id = TransactionId::new();
    let mentions = Mentions::with_user_ids([owned_user_id!("@ivan:sdk.rs")]);
    let config = AttachmentConfig::new()
        .thumbnail(Some(thumbnail))
        .txn_id(transaction_id.clone())
        .caption(Some(TextMessageEventContent::plain("caption")))
        .mentions(Some(mentions.clone()))
        .reply(Some(Reply {
            event_id: replied_to_event_id.into(),
            enforce_thread: matrix_sdk::room::reply::EnforceThread::Threaded(ReplyWithinThread::No),
        }))
        .info(attachment_info);

    // ----------------------
    // Prepare endpoints.
    mock.mock_authenticated_media_config().ok_default().mount().await;
    mock.mock_room_state_encryption().plain().mount().await;
    mock.mock_room_send().ok(event_id!("$1")).mock_once().mount().await;

    let f = EventFactory::new();
    mock.mock_room_event()
        .match_event_id()
        .ok(f
            .text_msg("Send me your attachments")
            .sender(*ALICE)
            .event_id(replied_to_event_id)
            .into())
        .mock_once()
        .mount()
        .await;

    let allow_upload_lock = Arc::new(Mutex::new(()));
    let block_upload = allow_upload_lock.lock().await;

    mock_jpeg_upload(&mock, mxc_uri!("mxc://sdk.rs/thumbnail"), allow_upload_lock.clone())
        .mock_once()
        .mount()
        .await;
    mock_jpeg_upload(&mock, mxc_uri!("mxc://sdk.rs/media"), allow_upload_lock.clone())
        .mock_once()
        .mount()
        .await;

    // ----------------------
    // Send the media.
    assert!(watch.is_empty());
    q.send_attachment(filename, content_type, data, config)
        .await
        .expect("queuing the attachment works");

    // ----------------------
    // Observe the local echo.
    let (txn, send_handle, content) = assert_update!((global_watch, watch) => local echo event);
    assert_eq!(txn, transaction_id);

    // Check mentions.
    let mentions = content.mentions.unwrap();
    assert!(!mentions.room);
    assert_eq!(
        mentions.user_ids.into_iter().collect::<Vec<_>>(),
        vec![owned_user_id!("@ivan:sdk.rs")]
    );

    // Check relations.
    assert_let!(Some(Relation::Thread(thread)) = content.relates_to);
    assert_eq!(thread.event_id, replied_to_event_id);
    assert_eq!(thread.in_reply_to.unwrap().event_id, replied_to_event_id);
    assert!(thread.is_falling_back);

    // Check metadata.
    assert_let!(MessageType::Image(img_content) = content.msgtype);
    assert_eq!(img_content.body, "caption");
    assert!(img_content.formatted_caption().is_none());
    assert_eq!(img_content.filename.as_deref().unwrap(), filename);

    let info = img_content.info.unwrap();
    assert_eq!(info.height, Some(uint!(14)));
    assert_eq!(info.width, Some(uint!(38)));
    assert_eq!(info.size, Some(uint!(43)));
    assert_eq!(info.mimetype.as_deref(), Some("image/jpeg"));
    assert!(info.blurhash.is_none());
    assert_eq!(info.is_animated, Some(false));

    // Check the data source: it should reference the send queue local storage.
    let local_source = img_content.source;
    assert_let!(MediaSource::Plain(mxc) = &local_source);
    assert!(mxc.to_string().starts_with("mxc://send-queue.localhost/"), "{mxc}");

    // The media is immediately available from the cache.
    let file_media = client
        .media()
        .get_media_content(
            &MediaRequestParameters { source: local_source, format: MediaFormat::File },
            true,
        )
        .await
        .expect("media should be found");
    assert_eq!(file_media, b"hello world");

    // ----------------------
    // Thumbnail.

    // Check metadata.
    let tinfo = info.thumbnail_info.unwrap();
    assert_eq!(tinfo.height, Some(uint!(13)));
    assert_eq!(tinfo.width, Some(uint!(37)));
    assert_eq!(tinfo.size, Some(uint!(42)));
    assert_eq!(tinfo.mimetype.as_deref(), Some("image/jpeg"));

    // Check the thumbnail source: it should reference the send queue local storage.
    let local_thumbnail_source = info.thumbnail_source.unwrap();
    assert_let!(MediaSource::Plain(mxc) = &local_thumbnail_source);
    assert!(mxc.to_string().starts_with("mxc://send-queue.localhost/"), "{mxc}");

    let thumbnail_media = client
        .media()
        .get_media_content(
            &MediaRequestParameters {
                source: local_thumbnail_source.clone(),
                format: MediaFormat::File,
            },
            true,
        )
        .await
        .expect("media should be found");
    assert_eq!(thumbnail_media, b"thumbnail");

    // The format should be ignored when requesting a local media.
    let thumbnail_media = client
        .media()
        .get_media_content(
            &MediaRequestParameters {
                source: local_thumbnail_source.clone(),
                format: MediaFormat::Thumbnail(MediaThumbnailSettings::new(
                    tinfo.width.unwrap(),
                    tinfo.height.unwrap(),
                )),
            },
            true,
        )
        .await
        .expect("media should be found");
    assert_eq!(thumbnail_media, b"thumbnail");

    // ----------------------
    // Send handle operations.

    // This operation should be invalid, we shouldn't turn a media into a
    // message.
    assert_matches!(
        send_handle.edit(RoomMessageEventContent::text_plain("hi").into()).await,
        Err(RoomSendQueueStorageError::OperationNotImplementedYet)
    );

    // ----------------------
    // Let the upload progress.
    assert!(watch.is_empty());
    drop(block_upload);

    assert_update!((global_watch, watch) => uploaded_with_progress {
        related_to = transaction_id,
        mxc = mxc_uri!("mxc://sdk.rs/thumbnail"),
        index = 0,
        progress_start = None,
        progress_end = size_thumbnail,
        progress_total = size_data + size_thumbnail
    });

    assert_update!((global_watch, watch) => uploaded_with_progress {
        related_to = transaction_id,
        mxc = mxc_uri!("mxc://sdk.rs/media"),
        index = 0,
        progress_start = Some(size_thumbnail),
        progress_end = size_data + size_thumbnail,
        progress_total = size_data + size_thumbnail
    });

    let edit_msg = assert_update!((global_watch, watch) => edit local echo {
        txn = transaction_id
    });
    assert_let!(MessageType::Image(new_content) = edit_msg.msgtype);

    assert_let!(MediaSource::Plain(new_uri) = &new_content.source);
    assert_eq!(new_uri, mxc_uri!("mxc://sdk.rs/media"));

    let file_media = client
        .media()
        .get_media_content(
            &MediaRequestParameters {
                source: new_content.source.clone(),
                format: MediaFormat::File,
            },
            true,
        )
        .await
        .expect("media should be found with its final MXC uri in the cache");
    assert_eq!(file_media, b"hello world");

    let new_thumbnail_source =
        new_content.info.as_ref().unwrap().thumbnail_source.as_ref().unwrap();
    assert_let!(MediaSource::Plain(new_uri) = new_thumbnail_source);
    assert_eq!(new_uri, mxc_uri!("mxc://sdk.rs/thumbnail"));

    // The thumbnail can be retrieved as a file, using its own MXC URI:
    let thumbnail_media_as_file = client
        .media()
        .get_media_content(
            &MediaRequestParameters {
                source: new_thumbnail_source.clone(),
                format: MediaFormat::File,
            },
            true,
        )
        .await
        .expect("media should be found");
    assert_eq!(thumbnail_media_as_file, b"thumbnail");

    // The thumbnail can be retrieved as a thumbnail of itself, using
    // the sent media MXC URI:
    let thumbnail_media_as_thumbnail = client
        .media()
        .get_media_content(
            &MediaRequestParameters {
                source: new_thumbnail_source.clone(),
                format: MediaFormat::Thumbnail(MediaThumbnailSettings::new(uint!(37), uint!(13))),
            },
            true,
        )
        .await
        .expect("media should be found");
    assert_eq!(thumbnail_media_as_thumbnail, b"thumbnail");

    // The local URI for the thumbnail does not work anymore (it's been renamed).
    client
        .media()
        .get_media_content(
            &MediaRequestParameters { source: local_thumbnail_source, format: MediaFormat::File },
            true,
        )
        .await
        .expect_err("media with local URI should not be found");

    // The event is sent, at some point.
    assert_update!((global_watch, watch) => sent {
        txn = transaction_id,
        event_id = event_id!("$1")
    });

    // That's all, folks!
    assert!(watch.is_empty());
}

#[cfg(feature = "unstable-msc4274")]
#[async_test]
async fn test_gallery_uploads() {
    let mock = MatrixMockServer::new().await;

    // Mark the room as joined.
    let room_id = room_id!("!a:b.c");
    let client = mock.client_builder().build().await;
    let room = mock.sync_joined_room(&client, room_id).await;

    let q = room.send_queue();
    let mut global_watch = client.send_queue().subscribe();

    let (local_echoes, mut watch) = q.subscribe().await.unwrap();
    assert!(local_echoes.is_empty());

    // ----------------------
    // Create the medias to send, with thumbnails.
    let filename1 = "surprise.jpeg.exe";
    let content_type1 = mime::IMAGE_JPEG;
    let data1 = b"hello world".to_vec();

    let filename2 = "onemore.jpeg.exe";
    let content_type2 = mime::IMAGE_JPEG;
    let data2 = b"hello again".to_vec();

    let replied_to_event_id = event_id!("$foo:bar.com");

    let thumbnail1 = Thumbnail {
        data: b"thumbnail".to_vec(),
        content_type: content_type1.clone(),
        height: uint!(13),
        width: uint!(37),
        size: uint!(42),
    };

    let thumbnail2 = Thumbnail {
        data: b"another thumbnail".to_vec(),
        content_type: content_type2.clone(),
        height: uint!(15),
        width: uint!(39),
        size: uint!(44),
    };

    let attachment_info1 = AttachmentInfo::Image(BaseImageInfo {
        height: Some(uint!(14)),
        width: Some(uint!(38)),
        size: Some(uint!(43)),
        is_animated: Some(false),
        ..Default::default()
    });

    let attachment_info2 = AttachmentInfo::Image(BaseImageInfo {
        height: Some(uint!(16)),
        width: Some(uint!(40)),
        size: Some(uint!(45)),
        is_animated: Some(false),
        ..Default::default()
    });

    let transaction_id = TransactionId::new();
    let mentions = Mentions::with_user_ids([owned_user_id!("@ivan:sdk.rs")]);
    let gallery = GalleryConfig::new()
        .txn_id(transaction_id.clone())
        .add_item(GalleryItemInfo {
            attachment_info: attachment_info1,
            content_type: content_type1,
            filename: filename1.into(),
            data: data1,
            thumbnail: Some(thumbnail1),
            caption: Some(TextMessageEventContent::plain("caption1")),
        })
        .add_item(GalleryItemInfo {
            attachment_info: attachment_info2,
            content_type: content_type2,
            filename: filename2.into(),
            data: data2,
            thumbnail: Some(thumbnail2),
            caption: Some(TextMessageEventContent::plain("caption2")),
        })
        .caption(Some(TextMessageEventContent::plain("caption")))
        .mentions(Some(mentions.clone()))
        .reply(Some(Reply {
            event_id: replied_to_event_id.into(),
            enforce_thread: matrix_sdk::room::reply::EnforceThread::Threaded(ReplyWithinThread::No),
        }));

    // ----------------------
    // Prepare endpoints.
    mock.mock_authenticated_media_config().ok_default().mount().await;
    mock.mock_room_state_encryption().plain().mount().await;
    mock.mock_room_send().ok(event_id!("$1")).mock_once().mount().await;

    let f = EventFactory::new();
    mock.mock_room_event()
        .match_event_id()
        .ok(f
            .text_msg("Send me your galleries")
            .sender(*ALICE)
            .event_id(replied_to_event_id)
            .into())
        .mock_once()
        .mount()
        .await;

    let allow_upload_lock = Arc::new(Mutex::new(()));
    let block_upload = allow_upload_lock.lock().await;

    mock_jpeg_upload(&mock, mxc_uri!("mxc://sdk.rs/thumbnail1"), allow_upload_lock.clone())
        .mock_once()
        .mount()
        .await;
    mock_jpeg_upload(&mock, mxc_uri!("mxc://sdk.rs/media1"), allow_upload_lock.clone())
        .mock_once()
        .mount()
        .await;
    mock_jpeg_upload(&mock, mxc_uri!("mxc://sdk.rs/thumbnail2"), allow_upload_lock.clone())
        .mock_once()
        .mount()
        .await;
    mock_jpeg_upload(&mock, mxc_uri!("mxc://sdk.rs/media2"), allow_upload_lock.clone())
        .mock_once()
        .mount()
        .await;

    // ----------------------
    // Send the media.
    assert!(watch.is_empty());
    q.send_gallery(gallery).await.expect("queuing the gallery works");

    // ----------------------
    // Observe the local echo.
    let (txn, send_handle, content) = assert_update!((global_watch, watch) => local echo event);
    assert_eq!(txn, transaction_id);

    // Check mentions.
    let mentions = content.mentions.unwrap();
    assert!(!mentions.room);
    assert_eq!(
        mentions.user_ids.into_iter().collect::<Vec<_>>(),
        vec![owned_user_id!("@ivan:sdk.rs")]
    );

    // Check relations.
    assert_let!(Some(Relation::Thread(thread)) = content.relates_to);
    assert_eq!(thread.event_id, replied_to_event_id);
    assert_eq!(thread.in_reply_to.unwrap().event_id, replied_to_event_id);
    assert!(thread.is_falling_back);

    // Check metadata.
    assert_let!(MessageType::Gallery(gallery_content) = content.msgtype);
    assert_eq!(gallery_content.body, "caption");
    assert!(gallery_content.formatted.is_none());
    assert_eq!(gallery_content.itemtypes.len(), 2);

    // ----------------------
    // Media 1.
    assert_let!(GalleryItemType::Image(img_content) = gallery_content.itemtypes.first().unwrap());
    assert_eq!(img_content.filename.as_deref().unwrap(), filename1);
    assert_eq!(img_content.body, "caption1");

    let info = img_content.info.as_ref().unwrap();
    assert_eq!(info.height, Some(uint!(14)));
    assert_eq!(info.width, Some(uint!(38)));
    assert_eq!(info.size, Some(uint!(43)));
    assert_eq!(info.mimetype.as_deref(), Some("image/jpeg"));
    assert!(info.blurhash.is_none());
    assert_eq!(info.is_animated, Some(false));

    // Check the data source: it should reference the send queue local storage.
    let local_source = img_content.source.clone();
    assert_let!(MediaSource::Plain(mxc) = &local_source);
    assert!(mxc.to_string().starts_with("mxc://send-queue.localhost/"), "{mxc}");

    // The media is immediately available from the cache.
    let file_media = client
        .media()
        .get_media_content(
            &MediaRequestParameters { source: local_source, format: MediaFormat::File },
            true,
        )
        .await
        .expect("media should be found");
    assert_eq!(file_media, b"hello world");

    // ----------------------
    // Thumbnail 1.

    // Check metadata.
    let tinfo = info.thumbnail_info.as_ref().unwrap();
    assert_eq!(tinfo.height, Some(uint!(13)));
    assert_eq!(tinfo.width, Some(uint!(37)));
    assert_eq!(tinfo.size, Some(uint!(42)));
    assert_eq!(tinfo.mimetype.as_deref(), Some("image/jpeg"));

    // Check the thumbnail source: it should reference the send queue local storage.
    let local_thumbnail_source1 = info.thumbnail_source.as_ref().unwrap();
    assert_let!(MediaSource::Plain(mxc) = &local_thumbnail_source1);
    assert!(mxc.to_string().starts_with("mxc://send-queue.localhost/"), "{mxc}");

    let thumbnail_media = client
        .media()
        .get_media_content(
            &MediaRequestParameters {
                source: local_thumbnail_source1.clone(),
                format: MediaFormat::File,
            },
            true,
        )
        .await
        .expect("media should be found");
    assert_eq!(thumbnail_media, b"thumbnail");

    // The format should be ignored when requesting a local media.
    let thumbnail_media = client
        .media()
        .get_media_content(
            &MediaRequestParameters {
                source: local_thumbnail_source1.clone(),
                format: MediaFormat::Thumbnail(MediaThumbnailSettings::new(
                    tinfo.width.unwrap(),
                    tinfo.height.unwrap(),
                )),
            },
            true,
        )
        .await
        .expect("media should be found");
    assert_eq!(thumbnail_media, b"thumbnail");

    // ----------------------
    // Media 2.
    assert_let!(GalleryItemType::Image(img_content) = gallery_content.itemtypes.get(1).unwrap());
    assert_eq!(img_content.filename.as_deref().unwrap(), filename2);
    assert_eq!(img_content.body, "caption2");

    let info = img_content.info.as_ref().unwrap();
    assert_eq!(info.height, Some(uint!(16)));
    assert_eq!(info.width, Some(uint!(40)));
    assert_eq!(info.size, Some(uint!(45)));
    assert_eq!(info.mimetype.as_deref(), Some("image/jpeg"));
    assert!(info.blurhash.is_none());
    assert_eq!(info.is_animated, Some(false));

    // Check the data source: it should reference the send queue local storage.
    let local_source = img_content.source.clone();
    assert_let!(MediaSource::Plain(mxc) = &local_source);
    assert!(mxc.to_string().starts_with("mxc://send-queue.localhost/"), "{mxc}");

    // The media is immediately available from the cache.
    let file_media = client
        .media()
        .get_media_content(
            &MediaRequestParameters { source: local_source, format: MediaFormat::File },
            true,
        )
        .await
        .expect("media should be found");
    assert_eq!(file_media, b"hello again");

    // ----------------------
    // Thumbnail 2.

    // Check metadata.
    let tinfo = info.thumbnail_info.as_ref().unwrap();
    assert_eq!(tinfo.height, Some(uint!(15)));
    assert_eq!(tinfo.width, Some(uint!(39)));
    assert_eq!(tinfo.size, Some(uint!(44)));
    assert_eq!(tinfo.mimetype.as_deref(), Some("image/jpeg"));

    // Check the thumbnail source: it should reference the send queue local storage.
    let local_thumbnail_source2 = info.thumbnail_source.as_ref().unwrap();
    assert_let!(MediaSource::Plain(mxc) = &local_thumbnail_source2);
    assert!(mxc.to_string().starts_with("mxc://send-queue.localhost/"), "{mxc}");

    let thumbnail_media = client
        .media()
        .get_media_content(
            &MediaRequestParameters {
                source: local_thumbnail_source2.clone(),
                format: MediaFormat::File,
            },
            true,
        )
        .await
        .expect("media should be found");
    assert_eq!(thumbnail_media, b"another thumbnail");

    // The format should be ignored when requesting a local media.
    let thumbnail_media = client
        .media()
        .get_media_content(
            &MediaRequestParameters {
                source: local_thumbnail_source2.clone(),
                format: MediaFormat::Thumbnail(MediaThumbnailSettings::new(
                    tinfo.width.unwrap(),
                    tinfo.height.unwrap(),
                )),
            },
            true,
        )
        .await
        .expect("media should be found");
    assert_eq!(thumbnail_media, b"another thumbnail");

    // ----------------------
    // Send handle operations.

    // This operation should be invalid, we shouldn't turn a gallery into a
    // message.
    assert_matches!(
        send_handle.edit(RoomMessageEventContent::text_plain("hi").into()).await,
        Err(RoomSendQueueStorageError::OperationNotImplementedYet)
    );

    // ----------------------
    // Let the upload progress.
    assert!(watch.is_empty());
    drop(block_upload);

    assert_update!((global_watch, watch) => uploaded {
        related_to = transaction_id,
        mxc = mxc_uri!("mxc://sdk.rs/thumbnail1")
    });

    assert_update!((global_watch, watch) => uploaded {
        related_to = transaction_id,
        mxc = mxc_uri!("mxc://sdk.rs/media1")
    });

    assert_update!((global_watch, watch) => uploaded {
        related_to = transaction_id,
        mxc = mxc_uri!("mxc://sdk.rs/thumbnail2")
    });

    assert_update!((global_watch, watch) => uploaded {
        related_to = transaction_id,
        mxc = mxc_uri!("mxc://sdk.rs/media2")
    });

    let edit_msg = assert_update!((global_watch, watch) => edit local echo {
        txn = transaction_id
    });
    assert_let!(MessageType::Gallery(gallery_content) = edit_msg.msgtype);
    assert_eq!(gallery_content.itemtypes.len(), 2);

    // ----------------------
    // Media & thumbnail 1.
    assert_let!(GalleryItemType::Image(new_content) = gallery_content.itemtypes.first().unwrap());

    assert_let!(MediaSource::Plain(new_uri) = &new_content.source);
    assert_eq!(new_uri, mxc_uri!("mxc://sdk.rs/media1"));

    let file_media = client
        .media()
        .get_media_content(
            &MediaRequestParameters {
                source: new_content.source.clone(),
                format: MediaFormat::File,
            },
            true,
        )
        .await
        .expect("media should be found with its final MXC uri in the cache");
    assert_eq!(file_media, b"hello world");

    let new_thumbnail_source = new_content.info.clone().unwrap().thumbnail_source.unwrap();
    assert_let!(MediaSource::Plain(new_uri) = &new_thumbnail_source);
    assert_eq!(new_uri, mxc_uri!("mxc://sdk.rs/thumbnail1"));

    let thumbnail_media = client
        .media()
        .get_media_content(
            &MediaRequestParameters { source: new_thumbnail_source, format: MediaFormat::File },
            true,
        )
        .await
        .expect("media should be found");
    assert_eq!(thumbnail_media, b"thumbnail");

    // The local URI does not work anymore.
    client
        .media()
        .get_media_content(
            &MediaRequestParameters {
                source: local_thumbnail_source1.clone(),
                format: MediaFormat::File,
            },
            true,
        )
        .await
        .expect_err("media with local URI should not be found");

    // ----------------------
    // Media & thumbnail 2.
    assert_let!(GalleryItemType::Image(new_content) = gallery_content.itemtypes.get(1).unwrap());

    assert_let!(MediaSource::Plain(new_uri) = &new_content.source);
    assert_eq!(new_uri, mxc_uri!("mxc://sdk.rs/media2"));

    let file_media = client
        .media()
        .get_media_content(
            &MediaRequestParameters {
                source: new_content.source.clone(),
                format: MediaFormat::File,
            },
            true,
        )
        .await
        .expect("media should be found with its final MXC uri in the cache");
    assert_eq!(file_media, b"hello again");

    let new_thumbnail_source = new_content.info.clone().unwrap().thumbnail_source.unwrap();
    assert_let!(MediaSource::Plain(new_uri) = &new_thumbnail_source);
    assert_eq!(new_uri, mxc_uri!("mxc://sdk.rs/thumbnail2"));

    let thumbnail_media = client
        .media()
        .get_media_content(
            &MediaRequestParameters { source: new_thumbnail_source, format: MediaFormat::File },
            true,
        )
        .await
        .expect("media should be found");
    assert_eq!(thumbnail_media, b"another thumbnail");

    // The local URI does not work anymore.
    client
        .media()
        .get_media_content(
            &MediaRequestParameters {
                source: local_thumbnail_source2.clone(),
                format: MediaFormat::File,
            },
            true,
        )
        .await
        .expect_err("media with local URI should not be found");

    // The event is sent, at some point.
    assert_update!((global_watch, watch) => sent {
        txn = transaction_id,
        event_id = event_id!("$1")
    });

    // That's all, folks!
    assert!(watch.is_empty());
}

#[async_test]
async fn test_media_upload_retry() {
    let mock = MatrixMockServer::new().await;

    // Mark the room as joined.
    let room_id = room_id!("!a:b.c");
    let client = mock.client_builder().build().await;
    let room = mock.sync_joined_room(&client, room_id).await;

    let q = room.send_queue();
    let (local_echoes, mut watch) = q.subscribe().await.unwrap();
    assert!(local_echoes.is_empty());

    let mut global_watch = client.send_queue().subscribe();

    // Prepare endpoints.
    mock.mock_authenticated_media_config().ok_default().mount().await;
    mock.mock_room_state_encryption().plain().mount().await;

    // Fail for the first three attempts.
    mock.mock_upload()
        .expect_mime_type("image/jpeg")
        .error500()
        .up_to_n_times(3)
        .expect(3)
        .mount()
        .await;

    // Send the media.
    assert!(watch.is_empty());
    let (_handle, filename) = queue_attachment_no_thumbnail(&q).await;

    // Observe the local echo.
    let (event_txn, _send_handle, content) =
        assert_update!((global_watch, watch) => local echo event);
    assert_let!(MessageType::Image(img_content) = content.msgtype);
    assert_eq!(img_content.body, filename);

    // Let the upload stumble and the queue disable itself.
    let error = assert_update!((global_watch, watch) => error { recoverable=true, txn=event_txn });
    let error = error.as_client_api_error().unwrap();
    assert_eq!(error.status_code, 500);
    assert!(q.is_enabled().not());

    // Mount the mock for the upload and sending the event.
    mock.mock_upload()
        .expect_mime_type("image/jpeg")
        .ok(mxc_uri!("mxc://sdk.rs/media"))
        .mock_once()
        .mount()
        .await;
    mock.mock_room_send().ok(event_id!("$1")).mock_once().mount().await;

    // Restart the send queue.
    q.set_enabled(true);

    assert_update!((global_watch, watch) => uploaded {
        related_to = event_txn,
        mxc = mxc_uri!("mxc://sdk.rs/media")
    });

    let edit_msg = assert_update!((global_watch, watch) => edit local echo {
        txn = event_txn
    });
    assert_let!(MessageType::Image(new_content) = edit_msg.msgtype);
    assert_let!(MediaSource::Plain(new_uri) = &new_content.source);
    assert_eq!(new_uri, mxc_uri!("mxc://sdk.rs/media"));

    // The event is sent, at some point.
    assert_update!((global_watch, watch) => sent {
        txn = event_txn,
        event_id = event_id!("$1")
    });

    // That's all, folks!
    assert!(watch.is_empty());
}

#[async_test]
async fn test_media_upload_retry_with_520_http_status_code() {
    let mock = MatrixMockServer::new().await;

    // Mark the room as joined.
    let room_id = room_id!("!a:b.c");
    let client = mock.client_builder().build().await;
    let room = mock.sync_joined_room(&client, room_id).await;

    let q = room.send_queue();
    let (local_echoes, mut watch) = q.subscribe().await.unwrap();
    assert!(local_echoes.is_empty());

    let mut global_watch = client.send_queue().subscribe();

    // Prepare endpoints.
    mock.mock_room_state_encryption().plain().mount().await;

    mock.mock_authenticated_media_config().ok_default().mount().await;

    // Fail with a 520 http status code
    mock.mock_upload()
        .expect_mime_type("image/jpeg")
        .respond_with(ResponseTemplate::new(520).set_body_json(json!({})))
        .up_to_n_times(1)
        .expect(1)
        .mount()
        .await;

    // Send the media.
    assert!(watch.is_empty());
    let (_handle, filename) = queue_attachment_no_thumbnail(&q).await;

    // Observe the local echo.
    let (event_txn, _send_handle, content) =
        assert_update!((global_watch, watch) => local echo event);
    assert_let!(MessageType::Image(img_content) = content.msgtype);
    assert_eq!(img_content.body, filename);

    // Let the upload stumble and the queue disable itself.
    let error = assert_update!((global_watch, watch) => error { recoverable=false, txn=event_txn });
    let error = error.as_client_api_error().unwrap();
    assert_eq!(error.status_code, 520);
    assert!(q.is_enabled().not());
}

#[async_test]
async fn test_unwedging_media_upload() {
    let mock = MatrixMockServer::new().await;

    // Mark the room as joined.
    let room_id = room_id!("!a:b.c");
    let client = mock.client_builder().build().await;
    let room = mock.sync_joined_room(&client, room_id).await;

    let q = room.send_queue();
    let (local_echoes, mut watch) = q.subscribe().await.unwrap();
    assert!(local_echoes.is_empty());

    let mut global_watch = client.send_queue().subscribe();

    // Prepare endpoints.
    mock.mock_authenticated_media_config().ok_default().mount().await;
    mock.mock_room_state_encryption().plain().mount().await;

    // Fail for the first attempt with an error indicating the media's too large,
    // wedging the upload.
    mock.mock_upload().error_too_large().mock_once().mount().await;

    // Send the media.
    assert!(watch.is_empty());
    let (_handle, filename) = queue_attachment_no_thumbnail(&q).await;

    // Observe the local echo.
    let (event_txn, send_handle, content) =
        assert_update!((global_watch, watch) => local echo event);
    assert_let!(MessageType::Image(img_content) = content.msgtype);
    assert_eq!(img_content.body, filename);

    // Although the actual error happens on the file upload transaction id, it must
    // be reported with the *event* transaction id.
    let error = assert_update!((global_watch, watch) => error { recoverable=false, txn=event_txn });
    let error = error.as_client_api_error().unwrap();
    assert_eq!(error.status_code, 413);
    assert!(!q.is_enabled());

    // Mount the mock for the upload and sending the event.
    mock.mock_upload().ok(mxc_uri!("mxc://sdk.rs/media")).mock_once().mount().await;
    mock.mock_room_send().ok(event_id!("$1")).mock_once().mount().await;

    // Re-enable the room queue.
    q.set_enabled(true);

    // Unwedge the upload.
    send_handle.unwedge().await.unwrap();

    // Observe the notification for the retry itself.
    assert_update!((global_watch, watch) => retry { txn = event_txn });

    // Observe the upload succeeding at some point.
    assert_update!((global_watch, watch) => uploaded { related_to = event_txn, mxc = mxc_uri!("mxc://sdk.rs/media") });

    let edit_msg = assert_update!((global_watch, watch) => edit local echo { txn = event_txn });
    assert_let!(MessageType::Image(new_content) = edit_msg.msgtype);
    assert_let!(MediaSource::Plain(new_uri) = &new_content.source);
    assert_eq!(new_uri, mxc_uri!("mxc://sdk.rs/media"));

    // The event is sent, at some point.
    assert_update!((global_watch, watch) => sent { txn = event_txn, event_id = event_id!("$1") });

    // That's all, folks!
    assert!(watch.is_empty());
}

/// Aborts an ongoing media upload and checks post-conditions:
/// - we could abort
/// - we get the notification about the aborted upload
/// - the medias aren't present in the cache store
async fn abort_and_verify(
    client: &Client,
    global_watch: &mut Receiver<SendQueueUpdate>,
    watch: &mut Receiver<RoomSendQueueUpdate>,
    img_content: ImageMessageEventContent,
    upload_handle: SendHandle,
    upload_txn: OwnedTransactionId,
) {
    let file_source = img_content.source;
    let info = img_content.info.unwrap();
    let thumbnail_source = info.thumbnail_source.unwrap();

    let aborted = upload_handle.abort().await.unwrap();
    assert!(aborted, "upload must have been aborted");

    assert_update!((global_watch, watch) => cancelled { txn = upload_txn });

    // The event cache doesn't contain the medias anymore.
    client
        .media()
        .get_media_content(
            &MediaRequestParameters { source: file_source, format: MediaFormat::File },
            true,
        )
        .await
        .unwrap_err();

    client
        .media()
        .get_media_content(
            &MediaRequestParameters { source: thumbnail_source, format: MediaFormat::File },
            true,
        )
        .await
        .unwrap_err();
}

#[async_test]
async fn test_media_event_is_sent_in_order() {
    // Test that despite happening in multiple requests, sending a media maintains
    // the ordering.
    let mock = MatrixMockServer::new().await;

    // Mark the room as joined.
    let room_id = room_id!("!a:b.c");
    let client = mock.client_builder().build().await;
    let room = mock.sync_joined_room(&client, room_id).await;

    let q = room.send_queue();

    let (local_echoes, mut watch) = q.subscribe().await.unwrap();
    assert!(local_echoes.is_empty());
    let mut global_watch = client.send_queue().subscribe();

    // Prepare endpoints.
    mock.mock_authenticated_media_config().ok_default().mount().await;
    mock.mock_room_state_encryption().plain().mount().await;
    mock.mock_upload().ok(mxc_uri!("mxc://sdk.rs/media")).mock_once().mount().await;

    assert!(watch.is_empty());

    {
        // 1. Send a text message that will get wedged.
        mock.mock_room_send().error_too_large().mock_once().mount().await;
        q.send(RoomMessageEventContent::text_plain("error").into()).await.unwrap();
        let (text_txn, _send_handle) =
            assert_update!((global_watch, watch) => local echo { body = "error" });
        assert_update!((global_watch, watch) => error { recoverable = false, txn = text_txn });
    }

    // Re-enable the send queue after the permanent error.
    q.set_enabled(true);

    // We'll then send a media event, and then a text event with success.
    mock.mock_room_send().ok(event_id!("$media")).mock_once().mount().await;
    mock.mock_room_send().ok(event_id!("$text")).mock_once().mount().await;

    // 2. Queue the media.
    let (_handle, filename) = queue_attachment_no_thumbnail(&q).await;

    // 3. Queue the message.
    q.send(RoomMessageEventContent::text_plain("hello world").into()).await.unwrap();

    // Observe the local echo for the media.
    let (event_txn, _send_handle, content) =
        assert_update!((global_watch, watch) => local echo event);
    assert_let!(MessageType::Image(img_content) = content.msgtype);
    assert_eq!(img_content.body, filename);

    // Observe the local echo for the message.
    let (text_txn, _send_handle) =
        assert_update!((global_watch, watch) => local echo { body = "hello world" });

    // The media gets uploaded.
    assert_update!((global_watch, watch) => uploaded { related_to = event_txn, mxc = mxc_uri!("mxc://sdk.rs/media") });

    // The media event gets updated with the final MXC IDs.
    assert_update!((global_watch, watch) => edit local echo { txn = event_txn });

    // This is the main thing we're testing: the media must be effectively sent
    // *before* the text message, despite implementation details (the media is
    // sent over multiple send queue requests).

    assert_update!((global_watch, watch) => sent { txn = event_txn, event_id = event_id!("$media") });
    assert_update!((global_watch, watch) => sent { txn = text_txn, event_id = event_id!("$text") });

    // That's all, folks!
    assert!(watch.is_empty());

    // When reopening the send queue, we still see the wedged event.
    let (local_echoes, _watch) = q.subscribe().await.unwrap();
    assert_eq!(local_echoes.len(), 1);
    assert_let!(LocalEchoContent::Event { send_error, .. } = &local_echoes[0].content);
    assert!(send_error.is_some());
}

#[async_test]
async fn test_cancel_upload_before_active() {
    let mock = MatrixMockServer::new().await;

    // Mark the room as joined.
    let room_id = room_id!("!a:b.c");
    let client = mock.client_builder().build().await;
    let room = mock.sync_joined_room(&client, room_id).await;

    let q = room.send_queue();
    let mut global_watch = client.send_queue().subscribe();

    let (local_echoes, mut watch) = q.subscribe().await.unwrap();
    assert!(local_echoes.is_empty());

    // Prepare endpoints.
    mock.mock_room_state_encryption().plain().mount().await;

    mock.mock_room_send()
        .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(1)).set_body_json(
            json!({
              "event_id": event_id!("$msg")
            }),
        ))
        .mock_once()
        .mount()
        .await;

    // Send an event which sending will be "slow" (blocked by mutex).
    q.send(RoomMessageEventContent::text_plain("hey").into()).await.unwrap();
    let (msg_txn, _handle) = assert_update!((global_watch, watch) => local echo { body = "hey" });

    // Send the media.
    assert!(watch.is_empty());

    let (upload_handle, filename) = queue_attachment_with_thumbnail(&q).await;

    let (upload_txn, _send_handle, content) =
        assert_update!((global_watch, watch) => local echo event);

    assert_let!(MessageType::Image(img_content) = content.msgtype);
    assert_eq!(img_content.filename(), filename);

    // Abort the upload.
    abort_and_verify(
        &client,
        &mut global_watch,
        &mut watch,
        img_content,
        upload_handle,
        upload_txn,
    )
    .await;

    // Let the sending progress.
    assert!(watch.is_empty());
    sleep(Duration::from_secs(1)).await;

    // The text event is sent, at some point.
    assert_update!((global_watch, watch) => sent { txn = msg_txn, });

    // Wait a bit of time for things to settle.
    sleep(Duration::from_millis(500)).await;

    // That's all, folks!
    assert!(watch.is_empty());
}

#[async_test]
async fn test_cancel_upload_with_thumbnail_active() {
    let mock = MatrixMockServer::new().await;

    // Mark the room as joined.
    let room_id = room_id!("!a:b.c");
    let client = mock.client_builder().build().await;
    let room = mock.sync_joined_room(&client, room_id).await;

    let q = room.send_queue();
    let mut global_watch = client.send_queue().subscribe();

    let (local_echoes, mut watch) = q.subscribe().await.unwrap();
    assert!(local_echoes.is_empty());

    // Prepare endpoints.
    mock.mock_authenticated_media_config().ok_default().mount().await;
    mock.mock_room_state_encryption().plain().mount().await;
    mock.mock_room_send().ok(event_id!("$msg")).mock_once().mount().await;

    // Have the thumbnail upload take forever and time out, if continued. This will
    // be interrupted when aborting, so this will never have to complete.
    mock.mock_upload()
        .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(60)))
        .expect(1)
        .mount()
        .await;

    // Send the media.
    assert!(watch.is_empty());

    let (upload_handle, filename) = queue_attachment_with_thumbnail(&q).await;

    let (upload_txn, _send_handle, content) =
        assert_update!((global_watch, watch) => local echo event);
    assert_let!(MessageType::Image(img_content) = content.msgtype);
    assert_eq!(img_content.filename(), filename);

    // Let the upload request start.
    sleep(Duration::from_millis(500)).await;

    // Abort the upload.
    abort_and_verify(
        &client,
        &mut global_watch,
        &mut watch,
        img_content,
        upload_handle,
        upload_txn,
    )
    .await;

    // To prove we're not waiting for the upload to finish, send a message and
    // observe it's immediately sent.
    q.send(RoomMessageEventContent::text_plain("hi").into()).await.unwrap();
    let (msg_txn, _handle) = assert_update!((global_watch, watch) => local echo { body = "hi" });
    assert_update!((global_watch, watch) => sent { txn = msg_txn, });

    // That's all, folks!
    assert!(watch.is_empty());
}

#[async_test]
async fn test_cancel_upload_with_uploaded_thumbnail_and_file_active() {
    let mock = MatrixMockServer::new().await;

    // Mark the room as joined.
    let room_id = room_id!("!a:b.c");
    let client = mock.client_builder().build().await;
    let room = mock.sync_joined_room(&client, room_id).await;

    let q = room.send_queue();
    let mut global_watch = client.send_queue().subscribe();

    let (local_echoes, mut watch) = q.subscribe().await.unwrap();
    assert!(local_echoes.is_empty());

    // Prepare endpoints.
    mock.mock_authenticated_media_config().ok_default().mount().await;
    mock.mock_room_state_encryption().plain().mount().await;
    mock.mock_room_send().ok(event_id!("$msg")).mock_once().named("send event").mount().await;

    // Have the thumbnail upload finish early.
    mock.mock_upload()
        .ok(mxc_uri!("mxc://sdk.rs/thumbnail"))
        .mock_once()
        .named("thumbnail upload")
        .mount()
        .await;

    // Have the file upload take forever and time out, if continued. This will
    // be interrupted when aborting, so this will never have to complete.
    mock.mock_upload()
        .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(60)))
        .expect(1)
        .named("file upload")
        .mount()
        .await;

    // Send the media.
    assert!(watch.is_empty());

    let (upload_handle, filename) = queue_attachment_with_thumbnail(&q).await;

    let (upload_txn, _send_handle, content) =
        assert_update!((global_watch, watch) => local echo event);
    assert_let!(MessageType::Image(img_content) = content.msgtype);
    assert_eq!(img_content.filename(), filename);

    // The thumbnail uploads just fine.
    assert_update!((global_watch, watch) => uploaded { related_to = upload_txn, mxc = mxc_uri!("mxc://sdk.rs/thumbnail") });

    // Let the file upload request start.
    sleep(Duration::from_millis(500)).await;

    // Abort the upload.
    abort_and_verify(
        &client,
        &mut global_watch,
        &mut watch,
        img_content,
        upload_handle,
        upload_txn,
    )
    .await;

    // To prove we're not waiting for the upload to finish, send a message and
    // observe it's immediately sent.
    q.send(RoomMessageEventContent::text_plain("hi").into()).await.unwrap();
    let (msg_txn, _handle) = assert_update!((global_watch, watch) => local echo { body = "hi" });
    assert_update!((global_watch, watch) => sent { txn = msg_txn, });

    // That's all, folks!
    assert!(watch.is_empty());
}

#[async_test]
async fn test_cancel_upload_only_file_with_file_active() {
    let mock = MatrixMockServer::new().await;

    // Mark the room as joined.
    let room_id = room_id!("!a:b.c");
    let client = mock.client_builder().build().await;
    let room = mock.sync_joined_room(&client, room_id).await;

    let q = room.send_queue();
    let mut global_watch = client.send_queue().subscribe();

    let (local_echoes, mut watch) = q.subscribe().await.unwrap();
    assert!(local_echoes.is_empty());

    // Prepare endpoints.
    mock.mock_authenticated_media_config().ok_default().mount().await;
    mock.mock_room_state_encryption().plain().mount().await;
    mock.mock_room_send().ok(event_id!("$msg")).mock_once().named("send event").mount().await;

    // Have the file upload take forever and time out, if continued. This will
    // be interrupted when aborting, so this will never have to complete.
    mock.mock_upload()
        .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(60)))
        .expect(1)
        .named("file upload")
        .mount()
        .await;

    // Send the media.
    assert!(watch.is_empty());

    let (upload_handle, filename) = queue_attachment_no_thumbnail(&q).await;

    let (upload_txn, _send_handle, content) =
        assert_update!((global_watch, watch) => local echo event);
    assert_let!(MessageType::Image(img_content) = content.msgtype);
    assert_eq!(img_content.filename(), filename);

    // Let the upload request start.
    sleep(Duration::from_millis(500)).await;

    // Abort the upload.
    let aborted = upload_handle.abort().await.unwrap();
    assert!(aborted, "upload must have been aborted");

    assert_update!((global_watch, watch) => cancelled { txn = upload_txn });

    // The event cache doesn't contain the medias anymore.
    client
        .media()
        .get_media_content(
            &MediaRequestParameters { source: img_content.source, format: MediaFormat::File },
            true,
        )
        .await
        .unwrap_err();

    // To prove we're not waiting for the upload to finish, send a message and
    // observe it's immediately sent.
    q.send(RoomMessageEventContent::text_plain("hi").into()).await.unwrap();
    let (msg_txn, _handle) = assert_update!((global_watch, watch) => local echo { body = "hi" });
    assert_update!((global_watch, watch) => sent { txn = msg_txn, });

    // That's all, folks!
    assert!(watch.is_empty());
}

#[async_test]
async fn test_cancel_upload_while_sending_event() {
    let mock = MatrixMockServer::new().await;

    // Mark the room as joined.
    let room_id = room_id!("!a:b.c");
    let client = mock.client_builder().build().await;
    let room = mock.sync_joined_room(&client, room_id).await;

    let q = room.send_queue();
    let mut global_watch = client.send_queue().subscribe();

    let (local_echoes, mut watch) = q.subscribe().await.unwrap();
    assert!(local_echoes.is_empty());

    // Prepare endpoints.
    mock.mock_authenticated_media_config().ok_default().mount().await;
    mock.mock_room_state_encryption().plain().mount().await;

    // File upload will succeed immediately.
    mock.mock_upload()
        .ok(mxc_uri!("mxc://sdk.rs/media"))
        .mock_once()
        .named("file upload")
        .mount()
        .await;

    // Sending of the media event will take 1 second, so we can abort it while it's
    // happening.
    mock.mock_room_send()
        .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(1)).set_body_json(
            json!({
              "event_id": "$media"
            }),
        ))
        .mock_once()
        .named("send event")
        .mock_once()
        .mount()
        .await;

    // A redaction will happen because the abort happens after the event is getting
    // sent.
    mock.mock_room_redact().ok(event_id!("$redaction")).mock_once().mount().await;

    // Send the media.
    assert!(watch.is_empty());

    let (upload_handle, filename) = queue_attachment_no_thumbnail(&q).await;

    let (upload_txn, _send_handle, content) =
        assert_update!((global_watch, watch) => local echo event);
    assert_let!(MessageType::Image(local_content) = content.msgtype);
    assert_eq!(local_content.filename(), filename);

    assert_update!((global_watch, watch) => uploaded { related_to = upload_txn, mxc = mxc_uri!("mxc://sdk.rs/media") });

    let edit_msg = assert_update!((global_watch, watch) => edit local echo { txn = upload_txn });
    assert_let!(MessageType::Image(remote_content) = edit_msg.msgtype);
    assert_let!(MediaSource::Plain(new_uri) = &remote_content.source);
    assert_eq!(new_uri, mxc_uri!("mxc://sdk.rs/media"));

    // Let the upload request start.
    sleep(Duration::from_millis(250)).await;

    // Abort the upload.
    let aborted = upload_handle.abort().await.unwrap();
    assert!(aborted, "upload must have been aborted");

    // We get a local echo for the cancelled media event…
    assert_update!((global_watch, watch) => cancelled { txn = upload_txn });
    // …But the event is still sent, before getting redacted.
    assert_update!((global_watch, watch) => sent { txn = upload_txn, });

    // The event cache doesn't contain the media with the local URI.
    client
        .media()
        .get_media_content(
            &MediaRequestParameters { source: local_content.source, format: MediaFormat::File },
            true,
        )
        .await
        .unwrap_err();

    // But it does contain the media with the remote URI, which hasn't been removed
    // from the remote server.
    client
        .media()
        .get_media_content(
            &MediaRequestParameters { source: remote_content.source, format: MediaFormat::File },
            true,
        )
        .await
        .unwrap();

    // Let things settle (and the redaction endpoint be called).
    sleep(Duration::from_secs(1)).await;

    // Trying to abort after it's been sent/redacted is a no-op
    let aborted = upload_handle.abort().await.unwrap();
    assert!(aborted.not());

    // That's all, folks!
    assert!(watch.is_empty());
}

#[async_test]
async fn test_update_caption_while_sending_media() {
    let mock = MatrixMockServer::new().await;

    // Mark the room as joined.
    let room_id = room_id!("!a:b.c");
    let client = mock.client_builder().build().await;
    let room = mock.sync_joined_room(&client, room_id).await;

    let q = room.send_queue();
    let mut global_watch = client.send_queue().subscribe();

    let (local_echoes, mut watch) = q.subscribe().await.unwrap();
    assert!(local_echoes.is_empty());

    // Prepare endpoints.
    mock.mock_authenticated_media_config().ok_default().mount().await;
    mock.mock_room_state_encryption().plain().mount().await;

    // File upload will take a second.
    mock.mock_upload()
        .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(1)).set_body_json(
            json!({
              "content_uri": "mxc://sdk.rs/media"
            }),
        ))
        .mock_once()
        .named("file upload")
        .mount()
        .await;

    // Sending of the media event will succeed.
    mock.mock_room_send()
        .ok(event_id!("$media"))
        .mock_once()
        .named("send event")
        .mock_once()
        .mount()
        .await;

    // Send the media.
    assert!(watch.is_empty());

    let (upload_handle, filename) = queue_attachment_no_thumbnail(&q).await;

    let (upload_txn, _send_handle, content) =
        assert_update!((global_watch, watch) => local echo event);
    assert_let!(MessageType::Image(local_content) = content.msgtype);
    assert_eq!(local_content.filename(), filename);

    // We can edit the caption while the file is being uploaded.
    let edited =
        upload_handle.edit_media_caption(Some("caption".to_owned()), None, None).await.unwrap();
    assert!(edited);

    {
        let new_content =
            assert_update!((global_watch, watch) => edit local echo { txn = upload_txn });
        assert_let!(MessageType::Image(image) = new_content.msgtype);
        assert_eq!(image.filename(), filename);
        assert_eq!(image.caption(), Some("caption"));
        assert!(image.formatted_caption().is_none());
    }

    // Then the media is uploaded.
    sleep(Duration::from_secs(1)).await;
    assert_update!((global_watch, watch) => uploaded { related_to = upload_txn, mxc = mxc_uri!("mxc://sdk.rs/media") });

    // Then the media event is updated with the MXC ID.
    {
        let edit_msg =
            assert_update!((global_watch, watch) => edit local echo { txn = upload_txn });
        assert_let!(MessageType::Image(image) = edit_msg.msgtype);
        assert_let!(MediaSource::Plain(new_uri) = &image.source);
        assert_eq!(new_uri, mxc_uri!("mxc://sdk.rs/media"));

        // Still has the new caption.
        assert_eq!(image.filename(), filename);
        assert_eq!(image.caption(), Some("caption"));
        assert!(image.formatted_caption().is_none());
    }

    // Then the event is sent.
    assert_update!((global_watch, watch) => sent { txn = upload_txn, });

    // That's all, folks!
    assert!(watch.is_empty());
}

#[async_test]
async fn test_update_caption_before_event_is_sent() {
    let mock = MatrixMockServer::new().await;

    mock.mock_authenticated_media_config().ok_default().mount().await;

    // Mark the room as joined.
    let room_id = room_id!("!a:b.c");
    let client = mock.client_builder().build().await;
    let room = mock.sync_joined_room(&client, room_id).await;

    let q = room.send_queue();
    let mut global_watch = client.send_queue().subscribe();

    let (local_echoes, mut watch) = q.subscribe().await.unwrap();
    assert!(local_echoes.is_empty());

    // Prepare endpoints.
    mock.mock_room_state_encryption().plain().mount().await;

    // File upload will take a second.
    mock.mock_upload()
        .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(1)).set_body_json(
            json!({
              "content_uri": "mxc://sdk.rs/media"
            }),
        ))
        .mock_once()
        .named("file upload")
        .mount()
        .await;

    // Sending of the media event will succeed.
    mock.mock_room_send()
        .ok(event_id!("$media"))
        .mock_once()
        .named("send event")
        .mock_once()
        .mount()
        .await;

    // Send the media.
    assert!(watch.is_empty());

    let (upload_handle, filename) = queue_attachment_no_thumbnail(&q).await;

    // Let the upload request start.
    sleep(Duration::from_millis(300)).await;

    // Stop the send queue before upload is done. This will stall sending of the
    // media event.
    q.set_enabled(false);

    let (upload_txn, _send_handle, content) =
        assert_update!((global_watch, watch) => local echo event);
    assert_let!(MessageType::Image(local_content) = content.msgtype);
    assert_eq!(local_content.filename(), filename);

    // Wait for the media to be uploaded.
    sleep(Duration::from_secs(1)).await;
    assert_update!((global_watch, watch) => uploaded { related_to = upload_txn, mxc = mxc_uri!("mxc://sdk.rs/media") });

    // The media event is updated with the remote MXC ID.
    let mxc = {
        let new_content =
            assert_update!((global_watch, watch) => edit local echo { txn = upload_txn });
        assert_let!(MessageType::Image(image) = new_content.msgtype);
        assert_eq!(image.filename(), filename);
        assert_eq!(image.caption(), None);
        assert!(image.formatted_caption().is_none());

        let mxc = as_variant!(image.source, MediaSource::Plain).unwrap();
        assert!(!mxc.to_string().starts_with("mxc://send-queue.localhost/"), "{mxc}");
        mxc
    };

    assert!(watch.is_empty());

    // We can edit the caption here.
    let edited =
        upload_handle.edit_media_caption(Some("caption".to_owned()), None, None).await.unwrap();
    assert!(edited);

    // The media event is updated with the captions.
    {
        let edit_msg =
            assert_update!((global_watch, watch) => edit local echo { txn = upload_txn });
        assert_let!(MessageType::Image(image) = edit_msg.msgtype);

        assert_eq!(image.filename(), filename);
        assert_eq!(image.caption(), Some("caption"));
        assert!(image.formatted_caption().is_none());

        // But kept the mxc.
        let new_mxc = as_variant!(image.source, MediaSource::Plain).unwrap();
        assert_eq!(new_mxc, mxc);
    }

    // Re-enable the send queue.
    q.set_enabled(true);

    // Then the event is sent.
    assert_update!((global_watch, watch) => sent { txn = upload_txn, });

    // That's all, folks!
    assert!(watch.is_empty());
}

#[async_test]
async fn test_add_mention_to_caption_before_media_sent() {
    let mock = MatrixMockServer::new().await;

    // Mark the room as joined.
    let room_id = room_id!("!a:b.c");
    let client = mock.client_builder().build().await;
    let room = mock.sync_joined_room(&client, room_id).await;

    let q = room.send_queue();
    let mut global_watch = client.send_queue().subscribe();

    let (local_echoes, mut watch) = q.subscribe().await.unwrap();
    assert!(local_echoes.is_empty());

    // Prepare endpoints.
    mock.mock_authenticated_media_config().ok_default().mount().await;
    mock.mock_room_state_encryption().plain().mount().await;

    // File upload will take a second.
    mock.mock_upload()
        .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(1)).set_body_json(
            json!({
              "content_uri": "mxc://sdk.rs/media"
            }),
        ))
        .mock_once()
        .named("file upload")
        .mount()
        .await;

    // Sending of the media event will succeed.
    mock.mock_room_send()
        .ok(event_id!("$media"))
        .mock_once()
        .named("send event")
        .mock_once()
        .mount()
        .await;

    // Send the media.
    assert!(watch.is_empty());

    let (upload_handle, filename) = queue_attachment_no_thumbnail(&q).await;

    // Let the upload request start.
    sleep(Duration::from_millis(300)).await;

    // Stop the send queue before upload is done. This will stall sending of the
    // media event.
    q.set_enabled(false);

    let (upload_txn, _send_handle, content) =
        assert_update!((global_watch, watch) => local echo event);
    assert_let!(MessageType::Image(local_content) = content.msgtype);
    assert_eq!(local_content.filename(), filename);

    // Wait for the media to be uploaded.
    sleep(Duration::from_secs(1)).await;
    assert_update!((global_watch, watch) => uploaded { related_to = upload_txn, mxc = mxc_uri!("mxc://sdk.rs/media") });

    // The media event is updated with the remote MXC ID.
    {
        let new_content =
            assert_update!((global_watch, watch) => edit local echo { txn = upload_txn });
        assert_let!(MessageType::Image(image) = new_content.msgtype);
        assert_eq!(image.filename(), filename);
        assert_eq!(image.caption(), None);
        assert!(image.formatted_caption().is_none());

        let mxc = as_variant!(image.source, MediaSource::Plain).unwrap();
        assert!(!mxc.to_string().starts_with("mxc://send-queue.localhost/"), "{mxc}");
    };

    assert!(watch.is_empty());

    // We can edit the caption here.
    let mentioned_user_id = owned_user_id!("@damir:rust.sdk");
    let mentions = Mentions::with_user_ids([mentioned_user_id.clone()]);
    let edited = upload_handle
        .edit_media_caption(Some("caption".to_owned()), None, Some(mentions))
        .await
        .unwrap();
    assert!(edited);

    // The media event is updated with the captions, including the mention.
    {
        let edit_msg =
            assert_update!((global_watch, watch) => edit local echo { txn = upload_txn });
        assert_let!(Some(mentions) = edit_msg.mentions);
        assert!(!mentions.room);
        assert_eq!(mentions.user_ids.into_iter().collect::<Vec<_>>(), vec![mentioned_user_id]);
    }

    // Re-enable the send queue.
    q.set_enabled(true);

    // Then the event is sent.
    assert_update!((global_watch, watch) => sent { txn = upload_txn, });

    // That's all, folks!
    assert!(watch.is_empty());
}

#[async_test]
async fn test_update_caption_while_sending_media_event() {
    let mock = MatrixMockServer::new().await;

    // Mark the room as joined.
    let room_id = room_id!("!a:b.c");
    let client = mock.client_builder().build().await;
    let room = mock.sync_joined_room(&client, room_id).await;

    let q = room.send_queue();
    let mut global_watch = client.send_queue().subscribe();

    let (local_echoes, mut watch) = q.subscribe().await.unwrap();
    assert!(local_echoes.is_empty());

    // Prepare endpoints.
    mock.mock_authenticated_media_config().ok_default().mount().await;
    mock.mock_room_state_encryption().plain().mount().await;

    // File upload will resolve immediately.
    mock.mock_upload()
        .ok(mxc_uri!("mxc://sdk.rs/media"))
        .mock_once()
        .named("file upload")
        .mount()
        .await;

    // Sending of the media event will take one second.
    mock.mock_room_send()
        .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(1)).set_body_json(
            json!({
                "event_id": "$1"
            }),
        ))
        .mock_once()
        .named("send event")
        .mock_once()
        .mount()
        .await;

    // There will be an edit event sent too; this one doesn't need to wait.
    mock.mock_room_send()
        .ok(event_id!("$edit"))
        .mock_once()
        .named("edit event")
        .mock_once()
        .mount()
        .await;

    // The /event endpoint is used to retrieve the original event, during creation
    // of the edit event.
    mock.mock_room_event()
        .room(room_id)
        .ok(EventFactory::new()
            .image("surprise.jpeg.exe".to_owned(), owned_mxc_uri!("mxc://sdk.rs/media"))
            .sender(client.user_id().unwrap())
            .room(room_id)
            .into_event())
        .expect(1)
        .named("room_event")
        .mount()
        .await;

    // Send the media.
    assert!(watch.is_empty());

    let (upload_handle, filename) = queue_attachment_no_thumbnail(&q).await;

    // See local echo.
    let (upload_txn, _send_handle, content) =
        assert_update!((global_watch, watch) => local echo event);
    assert_let!(MessageType::Image(local_content) = content.msgtype);
    assert_eq!(local_content.filename(), filename);

    // Wait for the media to be uploaded.
    assert_update!((global_watch, watch) => uploaded { related_to = upload_txn, mxc = mxc_uri!("mxc://sdk.rs/media") });

    // The media event is updated with the remote MXC ID.
    let mxc = {
        let new_content =
            assert_update!((global_watch, watch) => edit local echo { txn = upload_txn });
        assert_let!(MessageType::Image(image) = new_content.msgtype);
        assert_eq!(image.filename(), filename);
        assert_eq!(image.caption(), None);
        assert!(image.formatted_caption().is_none());

        let mxc = as_variant!(image.source, MediaSource::Plain).unwrap();
        assert!(!mxc.to_string().starts_with("mxc://send-queue.localhost/"), "{mxc}");
        mxc
    };

    // We can edit the caption while the event is beint sent.
    let edited =
        upload_handle.edit_media_caption(Some("caption".to_owned()), None, None).await.unwrap();
    assert!(edited);

    // The media event is updated with the captions.
    {
        let edit_msg =
            assert_update!((global_watch, watch) => edit local echo { txn = upload_txn });
        assert_let!(MessageType::Image(image) = edit_msg.msgtype);

        assert_eq!(image.filename(), filename);
        assert_eq!(image.caption(), Some("caption"));
        assert!(image.formatted_caption().is_none());

        // But kept the mxc.
        let new_mxc = as_variant!(image.source, MediaSource::Plain).unwrap();
        assert_eq!(new_mxc, mxc);
    }

    // Then the event is sent.
    sleep(Duration::from_secs(1)).await;
    assert_update!((global_watch, watch) => sent { txn = upload_txn, });

    // Then the edit event is set, with another transaction id we don't know about.
    assert_update!((global_watch, watch) => sent {});

    // That's all, folks!
    assert!(watch.is_empty());
}

#[async_test]
async fn test_sending_reply_in_thread_auto_subscribe() {
    let server = MatrixMockServer::new().await;

    // Assuming a client that's interested in thread subscriptions,
    let client = server
        .client_builder()
        .on_builder(|builder| {
            builder.with_threading_support(ThreadingSupport::Enabled { with_subscriptions: true })
        })
        .build()
        .await;

    client.event_cache().subscribe().unwrap();

    let mut thread_subscriber_updates = client.event_cache().subscribe_thread_subscriber_updates();

    let room_id = room_id!("!a:b.c");
    let room = server.sync_joined_room(&client, room_id).await;

    server.mock_room_state_encryption().plain().mount().await;

    // When I send a message to a thread,
    let thread_root = event_id!("$thread");

    let mut content = RoomMessageEventContent::text_plain("hello world");
    content.relates_to =
        Some(Relation::Thread(Thread::plain(thread_root.to_owned(), thread_root.to_owned())));

    server.mock_room_send().ok(event_id!("$reply")).mock_once().mount().await;

    server
        .mock_room_put_thread_subscription()
        .match_room_id(room_id.to_owned())
        .match_thread_id(thread_root.to_owned())
        .ok()
        .mock_once()
        .mount()
        .await;

    let (_, mut stream) = room.send_queue().subscribe().await.unwrap();
    room.send_queue().send(content.into()).await.unwrap();

    // Let the send queue process the event.
    assert_let_timeout!(Ok(RoomSendQueueUpdate::NewLocalEvent(..)) = stream.recv());
    assert_let_timeout!(Ok(RoomSendQueueUpdate::SentEvent { .. }) = stream.recv());
    assert_let_timeout!(Ok(()) = thread_subscriber_updates.recv());

    // Check the endpoints have been correctly called.
    server.server().reset().await;

    // Now, if I send a message in a thread I've already subscribed to, in automatic
    // mode, this promotes the subscription to manual.

    // Subscribed, automatically.
    server
        .mock_room_get_thread_subscription()
        .match_room_id(room_id.to_owned())
        .match_thread_id(thread_root.to_owned())
        .ok(true)
        .mount()
        .await;

    // I'll get one subscription.
    server
        .mock_room_put_thread_subscription()
        .match_room_id(room_id.to_owned())
        .match_thread_id(thread_root.to_owned())
        .ok()
        .mock_once()
        .mount()
        .await;

    server.mock_room_send().ok(event_id!("$reply")).mock_once().mount().await;

    let mut content = RoomMessageEventContent::text_plain("hello world");
    content.relates_to =
        Some(Relation::Thread(Thread::plain(thread_root.to_owned(), thread_root.to_owned())));
    room.send_queue().send(content.into()).await.unwrap();

    // Let the send queue process the event.
    assert_let_timeout!(Ok(RoomSendQueueUpdate::NewLocalEvent(..)) = stream.recv());
    assert_let_timeout!(Ok(RoomSendQueueUpdate::SentEvent { .. }) = stream.recv());
    assert_let_timeout!(Ok(()) = thread_subscriber_updates.recv());

    // Check the endpoints have been correctly called.
    server.server().reset().await;

    // Subscribed, but manually.
    server
        .mock_room_get_thread_subscription()
        .match_room_id(room_id.to_owned())
        .match_thread_id(thread_root.to_owned())
        .ok(false)
        .mount()
        .await;

    // I'll get zero subscription.
    server.mock_room_put_thread_subscription().ok().expect(0).mount().await;

    server.mock_room_send().ok(event_id!("$reply")).mock_once().mount().await;

    let mut content = RoomMessageEventContent::text_plain("hello world");
    content.relates_to =
        Some(Relation::Thread(Thread::plain(thread_root.to_owned(), thread_root.to_owned())));
    room.send_queue().send(content.into()).await.unwrap();

    // Let the send queue process the event.
    assert_let_timeout!(Ok(RoomSendQueueUpdate::NewLocalEvent(..)) = stream.recv());
    assert_let_timeout!(Ok(RoomSendQueueUpdate::SentEvent { .. }) = stream.recv());
    assert_let_timeout!(Ok(()) = thread_subscriber_updates.recv());

    sleep(Duration::from_millis(100)).await;
}
