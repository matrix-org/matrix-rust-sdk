use std::{
    ops::Not as _,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex as StdMutex,
    },
    time::Duration,
};

use assert_matches2::{assert_let, assert_matches};
use matrix_sdk::{
    config::{RequestConfig, StoreConfig},
    send_queue::{LocalEcho, LocalEchoContent, RoomSendQueueError, RoomSendQueueUpdate},
    test_utils::{
        events::EventFactory, logged_in_client, logged_in_client_with_server, set_client_session,
    },
    Client, MemoryStore,
};
use matrix_sdk_test::{
    async_test,
    mocks::{mock_encryption_state, mock_redaction},
    InvitedRoomBuilder, JoinedRoomBuilder, LeftRoomBuilder,
};
use ruma::{
    api::MatrixVersion,
    event_id,
    events::{
        room::message::RoomMessageEventContent, AnyMessageLikeEventContent, EventContent as _,
    },
    room_id,
    serde::Raw,
    EventId, OwnedEventId,
};
use serde_json::json;
use tokio::{
    sync::Mutex,
    time::{sleep, timeout},
};
use wiremock::{
    matchers::{header, method, path_regex},
    Mock, Request, ResponseTemplate,
};

use crate::mock_sync_with_new_room;

fn mock_send_event(returned_event_id: &EventId) -> Mock {
    Mock::given(method("PUT"))
        .and(path_regex(r"^/_matrix/client/r0/rooms/.*/send/.*"))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "event_id": returned_event_id,
        })))
}

/// Return a mock that will fail all requests to /rooms/ROOM_ID/send with a
/// transient 500 error.
fn mock_send_transient_failure() -> Mock {
    Mock::given(method("PUT"))
        .and(path_regex(r"^/_matrix/client/r0/rooms/.*/send/.*"))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(500))
}

// A macro to assert on a stream of `RoomSendQueueUpdate`s.
macro_rules! assert_update {
    // Check the next stream event is a local echo for a message with the content $body.
    // Returns a tuple of (transaction_id, send_handle).
    ($watch:ident => local echo { body = $body:expr }) => {{
        assert_let!(
            Ok(Ok(RoomSendQueueUpdate::NewLocalEvent(LocalEcho {
                content: LocalEchoContent::Event {
                    serialized_event,
                    send_handle,
                    // New local echoes should always start as not wedged.
                    is_wedged: false,
                },
                transaction_id: txn,
            }))) = timeout(Duration::from_secs(1), $watch.recv()).await
        );

        let content = serialized_event.deserialize().unwrap();
        assert_let!(AnyMessageLikeEventContent::RoomMessage(_msg) = content);
        assert_eq!(_msg.body(), $body);

        (txn, send_handle)
    }};

    // Check the next stream event is a local echo for a reaction with the content $key which
    // applies to the local echo with transaction id $parent.
    ($watch:ident => local reaction { key = $key:expr, parent = $parent_txn_id:expr }) => {{
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

        assert_eq!(key, $key);
        assert_eq!(applies_to, $parent_txn_id);

        txn
    }};

    // Check the next stream event is an edit for a local echo with the content $body, and that the
    // transaction id is the one we expect.
    ($watch:ident => edit { body = $body:expr, txn = $transaction_id:expr }) => {{
        assert_let!(
            Ok(Ok(RoomSendQueueUpdate::ReplacedLocalEvent {
                transaction_id: txn,
                new_content: serialized_event,
            })) = timeout(Duration::from_secs(1), $watch.recv()).await
        );

        let content = serialized_event.deserialize().unwrap();
        assert_let!(AnyMessageLikeEventContent::RoomMessage(_msg) = content);
        assert_eq!(_msg.body(), $body);

        assert_eq!(txn, $transaction_id);
    }};

    // Check the next stream event is a sent event, with optional checks on txn=$txn and
    // event_id=$event_id.
    ($watch:ident => sent { $(txn=$txn:expr,)? $(event_id=$event_id:expr)? }) => {
        assert_let!(
            Ok(Ok(RoomSendQueueUpdate::SentEvent { event_id: _event_id, transaction_id: _txn })) =
                timeout(Duration::from_secs(1), $watch.recv()).await
        );

        $(assert_eq!(_event_id, $event_id);)?
        $(assert_eq!(_txn, $txn);)?
    };

    // Check the next stream event is a send error, with optional assertions on the recoverable
    // status and transaction id.
    //
    // Returns the error for additional checks.
    ($watch:ident => error { $(recoverable=$recoverable:expr,)? $(txn=$txn:expr)? }) => {{
        assert_let!(
            Ok(Ok(RoomSendQueueUpdate::SendError { transaction_id: _txn, error, is_recoverable: _is_recoverable })) =
                timeout(Duration::from_secs(10), $watch.recv()).await
        );

        $(assert_eq!(_txn, $txn);)?
        $(assert_eq!(_is_recoverable, $recoverable);)?

        error
    }};

    // Check the next stream event is a cancelled local echo for the given transaction id.
    ($watch:ident => cancelled { txn = $txn:expr }) => {{
        assert_let!(
            Ok(Ok(RoomSendQueueUpdate::CancelledLocalEvent { transaction_id: txn })) =
                timeout(Duration::from_secs(10), $watch.recv()).await
        );
        assert_eq!(txn, $txn);
    }};
}

#[async_test]
async fn test_cant_send_invited_room() {
    let (client, server) = logged_in_client_with_server().await;

    // When I'm invited to a room,
    let room_id = room_id!("!a:b.c");

    let room = mock_sync_with_new_room(
        |builder| {
            builder.add_invited_room(InvitedRoomBuilder::new(room_id));
        },
        &client,
        &server,
        room_id,
    )
    .await;

    // I can't send message to it with the send queue.
    assert_matches!(
        room.send_queue().send(RoomMessageEventContent::text_plain("Hello, World!").into()).await,
        Err(RoomSendQueueError::RoomNotJoined)
    );
}

#[async_test]
async fn test_cant_send_left_room() {
    let (client, server) = logged_in_client_with_server().await;

    // When I've left a room,
    let room_id = room_id!("!a:b.c");

    let room = mock_sync_with_new_room(
        |builder| {
            builder.add_left_room(LeftRoomBuilder::new(room_id));
        },
        &client,
        &server,
        room_id,
    )
    .await;

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
    let (client, server) = logged_in_client_with_server().await;

    // Mark the room as joined.
    let room_id = room_id!("!a:b.c");

    let room = mock_sync_with_new_room(
        |builder| {
            builder.add_joined_room(JoinedRoomBuilder::new(room_id));
        },
        &client,
        &server,
        room_id,
    )
    .await;

    // When I disable the send queue,
    let event_id = event_id!("$1");
    mock_send_event(event_id).expect(0).mount(&server).await;

    client.send_queue().set_enabled(false).await;

    // A message is queued, but never sent.
    room.send_queue()
        .send(RoomMessageEventContent::text_plain("Hello, World!").into())
        .await
        .unwrap();

    // But I can still send it with room.send().
    server.verify().await;
    server.reset().await;

    mock_encryption_state(&server, false).await;
    mock_send_event(event_id).expect(1).mount(&server).await;

    let response = room.send(RoomMessageEventContent::text_plain("Hello, World!")).await.unwrap();
    assert_eq!(response.event_id, event_id);
}

#[async_test]
async fn test_smoke() {
    let (client, server) = logged_in_client_with_server().await;

    // Mark the room as joined.
    let room_id = room_id!("!a:b.c");

    let room = mock_sync_with_new_room(
        |builder| {
            builder.add_joined_room(JoinedRoomBuilder::new(room_id));
        },
        &client,
        &server,
        room_id,
    )
    .await;

    let q = room.send_queue();

    let (local_echoes, mut watch) = q.subscribe().await.unwrap();
    assert!(local_echoes.is_empty());
    assert!(watch.is_empty());

    // When the queue is enabled and I send message in some order, it does send it.
    let event_id = event_id!("$1");

    let lock = Arc::new(Mutex::new(()));
    let lock_guard = lock.lock().await;

    let mock_lock = lock.clone();

    mock_encryption_state(&server, false).await;

    Mock::given(method("PUT"))
        .and(path_regex(r"^/_matrix/client/r0/rooms/.*/send/.*"))
        .and(header("authorization", "Bearer 1234"))
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
        .mount(&server)
        .await;

    room.send_queue().send(RoomMessageEventContent::text_plain("1").into()).await.unwrap();

    let (txn1, _) = assert_update!(watch => local echo { body = "1" });

    {
        let (local_echoes, _) = q.subscribe().await.unwrap();

        assert_eq!(local_echoes.len(), 1);
        assert_eq!(local_echoes[0].transaction_id, txn1);
    }

    assert!(watch.is_empty());

    drop(lock_guard);

    assert_update!(watch => sent { txn = txn1, event_id = event_id });

    assert!(watch.is_empty());
}

#[async_test]
async fn test_smoke_raw() {
    let (client, server) = logged_in_client_with_server().await;

    // Mark the room as joined.
    let room_id = room_id!("!a:b.c");

    let room = mock_sync_with_new_room(
        |builder| {
            builder.add_joined_room(JoinedRoomBuilder::new(room_id));
        },
        &client,
        &server,
        room_id,
    )
    .await;

    let q = room.send_queue();

    let (local_echoes, mut watch) = q.subscribe().await.unwrap();
    assert!(local_echoes.is_empty());
    assert!(watch.is_empty());

    // When the queue is enabled and I send message in some order, it does send it.
    let event_id = event_id!("$1");

    mock_encryption_state(&server, false).await;
    mock_send_event(event_id!("$1")).mount(&server).await;

    let json_content = r#"{"baguette": 42}"#.to_owned();
    let event = Raw::from_json_string(json_content.clone()).unwrap();
    room.send_queue().send_raw(event, "m.room.frenchie".to_owned()).await.unwrap();

    assert_let!(
        Ok(Ok(RoomSendQueueUpdate::NewLocalEvent(LocalEcho {
            content: LocalEchoContent::Event { serialized_event, .. },
            transaction_id: txn1,
        }))) = timeout(Duration::from_secs(1), watch.recv()).await
    );

    let content = serialized_event.deserialize().unwrap();
    assert_matches!(&content, AnyMessageLikeEventContent::_Custom { .. });
    assert_eq!(content.event_type().to_string(), "m.room.frenchie");

    let (raw, event_type) = serialized_event.raw();
    assert_eq!(event_type, "m.room.frenchie");
    assert_eq!(raw.json().to_string(), json_content);

    assert_update!(watch => sent { txn = txn1, event_id = event_id });

    assert!(watch.is_empty());
}

#[async_test]
async fn test_error_then_locally_reenabling() {
    let (client, server) = logged_in_client_with_server().await;

    let mut errors = client.send_queue().subscribe_errors();

    // Starting with a globally enabled queue.
    assert!(client.send_queue().is_enabled());
    assert!(errors.is_empty());

    // Mark the room as joined.
    let room_id = room_id!("!a:b.c");

    let room = mock_sync_with_new_room(
        |builder| {
            builder.add_joined_room(JoinedRoomBuilder::new(room_id));
        },
        &client,
        &server,
        room_id,
    )
    .await;

    let q = room.send_queue();

    let (local_echoes, mut watch) = q.subscribe().await.unwrap();
    assert!(local_echoes.is_empty());
    assert!(watch.is_empty());

    let lock = Arc::new(Mutex::new(()));
    let lock_guard = lock.lock().await;

    let mock_lock = lock.clone();

    mock_encryption_state(&server, false).await;

    Mock::given(method("PUT"))
        .and(path_regex(r"^/_matrix/client/r0/rooms/.*/send/.*"))
        .and(header("authorization", "Bearer 1234"))
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
        .mount(&server)
        .await;

    q.send(RoomMessageEventContent::text_plain("1").into()).await.unwrap();

    let (txn1, _) = assert_update!(watch => local echo { body = "1" });

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
    let error = assert_update!(watch => error { recoverable=true, txn=txn1 });
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

    server.reset().await;
    mock_send_event(event_id!("$42")).expect(1).mount(&server).await;

    // Re-enabling the *room* queue will re-send the same message in that room.
    room.send_queue().set_enabled(true);

    assert!(errors.is_empty());

    assert!(client.send_queue().is_enabled());
    assert!(room.send_queue().is_enabled());

    assert_update!(watch => sent { txn=txn1, event_id=event_id!("$42") });

    assert!(errors.is_empty());
    assert!(watch.is_empty());
}

#[async_test]
async fn test_error_then_globally_reenabling() {
    let (client, server) = logged_in_client_with_server().await;

    let mut errors = client.send_queue().subscribe_errors();

    // Starting with a globally enabled queue.
    assert!(client.send_queue().is_enabled());
    assert!(errors.is_empty());

    // Mark the room as joined.
    let room_id = room_id!("!a:b.c");

    let room = mock_sync_with_new_room(
        |builder| {
            builder.add_joined_room(JoinedRoomBuilder::new(room_id));
        },
        &client,
        &server,
        room_id,
    )
    .await;

    let q = room.send_queue();

    let (local_echoes, mut watch) = q.subscribe().await.unwrap();
    assert!(local_echoes.is_empty());
    assert!(watch.is_empty());

    server.reset().await;
    mock_encryption_state(&server, false).await;
    mock_send_transient_failure().expect(3).mount(&server).await;

    q.send(RoomMessageEventContent::text_plain("1").into()).await.unwrap();

    let (txn1, _) = assert_update!(watch => local echo { body = "1" });

    assert!(watch.is_empty());

    // We receive an error.
    let report = errors.recv().await.unwrap();
    assert_eq!(report.room_id, room.room_id());
    assert!(report.is_recoverable);

    // The exponential backoff used when retrying a request introduces a bit of
    // non-determinism, so let it fail after a large amount of time (10
    // seconds).
    // It's the same transaction id that's used to signal the send error.
    assert_update!(watch => error { txn=txn1 });

    // The send queue is still globally enabled,
    assert!(client.send_queue().is_enabled());
    // But the room send queue is disabled.
    assert!(!room.send_queue().is_enabled());

    assert!(watch.is_empty());

    server.reset().await;
    mock_encryption_state(&server, false).await;
    mock_send_event(event_id!("$42")).expect(1).mount(&server).await;

    // Re-enabling the global queue will cause the event to be sent.
    client.send_queue().set_enabled(true).await;

    assert!(client.send_queue().is_enabled());
    assert!(room.send_queue().is_enabled());

    assert_update!(watch => sent { txn=txn1, event_id=event_id!("$42") });

    assert!(errors.is_empty());
    assert!(watch.is_empty());
}

#[async_test]
async fn test_reenabling_queue() {
    let (client, server) = logged_in_client_with_server().await;

    // Mark the room as joined.
    let room_id = room_id!("!a:b.c");

    let room = mock_sync_with_new_room(
        |builder| {
            builder.add_joined_room(JoinedRoomBuilder::new(room_id));
        },
        &client,
        &server,
        room_id,
    )
    .await;

    let errors = client.send_queue().subscribe_errors();

    assert!(errors.is_empty());

    // When I start with a disabled send queue,
    client.send_queue().set_enabled(false).await;

    assert!(!client.send_queue().is_enabled());
    assert!(!room.send_queue().is_enabled());
    assert!(errors.is_empty());

    let q = room.send_queue();

    let (local_echoes, mut watch) = q.subscribe().await.unwrap();

    assert!(local_echoes.is_empty());
    assert!(watch.is_empty());

    // Three messages are queued.
    q.send(RoomMessageEventContent::text_plain("msg1").into()).await.unwrap();
    q.send(RoomMessageEventContent::text_plain("msg2").into()).await.unwrap();
    q.send(RoomMessageEventContent::text_plain("msg3").into()).await.unwrap();

    for i in 1..=3 {
        assert_update!(watch => local echo { body = format!("msg{i}") });
    }

    {
        let (local_echoes, _) = q.subscribe().await.unwrap();
        assert_eq!(local_echoes.len(), 3);
    }

    assert!(watch.is_empty());

    // Messages aren't sent immediately.
    sleep(Duration::from_millis(500)).await;

    assert!(watch.is_empty());

    mock_encryption_state(&server, false).await;

    let num_request = std::sync::Mutex::new(1);
    Mock::given(method("PUT"))
        .and(path_regex(r"^/_matrix/client/r0/rooms/.*/send/.*"))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(move |_req: &Request| {
            let mut num_request = num_request.lock().unwrap();

            let event_id = format!("${}", *num_request);
            *num_request += 1;

            ResponseTemplate::new(200).set_body_json(json!({
                "event_id": event_id,
            }))
        })
        .expect(3)
        .mount(&server)
        .await;

    // But when reenabling the queue globally,
    client.send_queue().set_enabled(true).await;

    assert!(client.send_queue().is_enabled());
    assert!(room.send_queue().is_enabled());
    assert!(errors.is_empty());

    // They're sent, in the same order.
    for i in 1..=3 {
        let event_id = OwnedEventId::try_from(format!("${i}").as_str()).unwrap();
        assert_update!(watch => sent { event_id = event_id });
    }

    assert!(errors.is_empty());
    assert!(watch.is_empty());
}

#[async_test]
async fn test_disjoint_enabled_status() {
    let (client, server) = logged_in_client_with_server().await;

    // Mark the room as joined.
    let room_id1 = room_id!("!a:b.c");
    let room_id2 = room_id!("!b:b.c");
    let room1 = mock_sync_with_new_room(
        |builder| {
            builder
                .add_joined_room(JoinedRoomBuilder::new(room_id1))
                .add_joined_room(JoinedRoomBuilder::new(room_id2));
        },
        &client,
        &server,
        room_id1,
    )
    .await;
    let room2 = client.get_room(room_id2).unwrap();

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
    let (client, server) = logged_in_client_with_server().await;

    // Mark the room as joined.
    let room_id = room_id!("!a:b.c");

    let room = mock_sync_with_new_room(
        |builder| {
            builder.add_joined_room(JoinedRoomBuilder::new(room_id));
        },
        &client,
        &server,
        room_id,
    )
    .await;

    let q = room.send_queue();

    let (local_echoes, mut watch) = q.subscribe().await.unwrap();

    assert!(local_echoes.is_empty());
    assert!(watch.is_empty());

    let lock = Arc::new(Mutex::new(()));
    let lock_guard = lock.lock().await;

    let mock_lock = lock.clone();

    mock_encryption_state(&server, false).await;

    let num_request = std::sync::Mutex::new(1);
    Mock::given(method("PUT"))
        .and(path_regex(r"^/_matrix/client/r0/rooms/.*/send/.*"))
        .and(header("authorization", "Bearer 1234"))
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
        .mount(&server)
        .await;

    // The redact of txn1 will happen because we asked for it previously.
    mock_redaction(event_id!("$1")).expect(1).mount(&server).await;

    let handle1 = q.send(RoomMessageEventContent::text_plain("msg1").into()).await.unwrap();
    let handle2 = q.send(RoomMessageEventContent::text_plain("msg2").into()).await.unwrap();
    q.send(RoomMessageEventContent::text_plain("msg3").into()).await.unwrap();
    q.send(RoomMessageEventContent::text_plain("msg4").into()).await.unwrap();
    q.send(RoomMessageEventContent::text_plain("msg5").into()).await.unwrap();

    // Receiving updates for local echoes.
    let (txn1, _) = assert_update!(watch => local echo { body = "msg1" });
    let (txn2, _) = assert_update!(watch => local echo { body = "msg2" });
    let (txn3, handle3) = assert_update!(watch => local echo { body = "msg3" });
    let (txn4, _) = assert_update!(watch => local echo { body = "msg4" });
    let (txn5, _) = assert_update!(watch => local echo { body = "msg5" });
    assert!(watch.is_empty());

    // Let the background task start now.
    tokio::task::yield_now().await;

    // While the first item is being sent, the system records the intent to abort
    // it.
    assert!(handle1.abort().await.unwrap());
    assert_update!(watch => cancelled { txn = txn1 });
    assert!(watch.is_empty());

    // The second item is pending, so we can abort it, using the handle returned by
    // `send()`.
    assert!(handle2.abort().await.unwrap());
    assert_update!(watch => cancelled { txn = txn2 });
    assert!(watch.is_empty());

    // The third item is pending, so we can abort it, using the handle received from
    // the update.
    assert!(handle3.abort().await.unwrap());
    assert_update!(watch => cancelled { txn = txn3 });
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
    assert_update!(watch => cancelled { txn = txn4 });
    assert!(watch.is_empty());

    // Let the server process the responses.
    drop(lock_guard);

    // Now the server will process msg1 and msg5.
    assert_update!(watch => sent { txn = txn1, });
    assert_update!(watch => sent { txn = txn5, });
    assert!(watch.is_empty());
}

#[async_test]
async fn test_edit() {
    // Simplified version of test_cancellation: we don't test for *every single way*
    // to edit a local echo, since if the cancellation test passes, all ways
    // would work here too similarly.

    let (client, server) = logged_in_client_with_server().await;

    // TODO: (#3722) if the event cache isn't available, then making the edit event
    // will fail.
    client.event_cache().subscribe().unwrap();

    // Mark the room as joined.
    let room_id = room_id!("!a:b.c");

    let room = mock_sync_with_new_room(
        |builder| {
            builder.add_joined_room(JoinedRoomBuilder::new(room_id));
        },
        &client,
        &server,
        room_id,
    )
    .await;

    let q = room.send_queue();

    let (local_echoes, mut watch) = q.subscribe().await.unwrap();

    assert!(local_echoes.is_empty());
    assert!(watch.is_empty());

    let lock = Arc::new(Mutex::new(()));
    let lock_guard = lock.lock().await;

    let mock_lock = lock.clone();

    mock_encryption_state(&server, false).await;

    let num_request = std::sync::Mutex::new(1);
    Mock::given(method("PUT"))
        .and(path_regex(r"^/_matrix/client/r0/rooms/.*/send/.*"))
        .and(header("authorization", "Bearer 1234"))
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
        .mount(&server)
        .await;

    // The /event endpoint is used to retrieve the original event, during creation
    // of the edit event.
    Mock::given(method("GET"))
        .and(path_regex(r"^/_matrix/client/r0/rooms/.*/event/"))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(
                EventFactory::new()
                    .text_msg("msg1")
                    .sender(client.user_id().unwrap())
                    .room(room_id)
                    .into_raw_timeline()
                    .json(),
            ),
        )
        .expect(1)
        .named("get_event")
        .mount(&server)
        .await;

    let handle1 = q.send(RoomMessageEventContent::text_plain("msg1").into()).await.unwrap();
    let handle2 = q.send(RoomMessageEventContent::text_plain("msg2").into()).await.unwrap();

    // Receiving updates for local echoes.
    let (txn1, _) = assert_update!(watch => local echo { body = "msg1" });
    let (txn2, _) = assert_update!(watch => local echo { body = "msg2" });
    assert!(watch.is_empty());

    // Let the background task start now.
    tokio::task::yield_now().await;

    // While the first item is being sent, the system remembers the intent to edit
    // it, and will send it later.
    assert!(handle1
        .edit(RoomMessageEventContent::text_plain("it's never too late!").into())
        .await
        .unwrap());
    assert_update!(watch => edit { body = "it's never too late!", txn = txn1 });

    // The second item is pending, so we can edit it, using the handle returned by
    // `send()`.
    assert!(handle2
        .edit(RoomMessageEventContent::text_plain("new content, who diz").into())
        .await
        .unwrap());
    assert_update!(watch => edit { body = "new content, who diz", txn = txn2 });
    assert!(watch.is_empty());

    // Let the server process the responses.
    drop(lock_guard);

    // Now the server will process the messages in order.
    assert_update!(watch => sent { txn = txn1, });
    assert_update!(watch => sent { txn = txn2, });

    // Let a bit of time to process the edit event sent to the server for txn1.
    assert_update!(watch => sent {});

    assert!(watch.is_empty());
}

#[async_test]
async fn test_edit_while_being_sent_and_fails() {
    let (client, server) = logged_in_client_with_server().await;

    // TODO: (#3722) if the event cache isn't available, then making the edit event
    // will fail.
    client.event_cache().subscribe().unwrap();

    // Mark the room as joined.
    let room_id = room_id!("!a:b.c");

    let room = mock_sync_with_new_room(
        |builder| {
            builder.add_joined_room(JoinedRoomBuilder::new(room_id));
        },
        &client,
        &server,
        room_id,
    )
    .await;

    let q = room.send_queue();

    let (local_echoes, mut watch) = q.subscribe().await.unwrap();

    assert!(local_echoes.is_empty());
    assert!(watch.is_empty());

    let lock = Arc::new(Mutex::new(()));
    let lock_guard = lock.lock().await;

    let mock_lock = lock.clone();

    mock_encryption_state(&server, false).await;

    Mock::given(method("PUT"))
        .and(path_regex(r"^/_matrix/client/r0/rooms/.*/send/.*"))
        .and(header("authorization", "Bearer 1234"))
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
        .mount(&server)
        .await;

    let handle = q.send(RoomMessageEventContent::text_plain("yo").into()).await.unwrap();

    // Receiving updates for local echoes.
    let (txn1, _) = assert_update!(watch => local echo { body = "yo" });
    assert!(watch.is_empty());

    // Let the background task start now.
    tokio::task::yield_now().await;

    // While the first item is being sent, the system remembers the intent to edit
    // it, and will send it later.
    assert!(handle
        .edit(RoomMessageEventContent::text_plain("it's never too late!").into())
        .await
        .unwrap());
    assert_update!(watch => edit { body = "it's never too late!", txn = txn1 });

    // Let the server process the responses.
    drop(lock_guard);

    // Now the server will process the messages in order.
    assert_update!(watch => error { recoverable = true, txn = txn1 });

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
    let (client, server) = logged_in_client_with_server().await;

    // Mark the room as joined.
    let room_id = room_id!("!a:b.c");

    let room = mock_sync_with_new_room(
        |builder| {
            builder.add_joined_room(JoinedRoomBuilder::new(room_id));
        },
        &client,
        &server,
        room_id,
    )
    .await;

    let q = room.send_queue();

    let (local_echoes, mut watch) = q.subscribe().await.unwrap();

    assert!(local_echoes.is_empty());
    assert!(watch.is_empty());

    mock_encryption_state(&server, false).await;

    let send_mock_scope = Mock::given(method("PUT"))
        .and(path_regex(r"^/_matrix/client/r0/rooms/.*/send/.*"))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(413).set_body_json(json!({
            // From https://spec.matrix.org/v1.10/client-server-api/#standard-error-response
            "errcode": "M_TOO_LARGE",
        })))
        .expect(1)
        .mount_as_scoped(&server)
        .await;

    let handle =
        q.send(RoomMessageEventContent::text_plain("welcome to my ted talk").into()).await.unwrap();

    // Receiving an update for the local echo.
    let (txn, _) = assert_update!(watch => local echo { body = "welcome to my ted talk" });
    assert!(watch.is_empty());

    // Let the background task start now.
    tokio::task::yield_now().await;

    assert_update!(watch => error { recoverable = false, txn = txn });
    assert!(watch.is_empty());

    // Now edit the event's content (imagine we make it "shorter").
    drop(send_mock_scope);
    mock_send_event(event_id!("$1")).mount(&server).await;

    let edited = handle
        .edit(RoomMessageEventContent::text_plain("here's the summary of my ted talk").into())
        .await
        .unwrap();
    assert!(edited);

    // Let the server process the message.
    assert_update!(watch => edit { body = "here's the summary of my ted talk", txn = txn });
    assert_update!(watch => sent { txn = txn, });

    assert!(watch.is_empty());
}

#[async_test]
async fn test_abort_after_disable() {
    let (client, server) = logged_in_client_with_server().await;

    // Mark the room as joined.
    let room_id = room_id!("!a:b.c");

    let room = mock_sync_with_new_room(
        |builder| {
            builder.add_joined_room(JoinedRoomBuilder::new(room_id));
        },
        &client,
        &server,
        room_id,
    )
    .await;

    let mut errors = client.send_queue().subscribe_errors();

    assert!(errors.is_empty());

    // Start with an enabled sending queue.
    client.send_queue().set_enabled(true).await;

    assert!(client.send_queue().is_enabled());

    let q = room.send_queue();

    let (local_echoes, mut watch) = q.subscribe().await.unwrap();

    assert!(local_echoes.is_empty());
    assert!(watch.is_empty());

    server.reset().await;

    mock_encryption_state(&server, false).await;

    // Respond to /send with a transient 500 error.
    mock_send_transient_failure().expect(3).mount(&server).await;

    // One message is queued.
    let handle = q.send(RoomMessageEventContent::text_plain("hey there").into()).await.unwrap();

    // It is first seen as a local echo,
    let (txn, _) = assert_update!(watch => local echo { body = "hey there" });

    // Waiting for the global status to report the queue is getting disabled.
    let report = errors.recv().await.unwrap();
    assert_eq!(report.room_id, room.room_id());

    // The room updates will report the error, then the cancelled event, eventually.
    assert_update!(watch => error { recoverable=true, });

    // The room queue has been disabled, but not the client wide one.
    assert!(!room.send_queue().is_enabled());
    assert!(client.send_queue().is_enabled());

    // Aborting the sending should work.
    assert!(handle.abort().await.unwrap());

    assert_update!(watch => cancelled { txn = txn });

    assert!(watch.is_empty());
    assert!(errors.is_empty());
}

#[async_test]
async fn test_abort_or_edit_after_send() {
    let (client, server) = logged_in_client_with_server().await;

    // Mark the room as joined.
    let room_id = room_id!("!a:b.c");

    let room = mock_sync_with_new_room(
        |builder| {
            builder.add_joined_room(JoinedRoomBuilder::new(room_id));
        },
        &client,
        &server,
        room_id,
    )
    .await;

    // Start with an enabled sending queue.
    client.send_queue().set_enabled(true).await;

    let q = room.send_queue();

    let (local_echoes, mut watch) = q.subscribe().await.unwrap();
    assert!(local_echoes.is_empty());
    assert!(watch.is_empty());

    server.reset().await;
    mock_encryption_state(&server, false).await;
    mock_send_event(event_id!("$1")).mount(&server).await;

    let handle = q.send(RoomMessageEventContent::text_plain("hey there").into()).await.unwrap();

    // It is first seen as a local echo,
    let (txn, _) = assert_update!(watch => local echo { body = "hey there" });
    // Then sent.
    assert_update!(watch => sent { txn = txn, });

    // Editing shouldn't work anymore.
    assert!(handle
        .edit(RoomMessageEventContent::text_plain("i meant something completely different").into())
        .await
        .unwrap()
        .not());
    // Neither will aborting.
    assert!(handle.abort().await.unwrap().not());
    // Or sending a reaction.
    assert!(handle.react("😊".to_owned()).await.unwrap().is_none());

    assert!(watch.is_empty());
}

#[async_test]
async fn test_abort_while_being_sent_and_fails() {
    let (client, server) = logged_in_client_with_server().await;

    // Mark the room as joined.
    let room_id = room_id!("!a:b.c");

    let room = mock_sync_with_new_room(
        |builder| {
            builder.add_joined_room(JoinedRoomBuilder::new(room_id));
        },
        &client,
        &server,
        room_id,
    )
    .await;

    let q = room.send_queue();

    let (local_echoes, mut watch) = q.subscribe().await.unwrap();

    assert!(local_echoes.is_empty());
    assert!(watch.is_empty());

    let lock = Arc::new(Mutex::new(()));
    let lock_guard = lock.lock().await;

    let mock_lock = lock.clone();

    mock_encryption_state(&server, false).await;

    Mock::given(method("PUT"))
        .and(path_regex(r"^/_matrix/client/r0/rooms/.*/send/.*"))
        .and(header("authorization", "Bearer 1234"))
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
        .mount(&server)
        .await;

    let handle = q.send(RoomMessageEventContent::text_plain("yo").into()).await.unwrap();

    // Receiving updates for local echoes.
    let (txn1, _) = assert_update!(watch => local echo { body = "yo" });
    assert!(watch.is_empty());

    // Let the background task start now.
    tokio::task::yield_now().await;

    // While the item is being sent, the system remembers the intent to redact it
    // later.
    assert!(handle.abort().await.unwrap());
    assert_update!(watch => cancelled { txn = txn1 });

    // Let the server process the responses.
    drop(lock_guard);

    // Now the server will process the messages in order.
    assert_update!(watch => error { recoverable = true, txn = txn1 });

    assert!(watch.is_empty());

    // Looking back at the local echoes will indicate a local echo for `it's never
    // too late`.
    let (local_echoes, _) = q.subscribe().await.unwrap();
    assert!(local_echoes.is_empty());
}

#[async_test]
async fn test_unrecoverable_errors() {
    let (client, server) = logged_in_client_with_server().await;

    // Mark the room as joined.
    let room_id = room_id!("!a:b.c");

    let room = mock_sync_with_new_room(
        |builder| {
            builder.add_joined_room(JoinedRoomBuilder::new(room_id));
        },
        &client,
        &server,
        room_id,
    )
    .await;

    let mut errors = client.send_queue().subscribe_errors();

    assert!(errors.is_empty());

    // Start with an enabled sending queue.
    client.send_queue().set_enabled(true).await;

    assert!(client.send_queue().is_enabled());

    let q = room.send_queue();

    let (local_echoes, mut watch) = q.subscribe().await.unwrap();

    assert!(local_echoes.is_empty());
    assert!(watch.is_empty());

    server.reset().await;

    mock_encryption_state(&server, false).await;

    let respond_with_unrecoverable = AtomicBool::new(true);

    // Respond to the first /send with an unrecoverable error.
    Mock::given(method("PUT"))
        .and(path_regex(r"^/_matrix/client/r0/rooms/.*/send/.*"))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(move |_req: &Request| {
            // The first message gets M_TOO_LARGE, subsequent messages will encounter a
            // great success.
            if respond_with_unrecoverable.swap(false, Ordering::SeqCst) {
                ResponseTemplate::new(413).set_body_json(json!({
                    // From https://spec.matrix.org/v1.10/client-server-api/#standard-error-response
                    "errcode": "M_TOO_LARGE",
                }))
            } else {
                ResponseTemplate::new(200).set_body_json(json!({
                    "event_id": "$42",
                }))
            }
        })
        .expect(2)
        .mount(&server)
        .await;

    // Queue two messages.
    q.send(RoomMessageEventContent::text_plain("i'm too big for ya").into()).await.unwrap();
    q.send(RoomMessageEventContent::text_plain("aloha").into()).await.unwrap();

    // First message is seen as a local echo.
    let (txn1, _) = assert_update!(watch => local echo { body = "i'm too big for ya" });

    // Second message is seen as a local echo.
    let (txn2, _) = assert_update!(watch => local echo { body = "aloha" });

    // There will be an error report for the first message, indicating that the
    // error is unrecoverable.
    let report = errors.recv().await.unwrap();
    assert_eq!(report.room_id, room.room_id());
    assert!(!report.is_recoverable);

    // The room updates will report the error for the first message as unrecoverable
    // too.
    assert_update!(watch => error { recoverable=false, txn=txn1 });

    // The second message will be properly sent.
    assert_update!(watch => sent { txn=txn2, event_id=event_id!("$42") });

    // No queue is being disabled, because the error was unrecoverable.
    assert!(room.send_queue().is_enabled());
    assert!(client.send_queue().is_enabled());
}

#[async_test]
async fn test_no_network_access_error_is_recoverable() {
    // This is subtle, but for the `drop(server)` below to be effectful, it needs to
    // not be a pooled wiremock server (the default), which will keep the dropped
    // server in a static. Using the line below will create a "bare" server,
    // which is effectively dropped upon `drop()`.
    let server = wiremock::MockServer::builder().start().await;

    let client = logged_in_client(Some(server.uri().to_string())).await;

    // Mark the room as joined.
    let room_id = room_id!("!a:b.c");

    let room = mock_sync_with_new_room(
        |builder| {
            builder.add_joined_room(JoinedRoomBuilder::new(room_id));
        },
        &client,
        &server,
        room_id,
    )
    .await;

    // Dropping the server: any subsequent attempt to connect mimics an unreachable
    // server, which might be caused by missing network.
    drop(server);

    let mut errors = client.send_queue().subscribe_errors();
    assert!(errors.is_empty());

    // Start with an enabled sending queue.
    client.send_queue().set_enabled(true).await;
    assert!(client.send_queue().is_enabled());

    let q = room.send_queue();
    let (local_echoes, mut watch) = q.subscribe().await.unwrap();

    assert!(local_echoes.is_empty());
    assert!(watch.is_empty());

    // Queue two messages.
    q.send(RoomMessageEventContent::text_plain("is there anyone around here").into())
        .await
        .unwrap();

    // First message is seen as a local echo.
    let (txn1, _) = assert_update!(watch => local echo { body = "is there anyone around here" });

    // There will be an error report for the first message, indicating that the
    // error is recoverable: because network is unreachable.
    let report = errors.recv().await.unwrap();
    assert_eq!(report.room_id, room.room_id());
    assert!(report.is_recoverable);

    // The room updates will report the error for the first message as recoverable
    // too.
    assert_update!(watch => error { recoverable=true, txn=txn1});

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
    let client = Client::builder()
        .homeserver_url(server.uri())
        .server_versions([MatrixVersion::V1_0])
        .store_config(StoreConfig::new().state_store(store.clone()))
        .request_config(RequestConfig::new().disable_retry())
        .build()
        .await
        .unwrap();
    set_client_session(&client).await;

    // Mark two rooms as joined.
    let room = mock_sync_with_new_room(
        |builder| {
            builder
                .add_joined_room(JoinedRoomBuilder::new(room_id))
                .add_joined_room(JoinedRoomBuilder::new(room_id2));
        },
        &client,
        &server,
        room_id,
    )
    .await;

    let room2 = client.get_room(room_id2).unwrap();

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

    server.reset().await;

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
    mock_encryption_state(&server, false).await;

    let event_id = StdMutex::new(0);
    Mock::given(method("PUT"))
        .and(path_regex(r"^/_matrix/client/r0/rooms/.*/send/.*"))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(move |_req: &Request| {
            let mut event_id_guard = event_id.lock().unwrap();
            let event_id = *event_id_guard;
            *event_id_guard += 1;
            ResponseTemplate::new(200).set_body_json(json!({
                "event_id": event_id
            }))
        })
        .expect(2)
        .mount(&server)
        .await;

    let client = Client::builder()
        .homeserver_url(server.uri())
        .server_versions([MatrixVersion::V1_0])
        .store_config(StoreConfig::new().state_store(store))
        .request_config(RequestConfig::new().disable_retry())
        .build()
        .await
        .unwrap();
    set_client_session(&client).await;

    client.send_queue().respawn_tasks_for_rooms_with_unsent_events().await;

    // Let the sending queues process events.
    sleep(Duration::from_secs(1)).await;

    // The real assertion is on the expect(2) on the above Mock.
    server.verify().await;
}

#[async_test]
async fn test_reactions() {
    let (client, server) = logged_in_client_with_server().await;

    // Mark the room as joined.
    let room_id = room_id!("!a:b.c");

    let room = mock_sync_with_new_room(
        |builder| {
            builder.add_joined_room(JoinedRoomBuilder::new(room_id));
        },
        &client,
        &server,
        room_id,
    )
    .await;

    let q = room.send_queue();

    let (local_echoes, mut watch) = q.subscribe().await.unwrap();
    assert!(local_echoes.is_empty());
    assert!(watch.is_empty());

    let lock = Arc::new(Mutex::new(0));
    let lock_guard = lock.lock().await;

    let mock_lock = lock.clone();

    mock_encryption_state(&server, false).await;

    Mock::given(method("PUT"))
        .and(path_regex(r"^/_matrix/client/r0/rooms/.*/send/.*"))
        .and(header("authorization", "Bearer 1234"))
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
        .mount(&server)
        .await;

    // Sending of the second emoji has started; abort it, it will result in a redact
    // request.
    Mock::given(method("PUT"))
        .and(path_regex(r"^/_matrix/client/r0/rooms/.*/redact/.*?/.*?"))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"event_id": "$3"})))
        .expect(1)
        .mount(&server)
        .await;

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

    let (txn1, _) = assert_update!(watch => local echo { body = "1" });
    let emoji1_txn = assert_update!(watch => local reaction { key = "💯", parent = txn1 });
    let emoji2_txn = assert_update!(watch => local reaction { key = "🍭", parent = txn1 });
    let emoji3_txn = assert_update!(watch => local reaction { key = "👍", parent = txn1 });

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
    assert_update!(watch => cancelled { txn = emoji1_txn });
    assert!(watch.is_empty());

    // Let the original event be sent, and re-take the lock immediately so no
    // reactions aren't sent (since the lock is fair).
    drop(lock_guard);
    assert_update!(watch => sent { txn = txn1, event_id = event_id!("$0") });
    let lock_guard = lock.lock().await;
    assert!(watch.is_empty());

    // Abort sending of the second emoji. It was being sent, so it's first cancelled
    // *then* sent and redacted.
    let aborted = emoji_handle2.abort().await.unwrap();
    assert!(aborted);
    assert_update!(watch => cancelled { txn = emoji2_txn });
    assert!(watch.is_empty());

    // Drop the guard to let the mock server process events.
    drop(lock_guard);

    // Previous emoji has been sent; it will be redacted later.
    assert_update!(watch => sent { txn = emoji2_txn, event_id = event_id!("$1") });

    // The final emoji is sent.
    assert_update!(watch => sent { txn = emoji3_txn, event_id = event_id!("$2") });

    // Cancelling sending of the third emoji fails because it's been sent already.
    assert!(emoji_handle3.abort().await.unwrap().not());

    assert!(watch.is_empty());
}
