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

use std::{collections::BTreeMap, sync::Arc};

use assert_matches::assert_matches;
use assert_matches2::assert_let;
use eyeball_im::VectorDiff;
use matrix_sdk::{
    deserialized_responses::{AlgorithmInfo, EncryptionInfo, VerificationLevel, VerificationState},
    send_queue::AbstractProgress,
};
use matrix_sdk_base::{
    deserialized_responses::{DecryptedRoomEvent, TimelineEvent},
    store::QueueWedgeError,
};
use matrix_sdk_test::{ALICE, BOB, async_test};
use ruma::{
    EventId, event_id,
    events::{
        AnyMessageLikeEventContent,
        relation::Replacement,
        room::message::{
            MessageType, RedactedRoomMessageEventContent, Relation, RoomMessageEventContent,
            RoomMessageEventContentWithoutRelation,
        },
    },
    owned_event_id, room_id,
};
use stream_assert::{assert_next_matches, assert_pending};

use super::TestTimeline;
use crate::timeline::{EventSendState, MediaUploadProgress};

#[async_test]
async fn test_live_redacted() {
    let timeline = TestTimeline::new().await;
    let mut stream = timeline.subscribe().await;

    let f = &timeline.factory;

    timeline.handle_live_event(f.redacted(*ALICE, RedactedRoomMessageEventContent::new())).await;
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

    let date_divider = assert_next_matches!(stream, VectorDiff::PushFront { value } => value);
    assert!(date_divider.is_date_divider());
}

#[async_test]
async fn test_live_sanitized() {
    let timeline = TestTimeline::new().await;
    let mut stream = timeline.subscribe().await;

    let f = &timeline.factory;
    timeline
        .handle_live_event(
            f.text_html("**original** message", "<strong>original</strong> message").sender(&ALICE),
        )
        .await;

    let item = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);
    let first_event = item.as_event().unwrap();
    assert_let!(Some(message) = first_event.content().as_message());
    assert_let!(MessageType::Text(text) = message.msgtype());
    assert_eq!(text.body, "**original** message");
    assert_eq!(text.formatted.as_ref().unwrap().body, "<strong>original</strong> message");

    let date_divider = assert_next_matches!(stream, VectorDiff::PushFront { value } => value);
    assert!(date_divider.is_date_divider());

    let first_event_id = first_event.event_id().unwrap();

    let new_plain_content = "!!edited!! **better** message";
    let new_html_content = "<edited/> <strong>better</strong> message";
    timeline
        .handle_live_event(
            f.text_html(format!("* {new_plain_content}"), format!("* {new_html_content}"))
                .sender(&ALICE)
                .edit(
                    first_event_id,
                    MessageType::text_html(new_plain_content, new_html_content).into(),
                ),
        )
        .await;

    let item = assert_next_matches!(stream, VectorDiff::Set { index: 1, value } => value);
    let first_event = item.as_event().unwrap();
    assert_let!(Some(message) = first_event.content().as_message());
    assert_let!(MessageType::Text(text) = message.msgtype());
    assert_eq!(text.body, new_plain_content);
    assert_eq!(text.formatted.as_ref().unwrap().body, " <strong>better</strong> message");
}

#[async_test]
async fn test_aggregated_sanitized() {
    let timeline = TestTimeline::new().await;
    let mut stream = timeline.subscribe().await;

    let original_event_id = event_id!("$original");
    let edit_event_id = event_id!("$edit");

    let f = &timeline.factory;

    let ev = f
        .text_html("**original** message", "<strong>original</strong> message")
        .sender(*ALICE)
        .event_id(original_event_id)
        .with_bundled_edit(
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
            .sender(*ALICE),
        );

    timeline.handle_live_event(ev).await;

    let item = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);
    let first_event = item.as_event().unwrap();
    assert_let!(Some(message) = first_event.content().as_message());
    assert_let!(MessageType::Text(text) = message.msgtype());
    assert_eq!(text.body, "!!edited!! **better** message");
    assert_eq!(text.formatted.as_ref().unwrap().body, " <strong>better</strong> message");

    let date_divider = assert_next_matches!(stream, VectorDiff::PushFront { value } => value);
    assert!(date_divider.is_date_divider());
}

#[async_test]
async fn test_edit_updates_encryption_info() {
    let timeline = TestTimeline::new().await;
    let event_factory = &timeline.factory;

    let room_id = room_id!("!room:id");
    let original_event_id = event_id!("$original_event");

    let original_event = event_factory
        .text_msg("**original** message")
        .sender(*ALICE)
        .event_id(original_event_id)
        .room(room_id)
        .into_raw();

    let mut encryption_info = Arc::new(EncryptionInfo {
        sender: (*ALICE).into(),
        sender_device: None,
        forwarder: None,
        algorithm_info: AlgorithmInfo::MegolmV1AesSha2 {
            curve25519_key: "123".to_owned(),
            sender_claimed_keys: BTreeMap::new(),
            session_id: Some("mysessionid6333".to_owned()),
        },
        verification_state: VerificationState::Verified,
    });

    let original_event = TimelineEvent::from_decrypted(
        DecryptedRoomEvent {
            event: original_event,
            encryption_info: encryption_info.clone(),
            unsigned_encryption_info: None,
        },
        None,
    );

    timeline.handle_live_event(original_event).await;

    let items = timeline.controller.items().await;
    let first_event = items[1].as_event().unwrap();

    assert_eq!(
        first_event.encryption_info().unwrap().verification_state,
        VerificationState::Verified
    );

    assert_let!(Some(message) = first_event.content().as_message());
    assert_let!(MessageType::Text(text) = message.msgtype());
    assert_eq!(text.body, "**original** message");

    let edit_event = event_factory
        .text_msg(" * !!edited!! **better** message")
        .sender(*ALICE)
        .room(room_id)
        .edit(original_event_id, MessageType::text_plain("!!edited!! **better** message").into())
        .into_raw();
    Arc::make_mut(&mut encryption_info).verification_state =
        VerificationState::Unverified(VerificationLevel::UnverifiedIdentity);
    let edit_event = TimelineEvent::from_decrypted(
        DecryptedRoomEvent {
            event: edit_event,
            encryption_info: encryption_info.clone(),
            unsigned_encryption_info: None,
        },
        None,
    );

    timeline.handle_live_event(edit_event).await;

    let items = timeline.controller.items().await;
    let first_event = items[1].as_event().unwrap();

    assert_eq!(
        first_event.encryption_info().unwrap().verification_state,
        VerificationState::Unverified(VerificationLevel::UnverifiedIdentity)
    );

    assert_let!(Some(message) = first_event.content().as_message());
    assert_let!(MessageType::Text(text) = message.msgtype());
    assert_eq!(text.body, "!!edited!! **better** message");
}

#[async_test]
async fn test_relations_edit_overrides_pending_edit_msg() {
    let timeline = TestTimeline::new().await;
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
    let ev = f.text_msg("original").sender(*ALICE).event_id(original_event_id).with_bundled_edit(
        f.text_msg("* edit 2")
            .edit(original_event_id, MessageType::text_plain("edit 2").into())
            .event_id(edit2_event_id)
            .sender(*ALICE),
    );

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

    let date_divider = assert_next_matches!(stream, VectorDiff::PushFront { value } => value);
    assert!(date_divider.is_date_divider());

    assert_pending!(stream);
}

#[async_test]
async fn test_relations_edit_overrides_pending_edit_poll() {
    let timeline = TestTimeline::new().await;
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
    let ev = f
        .poll_start(
            "Can the fake slim shady please stand down?\nExcuse me?",
            "Can the fake slim shady please stand down?",
            vec!["Excuse me?"],
        )
        .sender(*ALICE)
        .event_id(original_event_id)
        .with_bundled_edit(
            f.poll_edit(
                original_event_id,
                "Can the real slim shady please stand up?",
                vec!["Excuse me?", "Please stand up 🎵", "Please stand up 🎶"],
            )
            .sender(*ALICE)
            .event_id(edit2_event_id),
        );

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
    assert_eq!(poll.poll_start.question.text, "Can the real slim shady please stand up?");
    assert_eq!(poll.poll_start.answers.len(), 3);

    let date_divider = assert_next_matches!(stream, VectorDiff::PushFront { value } => value);
    assert!(date_divider.is_date_divider());

    assert_pending!(stream);
}

#[async_test]
async fn test_updated_reply_doesnt_lose_latest_edit() {
    let timeline = TestTimeline::new().await;
    let mut stream = timeline.subscribe_events().await;

    let f = &timeline.factory;

    // Start with a message event.
    let target = event_id!("$1");
    timeline.handle_live_event(f.text_msg("hey").sender(&ALICE).event_id(target)).await;

    {
        let item = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);
        assert!(item.latest_edit_json().is_none());
        assert_eq!(item.content().as_message().unwrap().body(), "hey");
        assert_pending!(stream);
    }

    // Have someone send a reply.
    let reply = event_id!("$2");
    timeline
        .handle_live_event(f.text_msg("hallo").sender(&BOB).reply_to(target).event_id(reply))
        .await;

    {
        let item = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);
        assert!(item.latest_edit_json().is_none());
        assert_eq!(item.content().as_message().unwrap().body(), "hallo");
        assert_pending!(stream);
    }

    // Edit the reply.
    timeline
        .handle_live_event(
            f.text_msg("* guten tag")
                .sender(&BOB)
                .edit(reply, MessageType::text_plain("guten tag").into()),
        )
        .await;

    {
        let item = assert_next_matches!(stream, VectorDiff::Set { index: 1, value } => value);
        assert!(item.latest_edit_json().is_some());
        assert_eq!(item.content().as_message().unwrap().body(), "guten tag");
        assert_pending!(stream);
    }

    // Edit the original.
    timeline
        .handle_live_event(
            f.text_msg("* hello")
                .sender(&ALICE)
                .edit(target, MessageType::text_plain("hello").into()),
        )
        .await;

    // The original is updated.
    let item = assert_next_matches!(stream, VectorDiff::Set { index: 0, value } => value);
    // And now has a latest edit JSON.
    assert!(item.latest_edit_json().is_some());

    // The reply is updated.
    let item = assert_next_matches!(stream, VectorDiff::Set { index: 1, value } => value);
    // And still has the latest edit JSON.
    assert!(item.latest_edit_json().is_some());
    assert_eq!(item.content().as_message().unwrap().body(), "guten tag");

    assert_pending!(stream);
}

fn failed_state() -> EventSendState {
    EventSendState::SendingFailed {
        error: Arc::new(matrix_sdk::Error::SendQueueWedgeError(Box::new(
            QueueWedgeError::GenericApiError { msg: "nope".to_owned() },
        ))),
        is_recoverable: false,
    }
}

fn local_edit(original: &EventId, body: &str) -> AnyMessageLikeEventContent {
    let mut content = RoomMessageEventContent::text_plain(format!("* {body}"));
    content.relates_to = Some(Relation::Replacement(Replacement::new(
        original.to_owned(),
        RoomMessageEventContentWithoutRelation::text_plain(body),
    )));
    AnyMessageLikeEventContent::RoomMessage(content)
}

#[async_test]
async fn test_local_edit_send_state_transitions() {
    let timeline = TestTimeline::new().await;
    let mut stream = timeline.subscribe_events().await;
    let f = &timeline.factory;

    let original_id = event_id!("$original");
    timeline.handle_live_event(f.text_msg("hello").sender(*ALICE).event_id(original_id)).await;
    let item = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);
    assert!(item.content().as_message().unwrap().edit_send_state().is_none());

    // A pending local edit is applied and exposed as not sent yet.
    let txn_id = timeline.handle_local_event(local_edit(original_id, "edited")).await;
    let item = assert_next_matches!(stream, VectorDiff::Set { index: 0, value } => value);
    let msg = item.content().as_message().unwrap();
    assert_eq!(msg.body(), "edited");
    assert_matches!(msg.edit_send_state(), Some(EventSendState::NotSentYet { progress: None }));

    // Upload progress lands on the edit.
    timeline
        .controller
        .update_event_send_state(
            &txn_id,
            EventSendState::NotSentYet {
                progress: Some(MediaUploadProgress {
                    index: 0,
                    progress: AbstractProgress { current: 1, total: 2 },
                }),
            },
        )
        .await;
    let item = assert_next_matches!(stream, VectorDiff::Set { index: 0, value } => value);
    assert_matches!(
        item.content().as_message().unwrap().edit_send_state(),
        Some(EventSendState::NotSentYet { progress: Some(_) })
    );

    // A failure is exposed, the edited content stays.
    timeline.controller.update_event_send_state(&txn_id, failed_state()).await;
    let item = assert_next_matches!(stream, VectorDiff::Set { index: 0, value } => value);
    let msg = item.content().as_message().unwrap();
    assert_eq!(msg.body(), "edited");
    assert_matches!(
        msg.edit_send_state(),
        Some(EventSendState::SendingFailed { is_recoverable: false, .. })
    );

    // A retry resets it.
    timeline
        .controller
        .update_event_send_state(&txn_id, EventSendState::NotSentYet { progress: None })
        .await;
    let item = assert_next_matches!(stream, VectorDiff::Set { index: 0, value } => value);
    assert_matches!(
        item.content().as_message().unwrap().edit_send_state(),
        Some(EventSendState::NotSentYet { progress: None })
    );

    // Sent.
    let edit_id = event_id!("$edit");
    timeline
        .controller
        .update_event_send_state(&txn_id, EventSendState::Sent { event_id: edit_id.to_owned() })
        .await;
    let item = assert_next_matches!(stream, VectorDiff::Set { index: 0, value } => value);
    assert_matches!(
        item.content().as_message().unwrap().edit_send_state(),
        Some(EventSendState::Sent { .. })
    );

    // The remote echo of the edit clears it.
    timeline
        .handle_live_event(
            f.text_msg("* edited")
                .sender(*ALICE)
                .edit(original_id, MessageType::text_plain("edited").into())
                .event_id(edit_id),
        )
        .await;
    let item = assert_next_matches!(stream, VectorDiff::Set { index: 0, value } => value);
    let msg = item.content().as_message().unwrap();
    assert_eq!(msg.body(), "edited");
    assert!(msg.is_edited());
    assert!(msg.edit_send_state().is_none());

    assert_pending!(stream);
}

#[async_test]
async fn test_failed_edit_wins_over_a_later_pending_edit() {
    let timeline = TestTimeline::new().await;
    let mut stream = timeline.subscribe_events().await;
    let f = &timeline.factory;

    let original_id = event_id!("$original");
    timeline.handle_live_event(f.text_msg("hello").sender(*ALICE).event_id(original_id)).await;
    assert_next_matches!(stream, VectorDiff::PushBack { .. });

    let first = timeline.handle_local_event(local_edit(original_id, "first")).await;
    assert_next_matches!(stream, VectorDiff::Set { index: 0, .. });
    let _second = timeline.handle_local_event(local_edit(original_id, "second")).await;
    assert_next_matches!(stream, VectorDiff::Set { index: 0, .. });

    // The first edit fails: it blocks the second one, so the item says failed.
    timeline.controller.update_event_send_state(&first, failed_state()).await;
    let item = assert_next_matches!(stream, VectorDiff::Set { index: 0, value } => value);
    assert_matches!(
        item.content().as_message().unwrap().edit_send_state(),
        Some(EventSendState::SendingFailed { .. })
    );

    // Once the first one is sent, the second one's pending state shows.
    timeline
        .controller
        .update_event_send_state(&first, EventSendState::Sent { event_id: owned_event_id!("$e1") })
        .await;
    let item = assert_next_matches!(stream, VectorDiff::Set { index: 0, value } => value);
    assert_matches!(
        item.content().as_message().unwrap().edit_send_state(),
        Some(EventSendState::NotSentYet { .. })
    );

    assert_pending!(stream);
}

#[async_test]
async fn test_edit_remote_echo_before_sent_leaves_no_pending_state() {
    let timeline = TestTimeline::new().await;
    let mut stream = timeline.subscribe_events().await;
    let f = &timeline.factory;

    let original_id = event_id!("$original");
    timeline.handle_live_event(f.text_msg("hello").sender(*ALICE).event_id(original_id)).await;
    assert_next_matches!(stream, VectorDiff::PushBack { .. });

    let txn_id = timeline.handle_local_event(local_edit(original_id, "edited")).await;
    assert_next_matches!(stream, VectorDiff::Set { index: 0, .. });

    // The remote echo of the edit arrives before the send queue reports the send.
    let edit_id = event_id!("$edit");
    timeline
        .handle_live_event(
            f.text_msg("* edited")
                .sender(*ALICE)
                .edit(original_id, MessageType::text_plain("edited").into())
                .event_id(edit_id),
        )
        .await;
    assert_next_matches!(stream, VectorDiff::Set { index: 0, .. });

    // The late `Sent` lets the remote echo settle the item: nothing pending
    // anymore.
    timeline
        .controller
        .update_event_send_state(&txn_id, EventSendState::Sent { event_id: edit_id.to_owned() })
        .await;
    let item = assert_next_matches!(stream, VectorDiff::Set { index: 0, value } => value);
    let msg = item.content().as_message().unwrap();
    assert_eq!(msg.body(), "edited");
    assert!(msg.is_edited());
    assert!(msg.edit_send_state().is_none());

    assert_pending!(stream);
}
