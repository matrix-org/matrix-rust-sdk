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

use std::{iter, ops::Deref};

use matrix_sdk_base::media::{MediaFormat, MediaRequestParameters};
use ruma::{api::client::authenticated_media, events::room::MediaSource, OwnedUserId, UserId};
use tracing::{info, instrument, warn};

use crate::{
    crypto::types::{events::room_key_bundle::RoomKeyBundleContent, room_history::RoomKeyBundle},
    Error, Result, Room,
};

/// Share any shareable E2EE history in the given room with the given recipient,
/// as per [MSC4268].
///
/// [MSC4268]: https://github.com/matrix-org/matrix-spec-proposals/pull/4268
#[instrument(skip(room), fields(room_id = ?room.room_id()))]
pub async fn share_room_history(room: &Room, user_id: OwnedUserId) -> Result<()> {
    let client = &room.client;

    // 0. We can only share room history if our user has set up cross signing
    let own_identity = match client.user_id() {
        Some(own_user) => client.encryption().get_user_identity(own_user).await?,
        None => None,
    };
    if own_identity.is_none() {
        warn!("Not sharing message history as cross-signing is not set up");
        return Ok(());
    }

    info!("Sharing message history");

    // 1. Construct the key bundle
    let bundle = {
        let olm_machine = client.olm_machine().await;
        let olm_machine = olm_machine.as_ref().ok_or(Error::NoOlmMachine)?;
        olm_machine.store().build_room_key_bundle(room.room_id()).await?
    };

    if bundle.is_empty() {
        info!("No keys to share");
        return Ok(());
    }

    // 2. Upload to the server as an encrypted file
    let json = serde_json::to_vec(&bundle)?;
    let upload = client.upload_encrypted_file(&mut (json.as_slice())).await?;

    info!(
        media_url = ?upload.url,
        shared_keys = bundle.room_keys.len(),
        withheld_keys = bundle.withheld.len(),
        "Uploaded encrypted key blob"
    );

    // 3. Establish Olm sessions with all of the recipient's devices.
    client.claim_one_time_keys(iter::once(user_id.as_ref())).await?;

    // 4. Send to-device messages to the recipient to share the keys.
    let content = RoomKeyBundleContent { room_id: room.room_id().to_owned(), file: upload };
    let requests = {
        let olm_machine = client.olm_machine().await;
        let olm_machine = olm_machine.as_ref().ok_or(Error::NoOlmMachine)?;
        olm_machine
            .share_room_key_bundle_data(
                &user_id,
                &client.base_client().room_key_recipient_strategy,
                content,
            )
            .await?
    };

    for request in requests {
        let response = client.send_to_device(&request).await?;
        client.mark_request_as_sent(&request.txn_id, &response).await?;
    }
    Ok(())
}

/// Having accepted an invite for the given room from the given user, download a
/// key bundle and import the room keys, as per [MSC4268].
///
/// [MSC4268]: https://github.com/matrix-org/matrix-spec-proposals/pull/4268
#[instrument(skip(room), fields(room_id = ?room.room_id()))]
pub async fn accept_key_bundle(room: &Room, user_id: &UserId) -> Result<()> {
    // TODO: retry this if it gets interrupted or it fails.
    // TODO: do this in the background.

    let client = &room.client;
    let olm_machine = client.olm_machine().await;
    let Some(olm_machine) = olm_machine.deref() else {
        warn!("Not fetching room key bundle as the Olm machine is not available");
        return Ok(());
    };

    let Some(bundle) =
        olm_machine.store().get_received_room_key_bundle_data(room.room_id(), user_id).await?
    else {
        // No bundle received (yet).
        // TODO: deal with the bundle arriving later (https://github.com/matrix-org/matrix-rust-sdk/issues/4926)
        return Ok(());
    };

    // TODO
    // olm_machine.store().clear_received_room_key_bundle_data(room.room_id(),
    // user_id).await?;

    let bundle_content = client
        .media()
        .get_media_content(
            &MediaRequestParameters {
                source: MediaSource::Encrypted(Box::new(bundle.bundle_data.file)),
                format: MediaFormat::File,
            },
            false,
        )
        .await?;

    let bundle_content: RoomKeyBundle = match serde_json::from_slice(&bundle_content) {
        Ok(bundle_content) => bundle_content,
        Err(err) => {
            warn!("Failed to deserialize room key bundle: {}", err);
            return Ok(());
        }
    };

    olm_machine
        .receive_room_key_bundle(
            &bundle_content,
            room.room_id(),
            &bundle.sender_user,
            &bundle.sender_data,
        )
        .await?;

    Ok(())
}
