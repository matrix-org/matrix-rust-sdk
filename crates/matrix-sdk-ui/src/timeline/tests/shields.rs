use assert_matches::assert_matches;
use eyeball_im::VectorDiff;
use matrix_sdk_test::{ALICE, async_test, event_factory::EventFactory};
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
