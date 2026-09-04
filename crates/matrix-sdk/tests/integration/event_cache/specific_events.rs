use std::time::Duration;

use imbl::Vector;
use matrix_sdk::{
    event_cache::{EventCacheError, TimelineVectorDiffs},
    test_utils::mocks::{MatrixMockServer, RoomRelationsResponseTemplate},
    timeout::timeout,
};
use matrix_sdk_base::event_cache::Event;
use matrix_sdk_test::{ALICE, JoinedRoomBuilder, async_test, event_factory::EventFactory};
use ruma::{
    EventId, OwnedEventId, event_id, events::room::message::RoomMessageEventContentWithoutRelation,
    room_id,
};
use tokio::sync::broadcast::Receiver;

fn room_id() -> &'static ruma::RoomId {
    room_id!("!galette:saucisse.bzh")
}

/// Wait for the next update, apply it and whatever follows it closely
/// (one sync can produce several) to `events`.
async fn apply_next_updates(
    subscriber: &mut Receiver<TimelineVectorDiffs>,
    events: &mut Vector<Event>,
) {
    let mut wait = Duration::from_secs(3);

    while let Ok(update) = timeout(subscriber.recv(), wait).await {
        for diff in update.expect("the update channel should be open").diffs {
            diff.apply(events);
        }
        wait = Duration::from_millis(300);
    }

    assert!(wait < Duration::from_secs(3), "an update should arrive");
}

/// Assert that no update arrives for a while.
async fn assert_no_update(subscriber: &mut Receiver<TimelineVectorDiffs>) {
    assert!(timeout(subscriber.recv(), Duration::from_millis(300)).await.is_err());
}

fn event_ids(events: &Vector<Event>) -> Vec<OwnedEventId> {
    events.iter().filter_map(|event| event.event_id().map(ToOwned::to_owned)).collect()
}

fn find<'a>(events: &'a Vector<Event>, event_id: &EventId) -> &'a Event {
    events.iter().find(|event| event.event_id() == Some(event_id)).expect("event should be present")
}

async fn subscribed_client(server: &MatrixMockServer) -> matrix_sdk::Client {
    let client = server.client_builder().build().await;
    client.event_cache().subscribe().unwrap();
    server.sync_joined_room(&client, room_id()).await;
    client
}

#[async_test]
async fn test_specific_events_are_loaded_with_their_relations() {
    let f = EventFactory::new().room(room_id()).sender(*ALICE);

    let first = f.text_msg("first").event_id(event_id!("$first")).server_ts(1).into_event();
    let second = f.text_msg("second").event_id(event_id!("$second")).server_ts(2).into_event();
    let reaction = f
        .reaction(event_id!("$first"), "👍")
        .event_id(event_id!("$reaction"))
        .server_ts(3)
        .into_event();
    let edit = f
        .text_msg("* second, edited")
        .edit(
            event_id!("$second"),
            RoomMessageEventContentWithoutRelation::text_plain("second, edited"),
        )
        .event_id(event_id!("$edit"))
        .server_ts(4)
        .into_event();

    let server = MatrixMockServer::new().await;
    server.mock_room_event().match_event_id().ok(first).mount().await;
    server.mock_room_event().match_event_id().ok(second).mount().await;
    server
        .mock_room_relations()
        .match_target_event(event_id!("$first").to_owned())
        .ok(RoomRelationsResponseTemplate::default()
            .events(vec![reaction.raw().clone().cast_unchecked()]))
        .mount()
        .await;
    server
        .mock_room_relations()
        .match_target_event(event_id!("$second").to_owned())
        .ok(RoomRelationsResponseTemplate::default()
            .events(vec![edit.raw().clone().cast_unchecked()]))
        .mount()
        .await;

    let client = subscribed_client(&server).await;

    // The IDs are given in any order, with a duplicate; the cache loads each
    // once, with its relations, sorted chronologically.
    let (cache, _drop_handles) = client
        .event_cache()
        .specific_events(
            room_id(),
            vec![
                event_id!("$second").to_owned(),
                event_id!("$first").to_owned(),
                event_id!("$first").to_owned(),
            ],
        )
        .await
        .unwrap();

    let (events, _subscriber) = cache.subscribe().await.unwrap();
    assert_eq!(
        event_ids(&events.into()),
        [event_id!("$first"), event_id!("$second"), event_id!("$reaction"), event_id!("$edit")]
    );
    assert_eq!(cache.events().await.unwrap().len(), 4);
}

#[async_test]
async fn test_specific_events_skip_events_that_cannot_be_loaded() {
    let f = EventFactory::new().room(room_id()).sender(*ALICE);

    let server = MatrixMockServer::new().await;
    server
        .mock_room_event()
        .match_event_id()
        .ok(f.text_msg("first").event_id(event_id!("$first")).into_event())
        .mount()
        .await;

    let client = subscribed_client(&server).await;

    // `$missing` isn't mocked: its request fails, the rest is loaded.
    let (cache, _drop_handles) = client
        .event_cache()
        .specific_events(
            room_id(),
            vec![event_id!("$first").to_owned(), event_id!("$missing").to_owned()],
        )
        .await
        .unwrap();
    assert_eq!(event_ids(&cache.events().await.unwrap().into()), [event_id!("$first")]);

    // When nothing can be loaded, that's an error.
    let result = client
        .event_cache()
        .specific_events(room_id(), vec![event_id!("$missing").to_owned()])
        .await;
    assert!(matches!(result, Err(EventCacheError::UnableToLoadSpecificEvents)));
}

#[async_test]
async fn test_specific_events_follow_sync_updates() {
    let f = EventFactory::new().room(room_id()).sender(*ALICE);

    let server = MatrixMockServer::new().await;
    server
        .mock_room_event()
        .match_event_id()
        .ok(f.text_msg("target").event_id(event_id!("$target")).server_ts(1).into_event())
        .mount()
        .await;

    let client = subscribed_client(&server).await;

    let (cache, _drop_handles) = client
        .event_cache()
        .specific_events(room_id(), vec![event_id!("$target").to_owned()])
        .await
        .unwrap();
    let (events, mut subscriber) = cache.subscribe().await.unwrap();
    let mut events: Vector<Event> = events.into();
    assert_eq!(event_ids(&events), [event_id!("$target")]);

    // A reaction and an edit of the target arrive from sync.
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id())
                .add_timeline_event(
                    f.reaction(event_id!("$target"), "👍").event_id(event_id!("$reaction")),
                )
                .add_timeline_event(
                    f.text_msg("* edited")
                        .edit(
                            event_id!("$target"),
                            RoomMessageEventContentWithoutRelation::text_plain("edited"),
                        )
                        .event_id(event_id!("$edit")),
                ),
        )
        .await;
    apply_next_updates(&mut subscriber, &mut events).await;
    assert_eq!(
        event_ids(&events),
        [event_id!("$target"), event_id!("$reaction"), event_id!("$edit")]
    );

    // An unrelated message doesn't reach the cache.
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id())
                .add_timeline_event(f.text_msg("noise").event_id(event_id!("$noise"))),
        )
        .await;
    assert_no_update(&mut subscriber).await;

    // Redacting the reaction replaces it with its redacted form.
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id()).add_timeline_event(
                f.redaction(event_id!("$reaction")).event_id(event_id!("$redaction_of_reaction")),
            ),
        )
        .await;
    apply_next_updates(&mut subscriber, &mut events).await;
    assert!(find(&events, event_id!("$reaction")).raw().deserialize().unwrap().is_redacted());
    assert!(!find(&events, event_id!("$target")).raw().deserialize().unwrap().is_redacted());

    // So does redacting the target itself.
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id()).add_timeline_event(
                f.redaction(event_id!("$target")).event_id(event_id!("$redaction_of_target")),
            ),
        )
        .await;
    apply_next_updates(&mut subscriber, &mut events).await;
    assert!(find(&events, event_id!("$target")).raw().deserialize().unwrap().is_redacted());

    assert!(!event_ids(&events).contains(&event_id!("$noise").to_owned()));
}

#[async_test]
async fn test_specific_events_set_event_ids_reloads_when_the_set_changes() {
    let f = EventFactory::new().room(room_id()).sender(*ALICE);

    let server = MatrixMockServer::new().await;
    server
        .mock_room_event()
        .match_event_id()
        .ok(f.text_msg("first").event_id(event_id!("$first")).server_ts(1).into_event())
        .mount()
        .await;
    server
        .mock_room_event()
        .match_event_id()
        .ok(f.text_msg("second").event_id(event_id!("$second")).server_ts(2).into_event())
        .mount()
        .await;

    let client = subscribed_client(&server).await;

    let (cache, _drop_handles) = client
        .event_cache()
        .specific_events(room_id(), vec![event_id!("$first").to_owned()])
        .await
        .unwrap();
    let (events, mut subscriber) = cache.subscribe().await.unwrap();
    let mut events: Vector<Event> = events.into();
    assert_eq!(event_ids(&events), [event_id!("$first")]);

    // The same set, in any order, is a no-op.
    cache.set_event_ids(vec![event_id!("$first").to_owned()]).await.unwrap();
    assert_no_update(&mut subscriber).await;

    // Growing the set loads the new event.
    cache
        .set_event_ids(vec![event_id!("$second").to_owned(), event_id!("$first").to_owned()])
        .await
        .unwrap();
    apply_next_updates(&mut subscriber, &mut events).await;
    assert_eq!(event_ids(&events), [event_id!("$first"), event_id!("$second")]);

    // Shrinking it drops the removed event.
    cache.set_event_ids(vec![event_id!("$second").to_owned()]).await.unwrap();
    apply_next_updates(&mut subscriber, &mut events).await;
    assert_eq!(event_ids(&events), [event_id!("$second")]);

    // A set that can't be loaded at all is an error, and the previous set is kept.
    let result = cache.set_event_ids(vec![event_id!("$missing").to_owned()]).await;
    assert!(matches!(result, Err(EventCacheError::UnableToLoadSpecificEvents)));
    assert_no_update(&mut subscriber).await;
    assert_eq!(event_ids(&cache.events().await.unwrap().into()), [event_id!("$second")]);
    cache.set_event_ids(vec![event_id!("$second").to_owned()]).await.unwrap();
    assert_no_update(&mut subscriber).await;
}

#[async_test]
async fn test_specific_events_require_the_event_cache_to_be_subscribed() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    server.sync_joined_room(&client, room_id()).await;

    let result = client.event_cache().specific_events(room_id(), vec![]).await;
    assert!(matches!(result, Err(EventCacheError::NotSubscribedYet)));
}
