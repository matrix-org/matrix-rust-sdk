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
    event_cache::{ThreadEventCacheUpdate, TimelineVectorDiffs},
    test_utils::mocks::MatrixMockServer,
};
use matrix_sdk_test::{ALICE, JoinedRoomBuilder, async_test, event_factory::EventFactory};
use ruma::{event_id, room_id};

/// A thread's replies aren't counted until one of them shows up, so the thread
/// root showing up on its own doesn't give the thread a count of zero.
#[async_test]
async fn test_thread_info_is_not_counted_before_a_reply_shows_up() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    let event_cache = client.event_cache();
    event_cache.subscribe().unwrap();

    let room_id = room_id!("!r");
    let thread_id = event_id!("$t");
    let reply_id = event_id!("$reply");
    let f = EventFactory::new().room(room_id).sender(*ALICE);

    server.sync_joined_room(&client, room_id).await;
    let (thread, _drop_handles) = event_cache.thread(room_id, thread_id).await.unwrap();
    let (_, mut thread_stream) = thread.subscribe().await.unwrap();

    // The thread root shows up on its own, so there's nothing to count yet.
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id)
                .add_timeline_event(f.text_msg("thread root").event_id(thread_id)),
        )
        .await;
    assert_let_timeout!(
        Ok(ThreadEventCacheUpdate::UpdateTimelineEvents(TimelineVectorDiffs { .. })) =
            thread_stream.recv()
    );

    // Then a reply shows up, which is the first thing that gets counted.
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id).add_timeline_event(
                f.text_msg("reply").in_thread(thread_id, thread_id).event_id(reply_id),
            ),
        )
        .await;
    assert_let_timeout!(
        Ok(ThreadEventCacheUpdate::UpdateTimelineEvents(TimelineVectorDiffs { .. })) =
            thread_stream.recv()
    );
    assert_let_timeout!(Ok(ThreadEventCacheUpdate::UpdateSummary(summary)) = thread_stream.recv());
    assert_eq!(summary.num_replies, 1);
    assert_eq!(summary.latest_reply.as_deref(), Some(reply_id));

    let thread_info = event_cache.thread_info(room_id, thread_id).await.unwrap().unwrap();
    assert_eq!(thread_info.number_of_replies, Some(1));
}
