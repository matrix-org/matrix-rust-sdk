// Copyright 2022 Kévin Commaille
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

//! Types and traits for attachments.

use std::time::Duration;

use ruma::{
    OwnedTransactionId, UInt, assign,
    events::{
        Mentions,
        room::{
            ImageInfo, ThumbnailInfo,
            message::{AudioInfo, FileInfo, TextMessageEventContent, VideoInfo},
        },
    },
};

use crate::room::reply::Reply;

/// Base metadata about an image.
#[derive(Debug, Clone, Default)]
pub struct BaseImageInfo {
    /// The height of the image in pixels.
    pub height: Option<UInt>,
    /// The width of the image in pixels.
    pub width: Option<UInt>,
    /// The file size of the image in bytes.
    pub size: Option<UInt>,
    /// The [BlurHash](https://blurha.sh/) for this image.
    pub blurhash: Option<String>,
    /// Whether this image is animated.
    pub is_animated: Option<bool>,
}

/// Base metadata about a video.
#[derive(Debug, Clone, Default)]
pub struct BaseVideoInfo {
    /// The duration of the video.
    pub duration: Option<Duration>,
    /// The height of the video in pixels.
    pub height: Option<UInt>,
    /// The width of the video in pixels.
    pub width: Option<UInt>,
    /// The file size of the video in bytes.
    pub size: Option<UInt>,
    /// The [BlurHash](https://blurha.sh/) for this video.
    pub blurhash: Option<String>,
}

/// Base metadata about an audio clip.
#[derive(Debug, Clone, Default)]
pub struct BaseAudioInfo {
    /// The duration of the audio clip.
    pub duration: Option<Duration>,
    /// The file size of the audio clip in bytes.
    pub size: Option<UInt>,
    /// The waveform of the audio clip.
    ///
    /// Must only include values between 0 and 1.
    pub waveform: Option<Vec<f32>>,
}

/// Base metadata about a file.
#[derive(Debug, Clone, Default)]
pub struct BaseFileInfo {
    /// The size of the file in bytes.
    pub size: Option<UInt>,
}

/// Types of metadata for an attachment.
#[derive(Debug)]
pub enum AttachmentInfo {
    /// The metadata of an image.
    Image(BaseImageInfo),
    /// The metadata of a video.
    Video(BaseVideoInfo),
    /// The metadata of an audio clip.
    Audio(BaseAudioInfo),
    /// The metadata of a file.
    File(BaseFileInfo),
    /// The metadata of a voice message
    Voice(BaseAudioInfo),
}

impl From<AttachmentInfo> for ImageInfo {
    fn from(info: AttachmentInfo) -> Self {
        match info {
            AttachmentInfo::Image(info) => assign!(ImageInfo::new(), {
                height: info.height,
                width: info.width,
                size: info.size,
                blurhash: info.blurhash,
                is_animated: info.is_animated,
            }),
            _ => ImageInfo::new(),
        }
    }
}

impl From<AttachmentInfo> for VideoInfo {
    fn from(info: AttachmentInfo) -> Self {
        match info {
            AttachmentInfo::Video(info) => assign!(VideoInfo::new(), {
                duration: info.duration,
                height: info.height,
                width: info.width,
                size: info.size,
                blurhash: info.blurhash,
            }),
            _ => VideoInfo::new(),
        }
    }
}

impl From<AttachmentInfo> for AudioInfo {
    fn from(info: AttachmentInfo) -> Self {
        match info {
            AttachmentInfo::Audio(info) | AttachmentInfo::Voice(info) => {
                assign!(AudioInfo::new(), {
                    duration: info.duration,
                    size: info.size,
                })
            }
            _ => AudioInfo::new(),
        }
    }
}

impl From<AttachmentInfo> for FileInfo {
    fn from(info: AttachmentInfo) -> Self {
        match info {
            AttachmentInfo::File(info) => assign!(FileInfo::new(), {
                size: info.size,
            }),
            _ => FileInfo::new(),
        }
    }
}

/// A thumbnail to upload and send for an attachment.
#[derive(Debug)]
pub struct Thumbnail {
    /// The raw bytes of the thumbnail.
    pub data: Vec<u8>,
    /// The type of the thumbnail, this will be used as the content-type header.
    pub content_type: mime::Mime,
    /// The height of the thumbnail in pixels.
    pub height: UInt,
    /// The width of the thumbnail in pixels.
    pub width: UInt,
    /// The file size of the thumbnail in bytes.
    pub size: UInt,
}

impl Thumbnail {
    /// Convert this `Thumbnail` into a `(data, content_type, info)` tuple.
    pub fn into_parts(self) -> (Vec<u8>, mime::Mime, Box<ThumbnailInfo>) {
        let thumbnail_info = assign!(ThumbnailInfo::new(), {
            height: Some(self.height),
            width: Some(self.width),
            size: Some(self.size),
            mimetype: Some(self.content_type.to_string())
        });
        (self.data, self.content_type, Box::new(thumbnail_info))
    }
}

/// Configuration for sending an attachment.
#[derive(Debug, Default)]
pub struct AttachmentConfig {
    /// A fixed transaction id to be used for sending this attachment.
    ///
    /// Otherwise, a random one will be generated.
    pub txn_id: Option<OwnedTransactionId>,

    /// Type-specific metadata about the attachment.
    pub info: Option<AttachmentInfo>,

    /// An optional thumbnail to send with the attachment.
    pub thumbnail: Option<Thumbnail>,

    /// An optional caption for the attachment.
    pub caption: Option<TextMessageEventContent>,

    /// Intentional mentions to be included in the media event.
    pub mentions: Option<Mentions>,

    /// Reply parameters for the attachment (replied-to event and thread-related
    /// metadata).
    pub reply: Option<Reply>,
}

impl AttachmentConfig {
    /// Create a new empty `AttachmentConfig`.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the thumbnail to send.
    ///
    /// # Arguments
    ///
    /// * `thumbnail` - The thumbnail of the media. If the `content_type` does
    ///   not support it (e.g. audio clips), it is ignored.
    #[must_use]
    pub fn thumbnail(mut self, thumbnail: Option<Thumbnail>) -> Self {
        self.thumbnail = thumbnail;
        self
    }

    /// Set the transaction ID to send.
    ///
    /// # Arguments
    ///
    /// * `txn_id` - A unique ID that can be attached to a `MessageEvent` held
    ///   in its unsigned field as `transaction_id`. If not given, one is
    ///   created for the message.
    #[must_use]
    pub fn txn_id(mut self, txn_id: OwnedTransactionId) -> Self {
        self.txn_id = Some(txn_id);
        self
    }

    /// Set the media metadata to send.
    ///
    /// # Arguments
    ///
    /// * `info` - The metadata of the media. If the `AttachmentInfo` type
    ///   doesn't match the `content_type`, it is ignored.
    #[must_use]
    pub fn info(mut self, info: AttachmentInfo) -> Self {
        self.info = Some(info);
        self
    }

    /// Set the optional caption.
    ///
    /// # Arguments
    ///
    /// * `caption` - The optional caption.
    pub fn caption(mut self, caption: Option<TextMessageEventContent>) -> Self {
        self.caption = caption;
        self
    }

    /// Set the mentions of the message.
    ///
    /// # Arguments
    ///
    /// * `mentions` - The mentions of the message.
    pub fn mentions(mut self, mentions: Option<Mentions>) -> Self {
        self.mentions = mentions;
        self
    }

    /// Set the reply information of the message.
    ///
    /// # Arguments
    ///
    /// * `reply` - The reply information of the message.
    pub fn reply(mut self, reply: Option<Reply>) -> Self {
        self.reply = reply;
        self
    }
}

/// Configuration for sending a gallery.
#[cfg(feature = "unstable-msc4274")]
#[derive(Debug, Default)]
pub struct GalleryConfig {
    pub(crate) txn_id: Option<OwnedTransactionId>,
    pub(crate) items: Vec<GalleryItemInfo>,
    pub(crate) caption: Option<TextMessageEventContent>,
    pub(crate) mentions: Option<Mentions>,
    pub(crate) reply: Option<Reply>,
}

#[cfg(feature = "unstable-msc4274")]
impl GalleryConfig {
    /// Create a new empty `GalleryConfig`.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the transaction ID to send.
    ///
    /// # Arguments
    ///
    /// * `txn_id` - A unique ID that can be attached to a `MessageEvent` held
    ///   in its unsigned field as `transaction_id`. If not given, one is
    ///   created for the message.
    #[must_use]
    pub fn txn_id(mut self, txn_id: OwnedTransactionId) -> Self {
        self.txn_id = Some(txn_id);
        self
    }

    /// Adds a media item to the gallery.
    ///
    /// # Arguments
    ///
    /// * `item` - Information about the item to be added.
    #[must_use]
    pub fn add_item(mut self, item: GalleryItemInfo) -> Self {
        self.items.push(item);
        self
    }

    /// Set the optional caption.
    ///
    /// # Arguments
    ///
    /// * `caption` - The optional caption.
    pub fn caption(mut self, caption: Option<TextMessageEventContent>) -> Self {
        self.caption = caption;
        self
    }

    /// Set the mentions of the message.
    ///
    /// # Arguments
    ///
    /// * `mentions` - The mentions of the message.
    pub fn mentions(mut self, mentions: Option<Mentions>) -> Self {
        self.mentions = mentions;
        self
    }

    /// Set the reply information of the message.
    ///
    /// # Arguments
    ///
    /// * `reply` - The reply information of the message.
    pub fn reply(mut self, reply: Option<Reply>) -> Self {
        self.reply = reply;
        self
    }

    /// Returns the number of media items in the gallery.
    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// Checks whether the gallery contains any media items or not.
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
}

#[cfg(feature = "unstable-msc4274")]
#[derive(Debug)]
/// Metadata for a gallery item
pub struct GalleryItemInfo {
    /// The filename.
    pub filename: String,
    /// The mime type.
    pub content_type: mime::Mime,
    /// The binary data.
    pub data: Vec<u8>,
    /// The attachment info.
    pub attachment_info: AttachmentInfo,
    /// The caption.
    pub caption: Option<TextMessageEventContent>,
    /// The thumbnail.
    pub thumbnail: Option<Thumbnail>,
}
