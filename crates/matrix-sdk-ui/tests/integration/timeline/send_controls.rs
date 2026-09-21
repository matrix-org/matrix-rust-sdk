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
    assert!(!timeline.abort_send(&item_id, SendTarget::Edit).await.unwrap());
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
    assert!(!timeline.abort_send(&item_id, SendTarget::Edit).await.unwrap());
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
    let reactions = item.as_event().unwrap().reactions();
    assert_matches!(
        reactions.get(key).unwrap().get(user_id).unwrap().send_state,
        Some(EventSendState::NotSentYet { .. })
    );

    assert_let_timeout!(Some(updates) = stream.next());
    assert_eq!(updates.len(), 1);
    assert_let!(VectorDiff::Set { index: 1, value: item } = &updates[0]);
    let reactions = item.as_event().unwrap().reactions();
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
    assert!(item.as_event().unwrap().reactions().get(key).is_none());

    // Again, this time retrying.
    timeline.toggle_reaction(&item_id, key).await.unwrap();

    assert_let_timeout!(Some(updates) = stream.next());
    assert_eq!(updates.len(), 1);
    assert_let!(VectorDiff::Set { index: 1, value: item } = &updates[0]);
    let reactions = item.as_event().unwrap().reactions();
    assert_matches!(
        reactions.get(key).unwrap().get(user_id).unwrap().send_state,
        Some(EventSendState::NotSentYet { .. })
    );

    assert_let_timeout!(Some(updates) = stream.next());
    assert_eq!(updates.len(), 1);
    assert_let!(VectorDiff::Set { index: 1, value: item } = &updates[0]);
    let reactions = item.as_event().unwrap().reactions();
    assert_matches!(
        reactions.get(key).unwrap().get(user_id).unwrap().send_state,
        Some(EventSendState::SendingFailed { is_recoverable: false, .. })
    );

    timeline.retry_send(&item_id, SendTarget::Reaction { key: key.to_owned() }).await.unwrap();

    assert_let_timeout!(Some(updates) = stream.next());
    assert_eq!(updates.len(), 1);
    assert_let!(VectorDiff::Set { index: 1, value: item } = &updates[0]);
    let reactions = item.as_event().unwrap().reactions();
    assert_matches!(
        reactions.get(key).unwrap().get(user_id).unwrap().send_state,
        Some(EventSendState::NotSentYet { .. })
    );

    // Sent, then the remote echo clears the send state.
    assert_let_timeout!(Some(updates) = stream.next());
    assert_eq!(updates.len(), 2);

    assert_let!(VectorDiff::Set { index: 1, value: item } = &updates[0]);
    let reactions = item.as_event().unwrap().reactions();
    assert_matches!(
        reactions.get(key).unwrap().get(user_id).unwrap().send_state,
        Some(EventSendState::Sent { .. })
    );

    assert_let!(VectorDiff::Set { index: 1, value: item } = &updates[1]);
    let reactions = item.as_event().unwrap().reactions();
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
async fn test_no_op_when_there_is_nothing_pending() {
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
        assert!(!timeline.retry_send(&item_id, target.clone()).await.unwrap());
        assert!(!timeline.abort_send(&item_id, target).await.unwrap());
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

#[async_test]
async fn test_abort_puts_back_the_remote_edit() {
    let room_id = room_id!("!a:b.c");
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let room = server.sync_joined_room(&client, room_id).await;
    server.mock_room_state_encryption().plain().mount().await;

    let timeline = room.timeline().await.unwrap();
    let (_, mut stream) = timeline.subscribe().await;

    // Our message, already edited from another device.
    let f = EventFactory::new();
    let own_user = client.user_id().unwrap();
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id)
                .add_timeline_event(f.text_msg("hello").sender(own_user).event_id(event_id!("$1")))
                .add_timeline_event(
                    f.text_msg("* remote").sender(own_user).event_id(event_id!("$2")).edit(
                        event_id!("$1"),
                        RoomMessageEventContent::text_plain("remote").into(),
                    ),
                ),
        )
        .await;

    assert_let_timeout!(Some(_) = stream.next());
    let items = timeline.items().await;
    let item = items[1].as_event().unwrap();
    let item_id = item.identifier();
    assert_eq!(item.content().as_message().unwrap().body(), "remote");
    let remote_edit_json = item.latest_edit_json().unwrap().json().get().to_owned();

    server.mock_room_send().error_too_large().mock_once().mount().await;
    timeline.edit(&item_id, text_edit("local")).await.unwrap();

    assert_let_timeout!(Some(updates) = stream.next());
    assert_eq!(updates.len(), 1);
    assert_let!(VectorDiff::Set { index: 1, value: item } = &updates[0]);
    assert_eq!(item.as_event().unwrap().content().as_message().unwrap().body(), "local");

    assert_let_timeout!(Some(updates) = stream.next());
    assert_eq!(updates.len(), 1);
    assert_let!(VectorDiff::Set { index: 1, value: item } = &updates[0]);
    assert_matches!(
        item.as_event().unwrap().edit_send_state(),
        Some(EventSendState::SendingFailed { is_recoverable: false, .. })
    );

    assert!(timeline.abort_send(&item_id, SendTarget::Edit).await.unwrap());

    // Back to the remote edit, content and JSON alike.
    assert_let_timeout!(Some(updates) = stream.next());
    assert_eq!(updates.len(), 1);
    assert_let!(VectorDiff::Set { index: 1, value: item } = &updates[0]);
    let item = item.as_event().unwrap();
    assert_eq!(item.content().as_message().unwrap().body(), "remote");
    assert_eq!(item.latest_edit_json().unwrap().json().get(), remote_edit_json);
    assert_matches!(item.edit_send_state(), None);
    assert_pending!(stream);
}

#[async_test]
async fn test_retry_resends_without_touching_the_room_queue() {
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
    assert_let!(VectorDiff::PushBack { value: item } = &updates[0]);
    let item_id = item.as_event().unwrap().identifier();

    // Fails once, then goes out.
    server.mock_room_send().error_too_large().mock_once().mount().await;
    server.mock_room_send().ok(event_id!("$edit")).mock_once().mount().await;

    timeline.edit(&item_id, text_edit("edited")).await.unwrap();

    assert_let_timeout!(Some(_) = stream.next()); // NotSentYet
    assert_let_timeout!(Some(updates) = stream.next()); // SendingFailed
    assert_let!(VectorDiff::Set { index: 1, value: item } = &updates[0]);
    assert_matches!(
        item.as_event().unwrap().edit_send_state(),
        Some(EventSendState::SendingFailed { is_recoverable: false, .. })
    );

    // Retrying is all a client has to do: the wedge blocked the queue, it never
    // disabled it.
    assert!(room.send_queue().is_enabled());
    timeline.retry_send(&item_id, SendTarget::Edit).await.unwrap();

    assert_let_timeout!(Some(_) = stream.next()); // NotSentYet again

    assert_let_timeout!(Some(updates) = stream.next());
    assert_let!(VectorDiff::Set { index: 1, value: item } = &updates[0]);
    let item = item.as_event().unwrap();
    assert_eq!(item.content().as_message().unwrap().body(), "edited");
    assert_matches!(item.edit_send_state(), Some(EventSendState::Sent { .. }));
}

#[async_test]
async fn test_abort_unblocks_the_rest_of_the_queue() {
    let room_id = room_id!("!a:b.c");
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let room = server.sync_joined_room(&client, room_id).await;
    server.mock_room_state_encryption().plain().mount().await;

    let timeline = room.timeline().await.unwrap();
    let (_, mut stream) = timeline.subscribe().await;

    // The first message wedges. The success mock is only mounted further down, so a
    // request sneaking past the wedge fails loudly.
    server.mock_room_send().error_too_large().mock_once().mount().await;

    timeline.send(RoomMessageEventContent::text_plain("first").into()).await.unwrap();

    assert_let_timeout!(Some(updates) = stream.next());
    assert_let!(VectorDiff::PushBack { value: item } = &updates[0]);
    let first_id = item.as_event().unwrap().identifier();

    assert_let_timeout!(Some(updates) = stream.next());
    assert_let!(VectorDiff::Set { index: 1, value: item } = &updates[0]);
    assert_matches!(
        item.as_event().unwrap().send_state(),
        Some(EventSendState::SendingFailed { is_recoverable: false, .. })
    );

    // A second message piles up behind the wedged one, and stays there.
    timeline.send(RoomMessageEventContent::text_plain("second").into()).await.unwrap();
    assert_let_timeout!(Some(updates) = stream.next());
    assert_let!(VectorDiff::PushBack { value: item } = &updates[0]);
    assert_matches!(item.as_event().unwrap().send_state(), Some(EventSendState::NotSentYet { .. }));
    assert_pending!(stream);

    // Dropping the failed one lets the rest of the queue flow again.
    server.mock_room_send().ok(event_id!("$second")).mock_once().mount().await;
    assert!(timeline.abort_send(&first_id, SendTarget::Event).await.unwrap());

    assert_let_timeout!(Some(updates) = stream.next());
    assert_matches!(&updates[0], VectorDiff::Remove { index: 1 });

    assert_let_timeout!(Some(updates) = stream.next());
    assert_let!(VectorDiff::Set { index: 1, value: item } = &updates[0]);
    assert_matches!(item.as_event().unwrap().send_state(), Some(EventSendState::Sent { .. }));
}

#[async_test]
async fn test_abort_reverts_a_poll_edit_but_keeps_its_votes() {
    use ruma::events::poll::unstable_start::{UnstablePollAnswer, UnstablePollStartContentBlock};

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
            JoinedRoomBuilder::new(room_id).add_timeline_event(
                f.poll_start("q\n1. a\n2. b", "original question", vec!["a", "b"])
                    .sender(own_user)
                    .event_id(event_id!("$1")),
            ),
        )
        .await;

    assert_let_timeout!(Some(updates) = stream.next());
    assert_let!(VectorDiff::PushBack { value: item } = &updates[0]);
    let item_id = item.as_event().unwrap().identifier();
    assert_eq!(
        item.as_event().unwrap().content().as_poll().unwrap().results().question,
        "original question"
    );

    server.mock_room_send().error_too_large().mock_once().mount().await;
    let answers: Vec<UnstablePollAnswer> =
        vec![UnstablePollAnswer::new("0", "a"), UnstablePollAnswer::new("1", "b")];
    timeline
        .edit(
            &item_id,
            EditedContent::PollStart {
                fallback_text: "edited".to_owned(),
                new_content: UnstablePollStartContentBlock::new(
                    "edited question",
                    answers.try_into().unwrap(),
                ),
            },
        )
        .await
        .unwrap();

    assert_let_timeout!(Some(_) = stream.next()); // NotSentYet
    assert_let_timeout!(Some(updates) = stream.next()); // SendingFailed
    assert_let!(VectorDiff::Set { index: 1, value: item } = &updates[0]);
    let item = item.as_event().unwrap();
    assert_matches!(
        item.edit_send_state(),
        Some(EventSendState::SendingFailed { is_recoverable: false, .. })
    );
    assert_eq!(item.content().as_poll().unwrap().results().question, "edited question");

    // A vote comes in while our edit is stuck.
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id).add_timeline_event(
                f.poll_response(vec!["0"], event_id!("$1"))
                    .sender(&ALICE)
                    .event_id(event_id!("$vote")),
            ),
        )
        .await;

    assert_let_timeout!(Some(_) = stream.next());
    let items = timeline.items().await;
    let results = items[1].as_event().unwrap().content().as_poll().unwrap().results();
    assert_eq!(results.votes["0"], vec![ALICE.to_string()]);

    assert!(timeline.abort_send(&item_id, SendTarget::Edit).await.unwrap());

    // The question is back to what it was, and the vote cast meanwhile is still
    // there.
    assert_let_timeout!(Some(updates) = stream.next());
    assert_let!(VectorDiff::Set { index: 1, value: item } = updates.last().unwrap());
    let item = item.as_event().unwrap();
    let results = item.content().as_poll().unwrap().results();
    assert_eq!(results.question, "original question");
    assert!(!results.has_been_edited);
    assert_eq!(results.votes["0"], vec![ALICE.to_string()]);
    assert_matches!(item.edit_send_state(), None);
    assert_pending!(stream);
}
