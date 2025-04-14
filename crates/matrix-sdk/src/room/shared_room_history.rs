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

use crate::{crypto::types::events::room_key_bundle::RoomKeyBundleContent, Result, Room};

/// Share any shareable E2EE history in the given room with the given recipient,
/// as per [MSC4268].
///
/// # Panics
///
/// Panics if the OlmMachine hasn't been set up on the client.
///
/// [MSC4268]: https://github.com/matrix-org/matrix-spec-proposals/pull/4268
pub async fn share_room_history(room: &Room, user_id: OwnedUserId) -> Result<()> {
    tracing::info!("Sharing message history in {} with {}", room.room_id(), user_id);
    let client = &room.client;

    // 1. Construct the key bundle
    let bundle = {
        let olm_machine = client.olm_machine().await;
        let olm_machine =
            olm_machine.as_ref().expect("This should only be called once we have an OlmMachine");
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

    // 3. Establish Olm sessions with all of the recipient's devices.
    client.claim_one_time_keys(iter::once(user_id.as_ref())).await?;

    // 4. Send to-device messages to the recipient to share the keys.
    let requests = client
        .base_client()
        .share_room_key_bundle_data(
            &user_id,
            RoomKeyBundleContent { room_id: room.room_id().to_owned(), file: upload },
        )
        .await?;

    for request in requests {
        let response = client.send_to_device(&request).await?;
        client.mark_request_as_sent(&request.txn_id, &response).await?;
    }
    Ok(())
}
