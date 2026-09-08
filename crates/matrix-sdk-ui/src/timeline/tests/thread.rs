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

use assert_matches2::assert_let;
use eyeball_im::VectorDiff;
use imbl::vector;
use matrix_sdk_test::{ALICE, BOB, async_test};
use ruma::event_id;
use stream_assert::assert_next_matches;

use super::TestTimeline;
use crate::timeline::{TimelineDetails, TimelineEventItemId, event_item::RemoteEventOrigin};

#[async_test]
async fn test_thread_root_uses_its_bundled_latest_reply() {
    // `TestRoomDataProvider::load_event` is not implemented, so this only passes
    // if the latest reply is taken from the bundle, not loaded.
    let timeline = TestTimeline::new().await;
    let mut stream = timeline.subscribe().await;

    let f = &timeline.factory;
    let thread_root_id = event_id!("$thread_root");
    let latest_reply_id = event_id!("$latest_reply");

    let thread_root = f
        .text_msg("thread root")
        .sender(*ALICE)
        .event_id(thread_root_id)
        .with_bundled_thread_summary(
            f.text_msg("the last one!").sender(*BOB).event_id(latest_reply_id).into(),
            3,
            false,
        )
        .into_event();

    timeline
        .controller
        .handle_remote_events_with_diffs(
            vec![VectorDiff::Append { values: vector![thread_root] }],
            RemoteEventOrigin::Sync,
        )
        .await;

    let item = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);
    let event_item = item.as_event().unwrap();
    assert_eq!(event_item.event_id().unwrap(), thread_root_id);

    assert_let!(Some(summary) = event_item.content().thread_summary());
    assert_eq!(summary.num_replies, 3);

    assert_let!(TimelineDetails::Ready(latest_reply) = summary.latest_event);
    assert_eq!(latest_reply.identifier, TimelineEventItemId::EventId(latest_reply_id.to_owned()));
    assert_eq!(latest_reply.content.as_message().unwrap().body(), "the last one!");
    assert_eq!(latest_reply.sender, *BOB);
}
