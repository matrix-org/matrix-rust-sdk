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

//! All data types related to the send queue.

use std::{collections::BTreeMap, fmt, ops::Deref};

use as_variant::as_variant;
use ruma::{
    MilliSecondsSinceUnixEpoch, OwnedDeviceId, OwnedEventId, OwnedTransactionId, OwnedUserId,
    TransactionId, UInt,
    events::{
        AnyMessageLikeEventContent, MessageLikeEventContent as _, RawExt as _,
        room::{MediaSource, message::RoomMessageEventContent},
    },
    serde::Raw,
};
use serde::{Deserialize, Serialize};

use crate::media::MediaRequestParameters;

/// A thin wrapper to serialize a `AnyMessageLikeEventContent`.
#[derive(Clone, Serialize, Deserialize)]
pub struct SerializableEventContent {
    event: Raw<AnyMessageLikeEventContent>,
    event_type: String,
}

#[cfg(not(tarpaulin_include))]
impl fmt::Debug for SerializableEventContent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Don't include the event in the debug display.
        f.debug_struct("SerializedEventContent")
            .field("event_type", &self.event_type)
            .finish_non_exhaustive()
    }
}

impl SerializableEventContent {
    /// Create a [`SerializableEventContent`] from a raw
    /// [`AnyMessageLikeEventContent`] along with its type.
    pub fn from_raw(event: Raw<AnyMessageLikeEventContent>, event_type: String) -> Self {
        Self { event_type, event }
    }

    /// Create a [`SerializableEventContent`] from an
    /// [`AnyMessageLikeEventContent`].
    pub fn new(event: &AnyMessageLikeEventContent) -> Result<Self, serde_json::Error> {
        Ok(Self::from_raw(Raw::new(event)?, event.event_type().to_string()))
    }

    /// Convert a [`SerializableEventContent`] back into a
    /// [`AnyMessageLikeEventContent`].
    pub fn deserialize(&self) -> Result<AnyMessageLikeEventContent, serde_json::Error> {
        self.event.deserialize_with_type(&self.event_type)
    }

    /// Returns the raw event content along with its type, borrowed variant.
    ///
    /// Useful for callers manipulating custom events.
    pub fn raw(&self) -> (&Raw<AnyMessageLikeEventContent>, &str) {
        (&self.event, &self.event_type)
    }

    /// Returns the raw event content along with its type, owned variant.
    ///
    /// Useful for callers manipulating custom events.
    pub fn into_raw(self) -> (Raw<AnyMessageLikeEventContent>, String) {
        (self.event, self.event_type)
    }
}

/// The kind of a send queue request.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum QueuedRequestKind {
    /// An event to be sent via the send queue.
    Event {
        /// The content of the message-like event we'd like to send.
        content: SerializableEventContent,
    },

    /// Content to upload on the media server.
    ///
    /// The bytes must be stored in the media cache, and are identified by the
    /// cache key.
    MediaUpload {
        /// Content type of the media to be uploaded.
        ///
        /// Stored as a `String` because `Mime` which we'd really want to use
        /// here, is not serializable. Oh well.
        content_type: String,

        /// The cache key used to retrieve the media's bytes in the event cache
        /// store.
        cache_key: MediaRequestParameters,

        /// An optional media source for a thumbnail already uploaded.
        thumbnail_source: Option<MediaSource>,

        /// To which media event transaction does this upload relate?
        related_to: OwnedTransactionId,

        /// Accumulated list of infos for previously uploaded files and
        /// thumbnails if used during a gallery transaction. Otherwise empty.
        #[cfg(feature = "unstable-msc4274")]
        #[serde(default)]
        accumulated: Vec<AccumulatedSentMediaInfo>,
    },
}

impl From<SerializableEventContent> for QueuedRequestKind {
    fn from(content: SerializableEventContent) -> Self {
        Self::Event { content }
    }
}

/// A request to be sent with a send queue.
#[derive(Clone)]
pub struct QueuedRequest {
    /// The kind of queued request we're going to send.
    pub kind: QueuedRequestKind,

    /// Unique transaction id for the queued request, acting as a key.
    pub transaction_id: OwnedTransactionId,

    /// Error returned when the request couldn't be sent and is stuck in the
    /// unrecoverable state.
    ///
    /// `None` if the request is in the queue, waiting to be sent.
    pub error: Option<QueueWedgeError>,

    /// At which priority should this be handled?
    ///
    /// The bigger the value, the higher the priority at which this request
    /// should be handled.
    pub priority: usize,

    /// The time that the request was originally attempted.
    pub created_at: MilliSecondsSinceUnixEpoch,
}

impl QueuedRequest {
    /// Returns `Some` if the queued request is about sending an event.
    pub fn as_event(&self) -> Option<&SerializableEventContent> {
        as_variant!(&self.kind, QueuedRequestKind::Event { content } => content)
    }

    /// True if the request couldn't be sent because of an unrecoverable API
    /// error. See [`Self::error`] for more details on the reason.
    pub fn is_wedged(&self) -> bool {
        self.error.is_some()
    }
}

/// Represents a failed to send unrecoverable error of an event sent via the
/// send queue.
///
/// It is a serializable representation of a client error, see
/// `From` implementation for more details. These errors can not be
/// automatically retried, but yet some manual action can be taken before retry
/// sending. If not the only solution is to delete the local event.
#[derive(Clone, Debug, Serialize, Deserialize, thiserror::Error)]
pub enum QueueWedgeError {
    /// This error occurs when there are some insecure devices in the room, and
    /// the current encryption setting prohibits sharing with them.
    #[error("There are insecure devices in the room")]
    InsecureDevices {
        /// The insecure devices as a Map of userID to deviceID.
        user_device_map: BTreeMap<OwnedUserId, Vec<OwnedDeviceId>>,
    },

    /// This error occurs when a previously verified user is not anymore, and
    /// the current encryption setting prohibits sharing when it happens.
    #[error("Some users that were previously verified are not anymore")]
    IdentityViolations {
        /// The users that are expected to be verified but are not.
        users: Vec<OwnedUserId>,
    },

    /// It is required to set up cross-signing and properly verify the current
    /// session before sending.
    #[error("Own verification is required")]
    CrossVerificationRequired,

    /// Media content was cached in the media store, but has disappeared before
    /// we could upload it.
    #[error("Media content disappeared")]
    MissingMediaContent,

    /// We tried to upload some media content with an unknown mime type.
    #[error("Invalid mime type '{mime_type}' for media")]
    InvalidMimeType {
        /// The observed mime type that's expected to be invalid.
        mime_type: String,
    },

    /// Other errors.
    #[error("Other unrecoverable error: {msg}")]
    GenericApiError {
        /// Description of the error.
        msg: String,
    },
}

/// The specific user intent that characterizes a [`DependentQueuedRequest`].
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum DependentQueuedRequestKind {
    /// The event should be edited.
    EditEvent {
        /// The new event for the content.
        new_content: SerializableEventContent,
    },

    /// The event should be redacted/aborted/removed.
    RedactEvent,

    /// The event should be reacted to, with the given key.
    ReactEvent {
        /// Key used for the reaction.
        key: String,
    },

    /// Upload a file or thumbnail depending on another file or thumbnail
    /// upload.
    #[serde(alias = "UploadFileWithThumbnail")]
    UploadFileOrThumbnail {
        /// Content type for the file or thumbnail.
        content_type: String,

        /// Media request necessary to retrieve the file or thumbnail itself.
        cache_key: MediaRequestParameters,

        /// To which media transaction id does this upload relate to?
        related_to: OwnedTransactionId,

        /// Whether the depended upon request was a thumbnail or a file upload.
        #[serde(default = "default_parent_is_thumbnail_upload")]
        parent_is_thumbnail_upload: bool,
    },

    /// Finish an upload by updating references to the media cache and sending
    /// the final media event with the remote MXC URIs.
    FinishUpload {
        /// Local echo for the event (containing the local MXC URIs).
        ///
        /// `Box` the local echo so that it reduces the size of the whole enum.
        local_echo: Box<RoomMessageEventContent>,

        /// Transaction id for the file upload.
        file_upload: OwnedTransactionId,

        /// Information about the thumbnail, if present.
        thumbnail_info: Option<FinishUploadThumbnailInfo>,
    },

    /// Finish a gallery upload by updating references to the media cache and
    /// sending the final gallery event with the remote MXC URIs.
    #[cfg(feature = "unstable-msc4274")]
    FinishGallery {
        /// Local echo for the event (containing the local MXC URIs).
        ///
        /// `Box` the local echo so that it reduces the size of the whole enum.
        local_echo: Box<RoomMessageEventContent>,

        /// Metadata about the gallery items.
        item_infos: Vec<FinishGalleryItemInfo>,
    },
}

/// If parent_is_thumbnail_upload is missing, we assume the request is for a
/// file upload following a thumbnail upload. This was the only possible case
/// before parent_is_thumbnail_upload was introduced.
fn default_parent_is_thumbnail_upload() -> bool {
    true
}

/// Detailed record about a thumbnail used when finishing a media upload.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FinishUploadThumbnailInfo {
    /// Transaction id for the thumbnail upload.
    pub txn: OwnedTransactionId,
    /// Thumbnail's width.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<UInt>,
    /// Thumbnail's height.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<UInt>,
}

/// Detailed record about a file and thumbnail. When finishing a gallery
/// upload, one [`FinishGalleryItemInfo`] will be used for each media in the
/// gallery.
#[cfg(feature = "unstable-msc4274")]
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FinishGalleryItemInfo {
    /// Transaction id for the file upload.
    pub file_upload: OwnedTransactionId,
    /// Information about the thumbnail, if present.
    pub thumbnail_info: Option<FinishUploadThumbnailInfo>,
}

/// A transaction id identifying a [`DependentQueuedRequest`] rather than its
/// parent [`QueuedRequest`].
///
/// This thin wrapper adds some safety to some APIs, making it possible to
/// distinguish between the parent's `TransactionId` and the dependent event's
/// own `TransactionId`.
#[repr(transparent)]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ChildTransactionId(OwnedTransactionId);

impl ChildTransactionId {
    /// Returns a new [`ChildTransactionId`].
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        Self(TransactionId::new())
    }
}

impl Deref for ChildTransactionId {
    type Target = TransactionId;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl From<String> for ChildTransactionId {
    fn from(val: String) -> Self {
        Self(val.into())
    }
}

impl From<ChildTransactionId> for OwnedTransactionId {
    fn from(val: ChildTransactionId) -> Self {
        val.0
    }
}

impl From<OwnedTransactionId> for ChildTransactionId {
    fn from(val: OwnedTransactionId) -> Self {
        Self(val)
    }
}

/// Information about a media (and its thumbnail) that have been sent to a
/// homeserver.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SentMediaInfo {
    /// File that was uploaded by this request.
    ///
    /// If the request related to a thumbnail upload, this contains the
    /// thumbnail media source.
    pub file: MediaSource,

    /// Optional thumbnail previously uploaded, when uploading a file.
    ///
    /// When uploading a thumbnail, this is set to `None`.
    pub thumbnail: Option<MediaSource>,

    /// Accumulated list of infos for previously uploaded files and thumbnails
    /// if used during a gallery transaction. Otherwise empty.
    #[cfg(feature = "unstable-msc4274")]
    #[serde(default)]
    pub accumulated: Vec<AccumulatedSentMediaInfo>,
}

/// Accumulated information about a media (and its thumbnail) that have been
/// sent to a homeserver.
#[cfg(feature = "unstable-msc4274")]
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AccumulatedSentMediaInfo {
    /// File that was uploaded by this request.
    ///
    /// If the request related to a thumbnail upload, this contains the
    /// thumbnail media source.
    pub file: MediaSource,

    /// Optional thumbnail previously uploaded, when uploading a file.
    ///
    /// When uploading a thumbnail, this is set to `None`.
    pub thumbnail: Option<MediaSource>,
}

#[cfg(feature = "unstable-msc4274")]
impl From<AccumulatedSentMediaInfo> for SentMediaInfo {
    fn from(value: AccumulatedSentMediaInfo) -> Self {
        Self { file: value.file, thumbnail: value.thumbnail, accumulated: vec![] }
    }
}

/// A unique key (identifier) indicating that a transaction has been
/// successfully sent to the server.
///
/// The owning child transactions can now be resolved.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum SentRequestKey {
    /// The parent transaction returned an event when it succeeded.
    Event {
        /// The event ID returned by the server.
        event_id: OwnedEventId,

        /// The sent event.
        event: Raw<AnyMessageLikeEventContent>,

        /// The type of the sent event.
        event_type: String,
    },

    /// The parent transaction returned an uploaded resource URL.
    Media(SentMediaInfo),
}

impl SentRequestKey {
    /// Converts the current parent key into an event id, if possible.
    pub fn into_event_id(self) -> Option<OwnedEventId> {
        as_variant!(self, Self::Event { event_id, .. } => event_id)
    }

    /// Converts the current parent key into information about a sent media, if
    /// possible.
    pub fn into_media(self) -> Option<SentMediaInfo> {
        as_variant!(self, Self::Media)
    }
}

/// A request to be sent, depending on a [`QueuedRequest`] to be sent first.
///
/// Depending on whether the parent request has been sent or not, this will
/// either update the local echo in the storage, or materialize an equivalent
/// request implementing the user intent to the homeserver.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DependentQueuedRequest {
    /// Unique identifier for this dependent queued request.
    ///
    /// Useful for deletion.
    pub own_transaction_id: ChildTransactionId,

    /// The kind of user intent.
    pub kind: DependentQueuedRequestKind,

    /// Transaction id for the parent's local echo / used in the server request.
    ///
    /// Note: this is the transaction id used for the depended-on request, i.e.
    /// the one that was originally sent and that's being modified with this
    /// dependent request.
    pub parent_transaction_id: OwnedTransactionId,

    /// If the parent request has been sent, the parent's request identifier
    /// returned by the server once the local echo has been sent out.
    pub parent_key: Option<SentRequestKey>,

    /// The time that the request was originally attempted.
    pub created_at: MilliSecondsSinceUnixEpoch,
}

impl DependentQueuedRequest {
    /// Does the dependent request represent a new event that is *not*
    /// aggregated, aka it is going to be its own item in a timeline?
    pub fn is_own_event(&self) -> bool {
        match self.kind {
            DependentQueuedRequestKind::EditEvent { .. }
            | DependentQueuedRequestKind::RedactEvent
            | DependentQueuedRequestKind::ReactEvent { .. }
            | DependentQueuedRequestKind::UploadFileOrThumbnail { .. } => {
                // These are all aggregated events, or non-visible items (file upload producing
                // a new MXC ID).
                false
            }
            DependentQueuedRequestKind::FinishUpload { .. } => {
                // This one graduates into a new media event.
                true
            }
            #[cfg(feature = "unstable-msc4274")]
            DependentQueuedRequestKind::FinishGallery { .. } => {
                // This one graduates into a new gallery event.
                true
            }
        }
    }
}

#[cfg(not(tarpaulin_include))]
impl fmt::Debug for QueuedRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Hide the content from the debug log.
        f.debug_struct("QueuedRequest")
            .field("transaction_id", &self.transaction_id)
            .field("is_wedged", &self.is_wedged())
            .finish_non_exhaustive()
    }
}
