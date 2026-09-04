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

use eyeball_im::VectorDiff;
use matrix_sdk_test::{ALICE, async_test, event_factory::EventFactory};
use ruma::event_id;
use stream_assert::assert_next_matches;

use crate::timeline::{
    TimelineReadReceiptTracking,
    controller::TimelineSettings,
    tests::{TestRoomDataProvider, TestTimelineBuilder},
};

#[async_test]
async fn test_thread_root_loads_its_latest_reply_once() {
    let f = EventFactory::new().sender(&ALICE);

    let thread_root_id = event_id!("$thread_root");
    let latest_reply_id = event_id!("$latest_reply");

    let provider = TestRoomDataProvider::default()
        .with_loadable_event(f.text_msg("the last one!").event_id(latest_reply_id).into_event());

    // Read receipt tracking is what makes the receipts computation look at the
    // latest reply, in addition to the embedded preview.
    let timeline = TestTimelineBuilder::new()
        .provider(provider)
        .settings(TimelineSettings {
            track_read_receipts: TimelineReadReceiptTracking::AllEvents,
            ..Default::default()
        })
        .build()
        .await;

    let mut stream = timeline.subscribe_events().await;

    timeline
        .handle_live_event(
            f.text_msg("thready thread mcthreadface")
                .event_id(thread_root_id)
                .with_bundled_thread_summary(
                    f.text_msg("the last one!").event_id(latest_reply_id).into(),
                    1,
                    false,
                ),
        )
        .await;

    let item = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);
    let summary = item.content().thread_summary().unwrap();
    assert!(summary.latest_event.is_ready());

    // The latest reply is needed both for the embedded preview and for the
    // implicit read receipt, but it must be loaded only once.
    assert_eq!(*timeline.data().loaded_events.read().await, vec![latest_reply_id.to_owned()]);
}
