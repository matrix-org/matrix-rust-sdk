use std::{sync::Arc, time::Duration};

use assert_matches2::{assert_let, assert_matches};
use futures_util::FutureExt as _;
use matrix_sdk::{
    send_queue::{LocalEcho, RoomSendQueueError, RoomSendQueueUpdate},
    test_utils::logged_in_client_with_server,
};
use matrix_sdk_test::{async_test, InvitedRoomBuilder, JoinedRoomBuilder, LeftRoomBuilder};
use ruma::{
    event_id,
    events::{room::message::RoomMessageEventContent, AnyMessageLikeEventContent},
    room_id, EventId,
};
use serde_json::json;
use tokio::{sync::Mutex, time::timeout};
use wiremock::{
    matchers::{header, method, path_regex},
    Mock, Request, ResponseTemplate,
};

use crate::{mock_encryption_state, mock_sync_with_new_room};

fn mock_send_event(returned_event_id: &EventId) -> Mock {
    Mock::given(method("PUT"))
        .and(path_regex(r"^/_matrix/client/r0/rooms/.*/send/.*"))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "event_id": returned_event_id,
        })))
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

    client.send_queue().disable();

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

    let (local_echoes, mut watch) = q.subscribe().await;
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

    assert_let!(
        Ok(Ok(RoomSendQueueUpdate::NewLocalEvent(LocalEcho {
            content: AnyMessageLikeEventContent::RoomMessage(msg),
            transaction_id: txn1,
            ..
        }))) = timeout(Duration::from_secs(1), watch.recv()).await
    );
    assert_eq!(msg.body(), "1");

    {
        let (local_echoes, _) = q.subscribe().await;

        assert_eq!(local_echoes.len(), 1);
        assert_eq!(local_echoes[0].transaction_id, txn1);
    }

    assert!(watch.is_empty());

    drop(lock_guard);

    assert_let!(
        Ok(Ok(RoomSendQueueUpdate::SentEvent {
            event_id: response_event_id,
            transaction_id: txn2
        })) = timeout(Duration::from_secs(1), watch.recv()).await
    );

    assert_eq!(event_id, response_event_id);
    assert_eq!(txn1, txn2);

    assert!(watch.is_empty());
}

#[async_test]
async fn test_error() {
    let (client, server) = logged_in_client_with_server().await;

    let mut global_status = client.send_queue().subscribe_status();

    assert!(client.send_queue().is_enabled());
    assert!(global_status.next_now());

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

    let (local_echoes, mut watch) = q.subscribe().await;
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

    assert_let!(
        Ok(Ok(RoomSendQueueUpdate::NewLocalEvent(LocalEcho {
            content: AnyMessageLikeEventContent::RoomMessage(msg),
            transaction_id: txn1,
            ..
        }))) = timeout(Duration::from_secs(1), watch.recv()).await
    );
    assert_eq!(msg.body(), "1");

    {
        let (local_echoes, _) = q.subscribe().await;

        assert_eq!(local_echoes.len(), 1);
        assert_eq!(local_echoes[0].transaction_id, txn1);
    }

    assert!(watch.is_empty());

    // No new update on the global status.
    assert!(global_status.next().now_or_never().is_none());

    drop(lock_guard);

    // The exponential backoff used when retrying a request introduces a bit of
    // non-determinism, so let it fail after a large amount of time (10
    // seconds).
    assert_let!(
        Ok(Ok(RoomSendQueueUpdate::SendError { transaction_id: txn2, error })) =
            timeout(Duration::from_secs(10), watch.recv()).await
    );

    // It's the same transaction id that's used to signal the send error.
    assert_eq!(txn1, txn2);

    let error = error.as_client_api_error().unwrap();
    assert_eq!(error.status_code, 500);

    tokio::time::sleep(Duration::from_millis(50)).await;

    assert!(watch.is_empty());

    assert!(!global_status.next().await.unwrap(), "the queue should be disabled next");
    assert!(!client.send_queue().is_enabled());

    server.reset().await;
    Mock::given(method("PUT"))
        .and(path_regex(r"^/_matrix/client/r0/rooms/.*/send/.*"))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "event_id": "$42"
        })))
        .expect(1)
        .mount(&server)
        .await;

    // Re-enabling the queue will re-send the same message in that room.
    client.send_queue().enable();

    assert!(
        global_status.next().await.unwrap(),
        "the queue should be re-enabled after the user action"
    );
    assert!(client.send_queue().is_enabled());

    assert_let!(
        Ok(Ok(RoomSendQueueUpdate::SentEvent { event_id, transaction_id: txn3 })) =
            timeout(Duration::from_secs(1), watch.recv()).await
    );

    assert_eq!(txn1, txn3);
    assert_eq!(event_id, event_id!("$42"));

    assert!(global_status.next().now_or_never().is_none());
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

    let mut global_status = client.send_queue().subscribe_status();

    assert!(global_status.next_now());

    // When I start with a disabled send queue,
    client.send_queue().disable();

    assert!(!client.send_queue().is_enabled());
    assert!(!global_status.next().await.unwrap());

    let q = room.send_queue();

    let (local_echoes, mut watch) = q.subscribe().await;

    assert!(local_echoes.is_empty());
    assert!(watch.is_empty());

    // Three messages are queued.
    q.send(RoomMessageEventContent::text_plain("msg1").into()).await.unwrap();
    q.send(RoomMessageEventContent::text_plain("msg2").into()).await.unwrap();
    q.send(RoomMessageEventContent::text_plain("msg3").into()).await.unwrap();

    for i in 1..=3 {
        assert_let!(
            Ok(Ok(RoomSendQueueUpdate::NewLocalEvent(LocalEcho {
                content: AnyMessageLikeEventContent::RoomMessage(msg),
                ..
            }))) = timeout(Duration::from_secs(1), watch.recv()).await
        );
        assert_eq!(msg.body(), format!("msg{i}"));
    }

    {
        let (local_echoes, _) = q.subscribe().await;
        assert_eq!(local_echoes.len(), 3);
    }

    assert!(watch.is_empty());

    // Messages aren't sent immediately.
    tokio::time::sleep(Duration::from_millis(500)).await;

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

    // But when reenabling the queue,
    client.send_queue().enable();

    assert!(client.send_queue().is_enabled());
    assert!(global_status.next().await.unwrap());

    // They're sent, in the same ordering.
    for i in 1..=3 {
        assert_let!(
            Ok(Ok(RoomSendQueueUpdate::SentEvent { event_id, .. })) =
                timeout(Duration::from_secs(1), watch.recv()).await
        );
        assert_eq!(event_id.as_str(), format!("${i}"));
    }

    assert!(global_status.next().now_or_never().is_none());
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

    let (local_echoes, mut watch) = q.subscribe().await;

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

    let handle1 = q.send(RoomMessageEventContent::text_plain("msg1").into()).await.unwrap();
    let handle2 = q.send(RoomMessageEventContent::text_plain("msg2").into()).await.unwrap();
    q.send(RoomMessageEventContent::text_plain("msg3").into()).await.unwrap();
    q.send(RoomMessageEventContent::text_plain("msg4").into()).await.unwrap();
    q.send(RoomMessageEventContent::text_plain("msg5").into()).await.unwrap();

    // Receiving update for msg1.
    assert_let!(
        Ok(Ok(RoomSendQueueUpdate::NewLocalEvent(LocalEcho {
            content: AnyMessageLikeEventContent::RoomMessage(_),
            transaction_id: txn1,
            ..
        }))) = timeout(Duration::from_secs(1), watch.recv()).await
    );

    // Receiving update for msg2.
    assert_let!(
        Ok(Ok(RoomSendQueueUpdate::NewLocalEvent(LocalEcho {
            content: AnyMessageLikeEventContent::RoomMessage(_),
            transaction_id: txn2,
            ..
        }))) = timeout(Duration::from_secs(1), watch.recv()).await
    );

    // Receiving update for msg3.
    assert_let!(
        Ok(Ok(RoomSendQueueUpdate::NewLocalEvent(LocalEcho {
            content: AnyMessageLikeEventContent::RoomMessage(_),
            transaction_id: txn3,
            abort_handle: handle3,
        }))) = timeout(Duration::from_secs(1), watch.recv()).await
    );

    // Receiving update for msg4.
    assert_let!(
        Ok(Ok(RoomSendQueueUpdate::NewLocalEvent(LocalEcho {
            content: AnyMessageLikeEventContent::RoomMessage(_),
            transaction_id: txn4,
            ..
        }))) = timeout(Duration::from_secs(1), watch.recv()).await
    );

    // Receiving update for msg5.
    assert_let!(
        Ok(Ok(RoomSendQueueUpdate::NewLocalEvent(LocalEcho {
            content: AnyMessageLikeEventContent::RoomMessage(_),
            transaction_id: txn5,
            ..
        }))) = timeout(Duration::from_secs(1), watch.recv()).await
    );

    assert!(watch.is_empty());

    // Let the background task start now.
    tokio::task::yield_now().await;

    // The first item is already being sent, so we can't abort it.
    assert!(!handle1.abort().await);

    assert!(watch.is_empty());

    // The second item is pending, so we can abort it, using the handle returned by
    // `send()`.
    assert!(handle2.abort().await);

    assert_let!(
        Ok(Ok(RoomSendQueueUpdate::CancelledLocalEvent {
            transaction_id: cancelled_transaction_id
        })) = timeout(Duration::from_secs(1), watch.recv()).await
    );

    assert_eq!(cancelled_transaction_id, txn2);

    assert!(watch.is_empty());

    // The third item is pending, so we can abort it, using the handle received from
    // the update.
    assert!(handle3.abort().await);

    assert_let!(
        Ok(Ok(RoomSendQueueUpdate::CancelledLocalEvent {
            transaction_id: cancelled_transaction_id
        })) = timeout(Duration::from_secs(1), watch.recv()).await
    );

    assert_eq!(cancelled_transaction_id, txn3);

    assert!(watch.is_empty());

    // The fourth item is pending, so we can abort it, using an handle provided by
    // the initial array of values.
    let (mut local_echoes, _) = q.subscribe().await;

    // At this point, local echoes = txn1, txn4, txn5.
    assert_eq!(local_echoes.len(), 3);

    let local_echo4 = local_echoes.remove(1);
    assert_eq!(local_echo4.transaction_id, txn4);

    let handle4 = local_echo4.abort_handle;

    assert!(handle4.abort().await);

    assert_let!(
        Ok(Ok(RoomSendQueueUpdate::CancelledLocalEvent {
            transaction_id: cancelled_transaction_id
        })) = timeout(Duration::from_secs(1), watch.recv()).await
    );

    assert_eq!(cancelled_transaction_id, txn4);

    assert!(watch.is_empty());

    // Let the server process the responses.
    drop(lock_guard);

    // Now the server will process msg1 and msg3.
    assert_let!(
        Ok(Ok(RoomSendQueueUpdate::SentEvent { transaction_id: sent_txn, .. })) =
            timeout(Duration::from_secs(1), watch.recv()).await
    );
    assert_eq!(sent_txn, txn1,);

    assert_let!(
        Ok(Ok(RoomSendQueueUpdate::SentEvent { transaction_id: sent_txn, .. })) =
            timeout(Duration::from_secs(1), watch.recv()).await
    );
    assert_eq!(sent_txn, txn5);

    assert!(watch.is_empty());
}

#[async_test]
async fn test_abort_reenable() {
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

    let mut global_status = client.send_queue().subscribe_status();

    assert!(global_status.next_now());

    // When I start with an enabled sending queue,
    client.send_queue().enable();

    assert!(client.send_queue().is_enabled());

    let q = room.send_queue();

    let (local_echoes, mut watch) = q.subscribe().await;

    assert!(local_echoes.is_empty());
    assert!(watch.is_empty());

    // One message is queued.
    let abort_send_handle =
        q.send(RoomMessageEventContent::text_plain("hey there").into()).await.unwrap();

    // It is first seen as a local echo,
    assert_let!(
        Ok(Ok(RoomSendQueueUpdate::NewLocalEvent(LocalEcho {
            content: AnyMessageLikeEventContent::RoomMessage(msg),
            ..
        }))) = timeout(Duration::from_secs(1), watch.recv()).await
    );
    assert_eq!(msg.body(), format!("hey there"));

    // Waiting for the global status to report the queue is getting disabled.
    assert!(!global_status.next().await.unwrap());

    // Aborting the sending should work.
    assert!(abort_send_handle.abort().await);

    // The room updates will report the error, then the cancelled event, eventually.
    assert_let!(
        Ok(Ok(RoomSendQueueUpdate::SendError { .. })) =
            timeout(Duration::from_secs(1), watch.recv()).await
    );

    assert_let!(
        Ok(Ok(RoomSendQueueUpdate::CancelledLocalEvent { .. })) =
            timeout(Duration::from_secs(1), watch.recv()).await
    );

    assert!(watch.is_empty());
}
