use std::io::Read;

use ruma::{
    assign,
    events::room::{
        message::{AudioInfo, FileInfo, VideoInfo},
        ImageInfo, ThumbnailInfo,
    },
    UInt,
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
    /// The duration of the video in milliseconds.
    pub duration: Option<UInt>,
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
    /// The duration of the audio clip in milliseconds.
    pub duration: Option<UInt>,
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
pub struct Thumbnail<'a, R: Read> {
    /// A `Reader` that will be used to fetch the raw bytes of the thumbnail.
    pub reader: &'a mut R,
    /// The type of the thumbnail, this will be used as the content-type header.
    pub content_type: &'a mime::Mime,
    /// The metadata of the thumbnail.
    pub info: Option<BaseThumbnailInfo>,
}

/// Typed `None` for an `<Option<Thumbnail>>`.
pub const NONE_THUMBNAIL: Option<Thumbnail<&[u8]>> = None;
