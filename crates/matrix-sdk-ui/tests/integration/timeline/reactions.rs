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
use futures_util::StreamExt as _;
use matrix_sdk::{assert_let_timeout, test_utils::mocks::MatrixMockServer};
use matrix_sdk_test::{ALICE, JoinedRoomBuilder, async_test, event_factory::EventFactory};
use matrix_sdk_ui::timeline::{EventSendState, ReactionStatus, RoomExt as _};
use ruma::{event_id, events::room::message::RoomMessageEventContent, room_id};
use stream_assert::assert_pending;
use tokio::time::sleep;

#[async_test]
async fn test_abort_before_being_sent() {
    // This test checks that a reaction could be aborted *before* or *while* it's
    // being sent by the send queue.

    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    let room_id = room_id!("!r0");
    let room = server.sync_joined_room(&client, room_id).await;

    server.mock_room_state_encryption().plain().mount().await;

    let timeline = room.timeline().await.unwrap();
    let (initial_items, mut stream) = timeline.subscribe().await;

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

    let item_id = {
        assert_let_timeout!(Some(timeline_updates) = stream.next());
        assert_eq!(timeline_updates.len(), 2);

        assert_let!(VectorDiff::PushBack { value: first } = &timeline_updates[0]);
        let item = first.as_event().unwrap();
        assert_eq!(item.content().as_message().unwrap().body(), "hello");

        assert_let!(VectorDiff::PushFront { value: date_divider } = &timeline_updates[1]);
        assert!(date_divider.is_date_divider());

        assert_pending!(stream);

        item.identifier()
    };

    // Now we try to add two reactions to this message…

    // Mock the send endpoint with a delay, to give us time to abort the sending.
    server
        .mock_room_send()
        .ok_with_delay(event_id!("$2"), Duration::from_millis(150))
        .mock_once()
        .named("send for the first reaction")
        .mount()
        .await;

    server
        .mock_room_redact()
        .ok(event_id!("$3"))
        .named("redact for the first reaction")
        .mock_once()
        .mount()
        .await;

    // We add a first reaction…
    timeline.toggle_reaction(&item_id, "👍").await.unwrap();

    let user_id = client.user_id().unwrap();

    // First toggle (adding the local echo for the first reaction).
    {
        assert_let_timeout!(Some(timeline_updates) = stream.next());
        assert_eq!(timeline_updates.len(), 1);

        assert_let!(VectorDiff::Set { index: 1, value: item } = &timeline_updates[0]);

        let reactions = item.as_event().unwrap().content().reactions().cloned().unwrap_or_default();
        assert_eq!(reactions.len(), 1);
        assert_matches!(
            &reactions.get("👍").unwrap().get(user_id).unwrap().status,
            ReactionStatus::LocalToRemote(_)
        );

        assert_pending!(stream);
    }

    // We toggle (add) another reaction at the same time…
    timeline.toggle_reaction(&item_id, "🥰").await.unwrap();

    // Second toggle (adding the local echo for the second reaction).
    {
        assert_let_timeout!(Some(timeline_updates) = stream.next());
        assert_eq!(timeline_updates.len(), 1);

        assert_let!(VectorDiff::Set { index: 1, value: item } = &timeline_updates[0]);

        let reactions = item.as_event().unwrap().content().reactions().cloned().unwrap_or_default();
        assert_eq!(reactions.len(), 2);
        assert_matches!(
            &reactions.get("👍").unwrap().get(user_id).unwrap().status,
            ReactionStatus::LocalToRemote(_)
        );
        assert_matches!(
            &reactions.get("🥰").unwrap().get(user_id).unwrap().status,
            ReactionStatus::LocalToRemote(_)
        );

        assert_pending!(stream);
    }

    // Then we remove the first one; because it was being sent, it should lead to a
    // redaction event.
    timeline.toggle_reaction(&item_id, "👍").await.unwrap();

    {
        assert_let_timeout!(Some(timeline_updates) = stream.next());
        assert_eq!(timeline_updates.len(), 1);

        assert_let!(VectorDiff::Set { index: 1, value: item } = &timeline_updates[0]);

        let reactions = item.as_event().unwrap().content().reactions().cloned().unwrap_or_default();
        assert_eq!(reactions.len(), 1);
        assert_matches!(
            &reactions.get("🥰").unwrap().get(user_id).unwrap().status,
            ReactionStatus::LocalToRemote(_)
        );

        assert_pending!(stream);
    }

    // But because the first one was being sent, this one won't and the local echo
    // could be discarded.
    timeline.toggle_reaction(&item_id, "🥰").await.unwrap();

    {
        assert_let_timeout!(Some(timeline_updates) = stream.next());
        assert_eq!(timeline_updates.len(), 1);

        assert_let!(VectorDiff::Set { index: 1, value: item } = &timeline_updates[0]);

        let reactions = item.as_event().unwrap().content().reactions().cloned().unwrap_or_default();
        assert!(reactions.is_empty());

        assert_pending!(stream);
    }

    // Let the send queue process the redaction in the background.
    sleep(Duration::from_millis(300)).await;

    assert_let_timeout!(Some(timeline_updates) = stream.next());
    assert_eq!(timeline_updates.len(), 1);

    // The remote event comes in.
    assert_matches!(&timeline_updates[0], VectorDiff::Set { index: 1, value: remote_event });
    let remote_event = remote_event.as_event().unwrap();
    assert_eq!(remote_event.event_id(), Some(event_id));
    assert_eq!(remote_event.content().reactions().unwrap().len(), 1);

    assert_pending!(stream);
}

#[async_test]
async fn test_redact_failed() {
    // This test checks that if a reaction redaction failed, then we re-insert the
    // reaction after displaying it was removed.

    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let user_id = client.user_id().unwrap();

    let room_id = room_id!("!a98sd12bjh:example.org");
    let room = server.sync_joined_room(&client, room_id).await;

    server.mock_room_state_encryption().plain().mount().await;

    let timeline = room.timeline().await.unwrap();
    let (initial_items, mut stream) = timeline.subscribe().await;

    assert!(initial_items.is_empty());

    let f = EventFactory::new();

    let event_id = event_id!("$1");
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id)
                .add_timeline_event(f.text_msg("hello").sender(&ALICE).event_id(event_id))
                .add_timeline_event(f.reaction(event_id, "😆").sender(user_id)),
        )
        .await;

    assert_let_timeout!(Some(timeline_updates) = stream.next());
    assert_eq!(timeline_updates.len(), 3);

    let item_id = {
        assert_let!(VectorDiff::PushBack { value: item } = &timeline_updates[0]);

        let item = item.as_event().unwrap();
        assert_eq!(item.content().as_message().unwrap().body(), "hello");
        assert!(item.content().reactions().cloned().unwrap_or_default().is_empty());

        item.identifier()
    };

    assert_let!(VectorDiff::Set { index: 0, value: item } = &timeline_updates[1]);
    assert_eq!(
        item.as_event().unwrap().content().reactions().cloned().unwrap_or_default().len(),
        1
    );

    assert_let!(VectorDiff::PushFront { value: date_divider } = &timeline_updates[2]);
    assert!(date_divider.is_date_divider());

    // Now, redact the annotation we previously added.
    server.mock_room_redact().error500().named("redact").mock_once().mount().await;

    // We toggle the reaction, which fails with an error.
    timeline.toggle_reaction(&item_id, "😆").await.unwrap_err();

    assert_let_timeout!(Some(timeline_updates) = stream.next());
    assert_eq!(timeline_updates.len(), 2);

    // The local echo is removed (assuming the redaction works)…
    assert_let!(VectorDiff::Set { index: 1, value: item } = &timeline_updates[0]);
    assert!(item.as_event().unwrap().content().reactions().cloned().unwrap_or_default().is_empty());

    // …then added back, after redaction failed.
    assert_let!(VectorDiff::Set { index: 1, value: item } = &timeline_updates[1]);
    assert_eq!(
        item.as_event().unwrap().content().reactions().cloned().unwrap_or_default().len(),
        1
    );

    sleep(Duration::from_millis(150)).await;
    assert_pending!(stream);
}

#[async_test]
async fn test_local_reaction_to_local_echo() {
    // This test checks that if a reaction redaction failed, then we re-insert the
    // reaction after displaying it was removed.

    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let user_id = client.user_id().unwrap();

    let room_id = room_id!("!a98sd12bjh:example.org");
    let room = server.sync_joined_room(&client, room_id).await;

    server.mock_room_state_encryption().plain().mount().await;

    let timeline = room.timeline().await.unwrap();
    let (initial_items, mut stream) = timeline.subscribe().await;

    assert!(initial_items.is_empty());

    // Mock for the first message.
    // Add a duration to the response, so we can check other things in the
    // meanwhile.
    server
        .mock_room_send()
        .ok_with_delay(event_id!("$0"), Duration::from_millis(150))
        .mock_once()
        .mount()
        .await;

    // Mock for the first reaction.
    server.mock_room_send().ok(event_id!("$1")).mock_once().mount().await;

    // Send a local event.
    let _ = timeline.send(RoomMessageEventContent::text_plain("lol").into()).await.unwrap();

    let item_id = {
        assert_let_timeout!(Some(timeline_updates) = stream.next());
        assert_eq!(timeline_updates.len(), 2);

        // Receive a local echo.
        assert_let!(VectorDiff::PushBack { value: item } = &timeline_updates[0]);

        let item = item.as_event().unwrap();
        assert!(item.is_local_echo());
        assert_matches!(item.send_state(), Some(EventSendState::NotSentYet { progress: None }));

        assert_eq!(item.content().as_message().unwrap().body(), "lol");
        assert!(item.content().reactions().cloned().unwrap_or_default().is_empty());

        // Good ol' date divider.
        assert_let!(VectorDiff::PushFront { value: date_divider } = &timeline_updates[1]);
        assert!(date_divider.is_date_divider());

        assert_pending!(stream);

        item.identifier()
    };

    // Add a reaction before the remote echo comes back.
    let key1 = "🤣";
    timeline.toggle_reaction(&item_id, key1).await.unwrap();

    {
        assert_let_timeout!(Some(timeline_updates) = stream.next());
        assert_eq!(timeline_updates.len(), 1);

        // The reaction is added to the local echo.
        assert_let!(VectorDiff::Set { index: 1, value: item } = &timeline_updates[0]);
        let item = item.as_event().unwrap();
        assert!(item.is_local_echo());
        assert_matches!(item.send_state(), Some(EventSendState::NotSentYet { progress: None }));

        let reactions = item.content().reactions().cloned().unwrap_or_default();
        assert_eq!(reactions.len(), 1);
        let reaction_info = reactions.get(key1).unwrap().get(user_id).unwrap();
        assert_matches!(&reaction_info.status, ReactionStatus::LocalToLocal(..));

        assert_pending!(stream);
    }

    // Add another reaction.
    let key2 = "😈";
    timeline.toggle_reaction(&item_id, key2).await.unwrap();

    {
        assert_let_timeout!(Some(timeline_updates) = stream.next());
        assert_eq!(timeline_updates.len(), 1);

        // Also comes as a local echo.
        assert_let!(VectorDiff::Set { index: 1, value: item } = &timeline_updates[0]);
        let item = item.as_event().unwrap();
        assert!(item.is_local_echo());
        assert_matches!(item.send_state(), Some(EventSendState::NotSentYet { progress: None }));

        let reactions = item.content().reactions().cloned().unwrap_or_default();
        assert_eq!(reactions.len(), 2);
        let reaction_info = reactions.get(key2).unwrap().get(user_id).unwrap();
        assert_matches!(&reaction_info.status, ReactionStatus::LocalToLocal(..));

        assert_pending!(stream);
    }

    // Remove second reaction. It's immediately removed, since it was a local echo,
    // and it wasn't being sent.
    timeline.toggle_reaction(&item_id, key2).await.unwrap();

    {
        assert_let_timeout!(Some(timeline_updates) = stream.next());
        assert_eq!(timeline_updates.len(), 1);

        assert_let!(VectorDiff::Set { index: 1, value: item } = &timeline_updates[0]);
        let item = item.as_event().unwrap();
        assert!(item.is_local_echo());
        assert_matches!(item.send_state(), Some(EventSendState::NotSentYet { progress: None }));

        let reactions = item.content().reactions().cloned().unwrap_or_default();
        assert_eq!(reactions.len(), 1);
        let reaction_info = reactions.get(key1).unwrap().get(user_id).unwrap();
        assert_matches!(&reaction_info.status, ReactionStatus::LocalToLocal(..));

        assert_pending!(stream);
    }

    // Now, wait for the remote echo for the message itself.
    {
        assert_let_timeout!(Duration::from_secs(2), Some(timeline_updates) = stream.next());
        assert_eq!(timeline_updates.len(), 1);

        assert_let!(VectorDiff::Set { index: 1, value: item } = &timeline_updates[0]);
        let item = item.as_event().unwrap();

        // Still a local echo, but now has a send state set to Sent.
        assert!(item.is_local_echo());
        assert_matches!(item.send_state(), Some(EventSendState::Sent { .. }));

        let reactions = item.content().reactions().cloned().unwrap_or_default();
        assert_eq!(reactions.len(), 1);
        let reaction_info = reactions.get(key1).unwrap().get(user_id).unwrap();
        // TODO: why not LocalToRemote here?
        assert_matches!(&reaction_info.status, ReactionStatus::LocalToLocal(..));

        assert_pending!(stream);
    }

    assert_let_timeout!(Some(timeline_updates) = stream.next());
    assert!(!timeline_updates.is_empty());

    // And then the remote echo for the reaction itself.
    for timeline_update in timeline_updates {
        assert_let!(VectorDiff::Set { index: 1, value: item } = timeline_update);
        let reactions = item.as_event().unwrap().content().reactions().cloned().unwrap_or_default();
        assert_eq!(reactions.len(), 1);
        let reaction_info = reactions.get(key1).unwrap().get(user_id).unwrap();
        assert_matches!(&reaction_info.status, ReactionStatus::RemoteToRemote(..));
    }

    // And we're done.
    sleep(Duration::from_millis(150)).await;
    assert_pending!(stream);
}
