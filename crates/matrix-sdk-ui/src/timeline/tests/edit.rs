// Copyright 2023 The Matrix.org Foundation C.I.C.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::collections::BTreeMap;

use assert_matches2::assert_let;
use eyeball_im::VectorDiff;
use matrix_sdk::deserialized_responses::{
    AlgorithmInfo, EncryptionInfo, VerificationLevel, VerificationState,
};
use matrix_sdk_test::{async_test, ALICE};
use ruma::{
    event_id,
    events::{
        room::message::{MessageType, RedactedRoomMessageEventContent},
        BundledMessageLikeRelations,
    },
};
use stream_assert::{assert_next_matches, assert_pending};

use super::TestTimeline;
use crate::timeline::TimelineItemContent;

#[async_test]
async fn test_live_redacted() {
    let timeline = TestTimeline::new();
    let mut stream = timeline.subscribe().await;

    let f = &timeline.factory;

    timeline
        .handle_live_redacted_message_event(*ALICE, RedactedRoomMessageEventContent::new())
        .await;
    let item = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);

    let redacted_event_id = item.as_event().unwrap().event_id().unwrap();

    timeline
        .handle_live_event(
            f.text_msg(" * test")
                .sender(&ALICE)
                .edit(redacted_event_id, MessageType::text_plain("test").into()),
        )
        .await;

    assert_eq!(timeline.controller.items().await.len(), 2);

    let day_divider = assert_next_matches!(stream, VectorDiff::PushFront { value } => value);
    assert!(day_divider.is_day_divider());
}

#[async_test]
async fn test_live_sanitized() {
    let timeline = TestTimeline::new();
    let mut stream = timeline.subscribe().await;

    let f = &timeline.factory;
    timeline
        .handle_live_event(
            f.text_html("**original** message", "<strong>original</strong> message").sender(&ALICE),
        )
        .await;

    let item = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);
    let first_event = item.as_event().unwrap();
    assert_let!(TimelineItemContent::Message(message) = first_event.content());
    assert_let!(MessageType::Text(text) = message.msgtype());
    assert_eq!(text.body, "**original** message");
    assert_eq!(text.formatted.as_ref().unwrap().body, "<strong>original</strong> message");

    let day_divider = assert_next_matches!(stream, VectorDiff::PushFront { value } => value);
    assert!(day_divider.is_day_divider());

    let first_event_id = first_event.event_id().unwrap();

    let new_plain_content = "!!edited!! **better** message";
    let new_html_content = "<edited/> <strong>better</strong> message";
    timeline
        .handle_live_event(
            f.text_html(format!("* {}", new_plain_content), format!("* {}", new_html_content))
                .sender(&ALICE)
                .edit(
                    first_event_id,
                    MessageType::text_html(new_plain_content, new_html_content).into(),
                ),
        )
        .await;

    let item = assert_next_matches!(stream, VectorDiff::Set { index: 1, value } => value);
    let first_event = item.as_event().unwrap();
    assert_let!(TimelineItemContent::Message(message) = first_event.content());
    assert_let!(MessageType::Text(text) = message.msgtype());
    assert_eq!(text.body, new_plain_content);
    assert_eq!(text.formatted.as_ref().unwrap().body, " <strong>better</strong> message");
}

#[async_test]
async fn test_aggregated_sanitized() {
    let timeline = TestTimeline::new();
    let mut stream = timeline.subscribe().await;

    let original_event_id = event_id!("$original");
    let edit_event_id = event_id!("$edit");

    let f = &timeline.factory;

    let mut relations = BundledMessageLikeRelations::new();
    relations.replace = Some(Box::new(
        f.text_html(
            "* !!edited!! **better** message",
            "* <edited/> <strong>better</strong> message",
        )
        .edit(
            original_event_id,
            MessageType::text_html(
                "!!edited!! **better** message",
                "<edited/> <strong>better</strong> message",
            )
            .into(),
        )
        .event_id(edit_event_id)
        .sender(*ALICE)
        .into_raw_sync(),
    ));

    let ev = f
        .text_html("**original** message", "<strong>original</strong> message")
        .sender(*ALICE)
        .event_id(original_event_id)
        .bundled_relations(relations);

    timeline.handle_live_event(ev).await;

    let item = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);
    let first_event = item.as_event().unwrap();
    assert_let!(TimelineItemContent::Message(message) = first_event.content());
    assert_let!(MessageType::Text(text) = message.msgtype());
    assert_eq!(text.body, "!!edited!! **better** message");
    assert_eq!(text.formatted.as_ref().unwrap().body, " <strong>better</strong> message");

    let day_divider = assert_next_matches!(stream, VectorDiff::PushFront { value } => value);
    assert!(day_divider.is_day_divider());
}

#[async_test]
async fn test_edit_updates_encryption_info() {
    let timeline = TestTimeline::new();
    let event_factory = &timeline.factory;

    let original_event_id = event_id!("$original_event");

    let mut original_event = event_factory
        .text_msg("**original** message")
        .sender(*ALICE)
        .event_id(original_event_id)
        .into_sync();

    let mut encryption_info = EncryptionInfo {
        sender: (*ALICE).into(),
        sender_device: None,
        algorithm_info: AlgorithmInfo::MegolmV1AesSha2 {
            curve25519_key: "123".to_owned(),
            sender_claimed_keys: BTreeMap::new(),
        },
        verification_state: VerificationState::Verified,
    };

    original_event.encryption_info = Some(encryption_info.clone());

    timeline.handle_live_event(original_event).await;

    let items = timeline.controller.items().await;
    let first_event = items[1].as_event().unwrap();

    assert_eq!(
        first_event.encryption_info().unwrap().verification_state,
        VerificationState::Verified
    );

    assert_let!(TimelineItemContent::Message(message) = first_event.content());
    assert_let!(MessageType::Text(text) = message.msgtype());
    assert_eq!(text.body, "**original** message");

    let mut edit_event = event_factory
        .text_msg(" * !!edited!! **better** message")
        .sender(*ALICE)
        .edit(original_event_id, MessageType::text_plain("!!edited!! **better** message").into())
        .into_sync();
    encryption_info.verification_state =
        VerificationState::Unverified(VerificationLevel::UnverifiedIdentity);
    edit_event.encryption_info = Some(encryption_info);

    timeline.handle_live_event(edit_event).await;

    let items = timeline.controller.items().await;
    let first_event = items[1].as_event().unwrap();

    assert_eq!(
        first_event.encryption_info().unwrap().verification_state,
        VerificationState::Unverified(VerificationLevel::UnverifiedIdentity)
    );

    assert_let!(TimelineItemContent::Message(message) = first_event.content());
    assert_let!(MessageType::Text(text) = message.msgtype());
    assert_eq!(text.body, "!!edited!! **better** message");
}

#[async_test]
async fn test_relations_edit_overrides_pending_edit_msg() {
    let timeline = TestTimeline::new();
    let mut stream = timeline.subscribe().await;

    let f = &timeline.factory;

    let original_event_id = event_id!("$original");
    let edit1_event_id = event_id!("$edit1");
    let edit2_event_id = event_id!("$edit2");

    // Pending edit is stashed, nothing comes from the stream.
    timeline
        .handle_live_event(
            f.text_msg("*edit 1")
                .sender(*ALICE)
                .edit(original_event_id, MessageType::text_plain("edit 1").into())
                .event_id(edit1_event_id),
        )
        .await;
    assert_pending!(stream);

    // Now we receive the original event, with a bundled relations group.
    let mut relations = BundledMessageLikeRelations::new();
    relations.replace = Some(Box::new(
        f.text_msg("* edit 2")
            .edit(original_event_id, MessageType::text_plain("edit 2").into())
            .event_id(edit2_event_id)
            .sender(*ALICE)
            .into_raw_sync(),
    ));

    let ev = f
        .text_msg("original")
        .sender(*ALICE)
        .event_id(original_event_id)
        .bundled_relations(relations);

    timeline.handle_live_event(ev).await;

    let item = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);

    // We receive the latest edit, not the pending one.
    let event = item.as_event().unwrap();
    assert_eq!(
        event
            .latest_edit_json()
            .expect("we should have an edit json")
            .deserialize()
            .unwrap()
            .event_id(),
        edit2_event_id
    );

    let text = event.content().as_message().unwrap();
    assert_eq!(text.body(), "edit 2");

    let day_divider = assert_next_matches!(stream, VectorDiff::PushFront { value } => value);
    assert!(day_divider.is_day_divider());

    assert_pending!(stream);
}

#[async_test]
async fn test_relations_edit_overrides_pending_edit_poll() {
    let timeline = TestTimeline::new();
    let mut stream = timeline.subscribe().await;

    let f = &timeline.factory;

    let original_event_id = event_id!("$original");
    let edit1_event_id = event_id!("$edit1");
    let edit2_event_id = event_id!("$edit2");

    // Pending edit is stashed, nothing comes from the stream.
    timeline
        .handle_live_event(
            f.poll_edit(
                original_event_id,
                "Can the fake slim shady please stand up?",
                vec!["Excuse me?"],
            )
            .sender(*ALICE)
            .event_id(edit1_event_id),
        )
        .await;
    assert_pending!(stream);

    // Now we receive the original event, with a bundled relations group.
    let mut relations = BundledMessageLikeRelations::new();
    relations.replace = Some(Box::new(
        f.poll_edit(
            original_event_id,
            "Can the real slim shady please stand up?",
            vec!["Excuse me?", "Please stand up 🎵", "Please stand up 🎶"],
        )
        .sender(*ALICE)
        .event_id(edit2_event_id)
        .into(),
    ));

    let ev = f
        .poll_start(
            "Can the fake slim shady please stand down?\nExcuse me?",
            "Can the fake slim shady please stand down?",
            vec!["Excuse me?"],
        )
        .sender(*ALICE)
        .event_id(original_event_id)
        .bundled_relations(relations);

    timeline.handle_live_event(ev).await;

    let item = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);

    // We receive the latest edit, not the pending one.
    let event = item.as_event().unwrap();
    assert_eq!(
        event
            .latest_edit_json()
            .expect("we should have an edit json")
            .deserialize()
            .unwrap()
            .event_id(),
        edit2_event_id
    );

    let poll = event.content().as_poll().unwrap();
    assert!(poll.has_been_edited);
    assert_eq!(
        poll.start_event_content.poll_start.question.text,
        "Can the real slim shady please stand up?"
    );
    assert_eq!(poll.start_event_content.poll_start.answers.len(), 3);

    let day_divider = assert_next_matches!(stream, VectorDiff::PushFront { value } => value);
    assert!(day_divider.is_day_divider());

    assert_pending!(stream);
}
