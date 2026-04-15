use assert_matches2::assert_matches;
use matrix_sdk::{
    EditValidityError, check_validity_of_replacement_events,
    deserialized_responses::{AlgorithmInfo, EncryptionInfo, VerificationState},
};
use matrix_sdk_test::event_factory::EventFactory;
use ruma::{
    event_id,
    events::{AnySyncTimelineEvent, room::message::RoomMessageEventContentWithoutRelation},
    serde::Raw,
    user_id,
};
use serde_json::json;

fn original_event() -> Raw<AnySyncTimelineEvent> {
    let event_factory = EventFactory::new();
    event_factory
        .sender(user_id!("@example:localhost"))
        .text_msg("Hello world")
        .event_id(event_id!("$asbc"))
        .into()
}

#[test]
fn test_edit_validity_invalid_sender() {
    let event_factory = EventFactory::new();
    let event = original_event();

    let replacement = event_factory
        .sender(user_id!("@not_example:localhost"))
        .text_msg("* edit")
        .edit(event_id!("$asbc"), RoomMessageEventContentWithoutRelation::text_plain("edit"))
        .into();

    let result = check_validity_of_replacement_events(&event, None, &replacement, None);

    assert_matches!(result, Err(EditValidityError::InvalidSender));
}

#[test]
fn test_edit_validity_replacement_is_state_event() {
    let event = original_event();

    let replacement = Raw::new(&json!({
        "content": {
            "body":"* edit",
            "msgtype": "m.text",
            "m.relates_to": {
                "rel_type": "m.replace",
                "event_id": "$asbc"
            }
        },
        "event_id":"$xG2xPOiRLXoEjG4Cgq:dummy.org",
        "origin_server_ts":0,
        "sender":"@example:localhost",
        "state_key":"@example:localhost",
        "type":"m.room.message"
    }))
    .unwrap()
    .cast_unchecked();

    let result = check_validity_of_replacement_events(&event, None, &replacement, None);

    assert_matches!(result, Err(EditValidityError::StateKeyPresent));
}

#[test]
fn test_edit_validity_original_is_edit_as_well() {
    let event_factory = EventFactory::new().sender(user_id!("@example:localhost"));

    let event = event_factory
        .text_msg("Hello world")
        .edit(
            event_id!("$some_event_id"),
            RoomMessageEventContentWithoutRelation::text_plain("* another edit"),
        )
        .event_id(event_id!("$asbc"))
        .into();

    let replacement = event_factory
        .text_msg("* edit")
        .edit(event_id!("$asbc"), RoomMessageEventContentWithoutRelation::text_plain("edit"))
        .into();

    let result = check_validity_of_replacement_events(&event, None, &replacement, None);

    assert_matches!(result, Err(EditValidityError::OriginalEventIsReplacement));
}

#[test]
fn test_edit_validity_mismatched_content() {
    let event_factory = EventFactory::new();
    let event = original_event();

    let replacement = event_factory
        .sender(user_id!("@example:localhost"))
        .poll_edit(event_id!("$asbc"), "Foo", vec!["bar"])
        .into();

    let result = check_validity_of_replacement_events(&event, None, &replacement, None);

    assert_matches!(result, Err(EditValidityError::MismatchContentType { .. }));
}

#[test]
fn test_edit_validity_replacement_not_replacement() {
    let event_factory = EventFactory::new();
    let event = original_event();

    let replacement =
        event_factory.sender(user_id!("@example:localhost")).text_msg("* edit").into();

    let result = check_validity_of_replacement_events(&event, None, &replacement, None);

    assert_matches!(result, Err(EditValidityError::NotReplacement));
}

#[test]
fn test_edit_validity_replacement_is_not_encrypted() {
    let user_id = user_id!("@example:localhost");
    let event_factory = EventFactory::new();
    let event = original_event();

    let original_encryption_info = Some(EncryptionInfo {
        sender: user_id.into(),
        sender_device: Some("DEVICEID".into()),
        forwarder: None,
        algorithm_info: AlgorithmInfo::MegolmV1AesSha2 {
            curve25519_key: "1337".to_owned(),
            sender_claimed_keys: Default::default(),
            session_id: Some("mysessionid9".to_owned()),
        },
        verification_state: VerificationState::Verified,
    });

    let replacement = event_factory
        .sender(user_id)
        .text_msg("* edit")
        .edit(event_id!("$asbc"), RoomMessageEventContentWithoutRelation::text_plain("edit"))
        .into();

    let result = check_validity_of_replacement_events(
        &event,
        original_encryption_info.as_ref(),
        &replacement,
        None,
    );

    assert_matches!(result, Err(EditValidityError::ReplacementNotEncrypted));
}

#[test]
fn test_edit_validity_replacement_is_missing_new_content() {
    let user_id = user_id!("@example:localhost");
    let event = original_event();

    let original_encryption_info = Some(EncryptionInfo {
        sender: user_id.into(),
        sender_device: Some("DEVICEID".into()),
        forwarder: None,
        algorithm_info: AlgorithmInfo::MegolmV1AesSha2 {
            curve25519_key: "1337".to_owned(),
            sender_claimed_keys: Default::default(),
            session_id: Some("mysessionid9".to_owned()),
        },
        verification_state: VerificationState::Verified,
    });

    let replacement = Raw::new(&json!({
        "content": {
            "body":"* edit",
            "msgtype": "m.text",
            "m.relates_to": {
                "rel_type": "m.replace",
                "event_id": "$asbc"
            }
        },
        "event_id":"$xG2xPOiRLXoEjG4Cgq:dummy.org",
        "origin_server_ts":0,
        "sender":"@example:localhost",
        "type":"m.room.message"
    }))
    .unwrap()
    .cast_unchecked();

    let result = check_validity_of_replacement_events(
        &event,
        original_encryption_info.as_ref(),
        &replacement,
        original_encryption_info.as_ref(),
    );

    assert_matches!(result, Err(EditValidityError::MissingNewContent));
}

#[test]
fn test_edit_validity_valid_edit() {
    let event_factory = EventFactory::new();
    let event = original_event();

    let replacement = event_factory
        .sender(user_id!("@example:localhost"))
        .text_msg("* edit")
        .edit(event_id!("$asbc"), RoomMessageEventContentWithoutRelation::text_plain("edit"))
        .into();

    let result = check_validity_of_replacement_events(&event, None, &replacement, None);

    assert_matches!(result, Ok(_));
}
