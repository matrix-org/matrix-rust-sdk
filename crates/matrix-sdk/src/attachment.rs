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

#[cfg(feature = "image-proc")]
use std::io::{BufRead, Cursor, Seek};
use std::time::Duration;

#[cfg(feature = "image-proc")]
use image::GenericImageView;
#[cfg(feature = "image-proc")]
pub use image::ImageFormat;
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

#[cfg(feature = "image-proc")]
use crate::ImageError;

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
    #[cfg(feature = "image-proc")]
    pub(crate) generate_thumbnail: bool,
    #[cfg(feature = "image-proc")]
    pub(crate) thumbnail_size: Option<(u32, u32)>,
    #[cfg(feature = "image-proc")]
    pub(crate) thumbnail_format: ThumbnailFormat,
}

impl AttachmentConfig {
    /// Create a new default `AttachmentConfig` without providing a thumbnail.
    ///
    /// To provide a thumbnail use [`AttachmentConfig::with_thumbnail()`].
    pub fn new() -> Self {
        Self::default()
    }

    /// Generate the thumbnail to send for this media.
    ///
    /// Uses [`generate_image_thumbnail()`].
    ///
    /// Thumbnails can only be generated for supported image attachments. For
    /// more information, see the [image](https://github.com/image-rs/image)
    /// crate.
    ///
    /// If generating the thumbnail failed, the error will be logged and sending
    /// the attachment will proceed without a thumbnail.
    ///
    /// # Arguments
    ///
    /// * `size` - The size of the thumbnail in pixels as a `(width, height)`
    /// tuple. If set to `None`, defaults to `(800, 600)`.
    ///
    /// * `format` - The image format to use to encode the thumbnail.
    #[cfg(feature = "image-proc")]
    #[must_use]
    pub fn generate_thumbnail(mut self, size: Option<(u32, u32)>, format: ThumbnailFormat) -> Self {
        self.generate_thumbnail = true;
        self.thumbnail_size = size;
        self.thumbnail_format = format;
        self
    }

    /// Create a new default `AttachmentConfig` with a `thumbnail`.
    ///
    /// # Arguments
    ///
    /// * `thumbnail` - The thumbnail of the media. If the `content_type` does
    /// not support it (eg audio clips), it is ignored.
    ///
    /// To generate automatically a thumbnail from an image, use
    /// [`AttachmentConfig::new()`] and
    /// [`AttachmentConfig::generate_thumbnail()`].
    pub fn with_thumbnail(thumbnail: Thumbnail) -> Self {
        Self { thumbnail: Some(thumbnail), ..Default::default() }
    }

    /// Set the transaction ID to send.
    ///
    /// # Arguments
    ///
    /// * `txn_id` - A unique ID that can be attached to a `MessageEvent` held
    /// in its unsigned field as `transaction_id`. If not given, one is created
    /// for the message.
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
    /// doesn't match the `content_type`, it is ignored.
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

/// Generate a thumbnail for an image.
///
/// This is a convenience method that uses the
/// [image](https://github.com/image-rs/image) crate.
///
/// # Arguments
/// * `content_type` - The type of the media, this will be used as the
///   content-type header.
///
/// * `reader` - A `Reader` that will be used to fetch the raw bytes of the
///   media.
///
/// * `size` - The size of the thumbnail in pixels as a `(width, height)` tuple.
///   If set to `None`, defaults to `(800, 600)`.
///
/// * `format` - The image format to use to encode the thumbnail.
///
/// # Examples
///
/// ```no_run
/// use std::{io::Cursor, path::PathBuf};
///
/// use matrix_sdk::attachment::{
///     generate_image_thumbnail, AttachmentConfig, Thumbnail, ThumbnailFormat,
/// };
/// use mime;
/// # use matrix_sdk::{Client, ruma::room_id };
/// # use url::Url;
/// #
/// # async {
/// # let homeserver = Url::parse("http://localhost:8080")?;
/// # let mut client = Client::new(homeserver).await?;
/// # let room_id = room_id!("!test:localhost");
/// let path = PathBuf::from("/home/example/my-cat.jpg");
/// let image = tokio::fs::read(path).await?;
///
/// let cursor = Cursor::new(&image);
/// let thumbnail = generate_image_thumbnail(
///     &mime::IMAGE_JPEG,
///     cursor,
///     None,
///     ThumbnailFormat::Original,
/// )?;
/// let config = AttachmentConfig::with_thumbnail(thumbnail);
///
/// if let Some(room) = client.get_room(&room_id) {
///     room.send_attachment(
///         "my_favorite_cat.jpg",
///         &mime::IMAGE_JPEG,
///         image,
///         config,
///     )
///     .await?;
/// }
/// # anyhow::Ok(()) };
/// ```
#[cfg(feature = "image-proc")]
pub fn generate_image_thumbnail<R: BufRead + Seek>(
    content_type: &mime::Mime,
    reader: R,
    size: Option<(u32, u32)>,
    format: ThumbnailFormat,
) -> Result<Thumbnail, ImageError> {
    use std::str::FromStr;

    let Some(image_format) = ImageFormat::from_mime_type(content_type) else {
        return Err(ImageError::FormatNotSupported);
    };

    let image = image::load(reader, image_format)?;
    let (original_width, original_height) = image.dimensions();

    let (width, height) = size.unwrap_or((800, 600));

    // Don't generate a thumbnail if it would be bigger than or equal to the
    // original.
    if height >= original_height && width >= original_width {
        return Err(ImageError::ThumbnailBiggerThanOriginal);
    }

    let thumbnail = image.thumbnail(width, height);
    let (thumbnail_width, thumbnail_height) = thumbnail.dimensions();

    let thumbnail_format = match format {
        ThumbnailFormat::Always(format) => format,
        ThumbnailFormat::Fallback(format) if !image_format.writing_enabled() => format,
        ThumbnailFormat::Fallback(_) | ThumbnailFormat::Original => image_format,
    };

    let mut data: Vec<u8> = vec![];
    thumbnail.write_to(&mut Cursor::new(&mut data), thumbnail_format)?;
    let data_size = data.len() as u32;

    let content_type = mime::Mime::from_str(thumbnail_format.to_mime_type())?;

    let info = BaseThumbnailInfo {
        width: Some(thumbnail_width.into()),
        height: Some(thumbnail_height.into()),
        size: Some(data_size.into()),
    };

    Ok(Thumbnail { data, content_type, info: Some(info) })
}

/// The format to use for encoding the thumbnail.
#[cfg(feature = "image-proc")]
#[derive(Debug, Default, Clone, Copy)]
pub enum ThumbnailFormat {
    /// Always use this format.
    ///
    /// Will always return an error if this format is not writable by the
    /// `image` crate.
    Always(ImageFormat),
    /// Try to use the same format as the original image, and fallback to this
    /// one if the original format is not writable.
    ///
    /// Will return an error if both the original format and this format are not
    /// writable by the `image` crate.
    Fallback(ImageFormat),
    /// Only try to use the format of the original image.
    ///
    /// Will return an error if the original format is not writable by the
    /// `image` crate.
    ///
    /// This is the default.
    #[default]
    Original,
}
