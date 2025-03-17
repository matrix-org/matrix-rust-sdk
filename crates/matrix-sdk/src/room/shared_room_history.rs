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

use std::iter;

use ruma::OwnedUserId;

use crate::{Error, Result, Room};

/// Share any shareable E2EE history in the given room with the given recipient,
/// as per [MSC4268].
///
/// [MSC4268]: https://github.com/matrix-org/matrix-spec-proposals/pull/4268
pub async fn share_room_history(room: &Room, user_id: OwnedUserId) -> Result<()> {
    tracing::info!("Sharing message history in {} with {}", room.room_id(), user_id);

    // 1. Construct the key bundle
    let bundle = {
        let olm_machine = room.client.olm_machine().await;
        let olm_machine = olm_machine.as_ref().ok_or(Error::NoOlmMachine)?;
        olm_machine.store().build_room_key_bundle(room.room_id()).await?
    };

    if bundle.is_empty() {
        tracing::info!("No keys to share");
        return Ok(());
    }

    // 2. Upload to the server as an encrypted file
    let json = serde_json::to_vec(&bundle)?;
    let upload =
        room.client.upload_encrypted_file(&mime::APPLICATION_JSON, &mut (json.as_slice())).await?;

    tracing::info!(
        media_url = ?upload.url,
        shared_keys = bundle.room_keys.len(),
        withheld_keys = bundle.withheld.len(),
        "Uploaded encrypted key blob"
    );

    // 3. Send to-device messages to the recipient to share the keys.
    // TODO

    Ok(())
}
