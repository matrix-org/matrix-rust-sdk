// Copyright 2025 The Matrix.org Foundation C.I.C.
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

use ruma::{
    RoomId,
    events::{
        AnySyncMessageLikeEvent, AnySyncTimelineEvent, SyncMessageLikeEvent,
        room::message::MessageType,
    },
};

use super::e2ee::E2EE;
use crate::Result;

/// Process the given event as a verification event if it is a candidate. The
/// event must be decrypted.
///
/// **Note**: If the supplied event is an `m.room.message` event with
/// `msgtype: m.key.verification.request`, then the device information for
/// the sending user must be up-to-date before calling this method
/// (otherwise, the request will be ignored). It is hard to guarantee this
/// is the case, but you can maximize your chances by explicitly making a
/// request for this user's device info by calling
/// [`OlmMachine::query_keys_for_users`], sending the request, and
/// processing the response with [`OlmMachine::mark_request_as_sent`].
pub async fn process_if_relevant(
    event: &AnySyncTimelineEvent,
    e2ee: E2EE<'_>,
    room_id: &RoomId,
) -> Result<()> {
    if !e2ee.verification_is_allowed {
        return Ok(());
    }

    let Some(olm) = e2ee.olm_machine else {
        return Ok(());
    };

    let AnySyncTimelineEvent::MessageLike(event) = event else {
        return Ok(());
    };

    match event {
        // This is an original (i.e. non-redacted) `m.room.message` event and its
        // content is a verification request…
        AnySyncMessageLikeEvent::RoomMessage(SyncMessageLikeEvent::Original(original_event))
            if matches!(&original_event.content.msgtype, MessageType::VerificationRequest(_)) => {}

        // … or this is verification request event.
        AnySyncMessageLikeEvent::KeyVerificationReady(_)
        | AnySyncMessageLikeEvent::KeyVerificationStart(_)
        | AnySyncMessageLikeEvent::KeyVerificationCancel(_)
        | AnySyncMessageLikeEvent::KeyVerificationAccept(_)
        | AnySyncMessageLikeEvent::KeyVerificationKey(_)
        | AnySyncMessageLikeEvent::KeyVerificationMac(_)
        | AnySyncMessageLikeEvent::KeyVerificationDone(_) => {}

        _ => {
            // No need to handle those other event types.
            return Ok(());
        }
    }

    Ok(olm.receive_verification_event(&event.clone().into_full_event(room_id.to_owned())).await?)
}
