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
use matrix_sdk_crypto::RoomEventDecryptionResult;
use ruma::RoomId;

use super::{super::verification, E2EE};
use crate::Result;

/// Attempt to decrypt the given raw event into a [`TimelineEvent`].
///
/// In the case of a decryption error, returns a [`TimelineEvent`]
/// representing the decryption error; in the case of problems with our
/// application, returns `Err`.
///
/// Returns `Ok(None)` if encryption is not configured.
///
/// The returned [`TimelineEvent`] has no push actions set up. It's the
/// responsibility of the caller to set them.
pub async fn sync_timeline_event(
    e2ee: E2EE<'_>,
    event: &TimelineEvent,
    room_id: &RoomId,
) -> Result<Option<TimelineEvent>> {
    let Some(olm) = e2ee.olm_machine else { return Ok(None) };

    Ok(Some(
        match olm
            .try_decrypt_room_event(
                event.raw().cast_ref_unchecked(),
                room_id,
                e2ee.decryption_settings,
            )
            .await?
        {
            RoomEventDecryptionResult::Decrypted(decrypted) => {
                // Note: the push actions are set by the caller.
                let timeline_event = event.to_decrypted(decrypted, None);

                if let Ok(sync_timeline_event) = timeline_event.raw().deserialize() {
                    verification::process_if_relevant(&sync_timeline_event, e2ee, room_id).await?;
                }

                timeline_event
            }
            RoomEventDecryptionResult::UnableToDecrypt(utd_info) => event.to_utd(utd_info),
        },
    ))
}
