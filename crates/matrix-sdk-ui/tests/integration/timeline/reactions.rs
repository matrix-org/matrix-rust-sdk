// Copyright 2024 The Matrix.org Foundation C.I.C.
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

use std::time::Duration;

use assert_matches2::{assert_let, assert_matches};
use eyeball_im::VectorDiff;
use futures_util::{FutureExt as _, StreamExt as _};
use matrix_sdk::test_utils::{events::EventFactory, logged_in_client_with_server};
use matrix_sdk_test::{
    async_test,
    mocks::{mock_encryption_state, mock_redaction},
    JoinedRoomBuilder, SyncResponseBuilder, ALICE,
};
use matrix_sdk_ui::timeline::{ReactionStatus, RoomExt as _};
use ruma::{event_id, events::relation::Annotation, room_id};
use serde_json::json;
use wiremock::{
    matchers::{header, method, path_regex},
    Mock, ResponseTemplate,
};

use crate::mock_sync;

#[async_test]
async fn test_abort_before_being_sent() {
    // This test checks that a reaction could be aborted *before* or *while* it's
    // being sent by the send queue.

    let room_id = room_id!("!a98sd12bjh:example.org");
    let (client, server) = logged_in_client_with_server().await;
    let user_id = client.user_id().unwrap();

    // Make the test aware of the room.
    let mut sync_builder = SyncResponseBuilder::new();
    sync_builder.add_joined_room(JoinedRoomBuilder::new(room_id));

    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(Default::default()).await.unwrap();
    server.reset().await;

    mock_encryption_state(&server, false).await;

    let room = client.get_room(room_id).unwrap();
    let timeline = room.timeline().await.unwrap();
    let (initial_items, mut stream) = timeline.subscribe().await;

    assert!(initial_items.is_empty());

    let f = EventFactory::new();

    let event_id = event_id!("$1");
    sync_builder.add_joined_room(
        JoinedRoomBuilder::new(room_id)
            .add_timeline_event(f.text_msg("hello").sender(&ALICE).event_id(event_id)),
    );

    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(Default::default()).await.unwrap();
    server.reset().await;

    assert_let!(Some(VectorDiff::PushBack { value: first }) = stream.next().await);
    let item = first.as_event().unwrap();
    assert_eq!(item.content().as_message().unwrap().body(), "hello");

    assert_let!(Some(VectorDiff::PushFront { value: day_divider }) = stream.next().await);
    assert!(day_divider.is_day_divider());

    // Now we try to add two reactions to this message…

    // Mock the send endpoint with a delay, to give us time to abort the sending.
    Mock::given(method("PUT"))
        .and(path_regex(r"^/_matrix/client/r0/rooms/.*/send/.*"))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({
                    "event_id": "$2",
                }))
                .set_delay(Duration::from_millis(150)),
        )
        .expect(1)
        .named("send")
        .mount(&server)
        .await;

    mock_redaction(event_id!("$3")).mount(&server).await;

    // We add the reaction…
    timeline.toggle_reaction(&Annotation::new(event_id.to_owned(), "👍".to_owned())).await.unwrap();

    // First toggle (local echo).
    {
        assert_let!(Some(VectorDiff::Set { index: 1, value: item }) = stream.next().await);

        let reactions = item.as_event().unwrap().reactions();
        assert_eq!(reactions.len(), 1);
        assert_matches!(
            &reactions.get("👍").unwrap().get(user_id).unwrap().status,
            ReactionStatus::LocalToRemote(_)
        );

        assert!(stream.next().now_or_never().is_none());
    }

    // We toggle another reaction at the same time…
    timeline.toggle_reaction(&Annotation::new(event_id.to_owned(), "🥰".to_owned())).await.unwrap();

    {
        assert_let!(Some(VectorDiff::Set { index: 1, value: item }) = stream.next().await);

        let reactions = item.as_event().unwrap().reactions();
        assert_eq!(reactions.len(), 2);
        assert_matches!(
            &reactions.get("👍").unwrap().get(user_id).unwrap().status,
            ReactionStatus::LocalToRemote(_)
        );
        assert_matches!(
            &reactions.get("🥰").unwrap().get(user_id).unwrap().status,
            ReactionStatus::LocalToRemote(_)
        );

        assert!(stream.next().now_or_never().is_none());
    }

    // Then we remove the first one; because it was being sent, it should lead to a
    // redaction event.
    timeline.toggle_reaction(&Annotation::new(event_id.to_owned(), "👍".to_owned())).await.unwrap();

    {
        assert_let!(Some(VectorDiff::Set { index: 1, value: item }) = stream.next().await);

        let reactions = item.as_event().unwrap().reactions();
        assert_eq!(reactions.len(), 1);
        assert_matches!(
            &reactions.get("🥰").unwrap().get(user_id).unwrap().status,
            ReactionStatus::LocalToRemote(_)
        );

        assert!(stream.next().now_or_never().is_none());
    }

    // But because the first one was being sent, this one won't and the local echo
    // could be discarded.
    timeline.toggle_reaction(&Annotation::new(event_id.to_owned(), "🥰".to_owned())).await.unwrap();

    {
        assert_let!(Some(VectorDiff::Set { index: 1, value: item }) = stream.next().await);

        let reactions = item.as_event().unwrap().reactions();
        assert!(reactions.is_empty());

        assert!(stream.next().now_or_never().is_none());
    }

    // In a real-world setup with a background sync, the reaction may flash to the
    // user, because the sync may return the reaction event, and later the
    // redaction of the reaction. In our case, we're done here.

    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(stream.next().now_or_never().is_none());
}
