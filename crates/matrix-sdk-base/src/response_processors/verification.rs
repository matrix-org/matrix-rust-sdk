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

use matrix_sdk_crypto::OlmMachine;
use ruma::{
    events::{
        room::message::MessageType, AnySyncMessageLikeEvent, AnySyncTimelineEvent,
        SyncMessageLikeEvent,
    },
    RoomId,
};

use super::Context;
use crate::Result;

/// Process the given event as a verification event if it is a candidate. The
/// event must be decrypted.
pub async fn process_if_candidate(
    context: &mut Context,
    event: &AnySyncTimelineEvent,
    verification_is_allowed: bool,
    olm_machine: Option<&OlmMachine>,
    room_id: &RoomId,
) -> Result<()> {
    if let AnySyncTimelineEvent::MessageLike(event) = event {
        // That's it, we are good, the event has been decrypted successfully.

        // However, let's run an additional action. Check if this is a verification
        // event (`m.key.verification.*`), and call `verification` accordingly.
        if match &event {
            // This is an original (i.e. non-redacted) `m.room.message` event and its
            // content is a verification request…
            AnySyncMessageLikeEvent::RoomMessage(SyncMessageLikeEvent::Original(
                original_event,
            )) => {
                matches!(&original_event.content.msgtype, MessageType::VerificationRequest(_))
            }

            // … or this is verification request event
            AnySyncMessageLikeEvent::KeyVerificationReady(_)
            | AnySyncMessageLikeEvent::KeyVerificationStart(_)
            | AnySyncMessageLikeEvent::KeyVerificationCancel(_)
            | AnySyncMessageLikeEvent::KeyVerificationAccept(_)
            | AnySyncMessageLikeEvent::KeyVerificationKey(_)
            | AnySyncMessageLikeEvent::KeyVerificationMac(_)
            | AnySyncMessageLikeEvent::KeyVerificationDone(_) => true,

            _ => false,
        } {
            verification(context, verification_is_allowed, olm_machine, event, room_id).await?;
        }
    }

    Ok(())
}

async fn verification(
    _context: &mut Context,
    verification_is_allowed: bool,
    olm_machine: Option<&OlmMachine>,
    event: &AnySyncMessageLikeEvent,
    room_id: &RoomId,
) -> Result<()> {
    if !verification_is_allowed {
        return Ok(());
    }

    if let Some(olm) = olm_machine {
        olm.receive_verification_event(&event.clone().into_full_event(room_id.to_owned())).await?;
    }

    Ok(())
}
