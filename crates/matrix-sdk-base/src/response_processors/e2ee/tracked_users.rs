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

use std::collections::BTreeSet;

use matrix_sdk_common::timer;
use matrix_sdk_crypto::OlmMachine;
use ruma::{OwnedUserId, RoomId};

use crate::{EncryptionState, Result, RoomMemberships, store::BaseStateStore};

/// Update tracked users, if the room is encrypted.
pub async fn update(
    olm_machine: Option<&OlmMachine>,
    room_encryption_state: EncryptionState,
    user_ids_to_track: &BTreeSet<OwnedUserId>,
) -> Result<()> {
    if room_encryption_state.is_encrypted()
        && let Some(olm) = olm_machine
        && !user_ids_to_track.is_empty()
    {
        olm.update_tracked_users(user_ids_to_track.iter().map(AsRef::as_ref)).await?
    }

    Ok(())
}

/// Update tracked users, if the room is encrypted, or if the room has become
/// encrypted.
pub async fn update_or_set_if_room_is_newly_encrypted(
    olm_machine: Option<&OlmMachine>,
    user_ids_to_track: &BTreeSet<OwnedUserId>,
    new_room_encryption_state: EncryptionState,
    previous_room_encryption_state: EncryptionState,
    room_id: &RoomId,
    state_store: &BaseStateStore,
) -> Result<()> {
    let _timer = timer!(tracing::Level::TRACE, "update_or_set_if_room_is_newly_encrypted");

    if new_room_encryption_state.is_encrypted()
        && let Some(olm) = olm_machine
    {
        if !previous_room_encryption_state.is_encrypted() {
            // The room turned on encryption in this sync, we need
            // to also get all the existing users and mark them for
            // tracking.
            let user_ids = state_store.get_user_ids(room_id, RoomMemberships::ACTIVE).await?;
            olm.update_tracked_users(user_ids.iter().map(AsRef::as_ref)).await?
        }

        if !user_ids_to_track.is_empty() {
            olm.update_tracked_users(user_ids_to_track.iter().map(AsRef::as_ref)).await?;
        }
    }

    Ok(())
}
