use futures_util::{FutureExt, StreamExt};
use matrix_sdk::{
    assert_decrypted_message_eq, assert_next_matches_with_timeout,
    deserialized_responses::{TimelineEvent, UnableToDecryptInfo, UnableToDecryptReason},
    encryption::EncryptionSettings,
    test_utils::mocks::MatrixMockServer,
};
use matrix_sdk_test::{
    InvitedRoomBuilder, JoinedRoomBuilder, StateTestEvent, async_test, event_factory::EventFactory,
};
use ruma::{
    device_id, event_id, events::room::message::RoomMessageEventContent, mxc_uri, room_id, user_id,
};

#[async_test]
async fn test_shared_history_out_of_order() {
    let room_id = room_id!("!test:localhost");
    let mxid = mxc_uri!("mxc://localhost/12345");

    let alice_user_id = user_id!("@alice:localhost");
    let alice_device_id = device_id!("ALICEDEVICE");
    let bob_user_id = user_id!("@bob:localhost");
    let bob_device_id = device_id!("BOBDEVICE");

    let matrix_mock_server = MatrixMockServer::new().await;
    matrix_mock_server.mock_crypto_endpoints_preset().await;
    matrix_mock_server.mock_invite_user_by_id().ok().mock_once().mount().await;

    let encryption_settings =
        EncryptionSettings { auto_enable_cross_signing: true, ..Default::default() };

    let alice = matrix_mock_server
        .client_builder_for_crypto_end_to_end(alice_user_id, alice_device_id)
        .on_builder(|builder| {
            builder
                .with_enable_share_history_on_invite(true)
                .with_encryption_settings(encryption_settings)
        })
        .build()
        .await;

    let bob = matrix_mock_server
        .client_builder_for_crypto_end_to_end(bob_user_id, bob_device_id)
        .on_builder(|builder| {
            builder
                .with_enable_share_history_on_invite(true)
                .with_encryption_settings(encryption_settings)
        })
        .build()
        .await;

    matrix_mock_server.exchange_e2ee_identities(&alice, &bob).await;

    let event_factory = EventFactory::new().room(room_id);
    let alice_member_event = event_factory.member(alice_user_id).into_raw();

    matrix_mock_server
        .mock_sync()
        .ok_and_run(&alice, |builder| {
            builder.add_joined_room(
                JoinedRoomBuilder::new(room_id)
                    .add_state_event(StateTestEvent::Create)
                    .add_state_event(StateTestEvent::Encryption),
            );
        })
        .await;

    let room =
        alice.get_room(room_id).expect("Alice should have access to the room now that we synced");

    let event_id = event_id!("$some_id");
    let (event_receiver, mock) =
        matrix_mock_server.mock_room_send().ok_with_capture(event_id, alice_user_id);

    mock.mock_once().named("send").mount().await;

    matrix_mock_server
        .mock_get_members()
        .ok(vec![alice_member_event.clone()])
        .mock_once()
        .mount()
        .await;

    let event_id = room
        .send(RoomMessageEventContent::text_plain("It's a secret to everybody"))
        .await
        .expect("We should be able to send an initial message")
        .response
        .event_id;

    matrix_mock_server
        .mock_authenticated_media_config()
        .ok_default()
        .mock_once()
        .named("media_config")
        .mount()
        .await;

    let (receiver, upload_mock) = matrix_mock_server.mock_upload().ok_with_capture(mxid);
    upload_mock.mock_once().mount().await;

    let (_guard, bundle_info) = matrix_mock_server.mock_capture_put_to_device(alice_user_id).await;

    room.invite_user_by_id(bob_user_id).await.expect("We should be able to invite Bob");
    let bundle = receiver.await.expect("We should have received a bundle now.");

    let mut bundle_stream = bob
        .encryption()
        .historic_room_key_stream()
        .await
        .expect("We should be able to get the bundle stream");

    let bob_member_event = event_factory.member(alice_user_id).invited(bob_user_id);

    matrix_mock_server
        .mock_sync()
        .ok_and_run(&bob, |builder| {
            builder.add_invited_room(
                InvitedRoomBuilder::new(room_id)
                    .add_state_event(alice_member_event.cast())
                    .add_state_event(bob_member_event),
            );
        })
        .await;

    let bob_room = bob.get_room(room_id).expect("Bob should have access to the invited room");

    matrix_mock_server.mock_room_join(room_id).ok().mock_once().named("join").mount().await;
    bob_room.join().await.expect("Bob should be able to join the room");

    let details = bob_room
        .invite_acceptance_details()
        .expect("We should have stored invite acceptance details");

    assert_eq!(
        details.inviter,
        alice.user_id().unwrap(),
        "We should have recorded that Alice has invited us"
    );

    let bundle_info = bundle_info.await;
    matrix_mock_server
        .mock_authed_media_download()
        .expect_any_access_token()
        .ok_bytes(bundle)
        .mock_once()
        .named("media_download")
        .mount()
        .await;

    let mut room_key_stream = bob
        .encryption()
        .room_keys_received_stream()
        .await
        .expect("We should be able to listen to received room keys");

    matrix_mock_server
        .mock_sync()
        .ok_and_run(&bob, |builder| {
            builder.add_to_device_event(
                bundle_info
                    .deserialize_as()
                    .expect("We should be able to deserialize the bundle info"),
            );
        })
        .await;

    let bundle_notification = bundle_stream
        .next()
        .now_or_never()
        .flatten()
        .expect("We should have been notiifed about the received bundle");

    assert_eq!(bundle_notification.sender, alice_user_id);
    assert_eq!(bundle_notification.room_id, room_id);

    assert_next_matches_with_timeout!(room_key_stream, 1000, Ok(room_key_infos) => assert_eq!(room_key_infos.len(), 1));

    let event = event_receiver.await.expect("We should have received Alice's event");

    matrix_mock_server
        .mock_room_event()
        .room(room_id)
        .match_event_id()
        .ok(TimelineEvent::from_utd(
            event,
            UnableToDecryptInfo { session_id: None, reason: UnableToDecryptReason::Unknown },
        ))
        .mock_once()
        .mount()
        .await;

    let event = bob_room
        .event(&event_id, None)
        .await
        .expect("Bob should be able to fetch the event Alice has sent");

    let encryption_info = event.encryption_info().expect("Event did not have encryption info");

    // Check Bob stored information about the key forwarder.
    let forwarder_info = encryption_info.forwarder.as_ref().unwrap();
    assert_eq!(forwarder_info.user_id, alice_user_id);
    assert_eq!(forwarder_info.device_id, alice_device_id);

    assert_decrypted_message_eq!(
        event,
        "It's a secret to everybody",
        "The decrypted event should match the message Alice has sent"
    );
}
