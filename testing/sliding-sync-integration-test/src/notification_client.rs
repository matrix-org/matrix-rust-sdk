use std::{
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{ensure, Result};
use assert_matches::assert_matches;
use matrix_sdk::{
    config::SyncSettings,
    ruma::{
        api::client::room::create_room::v3::Request as CreateRoomRequest,
        assign,
        events::{
            room::{member::MembershipState, message::RoomMessageEventContent},
            AnyStrippedStateEvent, SyncMessageLikeEvent, TimelineEventType,
        },
        OwnedEventId,
    },
    RoomState,
};
use matrix_sdk_integration_testing::helpers::get_client_for_user;
use matrix_sdk_ui::{
    notification_client::{
        Error, NotificationClient, NotificationEvent, NotificationItem, NotificationProcessSetup,
        NotificationStatus,
    },
    sync_service::SyncService,
};
use tracing::warn;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_notification() -> Result<()> {
    // Create new users for each test run, to avoid conflicts with invites existing
    // from previous runs.
    let time = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis();
    let alice = get_client_for_user(format!("alice{time}"), true).await?;
    let bob = get_client_for_user(format!("bob{time}"), true).await?;

    let dummy_sync_service = Arc::new(SyncService::builder(bob.clone()).build().await?);
    let process_setup =
        NotificationProcessSetup::SingleProcess { sync_service: dummy_sync_service };

    // Alice changes display name.
    const ALICE_NAME: &str = "Alice, Queen of Cryptography";
    alice.account().set_display_name(Some(ALICE_NAME)).await?;

    // Initial setup: Alice creates a room, invites Bob.
    let invite = vec![bob.user_id().expect("bob has a userid!").to_owned()];
    let request = assign!(CreateRoomRequest::new(), {
        invite,
        is_direct: true,
    });

    let alice_room = alice.create_room(request).await?;

    const ROOM_NAME: &str = "Kingdom of Integration Testing";
    alice_room.set_name(ROOM_NAME.to_owned()).await?;

    let room_id = alice_room.room_id().to_owned();

    // Bob receives a notification about it.

    let bob_invite_response = bob.sync_once(Default::default()).await?;
    let sync_token = bob_invite_response.next_batch;

    let mut invited_rooms = bob_invite_response.rooms.invite.into_iter();

    let (_, invited_room) = invited_rooms.next().expect("must be invited to one room");
    assert!(invited_rooms.next().is_none(), "no more invited rooms: {invited_rooms:#?}");

    if let Some(event_id) = invited_room.invite_state.events.iter().find_map(|event| {
        let Ok(AnyStrippedStateEvent::RoomMember(room_member_ev)) = event.deserialize() else {
            return None;
        };

        if room_member_ev.content.membership != MembershipState::Invite {
            return None;
        }

        let Ok(Some(event_id)) = event.get_field::<OwnedEventId>("event_id") else {
            return None;
        };

        Some(event_id)
    }) {
        warn!("We found the invite event!");

        // Try with sliding sync first.
        let notification_client =
            NotificationClient::builder(bob.clone(), process_setup.clone()).await.unwrap().build();
        let notification = assert_matches!(
            notification_client.get_notification_with_sliding_sync(&room_id, &event_id).await?,
            NotificationStatus::Event(event) => event
        );

        warn!("sliding_sync: checking invite notification");

        assert_eq!(notification.event.sender(), alice.user_id().unwrap());
        assert_eq!(notification.joined_members_count, 1);
        assert_eq!(notification.is_room_encrypted, None);
        assert!(notification.is_direct_message_room);

        assert_matches!(notification.event, NotificationEvent::Invite(observed_invite) => {
            assert_eq!(observed_invite.content.membership, MembershipState::Invite);
        });

        assert_eq!(notification.sender_display_name.as_deref(), Some(ALICE_NAME));

        // In theory, the room name ought to be ROOM_NAME here, but the sliding sync
        // proxy returns the other person's name as the room's name (as of
        // 2023-08-04).
        assert!(notification.room_display_name != ROOM_NAME);
        assert_eq!(notification.room_display_name, ALICE_NAME);

        // Then with /context.
        let notification_client =
            NotificationClient::builder(bob.clone(), process_setup.clone()).await.unwrap().build();
        let notification =
            notification_client.get_notification_with_context(&room_id, &event_id).await;
        // We aren't authorized to inspect events from rooms we were not invited to.
        assert!(matches!(notification.unwrap_err(), Error::SdkError(matrix_sdk::Error::Http(..))));
    } else {
        warn!("Couldn't get the invite event.");
    }

    // Bob accepts the invite, joins the room.
    {
        let room = bob.get_room(&room_id).expect("bob doesn't know about the room");
        ensure!(
            room.state() == RoomState::Invited,
            "The room alice invited bob in isn't an invite: {room:?}"
        );
        let details = room.invite_details().await?;
        let sender = details.inviter.expect("invite details doesn't have inviter");
        assert_eq!(sender.user_id(), alice.user_id().expect("alice has a user_id"));
    }

    // Bob joins the room.
    bob.get_room(alice_room.room_id()).unwrap().join().await?;

    // Now Alice sends a message to Bob.
    alice_room.send(RoomMessageEventContent::text_plain("Hello world!"), None).await?;

    // In this sync, bob receives the message from Alice.
    let bob_response = bob.sync_once(SyncSettings::default().token(sync_token)).await?;

    let mut joined_rooms = bob_response.rooms.join.into_iter();
    let (_, bob_room) = joined_rooms.next().expect("must have joined one room");
    assert!(joined_rooms.next().is_none(), "no more joined rooms: {joined_rooms:#?}");

    let event_id = bob_room
        .timeline
        .events
        .iter()
        .find_map(|event| {
            let event = event.event.deserialize().ok()?;
            if event.event_type() == TimelineEventType::RoomMessage {
                Some(event.event_id().to_owned())
            } else {
                None
            }
        })
        .expect("missing message from alice in bob's client");

    // Get the notification for the given message.
    let check_notification = |is_sliding_sync: bool, notification: NotificationItem| {
        warn!(
            "{}: checking message notification",
            if is_sliding_sync { "sliding sync" } else { "/context query" }
        );

        assert_eq!(notification.event.sender(), alice.user_id().unwrap());

        if is_sliding_sync {
            assert_eq!(notification.joined_members_count, 2);
        } else {
            // This can't be computed for /context, because we only get a single request,
            // and not a full sync response that would contain a room summary.
            warn!("joined member counts: {}", notification.joined_members_count);
        }

        assert_eq!(notification.is_room_encrypted, Some(false));
        assert!(notification.is_direct_message_room);

        assert_matches!(
            notification.event,
            NotificationEvent::Timeline(
                matrix_sdk::ruma::events::AnySyncTimelineEvent::MessageLike(
                    matrix_sdk::ruma::events::AnySyncMessageLikeEvent::RoomMessage(
                        SyncMessageLikeEvent::Original(event)
                    )
                )
            ) => {
                assert_matches!(event.content.msgtype,
                    matrix_sdk::ruma::events::room::message::MessageType::Text(text) => {
                        assert_eq!(text.body, "Hello world!");
                    });
                }
        );

        assert_eq!(notification.sender_display_name.as_deref(), Some(ALICE_NAME));
        assert_eq!(notification.room_display_name, ROOM_NAME);
    };

    let notification_client =
        NotificationClient::builder(bob.clone(), process_setup.clone()).await.unwrap().build();
    let notification = assert_matches!(
        notification_client.get_notification_with_sliding_sync(&room_id, &event_id).await?,
        NotificationStatus::Event(item) => item
    );
    check_notification(true, notification);

    let notification_client =
        NotificationClient::builder(bob.clone(), process_setup).await.unwrap().build();
    let notification = notification_client
        .get_notification_with_context(&room_id, &event_id)
        .await?
        .expect("missing notification for the message");
    check_notification(false, notification);

    Ok(())
}
