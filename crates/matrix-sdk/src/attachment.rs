use std::io::Read;
#[cfg(feature = "image_proc")]
use std::io::{BufRead, Seek};

#[cfg(feature = "image_proc")]
use image::GenericImageView;
use ruma::{
    assign,
    events::room::{
        message::{AudioInfo, FileInfo, VideoInfo},
        ImageInfo, ThumbnailInfo,
    },
    UInt,
};

#[cfg(feature = "image_proc")]
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

/// Generate a thumbnail for an image.
///
/// This is a convenience method that uses the
/// [image](https://github.com/image-rs/image) crate.
///
/// # Arguments
/// * `content_type` - The type of the media, this will be used as the
/// content-type header.
///
/// * `reader` - A `Reader` that will be used to fetch the raw bytes of the
/// media.
///
/// * `size` - The size of the thumbnail in pixels as a `(width, height)` tuple.
/// If set to `None`, defaults to `(800, 600)`.
///
/// # Examples
///
/// ```no_run
/// # use std::{path::PathBuf, fs::File, io::{BufReader, Read, Seek}};
/// # use matrix_sdk::{Client, attachment::{Thumbnail, generate_image_thumbnail}, ruma::room_id};
/// # use url::Url;
/// # use mime;
/// # use futures::executor::block_on;
/// # block_on(async {
/// # let homeserver = Url::parse("http://localhost:8080")?;
/// # let mut client = Client::new(homeserver)?;
/// # let room_id = room_id!("!test:localhost");
/// let path = PathBuf::from("/home/example/my-cat.jpg");
/// let mut image = BufReader::new(File::open(path)?);
///
/// let (thumbnail_data, thumbnail_info) = generate_image_thumbnail(
///     &mime::IMAGE_JPEG,
///     &mut image,
///     None
/// )?;
/// let thumbnail = Thumbnail {
///     reader: &mut thumbnail_data.as_slice(),
///     content_type: &mime::IMAGE_JPEG,
///     info: Some(thumbnail_info),
/// };
///
/// image.rewind()?;
///
/// if let Some(room) = client.get_joined_room(&room_id) {
///     room.send_attachment(
///         "My favorite cat",
///         &mime::IMAGE_JPEG,
///         &mut image,
///         None,
///         Some(thumbnail),
///         None,
///     ).await?;
/// }
/// # Result::<_, matrix_sdk::Error>::Ok(()) });
/// ```
#[cfg(feature = "image_proc")]
pub fn generate_image_thumbnail<R: BufRead + Seek>(
    content_type: &mime::Mime,
    reader: &mut R,
    size: Option<(u32, u32)>,
) -> Result<(Vec<u8>, BaseThumbnailInfo), ImageError> {
    let image_format = image_format_from_mime_type(content_type);
    if image_format.is_none() {
        return Err(ImageError::FormatNotSupported);
    }

    let image_format = image_format.unwrap();

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

    let mut data: Vec<u8> = vec![];
    thumbnail.write_to(&mut data, image_format)?;
    let data_size = data.len() as u32;

    Ok((
        data,
        BaseThumbnailInfo {
            width: Some(thumbnail_width.into()),
            height: Some(thumbnail_height.into()),
            size: Some(data_size.into()),
        },
    ))
}

// FIXME: Replace this method by ImageFormat::from_mime_type after "image"
// crate's next release.
/// Return the image format specified by a MIME type.
#[cfg(feature = "image_proc")]
fn image_format_from_mime_type<M>(mime_type: M) -> Option<image::ImageFormat>
where
    M: AsRef<str>,
{
    match mime_type.as_ref() {
        "image/avif" => Some(image::ImageFormat::Avif),
        "image/jpeg" => Some(image::ImageFormat::Jpeg),
        "image/png" => Some(image::ImageFormat::Png),
        "image/gif" => Some(image::ImageFormat::Gif),
        "image/webp" => Some(image::ImageFormat::WebP),
        "image/tiff" => Some(image::ImageFormat::Tiff),
        "image/x-targa" | "image/x-tga" => Some(image::ImageFormat::Tga),
        "image/vnd-ms.dds" => Some(image::ImageFormat::Dds),
        "image/bmp" => Some(image::ImageFormat::Bmp),
        "image/x-icon" => Some(image::ImageFormat::Ico),
        "image/vnd.radiance" => Some(image::ImageFormat::Hdr),
        "image/x-portable-bitmap"
        | "image/x-portable-graymap"
        | "image/x-portable-pixmap"
        | "image/x-portable-anymap" => Some(image::ImageFormat::Pnm),
        _ => None,
    }
}
