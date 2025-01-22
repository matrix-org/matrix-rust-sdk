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

use std::{sync::Mutex, time::Duration};

use assert_matches2::{assert_let, assert_matches};
use eyeball_im::VectorDiff;
use futures_util::{FutureExt as _, StreamExt as _};
use matrix_sdk::test_utils::{logged_in_client_with_server, mocks::MatrixMockServer};
use matrix_sdk_test::{
    async_test, event_factory::EventFactory, mocks::mock_encryption_state, JoinedRoomBuilder,
    SyncResponseBuilder, ALICE,
};
use matrix_sdk_ui::timeline::{ReactionStatus, RoomExt as _};
use ruma::{event_id, events::room::message::RoomMessageEventContent, room_id};
use serde_json::json;
use stream_assert::assert_pending;
use wiremock::{
    matchers::{header, method, path_regex},
    Mock, ResponseTemplate,
};

use crate::mock_sync;

#[async_test]
async fn test_abort_before_being_sent() {
    // This test checks that a reaction could be aborted *before* or *while* it's
    // being sent by the send queue.

    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    let room_id = room_id!("!a98sd12bjh:example.org");
    let room = server.sync_joined_room(&client, room_id).await;

    server.mock_room_state_encryption().plain().mount().await;

    let timeline = room.timeline().await.unwrap();
    let (initial_items, mut stream) = timeline.subscribe_batched().await;

    assert!(initial_items.is_empty());

    let f = EventFactory::new();

    let event_id = event_id!("$1");
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id)
                .add_timeline_event(f.text_msg("hello").sender(&ALICE).event_id(event_id)),
        )
        .await;

    assert_let!(Some(timeline_updates) = stream.next().await);
    assert_eq!(timeline_updates.len(), 2);

    assert_let!(VectorDiff::PushBack { value: first } = &timeline_updates[0]);
    let item = first.as_event().unwrap();
    let item_id = item.identifier();
    assert_eq!(item.content().as_message().unwrap().body(), "hello");

    assert_let!(VectorDiff::PushFront { value: date_divider } = &timeline_updates[1]);
    assert!(date_divider.is_date_divider());

    // Now we try to add two reactions to this message…

    // Mock the send endpoint with a delay, to give us time to abort the sending.
    server
        .mock_room_send()
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({
                    "event_id": "$2",
                }))
                .set_delay(Duration::from_millis(150)),
        )
        .mock_once()
        .named("send")
        .mount()
        .await;

    server.mock_room_redact().ok(event_id!("$3")).mock_once().mount().await;

    // We add the reaction…
    timeline.toggle_reaction(&item_id, "👍").await.unwrap();

    let user_id = client.user_id().unwrap();

    // First toggle (local echo).
    {
        assert_let!(Some(timeline_updates) = stream.next().await);
        assert_eq!(timeline_updates.len(), 1);

        assert_let!(VectorDiff::Set { index: 1, value: item } = &timeline_updates[0]);

        let reactions = item.as_event().unwrap().reactions();
        assert_eq!(reactions.len(), 1);
        assert_matches!(
            &reactions.get("👍").unwrap().get(user_id).unwrap().status,
            ReactionStatus::LocalToRemote(_)
        );

        assert_pending!(stream);
    }

    // We toggle another reaction at the same time…
    timeline.toggle_reaction(&item_id, "🥰").await.unwrap();

    {
        assert_let!(Some(timeline_updates) = stream.next().await);
        assert_eq!(timeline_updates.len(), 1);

        assert_let!(VectorDiff::Set { index: 1, value: item } = &timeline_updates[0]);

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
    timeline.toggle_reaction(&item_id, "👍").await.unwrap();

    {
        assert_let!(Some(timeline_updates) = stream.next().await);
        assert_eq!(timeline_updates.len(), 1);

        assert_let!(VectorDiff::Set { index: 1, value: item } = &timeline_updates[0]);

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
    timeline.toggle_reaction(&item_id, "🥰").await.unwrap();

    {
        assert_let!(Some(timeline_updates) = stream.next().await);
        assert_eq!(timeline_updates.len(), 1);

        assert_let!(VectorDiff::Set { index: 1, value: item } = &timeline_updates[0]);

        let reactions = item.as_event().unwrap().reactions();
        assert!(reactions.is_empty());

        assert!(stream.next().now_or_never().is_none());
    }

    // In a real-world setup with a background sync, the reaction may flash to the
    // user, because the sync may return the reaction event, and later the
    // redaction of the reaction. In our case, we're done here.

    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_pending!(stream);
}

#[async_test]
async fn test_redact_failed() {
    // This test checks that if a reaction redaction failed, then we re-insert the
    // reaction after displaying it was removed.

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
    let (initial_items, mut stream) = timeline.subscribe_batched().await;

    assert!(initial_items.is_empty());

    let f = EventFactory::new();

    let event_id = event_id!("$1");
    sync_builder.add_joined_room(
        JoinedRoomBuilder::new(room_id)
            .add_timeline_event(f.text_msg("hello").sender(&ALICE).event_id(event_id))
            .add_timeline_event(f.reaction(event_id, "😆").sender(user_id)),
    );

    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(Default::default()).await.unwrap();
    server.reset().await;

    assert_let!(Some(timeline_updates) = stream.next().await);
    assert_eq!(timeline_updates.len(), 3);

    let item_id = {
        assert_let!(VectorDiff::PushBack { value: item } = &timeline_updates[0]);

        let item = item.as_event().unwrap();
        assert_eq!(item.content().as_message().unwrap().body(), "hello");
        assert!(item.reactions().is_empty());

        item.identifier()
    };

    assert_let!(VectorDiff::Set { index: 0, value: item } = &timeline_updates[1]);
    assert_eq!(item.as_event().unwrap().reactions().len(), 1);

    assert_let!(VectorDiff::PushFront { value: date_divider } = &timeline_updates[2]);
    assert!(date_divider.is_date_divider());

    // Now, redact the annotation we previously added.

    Mock::given(method("PUT"))
        .and(path_regex(r"^/_matrix/client/r0/rooms/.*/redact/.*?/.*?"))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(500))
        .expect(1)
        .named("redact")
        .mount(&server)
        .await;

    // We toggle the reaction, which fails with an error.
    timeline.toggle_reaction(&item_id, "😆").await.unwrap_err();

    assert_let!(Some(timeline_updates) = stream.next().await);
    assert_eq!(timeline_updates.len(), 2);

    // The local echo is removed (assuming the redaction works)…
    assert_let!(VectorDiff::Set { index: 1, value: item } = &timeline_updates[0]);
    assert!(item.as_event().unwrap().reactions().is_empty());

    // …then added back, after redaction failed.
    assert_let!(VectorDiff::Set { index: 1, value: item } = &timeline_updates[1]);
    assert_eq!(item.as_event().unwrap().reactions().len(), 1);

    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_pending!(stream);
}

#[async_test]
async fn test_local_reaction_to_local_echo() {
    // This test checks that if a reaction redaction failed, then we re-insert the
    // reaction after displaying it was removed.

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
    let (initial_items, mut stream) = timeline.subscribe_batched().await;

    assert!(initial_items.is_empty());

    // Add a duration to the response, so we can check other things in the
    // meanwhile.
    let next_event_id = Mutex::new(0);
    Mock::given(method("PUT"))
        .and(path_regex(r"^/_matrix/client/r0/rooms/.*/send/.*"))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(move |_req: &wiremock::Request| {
            let mut next_event_id = next_event_id.lock().unwrap();
            let event_id = *next_event_id;
            *next_event_id += 1;
            let mut tmp = ResponseTemplate::new(200).set_body_json(json!({
                "event_id": format!("${event_id}"),
            }));

            if event_id == 0 {
                tmp = tmp.set_delay(Duration::from_secs(1));
            }

            tmp
        })
        .mount(&server)
        .await;

    // Send a local event.
    let _ = timeline.send(RoomMessageEventContent::text_plain("lol").into()).await.unwrap();

    assert_let!(Some(timeline_updates) = stream.next().await);
    assert_eq!(timeline_updates.len(), 2);

    // Receive a local echo.

    let item_id = {
        assert_let!(VectorDiff::PushBack { value: item } = &timeline_updates[0]);

        let item = item.as_event().unwrap();
        assert!(item.is_local_echo());
        assert_eq!(item.content().as_message().unwrap().body(), "lol");
        assert!(item.reactions().is_empty());
        item.identifier()
    };

    // Good ol' date divider.
    assert_let!(VectorDiff::PushFront { value: date_divider } = &timeline_updates[1]);
    assert!(date_divider.is_date_divider());

    // Add a reaction before the remote echo comes back.
    let key1 = "🤣";
    timeline.toggle_reaction(&item_id, key1).await.unwrap();

    assert_let!(Some(timeline_updates) = stream.next().await);
    assert_eq!(timeline_updates.len(), 1);

    // The reaction is added to the local echo.
    assert_let!(VectorDiff::Set { index: 1, value: item } = &timeline_updates[0]);
    let reactions = item.as_event().unwrap().reactions();
    assert_eq!(reactions.len(), 1);
    let reaction_info = reactions.get(key1).unwrap().get(user_id).unwrap();
    assert_matches!(&reaction_info.status, ReactionStatus::LocalToLocal(..));

    // Add another reaction.
    let key2 = "😈";
    timeline.toggle_reaction(&item_id, key2).await.unwrap();

    assert_let!(Some(timeline_updates) = stream.next().await);
    assert_eq!(timeline_updates.len(), 1);

    // Also comes as a local echo.
    assert_let!(VectorDiff::Set { index: 1, value: item } = &timeline_updates[0]);
    let reactions = item.as_event().unwrap().reactions();
    assert_eq!(reactions.len(), 2);
    let reaction_info = reactions.get(key2).unwrap().get(user_id).unwrap();
    assert_matches!(&reaction_info.status, ReactionStatus::LocalToLocal(..));

    // Remove second reaction. It's immediately removed, since it was a local echo,
    // and it wasn't being sent.
    timeline.toggle_reaction(&item_id, key2).await.unwrap();

    assert_let!(Some(timeline_updates) = stream.next().await);
    assert_eq!(timeline_updates.len(), 1);

    assert_let!(VectorDiff::Set { index: 1, value: item } = &timeline_updates[0]);
    let reactions = item.as_event().unwrap().reactions();
    assert_eq!(reactions.len(), 1);
    let reaction_info = reactions.get(key1).unwrap().get(user_id).unwrap();
    assert_matches!(&reaction_info.status, ReactionStatus::LocalToLocal(..));

    assert_let!(Some(timeline_updates) = stream.next().await);
    assert_eq!(timeline_updates.len(), 1);

    // Now, wait for the remote echo for the message itself.
    assert_let!(VectorDiff::Set { index: 1, value: item } = &timeline_updates[0]);
    let reactions = item.as_event().unwrap().reactions();
    assert_eq!(reactions.len(), 1);
    let reaction_info = reactions.get(key1).unwrap().get(user_id).unwrap();
    // TODO(bnjbvr): why not LocalToRemote here?
    assert_matches!(&reaction_info.status, ReactionStatus::LocalToLocal(..));

    assert_let!(Some(timeline_updates) = stream.next().await);
    assert_eq!(timeline_updates.len(), 1);

    // And then the remote echo for the reaction itself.
    assert_let!(VectorDiff::Set { index: 1, value: item } = &timeline_updates[0]);
    let reactions = item.as_event().unwrap().reactions();
    assert_eq!(reactions.len(), 1);
    let reaction_info = reactions.get(key1).unwrap().get(user_id).unwrap();
    assert_matches!(&reaction_info.status, ReactionStatus::RemoteToRemote(..));

    // And we're done.
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_pending!(stream);
}
