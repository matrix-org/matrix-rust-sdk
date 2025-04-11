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

use matrix_sdk_common::deserialized_responses::TimelineEvent;
use matrix_sdk_crypto::{
    DecryptionSettings, OlmMachine, RoomEventDecryptionResult, TrustRequirement,
};
use ruma::{events::AnySyncTimelineEvent, serde::Raw, RoomId};

use super::super::{verification, Context};
use crate::Result;

/// Attempt to decrypt the given raw event into a [`TimelineEvent`].
///
/// In the case of a decryption error, returns a [`TimelineEvent`]
/// representing the decryption error; in the case of problems with our
/// application, returns `Err`.
///
/// Returns `Ok(None)` if encryption is not configured.
pub async fn sync_timeline_event(
    context: &mut Context,
    olm_machine: Option<&OlmMachine>,
    event: &Raw<AnySyncTimelineEvent>,
    room_id: &RoomId,
    decryption_trust_requirement: TrustRequirement,
    verification_is_allowed: bool,
) -> Result<Option<TimelineEvent>> {
    let Some(olm) = olm_machine else { return Ok(None) };

    let decryption_settings =
        DecryptionSettings { sender_device_trust_requirement: decryption_trust_requirement };

    Ok(Some(
        match olm.try_decrypt_room_event(event.cast_ref(), room_id, &decryption_settings).await? {
            RoomEventDecryptionResult::Decrypted(decrypted) => {
                let timeline_event = TimelineEvent::from(decrypted);

                if let Ok(sync_timeline_event) = timeline_event.raw().deserialize() {
                    verification::process_if_relevant(
                        context,
                        &sync_timeline_event,
                        verification_is_allowed,
                        olm_machine,
                        room_id,
                    )
                    .await?;
                }

                timeline_event
            }
            RoomEventDecryptionResult::UnableToDecrypt(utd_info) => {
                TimelineEvent::new_utd_event(event.clone(), utd_info)
            }
        },
    ))
}
