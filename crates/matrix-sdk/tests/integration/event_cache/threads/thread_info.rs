// Copyright 2026 The Matrix.org Foundation C.I.C.
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

use matrix_sdk::{
    assert_let_timeout,
    event_cache::{RoomEventCacheUpdate, TimelineVectorDiffs},
    test_utils::mocks::MatrixMockServer,
};
use matrix_sdk_test::{ALICE, JoinedRoomBuilder, async_test, event_factory::EventFactory};
use ruma::{
    event_id,
    events::receipt::{ReceiptThread, ReceiptType},
    room_id,
};

/// The summary bundled with a thread root gets saved in the thread's
/// `ThreadInfo`, so it's still known once the root is reloaded from the store,
/// where its bundled relations have been stripped.
#[async_test]
async fn test_bundled_thread_summary_seeds_thread_info() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    let event_cache = client.event_cache();
    event_cache.subscribe().unwrap();

    let room_id = room_id!("!r");
    let thread_id = event_id!("$t");
    let latest_event_id = event_id!("$latest");
    let f = EventFactory::new().room(room_id).sender(*ALICE);

    server.sync_joined_room(&client, room_id).await;
    let (room_event_cache, _drop_handles) = event_cache.room(room_id).await.unwrap();
    let (_, mut room_stream) = room_event_cache.subscribe().await.unwrap();

    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id).add_timeline_event(
                f.text_msg("thread root").event_id(thread_id).with_bundled_thread_summary(
                    f.text_msg("latest reply").event_id(latest_event_id).into(),
                    42,
                    false,
                ),
            ),
        )
        .await;
    assert_let_timeout!(
        Ok(RoomEventCacheUpdate::UpdateTimelineEvents(TimelineVectorDiffs { .. })) =
            room_stream.recv()
    );

    let thread_info = event_cache.thread_info(room_id, thread_id).await.unwrap().unwrap();
    assert_eq!(thread_info.number_of_replies, Some(42));
    assert_eq!(thread_info.latest_event.as_deref(), Some(latest_event_id));
}

/// A bundled thread summary doesn't replace the summary that the event cache
/// computed from the replies it has seen.
#[async_test]
async fn test_bundled_thread_summary_does_not_override_computed_thread_info() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    let event_cache = client.event_cache();
    event_cache.subscribe().unwrap();

    let room_id = room_id!("!r");
    let thread_id = event_id!("$t");
    let reply_id = event_id!("$reply");
    let f = EventFactory::new().room(room_id).sender(*ALICE);

    server.sync_joined_room(&client, room_id).await;
    let (room_event_cache, _drop_handles) = event_cache.room(room_id).await.unwrap();
    let (thread, _drop_handles) = event_cache.thread(room_id, thread_id).await.unwrap();
    let mut thread_info_updates = thread.subscribe_to_thread_info().await.unwrap();

    // A reply in the thread makes the event cache compute the thread's summary.
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id).add_timeline_event(
                f.text_msg("reply").in_thread(thread_id, thread_id).event_id(reply_id),
            ),
        )
        .await;
    assert_let_timeout!(Some(thread_info) = thread_info_updates.next());
    assert_eq!(thread_info.number_of_replies, Some(1));

    // Then the thread root shows up, with a bundled summary that disagrees.
    let (_, mut room_stream) = room_event_cache.subscribe().await.unwrap();
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id).add_timeline_event(
                f.text_msg("thread root").event_id(thread_id).with_bundled_thread_summary(
                    f.text_msg("latest reply").event_id(event_id!("$latest")).into(),
                    42,
                    false,
                ),
            ),
        )
        .await;
    assert_let_timeout!(
        Ok(RoomEventCacheUpdate::UpdateTimelineEvents(TimelineVectorDiffs { .. })) =
            room_stream.recv()
    );

    let thread_info = event_cache.thread_info(room_id, thread_id).await.unwrap().unwrap();
    assert_eq!(thread_info.number_of_replies, Some(1));
    assert_eq!(thread_info.latest_event.as_deref(), Some(reply_id));
}

/// A thread cache that was created before its root showed up holds an
/// uncomputed `ThreadInfo` in memory, which mustn't overwrite the summary that
/// got seeded from the root's bundle when the thread cache saves it again.
#[async_test]
async fn test_seeded_thread_info_survives_thread_cache_update() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    let event_cache = client.event_cache();
    event_cache.subscribe().unwrap();

    let room_id = room_id!("!r");
    let thread_id = event_id!("$t");
    let latest_event_id = event_id!("$latest");
    let own_user_id = client.user_id().unwrap();
    let f = EventFactory::new().room(room_id).sender(*ALICE);

    server.sync_joined_room(&client, room_id).await;
    let (room_event_cache, _drop_handles) = event_cache.room(room_id).await.unwrap();
    let (_, mut room_stream) = room_event_cache.subscribe().await.unwrap();
    let (thread, _drop_handles) = event_cache.thread(room_id, thread_id).await.unwrap();
    let mut thread_info_updates = thread.subscribe_to_thread_info().await.unwrap();

    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id).add_timeline_event(
                f.text_msg("thread root").event_id(thread_id).with_bundled_thread_summary(
                    f.text_msg("latest reply").event_id(latest_event_id).into(),
                    42,
                    false,
                ),
            ),
        )
        .await;
    assert_let_timeout!(
        Ok(RoomEventCacheUpdate::UpdateTimelineEvents(TimelineVectorDiffs { .. })) =
            room_stream.recv()
    );
    let thread_info = event_cache.thread_info(room_id, thread_id).await.unwrap().unwrap();
    assert_eq!(thread_info.number_of_replies, Some(42));

    // A threaded read receipt makes the thread cache save its `ThreadInfo`
    // again.
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id).add_receipt(
                f.read_receipts()
                    .add(
                        latest_event_id,
                        own_user_id,
                        ReceiptType::Read,
                        ReceiptThread::Thread(thread_id.to_owned()),
                    )
                    .into_event(),
            ),
        )
        .await;
    assert_let_timeout!(Some(thread_info) = thread_info_updates.next());
    assert_eq!(thread_info.number_of_replies, Some(42));
    assert_eq!(thread_info.latest_event.as_deref(), Some(latest_event_id));

    let thread_info = event_cache.thread_info(room_id, thread_id).await.unwrap().unwrap();
    assert_eq!(thread_info.number_of_replies, Some(42));
    assert_eq!(thread_info.latest_event.as_deref(), Some(latest_event_id));
}
