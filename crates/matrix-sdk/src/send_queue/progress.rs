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

//! Progress facilities for the media upload system.

use std::ops::Add;

use eyeball::SharedObservable;
#[cfg(feature = "unstable-msc4274")]
use matrix_sdk_base::store::AccumulatedSentMediaInfo;
use matrix_sdk_base::{media::MediaRequestParameters, store::DependentQueuedRequestKind};
use matrix_sdk_common::executor::spawn;
use ruma::{TransactionId, events::room::MediaSource};
use tokio::sync::broadcast;
use tracing::warn;

use crate::{
    Room, TransmissionProgress,
    send_queue::{QueueStorage, RoomSendQueue, RoomSendQueueStorageError, RoomSendQueueUpdate},
};

/// Progress of an operation in abstract units.
///
/// Contrary to [`TransmissionProgress`], this allows tracking the progress
/// of sending or receiving a payload in estimated pseudo units representing a
/// percentage. This is helpful in cases where the exact progress in bytes isn't
/// known, for instance, because encryption (which changes the size) happens on
/// the fly.
#[derive(Clone, Copy, Debug, Default)]
pub struct AbstractProgress {
    /// How many units were already transferred.
    pub current: usize,
    /// How many units there are in total.
    pub total: usize,
}

// AbstractProgress can be added together, which adds their components.
impl Add for AbstractProgress {
    type Output = Self;

    fn add(self, other: Self) -> Self::Output {
        Self { current: self.current + other.current, total: self.total + other.total }
    }
}

/// Information needed to compute the progress of uploading a media and its
/// associated thumbnail.
#[derive(Clone, Copy, Debug)]
pub(super) struct MediaUploadProgressInfo {
    /// The index of the uploaded item if this is a gallery upload. Otherwise,
    /// zero.
    pub index: u64,
    /// The total number of bytes in the file currently being uploaded.
    pub bytes: usize,
    /// Some offset for the [`AbstractProgress`] computations.
    ///
    /// For a file upload, the offsets include the size of the thumbnail as part
    /// of the already uploaded data and total. For a thumbnail upload, this
    /// includes the size of the file to be uploaded in the total.
    pub offsets: AbstractProgress,
}

impl RoomSendQueue {
    /// Create metadata required to compute the progress of a media upload.
    pub(super) async fn create_media_upload_progress_info(
        own_txn_id: &TransactionId,
        related_to: &TransactionId,
        cache_key: &MediaRequestParameters,
        thumbnail_source: Option<&MediaSource>,
        #[cfg(feature = "unstable-msc4274")] accumulated: &[AccumulatedSentMediaInfo],
        room: &Room,
        queue: &QueueStorage,
    ) -> MediaUploadProgressInfo {
        // Determine the item's index, if this is a gallery upload.
        let index = {
            cfg_if::cfg_if! {
                if #[cfg(feature = "unstable-msc4274")] {
                    accumulated.len()
                } else {
                    0 // Before MSC4274 only a single file (and thumbnail) could be sent per event.
                }
            }
        };

        // Get the size of the file being uploaded from the event cache.
        let bytes = match room.client().media_store().lock().await {
            Ok(cache) => match cache.get_media_content(cache_key).await {
                Ok(Some(content)) => content.len(),
                Ok(None) => 0,
                Err(err) => {
                    warn!("error when reading media content from media store: {err}");
                    0
                }
            },
            Err(err) => {
                warn!("couldn't acquire media store lock: {err}");
                0
            }
        };

        let offsets = {
            // If we're uploading a file, we may have already uploaded a thumbnail; get its
            // size from the in-memory thumbnail sizes cache. This will account in the
            // current and total size, for the overall progress of
            // thumbnail+file.
            let already_uploaded_thumbnail_bytes = if thumbnail_source.is_some() {
                queue
                    .thumbnail_file_sizes
                    .lock()
                    .get(related_to)
                    .and_then(|sizes| sizes.get(index))
                    .copied()
                    .flatten()
            } else {
                None
            };

            let already_uploaded_thumbnail_bytes = already_uploaded_thumbnail_bytes.unwrap_or(0);

            // If we're uploading a thumbnail, get the size of the file to be uploaded after
            // it, from the database. This will account in the total progress of the
            // file+thumbnail upload (we're currently uploading the thumbnail,
            // in the first step).
            let pending_file_bytes = match RoomSendQueue::get_dependent_pending_file_upload_size(
                own_txn_id, room,
            )
            .await
            {
                Ok(maybe_size) => maybe_size.unwrap_or(0),
                Err(err) => {
                    warn!(
                        "error when getting pending file upload size: {err}; using 0 as fallback"
                    );
                    0
                }
            };

            // In nominal cases where the send queue is used correctly, only one of these
            // two values will be non-zero.
            AbstractProgress {
                current: already_uploaded_thumbnail_bytes,
                total: already_uploaded_thumbnail_bytes + pending_file_bytes,
            }
        };

        MediaUploadProgressInfo { index: index as u64, bytes, offsets }
    }

    /// Determine the size of a pending file upload, if this is a thumbnail
    /// upload or return 0 otherwise.
    async fn get_dependent_pending_file_upload_size(
        txn_id: &TransactionId,
        room: &Room,
    ) -> Result<Option<usize>, RoomSendQueueStorageError> {
        let client = room.client();
        let dependent_requests =
            client.state_store().load_dependent_queued_requests(room.room_id()).await?;

        // Try to find a depending request which depends on the target one, and that's a
        // media upload.
        let Some((cache_key, parent_is_thumbnail_upload)) =
            dependent_requests.into_iter().find_map(|r| {
                if r.parent_transaction_id != txn_id {
                    return None;
                }

                if let DependentQueuedRequestKind::UploadFileOrThumbnail {
                    cache_key,
                    parent_is_thumbnail_upload,
                    ..
                } = r.kind
                {
                    Some((cache_key, parent_is_thumbnail_upload))
                } else {
                    None
                }
            })
        else {
            // If there's none, we're done here.
            return Ok(None);
        };

        // If this is not a thumbnail upload, we're uploading a gallery and the
        // dependent request is for the next gallery item.
        if !parent_is_thumbnail_upload {
            return Ok(None);
        }

        let media_store_guard = client.media_store().lock().await?;

        let maybe_content = media_store_guard.get_media_content(&cache_key).await?;

        Ok(maybe_content.map(|c| c.len()))
    }

    /// Create an observable to watch a media's upload progress.
    pub(super) fn create_media_upload_progress_observable(
        media_upload_info: &MediaUploadProgressInfo,
        related_txn_id: &TransactionId,
        update_sender: &broadcast::Sender<RoomSendQueueUpdate>,
    ) -> SharedObservable<TransmissionProgress> {
        let progress: SharedObservable<TransmissionProgress> = Default::default();
        let mut subscriber = progress.subscribe();

        let related_txn_id = related_txn_id.to_owned();
        let update_sender = update_sender.clone();
        let media_upload_info = *media_upload_info;

        // Watch and communicate the progress on a detached background task. Once
        // the progress observable is dropped, next() will return None and the
        // task will end.
        spawn(async move {
            while let Some(progress) = subscriber.next().await {
                // Purposefully don't use `send_update` here, because we don't want to notify
                // the global listeners about an upload progress update.
                let _ = update_sender.send(RoomSendQueueUpdate::MediaUpload {
                    related_to: related_txn_id.clone(),
                    file: None,
                    index: media_upload_info.index,
                    progress: estimate_media_upload_progress(progress, media_upload_info.bytes)
                        + media_upload_info.offsets,
                });
            }
        });

        progress
    }
}

/// Estimates the upload progress for a single media file (either a thumbnail or
/// a file).
///
/// This proportionally maps the upload progress given in actual bytes sent
/// (possibly after encryption) into units of the unencrypted file sizes.
///
/// # Arguments
///
/// * `progress` - The [`TransmissionProgress`] of uploading the file (possibly
///   after encryption).
///
/// * `bytes` - The total number of bytes in the file before encryption.
fn estimate_media_upload_progress(
    progress: TransmissionProgress,
    bytes: usize,
) -> AbstractProgress {
    if progress.total == 0 {
        return AbstractProgress { current: 0, total: 0 };
    }

    // Did the file finish uploading?
    if progress.current == progress.total {
        return AbstractProgress { current: bytes, total: bytes };
    }

    // The file is still uploading. Use the rule of 3 to proportionally map the
    // progress into units of the original file size.
    AbstractProgress {
        current: (progress.current as f64 / progress.total as f64 * bytes as f64).round() as usize,
        total: bytes,
    }
}
