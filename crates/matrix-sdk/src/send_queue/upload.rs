// Copyright 2024 The Matrix.org Foundation C.I.C.
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

//! Private implementations of the media upload mechanism.

use matrix_sdk_base::{
    media::{MediaFormat, MediaRequestParameters, MediaThumbnailSettings},
    store::{
        ChildTransactionId, FinishUploadThumbnailInfo, QueuedRequestKind, SentMediaInfo,
        SentRequestKey, SerializableEventContent,
    },
    RoomState,
};
use mime::Mime;
use ruma::{
    assign,
    events::room::{
        message::{MessageType, RoomMessageEventContent},
        MediaSource, ThumbnailInfo,
    },
    media::Method,
    uint, OwnedMxcUri, OwnedTransactionId, TransactionId, UInt,
};
use tracing::{debug, error, instrument, trace, warn, Span};

use super::{QueueStorage, RoomSendQueue, RoomSendQueueError, SendAttachmentHandle};
use crate::{
    attachment::AttachmentConfig,
    send_queue::{
        LocalEcho, LocalEchoContent, RoomSendQueueStorageError, RoomSendQueueUpdate, SendHandle,
    },
    Client, Room,
};

/// Create a [`MediaRequest`] for a file we want to store locally before
/// sending it.
///
/// This uses a MXC ID that is only locally valid.
fn make_local_file_media_request(txn_id: &TransactionId) -> MediaRequestParameters {
    // This mustn't represent a potentially valid media server, otherwise it'd be
    // possible for an attacker to return malicious content under some
    // preconditions (e.g. the cache store has been cleared before the upload
    // took place). To mitigate against this, we use the .localhost TLD,
    // which is guaranteed to be on the local machine. As a result, the only attack
    // possible would be coming from the user themselves, which we consider a
    // non-threat.
    MediaRequestParameters {
        source: MediaSource::Plain(OwnedMxcUri::from(format!(
            "mxc://send-queue.localhost/{txn_id}"
        ))),
        format: MediaFormat::File,
    }
}

/// Create a [`MediaRequest`] for a file we want to store locally before
/// sending it.
///
/// This uses a MXC ID that is only locally valid.
fn make_local_thumbnail_media_request(
    txn_id: &TransactionId,
    height: UInt,
    width: UInt,
) -> MediaRequestParameters {
    // See comment in [`make_local_file_media_request`].
    let source =
        MediaSource::Plain(OwnedMxcUri::from(format!("mxc://send-queue.localhost/{}", txn_id)));
    let format = MediaFormat::Thumbnail(MediaThumbnailSettings {
        method: Method::Scale,
        width,
        height,
        animated: false,
    });
    MediaRequestParameters { source, format }
}

/// Replace the source by the final ones in all the media types handled by
/// [`Room::make_attachment_type()`].
fn update_media_event_after_upload(echo: &mut RoomMessageEventContent, sent: SentMediaInfo) {
    // Some variants look really similar below, but the `event` and `info` are all
    // different types…
    match &mut echo.msgtype {
        MessageType::Audio(event) => {
            event.source = sent.file;
        }
        MessageType::File(event) => {
            event.source = sent.file;
            if let Some(info) = event.info.as_mut() {
                info.thumbnail_source = sent.thumbnail;
            }
        }
        MessageType::Image(event) => {
            event.source = sent.file;
            if let Some(info) = event.info.as_mut() {
                info.thumbnail_source = sent.thumbnail;
            }
        }
        MessageType::Video(event) => {
            event.source = sent.file;
            if let Some(info) = event.info.as_mut() {
                info.thumbnail_source = sent.thumbnail;
            }
        }

        _ => {
            // All `MessageType` created by `Room::make_attachment_type` should be
            // handled here. The only way to end up here is that a message type has
            // been tampered with in the database.
            error!("Invalid message type in database: {}", echo.msgtype());
            // Only crash debug builds.
            debug_assert!(false, "invalid message type in database");
        }
    }
}

impl RoomSendQueue {
    /// Queues an attachment to be sent to the room, using the send queue.
    ///
    /// This returns quickly (without sending or uploading anything), and will
    /// push the event to be sent into a queue, handled in the background.
    ///
    /// Callers are expected to consume [`RoomSendQueueUpdate`] via calling
    /// the [`Self::subscribe()`] method to get updates about the sending of
    /// that event.
    ///
    /// By default, if sending failed on the first attempt, it will be retried a
    /// few times. If sending failed after those retries, the entire
    /// client's sending queue will be disabled, and it will need to be
    /// manually re-enabled by the caller (e.g. after network is back, or when
    /// something has been done about the faulty requests).
    #[instrument(skip_all, fields(event_txn))]
    pub async fn send_attachment(
        &self,
        filename: &str,
        content_type: Mime,
        data: Vec<u8>,
        mut config: AttachmentConfig,
    ) -> Result<SendAttachmentHandle, RoomSendQueueError> {
        let Some(room) = self.inner.room.get() else {
            return Err(RoomSendQueueError::RoomDisappeared);
        };

        if room.state() != RoomState::Joined {
            return Err(RoomSendQueueError::RoomNotJoined);
        }

        let upload_file_txn = TransactionId::new();
        let send_event_txn = config.txn_id.map_or_else(ChildTransactionId::new, Into::into);

        Span::current().record("event_txn", tracing::field::display(&*send_event_txn));
        debug!(filename, %content_type, %upload_file_txn, "sending an attachment");

        // Cache the file itself in the cache store.
        let file_media_request = make_local_file_media_request(&upload_file_txn);
        room.client()
            .event_cache_store()
            .add_media_content(&file_media_request, data.clone())
            .await
            .map_err(|err| RoomSendQueueError::StorageError(err.into()))?;

        // Process the thumbnail, if it's been provided.
        let (upload_thumbnail_txn, event_thumbnail_info, queue_thumbnail_info) = if let Some(
            thumbnail,
        ) =
            config.thumbnail.take()
        {
            // Normalize information to retrieve the thumbnail in the cache store.
            let info = thumbnail.info.as_ref();
            let height = info.and_then(|info| info.height).unwrap_or_else(|| {
                trace!("thumbnail height is unknown, using 0 for the cache entry");
                uint!(0)
            });
            let width = info.and_then(|info| info.width).unwrap_or_else(|| {
                trace!("thumbnail width is unknown, using 0 for the cache entry");
                uint!(0)
            });

            let txn = TransactionId::new();
            trace!(upload_thumbnail_txn = %txn, thumbnail_size = ?(height, width), "attachment has a thumbnail");

            // Cache thumbnail in the cache store.
            let thumbnail_media_request = make_local_thumbnail_media_request(&txn, height, width);
            room.client()
                .event_cache_store()
                .add_media_content(&thumbnail_media_request, thumbnail.data.clone())
                .await
                .map_err(|err| RoomSendQueueError::StorageError(err.into()))?;

            // Create the information required for filling the thumbnail section of the
            // media event.
            let thumbnail_info =
                Box::new(assign!(thumbnail.info.map(ThumbnailInfo::from).unwrap_or_default(), {
                    mimetype: Some(thumbnail.content_type.as_ref().to_owned())
                }));

            (
                Some(txn.clone()),
                Some((thumbnail_media_request.source.clone(), thumbnail_info)),
                Some((
                    FinishUploadThumbnailInfo { txn, width, height },
                    thumbnail_media_request,
                    thumbnail.content_type,
                )),
            )
        } else {
            Default::default()
        };

        // Create the content for the media event.
        let event_content = Room::make_attachment_event(
            room.make_attachment_type(
                &content_type,
                filename,
                file_media_request.source.clone(),
                config.caption,
                config.formatted_caption,
                config.info,
                event_thumbnail_info,
            ),
            config.mentions,
        );

        // Save requests in the queue storage.
        self.inner
            .queue
            .push_media(
                event_content.clone(),
                content_type,
                send_event_txn.clone().into(),
                upload_file_txn.clone(),
                file_media_request,
                queue_thumbnail_info,
            )
            .await?;

        trace!("manager sends a media to the background task");

        self.inner.notifier.notify_one();

        let _ = self.inner.updates.send(RoomSendQueueUpdate::NewLocalEvent(LocalEcho {
            transaction_id: send_event_txn.clone().into(),
            content: LocalEchoContent::Event {
                serialized_event: SerializableEventContent::new(&event_content.into())
                    .map_err(RoomSendQueueStorageError::JsonSerialization)?,
                // TODO: this should be a `SendAttachmentHandle`!
                send_handle: SendHandle {
                    room: self.clone(),
                    transaction_id: send_event_txn.clone().into(),
                    is_upload: true,
                },
                send_error: None,
            },
        }));

        Ok(SendAttachmentHandle {
            _room: self.clone(),
            _transaction_id: send_event_txn.into(),
            _file_upload: upload_file_txn,
            _thumbnail_transaction_id: upload_thumbnail_txn,
        })
    }
}

impl QueueStorage {
    /// Consumes a finished upload and queues sending of the final media event.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn handle_dependent_finish_upload(
        &self,
        client: &Client,
        event_txn: OwnedTransactionId,
        parent_key: SentRequestKey,
        mut local_echo: RoomMessageEventContent,
        file_upload_txn: OwnedTransactionId,
        thumbnail_info: Option<FinishUploadThumbnailInfo>,
        new_updates: &mut Vec<RoomSendQueueUpdate>,
    ) -> Result<(), RoomSendQueueError> {
        // Both uploads are ready: enqueue the event with its final data.
        let sent_media = parent_key
            .into_media()
            .ok_or(RoomSendQueueError::StorageError(RoomSendQueueStorageError::InvalidParentKey))?;

        // Update cache keys in the cache store.
        {
            // Do it for the file itself.
            let from_req = make_local_file_media_request(&file_upload_txn);

            trace!(from = ?from_req.source, to = ?sent_media.file, "renaming media file key in cache store");

            client
                .event_cache_store()
                .replace_media_key(
                    &from_req,
                    &MediaRequestParameters {
                        source: sent_media.file.clone(),
                        format: MediaFormat::File,
                    },
                )
                .await
                .map_err(RoomSendQueueStorageError::EventCacheStoreError)?;

            // Rename the thumbnail too, if needs be.
            if let Some((info, new_source)) =
                thumbnail_info.as_ref().zip(sent_media.thumbnail.clone())
            {
                let from_req =
                    make_local_thumbnail_media_request(&info.txn, info.height, info.width);

                trace!( from = ?from_req.source, to = ?new_source, "renaming thumbnail file key in cache store");

                // Reuse the same format for the cached thumbnail with the final MXC ID.
                let new_format = from_req.format.clone();

                client
                    .event_cache_store()
                    .replace_media_key(
                        &from_req,
                        &MediaRequestParameters { source: new_source, format: new_format },
                    )
                    .await
                    .map_err(RoomSendQueueStorageError::EventCacheStoreError)?;
            }
        }

        update_media_event_after_upload(&mut local_echo, sent_media);

        let new_content = SerializableEventContent::new(&local_echo.into())
            .map_err(RoomSendQueueStorageError::JsonSerialization)?;

        // Indicates observers that the upload finished, by editing the local echo for
        // the event into its final form before sending.
        new_updates.push(RoomSendQueueUpdate::ReplacedLocalEvent {
            transaction_id: event_txn.clone(),
            new_content: new_content.clone(),
        });

        trace!(%event_txn, "queueing media event after successfully uploading media(s)");

        client
            .store()
            .save_send_queue_request(&self.room_id, event_txn, new_content.into())
            .await
            .map_err(RoomSendQueueStorageError::StateStoreError)?;

        Ok(())
    }

    /// Consumes a finished upload of a thumbnail and queues the file upload.
    pub(super) async fn handle_dependent_file_upload_with_thumbnail(
        &self,
        client: &Client,
        next_upload_txn: OwnedTransactionId,
        parent_key: SentRequestKey,
        content_type: String,
        cache_key: MediaRequestParameters,
        event_txn: OwnedTransactionId,
    ) -> Result<(), RoomSendQueueError> {
        // The thumbnail has been sent, now transform the dependent file upload request
        // into a ready one.
        let sent_media = parent_key
            .into_media()
            .ok_or(RoomSendQueueError::StorageError(RoomSendQueueStorageError::InvalidParentKey))?;

        // The media we just uploaded was a thumbnail, so the thumbnail shouldn't have
        // a thumbnail itself.
        debug_assert!(sent_media.thumbnail.is_none());
        if sent_media.thumbnail.is_some() {
            warn!("unexpected thumbnail for a thumbnail!");
        }

        trace!(related_to = %event_txn, "done uploading thumbnail, now queuing a request to send the media file itself");

        let request = QueuedRequestKind::MediaUpload {
            content_type,
            cache_key,
            // The thumbnail for the next upload is the file we just uploaded here.
            thumbnail_source: Some(sent_media.file),
            related_to: event_txn,
        };

        client
            .store()
            .save_send_queue_request(&self.room_id, next_upload_txn, request)
            .await
            .map_err(RoomSendQueueStorageError::StateStoreError)?;

        Ok(())
    }
}
