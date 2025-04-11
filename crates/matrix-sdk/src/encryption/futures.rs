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

//! Named futures returned from methods on types in
//! [the `encryption` module][super].

#![deny(unreachable_pub)]

use std::{future::IntoFuture, io::Read, iter};

use eyeball::SharedObservable;
#[cfg(not(target_arch = "wasm32"))]
use eyeball::Subscriber;
use matrix_sdk_common::boxed_into_future;
use ruma::{
    events::room::{EncryptedFile, EncryptedFileInit},
    OwnedUserId,
};

use crate::{
    config::RequestConfig, crypto::types::events::room_key_bundle::RoomKeyBundleContent, Client,
    Media, Result, Room, TransmissionProgress,
};

/// Future returned by [`Client::upload_encrypted_file`].
#[allow(missing_debug_implementations)]
pub struct UploadEncryptedFile<'a, R: ?Sized> {
    client: &'a Client,
    content_type: &'a mime::Mime,
    reader: &'a mut R,
    send_progress: SharedObservable<TransmissionProgress>,
    request_config: Option<RequestConfig>,
}

impl<'a, R: ?Sized> UploadEncryptedFile<'a, R> {
    pub(crate) fn new(client: &'a Client, content_type: &'a mime::Mime, reader: &'a mut R) -> Self {
        Self {
            client,
            content_type,
            reader,
            send_progress: Default::default(),
            request_config: None,
        }
    }

    /// Replace the default `SharedObservable` used for tracking upload
    /// progress.
    ///
    /// Note that any subscribers obtained from
    /// [`subscribe_to_send_progress`][Self::subscribe_to_send_progress]
    /// will be invalidated by this.
    pub fn with_send_progress_observable(
        mut self,
        send_progress: SharedObservable<TransmissionProgress>,
    ) -> Self {
        self.send_progress = send_progress;
        self
    }

    /// Replace the default request config used for the upload request.
    ///
    /// The timeout value will be overridden with a reasonable default, based on
    /// the size of the encrypted payload.
    pub fn with_request_config(mut self, request_config: RequestConfig) -> Self {
        self.request_config = Some(request_config);
        self
    }

    /// Get a subscriber to observe the progress of sending the request
    /// body.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn subscribe_to_send_progress(&self) -> Subscriber<TransmissionProgress> {
        self.send_progress.subscribe()
    }
}

impl<'a, R> IntoFuture for UploadEncryptedFile<'a, R>
where
    R: Read + Send + ?Sized + 'a,
{
    type Output = Result<EncryptedFile>;
    boxed_into_future!(extra_bounds: 'a);

    fn into_future(self) -> Self::IntoFuture {
        let Self { client, content_type, reader, send_progress, request_config } = self;
        Box::pin(async move {
            let mut encryptor = matrix_sdk_base::crypto::AttachmentEncryptor::new(reader);

            let mut buf = Vec::new();
            encryptor.read_to_end(&mut buf)?;

            // Override the reasonable upload timeout value, based on the size of the
            // encrypted payload.
            let request_config =
                request_config.map(|config| config.timeout(Media::reasonable_upload_timeout(&buf)));

            let response = client
                .media()
                .upload(content_type, buf, request_config)
                .with_send_progress_observable(send_progress)
                .await?;

            let file: EncryptedFile = {
                let keys = encryptor.finish();
                EncryptedFileInit {
                    url: response.content_uri,
                    key: keys.key,
                    iv: keys.iv,
                    hashes: keys.hashes,
                    v: keys.version,
                }
                .into()
            };

            Ok(file)
        })
    }
}

/// Future returned by [`Room::share_history`].
#[derive(Debug)]
pub struct ShareRoomHistory<'a> {
    /// The room whose history should be shared.
    room: &'a Room,

    /// The recipient of the shared history.
    user_id: OwnedUserId,
}

impl<'a> ShareRoomHistory<'a> {
    pub(crate) fn new(room: &'a Room, user_id: OwnedUserId) -> Self {
        Self { room, user_id }
    }
}

impl<'a> IntoFuture for ShareRoomHistory<'a> {
    type Output = Result<()>;
    boxed_into_future!(extra_bounds: 'a);

    fn into_future(self) -> Self::IntoFuture {
        let Self { room, user_id } = self;
        Box::pin(async move {
            tracing::info!("Sharing message history in {} with {}", room.room_id(), user_id);
            let client = &room.client;

            // 1. Construct the key bundle
            let bundle = {
                let olm_machine = client.olm_machine().await;
                let olm_machine = olm_machine
                    .as_ref()
                    .expect("This should only be called once we have an OlmMachine");
                olm_machine.store().build_room_key_bundle(room.room_id()).await?
            };

            if bundle.is_empty() {
                tracing::info!("No keys to share");
                return Ok(());
            }

            // 2. Upload to the server as an encrypted file
            let json = serde_json::to_vec(&bundle)?;
            let upload = room
                .client
                .upload_encrypted_file(&mime::APPLICATION_JSON, &mut (json.as_slice()))
                .await?;

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
        })
    }
}
