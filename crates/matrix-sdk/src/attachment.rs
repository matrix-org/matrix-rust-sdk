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
    assign,
    events::{
        room::{
            message::{AudioInfo, FileInfo, FormattedBody, VideoInfo},
            ImageInfo, ThumbnailInfo,
        },
        Mentions,
    },
    OwnedTransactionId, TransactionId, UInt,
};

/// Base metadata about an image.
#[derive(Debug, Clone)]
pub struct BaseImageInfo {
    /// The height of the image in pixels.
    pub height: Option<UInt>,
    /// The width of the image in pixels.
    pub width: Option<UInt>,
    /// The file size of the image in bytes.
    pub size: Option<UInt>,
    /// The [BlurHash](https://blurha.sh/) for this image.
    pub blurhash: Option<String>,
}

/// Base metadata about a video.
#[derive(Debug, Clone)]
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
#[derive(Debug, Clone)]
pub struct BaseAudioInfo {
    /// The duration of the audio clip.
    pub duration: Option<Duration>,
    /// The file size of the audio clip in bytes.
    pub size: Option<UInt>,
}

/// Base metadata about a file.
#[derive(Debug, Clone)]
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
    Voice {
        /// The audio info
        audio_info: BaseAudioInfo,
        /// The waveform of the voice message
        waveform: Option<Vec<u16>>,
    },
}

impl From<AttachmentInfo> for ImageInfo {
    fn from(info: AttachmentInfo) -> Self {
        match info {
            AttachmentInfo::Image(info) => assign!(ImageInfo::new(), {
                height: info.height,
                width: info.width,
                size: info.size,
                blurhash: info.blurhash,
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
            AttachmentInfo::Audio(info) => assign!(AudioInfo::new(), {
                duration: info.duration,
                size: info.size,
            }),
            AttachmentInfo::Voice { audio_info, .. } => assign!(AudioInfo::new(), {
                duration: audio_info.duration,
                size: audio_info.size,
            }),
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

#[derive(Debug, Clone)]
/// Base metadata about a thumbnail.
pub struct BaseThumbnailInfo {
    /// The height of the thumbnail in pixels.
    pub height: Option<UInt>,
    /// The width of the thumbnail in pixels.
    pub width: Option<UInt>,
    /// The file size of the thumbnail in bytes.
    pub size: Option<UInt>,
}

impl From<BaseThumbnailInfo> for ThumbnailInfo {
    fn from(info: BaseThumbnailInfo) -> Self {
        assign!(ThumbnailInfo::new(), {
            height: info.height,
            width: info.width,
            size: info.size,
        })
    }
}

/// A thumbnail to upload and send for an attachment.
#[derive(Debug)]
pub struct Thumbnail {
    /// The raw bytes of the thumbnail.
    pub data: Vec<u8>,
    /// The type of the thumbnail, this will be used as the content-type header.
    pub content_type: mime::Mime,
    /// The metadata of the thumbnail.
    pub info: Option<BaseThumbnailInfo>,
}

/// Configuration for sending an attachment.
#[derive(Debug, Default)]
pub struct AttachmentConfig {
    pub(crate) txn_id: Option<OwnedTransactionId>,
    pub(crate) info: Option<AttachmentInfo>,
    pub(crate) thumbnail: Option<Thumbnail>,
    pub(crate) caption: Option<String>,
    pub(crate) formatted_caption: Option<FormattedBody>,
    pub(crate) mentions: Option<Mentions>,
}

impl AttachmentConfig {
    /// Create a new default `AttachmentConfig` without providing a thumbnail.
    ///
    /// To provide a thumbnail use [`AttachmentConfig::with_thumbnail()`].
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a new default `AttachmentConfig` with a `thumbnail`.
    ///
    /// # Arguments
    ///
    /// * `thumbnail` - The thumbnail of the media. If the `content_type` does
    ///   not support it (eg audio clips), it is ignored.
    pub fn with_thumbnail(thumbnail: Thumbnail) -> Self {
        Self { thumbnail: Some(thumbnail), ..Default::default() }
    }

    /// Set the transaction ID to send.
    ///
    /// # Arguments
    ///
    /// * `txn_id` - A unique ID that can be attached to a `MessageEvent` held
    ///   in its unsigned field as `transaction_id`. If not given, one is
    ///   created for the message.
    #[must_use]
    pub fn txn_id(mut self, txn_id: &TransactionId) -> Self {
        self.txn_id = Some(txn_id.to_owned());
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

    /// Set the optional caption
    ///
    /// # Arguments
    ///
    /// * `caption` - The optional caption
    pub fn caption(mut self, caption: Option<String>) -> Self {
        self.caption = caption;
        self
    }

    /// Set the optional formatted caption
    ///
    /// # Arguments
    ///
    /// * `formatted_caption` - The optional formatted caption
    pub fn formatted_caption(mut self, formatted_caption: Option<FormattedBody>) -> Self {
        self.formatted_caption = formatted_caption;
        self
    }

    /// Set the mentions of the message.
    ///
    /// # Arguments
    ///
    /// * `mentions` - The mentions of the message
    pub fn mentions(mut self, mentions: Option<Mentions>) -> Self {
        self.mentions = mentions;
        self
    }
}
