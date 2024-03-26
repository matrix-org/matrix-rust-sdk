use std::time::Duration;

use anyhow::Result;
use matrix_sdk::{
    ruma::{
        api::client::{
            room::create_room::v3::Request as CreateRoomRequest, state::send_state_event,
        },
        assign,
        events::{room::join_rules::RoomJoinRulesEventContent, StateEventType},
        serde::Raw,
    },
    RoomState,
};
use serde_json::{json, value::to_raw_value};
use tokio::{spawn, time::sleep};

use crate::helpers::TestClientBuilder;

/// This makes sure the sync worker does not panic. However this does not check
/// if the raw event values are recovered when the roominfo is loaded from the
/// database
#[tokio::test]
async fn test_send_bad_join_rules() -> Result<()> {
    let alice = TestClientBuilder::new("alice".to_owned())
        .randomize_username()
        .use_sqlite()
        .build()
        .await?;
    let bob =
        TestClientBuilder::new("bob".to_owned()).randomize_username().use_sqlite().build().await?;

    // The sync tasks will panic when there is a problem. At the end of this test,
    // we will check if they are still running
    let a = alice.clone();
    let alice_handle = spawn(async move {
        if let Err(err) = a.sync(Default::default()).await {
            panic!("alice sync errored: {err}");
        }
    });

    let b = bob.clone();
    let bob_handle = spawn(async move {
        if let Err(err) = b.sync(Default::default()).await {
            panic!("bob sync errored: {err}");
        }
    });

    // Alice creates a room.
    let alice_room = alice
        .create_room(assign!(CreateRoomRequest::new(), {
            invite: vec![],
            is_direct: false,
        }))
        .await?;

    let alice_room = alice.get_room(alice_room.room_id()).unwrap();
    assert_eq!(alice_room.state(), RoomState::Joined);

    // This join rule cannot be serialized:
    let content = RoomJoinRulesEventContent::new(
        serde_json::from_value(json!({ "join_rule": "test!"})).unwrap(),
    );
    assert_eq!(
        to_raw_value(&content).unwrap_err().to_string(),
        "the enum variant JoinRule::_Custom cannot be serialized"
    );

    // This will lead to a sync response for alice with a stateevent that fails to
    // serialize.
    let request = send_state_event::v3::Request::new_raw(
        alice_room.room_id().to_owned(),
        StateEventType::RoomJoinRules,
        "".to_owned(),
        Raw::from_json(to_raw_value(&json!({ "join_rule": "test!"})).unwrap()),
    );
    let response = alice.send(request, None).await?;
    dbg!(response);

    // This will lead to a sync response for bob with a strippedstateevent that
    // fails to serialize
    alice_room.invite_user_by_id(bob.user_id().unwrap()).await?;

    sleep(Duration::from_secs(1)).await;

    // The sync handlers should not have crashed
    assert!(!alice_handle.is_finished());
    assert!(!bob_handle.is_finished());

    alice_handle.abort();
    bob_handle.abort();

    Ok(())
}
