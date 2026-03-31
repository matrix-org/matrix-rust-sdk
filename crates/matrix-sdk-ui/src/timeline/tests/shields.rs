use std::{collections::BTreeMap, sync::Arc, time::Duration};

use assert_matches::assert_matches;
use eyeball_im::VectorDiff;
use matrix_sdk::deserialized_responses::{
    AlgorithmInfo, DecryptedRoomEvent, EncryptionInfo, VerificationState,
};
use matrix_sdk_common::deserialized_responses::TimelineEvent;
use matrix_sdk_test::{ALICE, DEFAULT_TEST_ROOM_ID, async_test, event_factory::EventFactory};
use ruma::{
    event_id,
    events::{
        AnyMessageLikeEventContent,
        room::{
            encrypted::{
                EncryptedEventScheme, MegolmV1AesSha2ContentInit, RoomEncryptedEventContent,
            },
            message::RoomMessageEventContent,
        },
    },
};
use stream_assert::{assert_next_matches, assert_pending};

use crate::timeline::{
    EventSendState, TimelineEventShieldState, TimelineEventShieldStateCode,
    tests::{TestTimeline, TestTimelineBuilder},
};

#[async_test]
async fn test_no_shield_in_unencrypted_room() {
    let timeline = TestTimeline::new();
    let mut stream = timeline.subscribe().await;
    let f = &timeline.factory;

    timeline.handle_live_event(f.text_msg("Unencrypted message.").sender(&ALICE)).await;

    let item = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);
    let shield = item.as_event().unwrap().get_shield(false);
    assert_eq!(shield, TimelineEventShieldState::None);
}

#[async_test]
async fn test_sent_in_clear_shield() {
    let timeline = TestTimelineBuilder::new().room_encrypted(true).build();
    let mut stream = timeline.subscribe().await;

    let f = &timeline.factory;
    timeline.handle_live_event(f.text_msg("Unencrypted message.").sender(&ALICE)).await;

    let item = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);
    let shield = item.as_event().unwrap().get_shield(false);
    assert_eq!(
        shield,
        TimelineEventShieldState::Red { code: TimelineEventShieldStateCode::SentInClear }
    );
}

#[async_test]
/// Note: This is a regression test for a bug where the SentInClear shield was
/// incorrectly shown on a local echo whilst sending and was then removed once
/// the local echo came back. There doesn't appear to be an obvious way to mock
/// trust in the timeline, so this test uses a message sent in the clear, making
/// sure the shield only appears once the remote echo is received.
async fn test_local_sent_in_clear_shield() {
    // Given an encrypted timeline.
    let timeline = TestTimelineBuilder::new().room_encrypted(true).build();
    let mut stream = timeline.subscribe().await;

    // When sending an unencrypted event.
    let event_id = event_id!("$W6mZSLWMmfuQQ9jhZWeTxFIM");
    let txn_id = timeline
        .handle_local_event(AnyMessageLikeEventContent::RoomMessage(
            RoomMessageEventContent::text_plain("Local message"),
        ))
        .await;
    let item = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);
    let event_item = item.as_event().unwrap();

    // Then it's local echo should not have a shield (as encryption info isn't
    // available).
    assert!(event_item.is_local_echo());
    let shield = event_item.get_shield(false);
    assert_eq!(shield, TimelineEventShieldState::None);

    {
        // The date divider comes in late.
        let date_divider = assert_next_matches!(stream, VectorDiff::PushFront { value } => value);
        assert!(date_divider.is_date_divider());
    }

    // When the event is sent (but without a remote echo).
    timeline
        .controller
        .update_event_send_state(&txn_id, EventSendState::Sent { event_id: event_id.to_owned() })
        .await;
    let item = assert_next_matches!(stream, VectorDiff::Set { value, index: 1 } => value);
    let event_item = item.as_event().unwrap();
    let timestamp = event_item.timestamp();
    assert_matches!(event_item.send_state(), Some(EventSendState::Sent { .. }));

    // Then the local echo still should not have a shield.
    assert!(event_item.is_local_echo());
    let shield = event_item.get_shield(false);
    assert_eq!(shield, TimelineEventShieldState::None);

    // When the remote echo comes in.
    timeline
        .handle_live_event(
            EventFactory::new()
                .text_msg("Local message")
                .sender(*ALICE)
                .event_id(event_id)
                .server_ts(timestamp)
                .into_event(),
        )
        .await;
    assert_next_matches!(stream, VectorDiff::Remove { index: 1 });
    let item = assert_next_matches!(stream, VectorDiff::PushFront { value } => value);
    let event_item = item.as_event().unwrap();

    // Then the remote echo should now be showing the shield.
    assert!(!event_item.is_local_echo());
    let shield = event_item.get_shield(false);
    assert_eq!(
        shield,
        TimelineEventShieldState::Red { code: TimelineEventShieldStateCode::SentInClear }
    );

    // Date divider is adjusted.
    let date_divider = assert_next_matches!(stream, VectorDiff::PushFront { value } => value);
    assert!(date_divider.is_date_divider());
    assert_next_matches!(stream, VectorDiff::Remove { index: 2 });

    assert_pending!(stream);
}

#[async_test]
/// A `beacon_info` state event cannot be encrypted (state events are always
/// sent in the clear), so a live-location timeline item with no aggregated
/// beacons must not show a "sent in clear" shield in an encrypted room.
///
/// Once a beacon location update arrives without encryption info (i.e. it was
/// sent in clear), the shield must switch to `SentInClear`.
async fn test_live_location_no_sent_in_clear_shield() {
    let timeline = TestTimelineBuilder::new().room_encrypted(true).build();
    let mut stream = timeline.subscribe_events().await;
    let beacon_id = event_id!("$beacon_info:example.org");

    let event = timeline
        .factory
        .beacon_info(None, Duration::from_secs(3600), true, None)
        .sender(&ALICE)
        .state_key(&**ALICE)
        .event_id(beacon_id);
    timeline.handle_live_event(event).await;

    // No beacons yet → shield should be None.
    let item = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);
    assert!(item.content().is_live_location(), "timeline item should be a live location");

    let shield = item.get_shield(false);
    assert_eq!(shield, TimelineEventShieldState::None);

    let shield_strict = item.get_shield(true);
    assert_eq!(shield_strict, TimelineEventShieldState::None);

    // Send an *encrypted* beacon location update.
    let ts_enc = ruma::MilliSecondsSinceUnixEpoch(ruma::uint!(500_000));
    let encrypted_beacon_raw = timeline
        .factory
        .beacon(beacon_id.to_owned(), 48.8566, 2.3522, 20, Some(ts_enc))
        .sender(&ALICE)
        .room(&DEFAULT_TEST_ROOM_ID)
        .into_raw_timeline();

    let encryption_info = Arc::new(EncryptionInfo {
        sender: (*ALICE).into(),
        sender_device: None,
        forwarder: None,
        algorithm_info: AlgorithmInfo::MegolmV1AesSha2 {
            curve25519_key: "fake_key".to_owned(),
            sender_claimed_keys: BTreeMap::new(),
            session_id: Some("fake_session".to_owned()),
        },
        verification_state: VerificationState::Verified,
    });

    let encrypted_beacon_event = TimelineEvent::from_decrypted(
        DecryptedRoomEvent {
            event: encrypted_beacon_raw,
            encryption_info,
            unsigned_encryption_info: None,
        },
        None,
    );
    timeline.handle_live_event(encrypted_beacon_event).await;

    // An encrypted, verified beacon → shield should be None.
    let item = assert_next_matches!(stream, VectorDiff::Set { index: 0, value } => value);
    let shield = item.get_shield(false);
    assert_eq!(shield, TimelineEventShieldState::None);

    let shield_strict = item.get_shield(true);
    assert_eq!(shield_strict, TimelineEventShieldState::None);

    // Send a beacon location update (plain-text, no encryption info).
    let ts = ruma::MilliSecondsSinceUnixEpoch(ruma::uint!(1_000_000));
    let beacon_event =
        timeline.factory.beacon(beacon_id.to_owned(), 51.5008, 0.1247, 35, Some(ts)).sender(&ALICE);
    timeline.handle_live_event(beacon_event).await;

    // A beacon arrived without encryption → shield should be SentInClear.
    let item = assert_next_matches!(stream, VectorDiff::Set { index: 0, value } => value);
    let shield = item.get_shield(false);
    assert_eq!(
        shield,
        TimelineEventShieldState::Red { code: TimelineEventShieldStateCode::SentInClear }
    );
}

#[async_test]
/// Test a bug that was causing unable to decrypt messages to have a `message
/// sent in clear` red warning.
async fn test_utd_shield() {
    // Given we are in an encrypted room
    let timeline = TestTimelineBuilder::new().room_encrypted(true).build();
    let mut stream = timeline.subscribe().await;

    let f = &timeline.factory;

    // When we receive a message that we can't decrypt
    timeline
        .handle_live_event(
            f.event(RoomEncryptedEventContent::new(
                EncryptedEventScheme::MegolmV1AesSha2(
                    MegolmV1AesSha2ContentInit {
                        ciphertext: "\
                            AwgAEpABNOd7Rxpc/98gaaOanApQ/h40uNyYE/aiFd8PKeQPH65bwuxBy/glodmteryH\
                            4t5d0cKSPjb+996yK90+A8YUevQKBuC+/+4iRF2CSqMNvArdOCnFHJdZBuCyRP6W82DZ\
                            sR1w5X/tKGs/A9egJdxomLCzMRZarayTXUlgMT8Kj7E9zKOgyLEZGki6Y9IPybfrU3+S\
                            b4VbF7RKY395/lIZFiLvJ5hUT+Ao1k13opeTE9GHtdOK0GzQPVFLnN61pRa3K/vV9Otk\
                            D0QbVS/4mE3C29+yIC1lEkwA"
                            .to_owned(),
                        sender_key: "peI8cfSKqZvTOAfY0Od2e7doDpJ1cxdBsOhSceTLU3E".to_owned(),
                        device_id: "KDCTEHOVSS".into(),
                        session_id: "C25PoE+4MlNidQD0YU5ibZqHawV0zZ/up7R8vYJBYTY".into(),
                    }
                    .into(),
                ),
                None,
            ))
            .sender(&ALICE)
            .into_utd_sync_timeline_event(),
        )
        .await;

    // Then the message is displayed with no shield
    let item = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);
    let shield = item.as_event().unwrap().get_shield(false);
    assert_eq!(shield, TimelineEventShieldState::None);
}
