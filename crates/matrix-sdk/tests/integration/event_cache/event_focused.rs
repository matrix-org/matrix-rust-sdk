use std::time::Duration;

use eyeball_im::VectorDiff;
use imbl::Vector;
use matrix_sdk::{
    event_cache::{EventFocusThreadMode, EventFocusedCache},
    test_utils::mocks::{MatrixMockServer, RoomContextResponseTemplate},
    timeout::timeout,
};
use matrix_sdk_base::event_cache::Event;
use matrix_sdk_test::{async_test, event_factory::EventFactory};
use ruma::{event_id, room_id, user_id};

#[async_test]
async fn test_ignored_user_removes_event_from_event_focused() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    client.event_cache().subscribe().unwrap();

    let room_id = room_id!("!a98sd12bjh:example.org");
    let target_event = event_id!("$1");
    let dexter = user_id!("@dexter:lab.org");

    let f = EventFactory::new().room(room_id).sender(dexter);

    server
        .mock_room_event_context()
        .room(room_id)
        .ok(RoomContextResponseTemplate::new(
            f.text_msg("focused by dexter").event_id(target_event).into_event(),
        )
        .start("prev1")
        .end("next1"))
        .mock_once()
        .mount()
        .await;

    server.mock_room_state_encryption().plain().mount().await;

    let _room = server.sync_joined_room(&client, room_id).await;

    // Given an event-focused cache containing dexter's event,
    let (event_focused, _drop_handles): (EventFocusedCache, _) = client
        .event_cache()
        .event_focused(room_id, target_event, EventFocusThreadMode::Automatic, 0)
        .await
        .unwrap();

    let (events, mut subscriber) = event_focused.subscribe().await.unwrap();
    let mut events: Vector<Event> = events.into();

    assert_eq!(events.len(), 1);
    assert_eq!(events[0].event_id(), Some(target_event));

    // When `dexter` is ignored,
    server
        .mock_sync()
        .ok_and_run(&client, |sync_builder| {
            sync_builder.add_global_account_data(f.ignored_user_list([dexter.to_owned()]));
        })
        .await;

    // Then his event is removed from the event-focused cache.
    {
        let mut removed = false;
        while let Ok(Ok(up)) = timeout(subscriber.recv(), Duration::from_millis(300)).await {
            for diff in &up.diffs {
                if matches!(diff, VectorDiff::Remove { .. }) {
                    removed = true;
                }
            }
            for diff in up.diffs {
                diff.apply(&mut events);
            }
            if removed && events.is_empty() {
                break;
            }
        }

        assert!(removed, "an event should have been removed");
        assert!(events.is_empty(), "event-focused cache should be empty after ignoring dexter");
    }
}
