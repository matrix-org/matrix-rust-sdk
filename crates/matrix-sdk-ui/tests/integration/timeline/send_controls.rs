use assert_matches::assert_matches;
use assert_matches2::assert_let;
use eyeball_im::VectorDiff;
use futures_util::StreamExt as _;
use matrix_sdk::{
    assert_let_timeout, room::edit::EditedContent, test_utils::mocks::MatrixMockServer,
};
use matrix_sdk_test::{ALICE, JoinedRoomBuilder, async_test, event_factory::EventFactory};
use matrix_sdk_ui::timeline::{
    Error, EventSendState, RoomExt as _, SendTarget, TimelineEventItemId,
};
use ruma::{
    event_id,
    events::room::message::{RoomMessageEventContent, RoomMessageEventContentWithoutRelation},
    room_id,
};
use stream_assert::assert_pending;
use tokio::task::yield_now;

fn text_edit(body: &str) -> EditedContent {
    EditedContent::RoomMessage(RoomMessageEventContentWithoutRelation::text_plain(body))
}

#[async_test]
async fn test_abort_failed_edit_reverts_content() {
    let room_id = room_id!("!a:b.c");
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let room = server.sync_joined_room(&client, room_id).await;
    server.mock_room_state_encryption().plain().mount().await;

    let timeline = room.timeline().await.unwrap();
    let (_, mut stream) = timeline.subscribe().await;

    let f = EventFactory::new();
    let own_user = client.user_id().unwrap();
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id)
                .add_timeline_event(f.text_msg("hello").sender(own_user).event_id(event_id!("$1"))),
        )
        .await;

    assert_let_timeout!(Some(updates) = stream.next());
    assert_eq!(updates.len(), 2);
    assert_let!(VectorDiff::PushBack { value: item } = &updates[0]);
    let item_id = item.as_event().unwrap().identifier();
    assert_let!(VectorDiff::PushFront { value: date_divider } = &updates[1]);
    assert!(date_divider.is_date_divider());

    server.mock_room_send().error_too_large().mock_once().mount().await;
    timeline.edit(&item_id, text_edit("edited")).await.unwrap();

    // Pending, then failed.
    assert_let_timeout!(Some(updates) = stream.next());
    assert_eq!(updates.len(), 1);
    assert_let!(VectorDiff::Set { index: 1, value: item } = &updates[0]);
    assert_matches!(
        item.as_event().unwrap().edit_send_state(),
        Some(EventSendState::NotSentYet { .. })
    );

    assert_let_timeout!(Some(updates) = stream.next());
    assert_eq!(updates.len(), 1);
    assert_let!(VectorDiff::Set { index: 1, value: item } = &updates[0]);
    assert_matches!(
        item.as_event().unwrap().edit_send_state(),
        Some(EventSendState::SendingFailed { is_recoverable: false, .. })
    );

    assert!(timeline.abort_send(&item_id, SendTarget::Edit).await.unwrap());

    // The edit is gone, the original content is back.
    assert_let_timeout!(Some(updates) = stream.next());
    assert_eq!(updates.len(), 1);
    assert_let!(VectorDiff::Set { index: 1, value: item } = &updates[0]);
    let item = item.as_event().unwrap();
    assert_eq!(item.content().as_message().unwrap().body(), "hello");
    assert_matches!(item.edit_send_state(), None);

    // Nothing left to act on.
    assert_matches!(
        timeline.abort_send(&item_id, SendTarget::Edit).await,
        Err(Error::NoPendingSend { .. })
    );
    assert_pending!(stream);
}

#[async_test]
async fn test_abort_acts_on_the_shown_edit() {
    let room_id = room_id!("!a:b.c");
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let room = server.sync_joined_room(&client, room_id).await;
    server.mock_room_state_encryption().plain().mount().await;

    let timeline = room.timeline().await.unwrap();
    let (_, mut stream) = timeline.subscribe().await;

    let f = EventFactory::new();
    let own_user = client.user_id().unwrap();
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id)
                .add_timeline_event(f.text_msg("hello").sender(own_user).event_id(event_id!("$1"))),
        )
        .await;

    assert_let_timeout!(Some(updates) = stream.next());
    assert_eq!(updates.len(), 2);
    assert_let!(VectorDiff::PushBack { value: item } = &updates[0]);
    let item_id = item.as_event().unwrap().identifier();
    assert_let!(VectorDiff::PushFront { value: date_divider } = &updates[1]);
    assert!(date_divider.is_date_divider());

    // The first edit fails; the second waits behind it, then goes out.
    server.mock_room_send().error_too_large().mock_once().mount().await;
    server.mock_room_send().ok(event_id!("$second")).mock_once().mount().await;

    timeline.edit(&item_id, text_edit("first")).await.unwrap();

    assert_let_timeout!(Some(updates) = stream.next());
    assert_eq!(updates.len(), 1);
    assert_let!(VectorDiff::Set { index: 1, value: item } = &updates[0]);
    assert_matches!(
        item.as_event().unwrap().edit_send_state(),
        Some(EventSendState::NotSentYet { .. })
    );

    assert_let_timeout!(Some(updates) = stream.next());
    assert_eq!(updates.len(), 1);
    assert_let!(VectorDiff::Set { index: 1, value: item } = &updates[0]);
    assert_matches!(
        item.as_event().unwrap().edit_send_state(),
        Some(EventSendState::SendingFailed { is_recoverable: false, .. })
    );

    timeline.edit(&item_id, text_edit("second")).await.unwrap();

    // Queueing the second one re-renders the item, still showing the first.
    assert_let_timeout!(Some(updates) = stream.next());
    assert_eq!(updates.len(), 1);
    assert_let!(VectorDiff::Set { index: 1, value: item } = &updates[0]);
    let item = item.as_event().unwrap();
    assert_eq!(item.content().as_message().unwrap().body(), "first");
    assert_matches!(
        item.edit_send_state(),
        Some(EventSendState::SendingFailed { is_recoverable: false, .. })
    );

    // Dropping it lets the second one through.
    room.send_queue().set_enabled(true);
    assert!(timeline.abort_send(&item_id, SendTarget::Edit).await.unwrap());

    assert_let_timeout!(Some(updates) = stream.next());
    assert_eq!(updates.len(), 1);
    assert_let!(VectorDiff::Set { index: 1, value: item } = &updates[0]);
    let item = item.as_event().unwrap();
    assert_eq!(item.content().as_message().unwrap().body(), "second");
    assert_matches!(item.edit_send_state(), Some(EventSendState::NotSentYet { .. }));

    // Sent, then the remote echo clears the send state.
    assert_let_timeout!(Some(updates) = stream.next());
    assert_eq!(updates.len(), 2);

    assert_let!(VectorDiff::Set { index: 1, value: item } = &updates[0]);
    let item = item.as_event().unwrap();
    assert_eq!(item.content().as_message().unwrap().body(), "second");
    assert_matches!(item.edit_send_state(), Some(EventSendState::Sent { .. }));

    assert_let!(VectorDiff::Set { index: 1, value: item } = &updates[1]);
    let item = item.as_event().unwrap();
    assert_eq!(item.content().as_message().unwrap().body(), "second");
    assert_matches!(item.edit_send_state(), None);

    assert_pending!(stream);
}

#[async_test]
async fn test_retry_failed_edit() {
    let room_id = room_id!("!a:b.c");
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let room = server.sync_joined_room(&client, room_id).await;
    server.mock_room_state_encryption().plain().mount().await;

    let timeline = room.timeline().await.unwrap();
    let (_, mut stream) = timeline.subscribe().await;

    let f = EventFactory::new();
    let own_user = client.user_id().unwrap();
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id)
                .add_timeline_event(f.text_msg("hello").sender(own_user).event_id(event_id!("$1"))),
        )
        .await;

    assert_let_timeout!(Some(updates) = stream.next());
    assert_eq!(updates.len(), 2);
    assert_let!(VectorDiff::PushBack { value: item } = &updates[0]);
    let item_id = item.as_event().unwrap().identifier();
    assert_let!(VectorDiff::PushFront { value: date_divider } = &updates[1]);
    assert!(date_divider.is_date_divider());

    // Fails once, then goes out.
    server.mock_room_send().error_too_large().mock_once().mount().await;
    server.mock_room_send().ok(event_id!("$edit")).mock_once().mount().await;

    timeline.edit(&item_id, text_edit("edited")).await.unwrap();

    assert_let_timeout!(Some(updates) = stream.next());
    assert_eq!(updates.len(), 1);
    assert_let!(VectorDiff::Set { index: 1, value: item } = &updates[0]);
    assert_matches!(
        item.as_event().unwrap().edit_send_state(),
        Some(EventSendState::NotSentYet { .. })
    );

    assert_let_timeout!(Some(updates) = stream.next());
    assert_eq!(updates.len(), 1);
    assert_let!(VectorDiff::Set { index: 1, value: item } = &updates[0]);
    assert_matches!(
        item.as_event().unwrap().edit_send_state(),
        Some(EventSendState::SendingFailed { is_recoverable: false, .. })
    );

    // The failure disabled the room's queue. Let the queue task park on the
    // wedged request first, so only the unwedge can wake it.
    room.send_queue().set_enabled(true);
    yield_now().await;
    timeline.retry_send(&item_id, SendTarget::Edit).await.unwrap();

    // Pending again, then sent.
    assert_let_timeout!(Some(updates) = stream.next());
    assert_eq!(updates.len(), 1);
    assert_let!(VectorDiff::Set { index: 1, value: item } = &updates[0]);
    assert_matches!(
        item.as_event().unwrap().edit_send_state(),
        Some(EventSendState::NotSentYet { .. })
    );

    // Sent, then the remote echo clears the send state.
    assert_let_timeout!(Some(updates) = stream.next());
    assert_eq!(updates.len(), 2);

    assert_let!(VectorDiff::Set { index: 1, value: item } = &updates[0]);
    let item = item.as_event().unwrap();
    assert_eq!(item.content().as_message().unwrap().body(), "edited");
    assert_matches!(item.edit_send_state(), Some(EventSendState::Sent { .. }));

    assert_let!(VectorDiff::Set { index: 1, value: item } = &updates[1]);
    let item = item.as_event().unwrap();
    assert_eq!(item.content().as_message().unwrap().body(), "edited");
    assert_matches!(item.edit_send_state(), None);

    // Once sent, there's nothing local left to abort.
    assert_matches!(
        timeline.abort_send(&item_id, SendTarget::Edit).await,
        Err(Error::NoPendingSend { .. })
    );
    assert_pending!(stream);
}

#[async_test]
async fn test_abort_failed_redaction_restores_item() {
    let room_id = room_id!("!a:b.c");
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let room = server.sync_joined_room(&client, room_id).await;
    server.mock_room_state_encryption().plain().mount().await;

    let timeline = room.timeline().await.unwrap();
    let (_, mut stream) = timeline.subscribe().await;

    let f = EventFactory::new();
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id)
                .add_timeline_event(f.text_msg("hello").sender(&ALICE).event_id(event_id!("$1"))),
        )
        .await;

    assert_let_timeout!(Some(updates) = stream.next());
    assert_eq!(updates.len(), 2);
    assert_let!(VectorDiff::PushBack { value: item } = &updates[0]);
    let item_id = item.as_event().unwrap().identifier();
    assert_let!(VectorDiff::PushFront { value: date_divider } = &updates[1]);
    assert!(date_divider.is_date_divider());

    server.mock_room_redact().error_too_large().mock_once().mount().await;
    timeline.redact(&item_id, None).await.unwrap();

    // Redacted locally, pending, then failed.
    assert_let_timeout!(Some(updates) = stream.next());
    assert_eq!(updates.len(), 1);
    assert_let!(VectorDiff::Set { index: 1, value: item } = &updates[0]);
    let item = item.as_event().unwrap();
    assert!(item.content().is_redacted());
    assert_matches!(item.redaction_send_state(), Some(EventSendState::NotSentYet { .. }));

    assert_let_timeout!(Some(updates) = stream.next());
    assert_eq!(updates.len(), 1);
    assert_let!(VectorDiff::Set { index: 1, value: item } = &updates[0]);
    assert_matches!(
        item.as_event().unwrap().redaction_send_state(),
        Some(EventSendState::SendingFailed { is_recoverable: false, .. })
    );

    assert!(timeline.abort_send(&item_id, SendTarget::Redaction).await.unwrap());

    // The message is back.
    assert_let_timeout!(Some(updates) = stream.next());
    assert_eq!(updates.len(), 1);
    assert_let!(VectorDiff::Set { index: 1, value: item } = &updates[0]);
    let item = item.as_event().unwrap();
    assert_eq!(item.content().as_message().unwrap().body(), "hello");
    assert_matches!(item.redaction_send_state(), None);
    assert_pending!(stream);
}

#[async_test]
async fn test_retry_failed_redaction() {
    let room_id = room_id!("!a:b.c");
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let room = server.sync_joined_room(&client, room_id).await;
    server.mock_room_state_encryption().plain().mount().await;

    let timeline = room.timeline().await.unwrap();
    let (_, mut stream) = timeline.subscribe().await;

    let f = EventFactory::new();
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id)
                .add_timeline_event(f.text_msg("hello").sender(&ALICE).event_id(event_id!("$1"))),
        )
        .await;

    assert_let_timeout!(Some(updates) = stream.next());
    assert_eq!(updates.len(), 2);
    assert_let!(VectorDiff::PushBack { value: item } = &updates[0]);
    let item_id = item.as_event().unwrap().identifier();
    assert_let!(VectorDiff::PushFront { value: date_divider } = &updates[1]);
    assert!(date_divider.is_date_divider());

    server.mock_room_redact().error_too_large().mock_once().mount().await;
    server.mock_room_redact().ok(event_id!("$redaction")).mock_once().mount().await;
    timeline.redact(&item_id, None).await.unwrap();

    assert_let_timeout!(Some(updates) = stream.next());
    assert_eq!(updates.len(), 1);
    assert_let!(VectorDiff::Set { index: 1, value: item } = &updates[0]);
    assert_matches!(
        item.as_event().unwrap().redaction_send_state(),
        Some(EventSendState::NotSentYet { .. })
    );

    assert_let_timeout!(Some(updates) = stream.next());
    assert_eq!(updates.len(), 1);
    assert_let!(VectorDiff::Set { index: 1, value: item } = &updates[0]);
    assert_matches!(
        item.as_event().unwrap().redaction_send_state(),
        Some(EventSendState::SendingFailed { is_recoverable: false, .. })
    );

    room.send_queue().set_enabled(true);
    yield_now().await;
    timeline.retry_send(&item_id, SendTarget::Redaction).await.unwrap();

    assert_let_timeout!(Some(updates) = stream.next());
    assert_eq!(updates.len(), 1);
    assert_let!(VectorDiff::Set { index: 1, value: item } = &updates[0]);
    assert_matches!(
        item.as_event().unwrap().redaction_send_state(),
        Some(EventSendState::NotSentYet { .. })
    );

    // Sent, then the remote echo clears the send state.
    assert_let_timeout!(Some(updates) = stream.next());
    assert_eq!(updates.len(), 2);

    assert_let!(VectorDiff::Set { index: 1, value: item } = &updates[0]);
    let item = item.as_event().unwrap();
    assert!(item.content().is_redacted());
    assert_matches!(item.redaction_send_state(), Some(EventSendState::Sent { .. }));

    assert_let!(VectorDiff::Set { index: 1, value: item } = &updates[1]);
    let item = item.as_event().unwrap();
    assert!(item.content().is_redacted());
    assert_matches!(item.redaction_send_state(), None);

    assert_pending!(stream);
}

#[async_test]
async fn test_abort_and_retry_failed_reaction() {
    let room_id = room_id!("!a:b.c");
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let room = server.sync_joined_room(&client, room_id).await;
    server.mock_room_state_encryption().plain().mount().await;

    let timeline = room.timeline().await.unwrap();
    let (_, mut stream) = timeline.subscribe().await;

    let f = EventFactory::new();
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id)
                .add_timeline_event(f.text_msg("hello").sender(&ALICE).event_id(event_id!("$1"))),
        )
        .await;

    assert_let_timeout!(Some(updates) = stream.next());
    assert_eq!(updates.len(), 2);
    assert_let!(VectorDiff::PushBack { value: item } = &updates[0]);
    let item_id = item.as_event().unwrap().identifier();
    assert_let!(VectorDiff::PushFront { value: date_divider } = &updates[1]);
    assert!(date_divider.is_date_divider());

    let user_id = client.user_id().unwrap();
    let key = "👍";

    // First reaction fails and gets aborted, second fails and gets retried.
    server.mock_room_send().error_too_large().mock_once().mount().await;
    server.mock_room_send().error_too_large().mock_once().mount().await;
    server.mock_room_send().ok(event_id!("$reaction")).mock_once().mount().await;

    timeline.toggle_reaction(&item_id, key).await.unwrap();

    assert_let_timeout!(Some(updates) = stream.next());
    assert_eq!(updates.len(), 1);
    assert_let!(VectorDiff::Set { index: 1, value: item } = &updates[0]);
    let reactions = item.as_event().unwrap().content().reactions().unwrap();
    assert_matches!(
        reactions.get(key).unwrap().get(user_id).unwrap().send_state,
        Some(EventSendState::NotSentYet { .. })
    );

    assert_let_timeout!(Some(updates) = stream.next());
    assert_eq!(updates.len(), 1);
    assert_let!(VectorDiff::Set { index: 1, value: item } = &updates[0]);
    let reactions = item.as_event().unwrap().content().reactions().unwrap();
    assert_matches!(
        reactions.get(key).unwrap().get(user_id).unwrap().send_state,
        Some(EventSendState::SendingFailed { is_recoverable: false, .. })
    );

    assert!(
        timeline.abort_send(&item_id, SendTarget::Reaction { key: key.to_owned() }).await.unwrap()
    );

    assert_let_timeout!(Some(updates) = stream.next());
    assert_eq!(updates.len(), 1);
    assert_let!(VectorDiff::Set { index: 1, value: item } = &updates[0]);
    assert!(item.as_event().unwrap().content().reactions().is_none_or(|r| r.get(key).is_none()));

    // Again, this time retrying.
    room.send_queue().set_enabled(true);
    timeline.toggle_reaction(&item_id, key).await.unwrap();

    assert_let_timeout!(Some(updates) = stream.next());
    assert_eq!(updates.len(), 1);
    assert_let!(VectorDiff::Set { index: 1, value: item } = &updates[0]);
    let reactions = item.as_event().unwrap().content().reactions().unwrap();
    assert_matches!(
        reactions.get(key).unwrap().get(user_id).unwrap().send_state,
        Some(EventSendState::NotSentYet { .. })
    );

    assert_let_timeout!(Some(updates) = stream.next());
    assert_eq!(updates.len(), 1);
    assert_let!(VectorDiff::Set { index: 1, value: item } = &updates[0]);
    let reactions = item.as_event().unwrap().content().reactions().unwrap();
    assert_matches!(
        reactions.get(key).unwrap().get(user_id).unwrap().send_state,
        Some(EventSendState::SendingFailed { is_recoverable: false, .. })
    );

    room.send_queue().set_enabled(true);
    yield_now().await;
    timeline.retry_send(&item_id, SendTarget::Reaction { key: key.to_owned() }).await.unwrap();

    assert_let_timeout!(Some(updates) = stream.next());
    assert_eq!(updates.len(), 1);
    assert_let!(VectorDiff::Set { index: 1, value: item } = &updates[0]);
    let reactions = item.as_event().unwrap().content().reactions().unwrap();
    assert_matches!(
        reactions.get(key).unwrap().get(user_id).unwrap().send_state,
        Some(EventSendState::NotSentYet { .. })
    );

    // Sent, then the remote echo clears the send state.
    assert_let_timeout!(Some(updates) = stream.next());
    assert_eq!(updates.len(), 2);

    assert_let!(VectorDiff::Set { index: 1, value: item } = &updates[0]);
    let reactions = item.as_event().unwrap().content().reactions().unwrap();
    assert_matches!(
        reactions.get(key).unwrap().get(user_id).unwrap().send_state,
        Some(EventSendState::Sent { .. })
    );

    assert_let!(VectorDiff::Set { index: 1, value: item } = &updates[1]);
    let reactions = item.as_event().unwrap().content().reactions().unwrap();
    assert_matches!(reactions.get(key).unwrap().get(user_id).unwrap().send_state, None);

    assert_pending!(stream);
}

#[async_test]
async fn test_event_target_acts_on_the_local_echo() {
    let room_id = room_id!("!a:b.c");
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let room = server.sync_joined_room(&client, room_id).await;
    server.mock_room_state_encryption().plain().mount().await;

    let timeline = room.timeline().await.unwrap();
    let (_, mut stream) = timeline.subscribe().await;

    server.mock_room_send().error_too_large().mock_once().mount().await;
    timeline.send(RoomMessageEventContent::text_plain("wall of text").into()).await.unwrap();

    // The local echo, then the date divider ahead of it.
    assert_let_timeout!(Some(updates) = stream.next());
    assert_eq!(updates.len(), 2);
    assert_let!(VectorDiff::PushBack { value: item } = &updates[0]);
    let item_id = item.as_event().unwrap().identifier();
    assert_let!(VectorDiff::PushFront { value: date_divider } = &updates[1]);
    assert!(date_divider.is_date_divider());

    assert_let_timeout!(Some(updates) = stream.next());
    assert_eq!(updates.len(), 1);
    assert_let!(VectorDiff::Set { index: 1, value: item } = &updates[0]);
    assert_matches!(
        item.as_event().unwrap().send_state(),
        Some(EventSendState::SendingFailed { is_recoverable: false, .. })
    );

    assert!(timeline.abort_send(&item_id, SendTarget::Event).await.unwrap());

    // The echo goes, and the date divider with it.
    assert_let_timeout!(Some(updates) = stream.next());
    assert_eq!(updates.len(), 2);
    assert_matches!(&updates[0], VectorDiff::Remove { index: 1 });
    assert_matches!(&updates[1], VectorDiff::Remove { index: 0 });
    assert_pending!(stream);
}

#[async_test]
async fn test_errors_when_there_is_nothing_pending() {
    let room_id = room_id!("!a:b.c");
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let room = server.sync_joined_room(&client, room_id).await;
    server.mock_room_state_encryption().plain().mount().await;

    let timeline = room.timeline().await.unwrap();
    let (_, mut stream) = timeline.subscribe().await;

    let f = EventFactory::new();
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id)
                .add_timeline_event(f.text_msg("hello").sender(&ALICE).event_id(event_id!("$1"))),
        )
        .await;

    assert_let_timeout!(Some(updates) = stream.next());
    assert_eq!(updates.len(), 2);
    assert_let!(VectorDiff::PushBack { value: item } = &updates[0]);
    let item_id = item.as_event().unwrap().identifier();
    assert_let!(VectorDiff::PushFront { value: date_divider } = &updates[1]);
    assert!(date_divider.is_date_divider());

    for target in [
        SendTarget::Event,
        SendTarget::Edit,
        SendTarget::Redaction,
        SendTarget::Reaction { key: "👍".to_owned() },
    ] {
        assert_matches!(
            timeline.retry_send(&item_id, target.clone()).await,
            Err(Error::NoPendingSend { .. })
        );
        assert_matches!(
            timeline.abort_send(&item_id, target).await,
            Err(Error::NoPendingSend { .. })
        );
    }

    let unknown = TimelineEventItemId::EventId(event_id!("$nope").to_owned());
    assert_matches!(
        timeline.retry_send(&unknown, SendTarget::Edit).await,
        Err(Error::EventNotInTimeline(_))
    );
    assert_matches!(
        timeline.abort_send(&unknown, SendTarget::Edit).await,
        Err(Error::EventNotInTimeline(_))
    );
    assert_pending!(stream);
}
