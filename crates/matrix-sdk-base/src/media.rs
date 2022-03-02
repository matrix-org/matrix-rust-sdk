//! Common types for [media content](https://matrix.org/docs/spec/client_server/r0.6.1#id66).

use ruma::{
    api::client::media::get_content_thumbnail::v3::Method,
    events::{
        room::{
            message::{
                AudioMessageEventContent, FileMessageEventContent, ImageMessageEventContent,
                LocationMessageEventContent, VideoMessageEventContent,
            },
            EncryptedFile,
        },
        sticker::StickerEventContent,
    },
    MxcUri, UInt,
};

const UNIQUE_SEPARATOR: &str = "_";

/// A trait to uniquely identify values of the same type.
pub trait UniqueKey {
    /// A string that uniquely identifies `Self` compared to other values of
    /// the same type.
    fn unique_key(&self) -> String;
}

/// The requested format of a media file.
#[derive(Clone, Debug)]
pub enum MediaFormat {
    /// The file that was uploaded.
    File,

    /// A thumbnail of the file that was uploaded.
    Thumbnail(MediaThumbnailSize),
}

impl UniqueKey for MediaFormat {
    fn unique_key(&self) -> String {
        match self {
            Self::File => "file".into(),
            Self::Thumbnail(size) => size.unique_key(),
        }
    }
}

/// The requested size of a media thumbnail.
#[derive(Clone, Debug)]
pub struct MediaThumbnailSize {
    /// The desired resizing method.
    pub method: Method,

    /// The desired width of the thumbnail. The actual thumbnail may not match
    /// the size specified.
    pub width: UInt,

    /// The desired height of the thumbnail. The actual thumbnail may not match
    /// the size specified.
    pub height: UInt,
}

impl UniqueKey for MediaThumbnailSize {
    fn unique_key(&self) -> String {
        format!("{}{}{}x{}", self.method, UNIQUE_SEPARATOR, self.width, self.height)
    }
}

/// A request for media data.
#[derive(Clone, Debug)]
pub enum MediaType {
    /// A media content URI.
    Uri(Box<MxcUri>),

    /// An encrypted media content.
    Encrypted(Box<EncryptedFile>),
}

impl UniqueKey for MediaType {
    fn unique_key(&self) -> String {
        match self {
            Self::Uri(uri) => uri.to_string(),
            Self::Encrypted(file) => file.url.to_string(),
        }
    }
}

/// A request for media data.
#[derive(Clone, Debug)]
pub struct MediaRequest {
    /// The type of the media file.
    pub media_type: MediaType,

    /// The requested format of the media data.
    pub format: MediaFormat,
}

impl UniqueKey for MediaRequest {
    fn unique_key(&self) -> String {
        format!("{}{}{}", self.media_type.unique_key(), UNIQUE_SEPARATOR, self.format.unique_key())
    }
}

/// Trait for media event content.
pub trait MediaEventContent {
    /// Get the type of the file for `Self`.
    ///
    /// Returns `None` if `Self` has no file.
    fn file(&self) -> Option<MediaType>;

    /// Get the type of the thumbnail for `Self`.
    ///
    /// Returns `None` if `Self` has no thumbnail.
    fn thumbnail(&self) -> Option<MediaType>;
}

impl MediaEventContent for StickerEventContent {
    fn file(&self) -> Option<MediaType> {
        Some(MediaType::Uri(self.url.clone()))
    }

    fn thumbnail(&self) -> Option<MediaType> {
        None
    }
}

impl MediaEventContent for AudioMessageEventContent {
    fn file(&self) -> Option<MediaType> {
        self.file
            .as_ref()
            .map(|e| MediaType::Encrypted(e.clone()))
            .or_else(|| self.url.as_ref().map(|uri| MediaType::Uri(uri.clone())))
    }

    fn thumbnail(&self) -> Option<MediaType> {
        None
    }
}

impl MediaEventContent for FileMessageEventContent {
    fn file(&self) -> Option<MediaType> {
        self.file
            .as_ref()
            .map(|e| MediaType::Encrypted(e.clone()))
            .or_else(|| self.url.as_ref().map(|uri| MediaType::Uri(uri.clone())))
    }

    fn thumbnail(&self) -> Option<MediaType> {
        self.info.as_ref().and_then(|info| {
            info.thumbnail_file
                .as_ref()
                .map(|file| MediaType::Encrypted(file.clone()))
                .or_else(|| info.thumbnail_url.as_ref().map(|uri| MediaType::Uri(uri.clone())))
        })
    }
}

impl MediaEventContent for ImageMessageEventContent {
    fn file(&self) -> Option<MediaType> {
        self.file
            .as_ref()
            .map(|e| MediaType::Encrypted(e.clone()))
            .or_else(|| self.url.as_ref().map(|uri| MediaType::Uri(uri.clone())))
    }

    fn thumbnail(&self) -> Option<MediaType> {
        self.info
            .as_ref()
            .and_then(|info| {
                info.thumbnail_file
                    .as_ref()
                    .map(|file| MediaType::Encrypted(file.clone()))
                    .or_else(|| info.thumbnail_url.as_ref().map(|uri| MediaType::Uri(uri.clone())))
            })
            .or_else(|| self.url.as_ref().map(|uri| MediaType::Uri(uri.clone())))
    }
}

impl MediaEventContent for VideoMessageEventContent {
    fn file(&self) -> Option<MediaType> {
        self.file
            .as_ref()
            .map(|e| MediaType::Encrypted(e.clone()))
            .or_else(|| self.url.as_ref().map(|uri| MediaType::Uri(uri.clone())))
    }

    fn thumbnail(&self) -> Option<MediaType> {
        self.info
            .as_ref()
            .and_then(|info| {
                info.thumbnail_file
                    .as_ref()
                    .map(|file| MediaType::Encrypted(file.clone()))
                    .or_else(|| info.thumbnail_url.as_ref().map(|uri| MediaType::Uri(uri.clone())))
            })
            .or_else(|| self.url.as_ref().map(|uri| MediaType::Uri(uri.clone())))
    }
}

impl MediaEventContent for LocationMessageEventContent {
    fn file(&self) -> Option<MediaType> {
        None
    }

    fn thumbnail(&self) -> Option<MediaType> {
        self.info.as_ref().and_then(|info| {
            info.thumbnail_file
                .as_ref()
                .map(|file| MediaType::Encrypted(file.clone()))
                .or_else(|| info.thumbnail_url.as_ref().map(|uri| MediaType::Uri(uri.clone())))
        })
    }
}

#[cfg(all(feature = "encryption", test))]
pub(crate) mod test {
    use ruma::events::room::{
        message::{FileInfo, LocationInfo, VideoInfo},
        ImageInfo,
    };

    use super::*;

    fn encrypted_test_data() -> (Box<MxcUri>, EncryptedFile) {
        let c = &mut std::io::Cursor::new("some content");
        let reader = crate::crypto::AttachmentEncryptor::new(c);

        let keys = reader.finish();
        let file: EncryptedFile = ruma::events::room::EncryptedFileInit {
            url: "foobar".into(),
            key: keys.web_key,
            iv: keys.iv,
            hashes: keys.hashes,
            v: keys.version,
        }
        .into();
        let url = file.url.clone();
        (url, file)
    }

    #[test]
    fn test_audio_content_prefer_crypt_type() {
        let (u, f) = encrypted_test_data();
        let mut c = AudioMessageEventContent::encrypted("foo".to_owned(), f);
        c.url = Some(u);
        assert!(matches!(c.file(), Some(MediaType::Encrypted(_))));
    }

    #[test]
    fn test_file_content_prefer_crypt_type() {
        let (u, f) = encrypted_test_data();
        let mut c = FileMessageEventContent::encrypted("foo".to_owned(), f.clone());
        c.url = Some(u.clone());
        let mut info = FileInfo::default();
        info.thumbnail_url = Some(u);
        info.thumbnail_file = Some(Box::new(f));
        c.info = Some(Box::new(info));
        assert!(matches!(c.file(), Some(MediaType::Encrypted(_))));
        assert!(matches!(c.thumbnail(), Some(MediaType::Encrypted(_))));
    }

    #[test]
    fn test_image_content_prefer_crypt_type() {
        let (u, f) = encrypted_test_data();
        let mut c = ImageMessageEventContent::encrypted("foo".to_owned(), f.clone());
        c.url = Some(u.clone());
        let mut info = ImageInfo::default();
        info.thumbnail_url = Some(u);
        info.thumbnail_file = Some(Box::new(f));
        c.info = Some(Box::new(info));
        assert!(matches!(c.file(), Some(MediaType::Encrypted(_))));
        assert!(matches!(c.thumbnail(), Some(MediaType::Encrypted(_))));
    }

    #[test]
    fn test_video_content_prefer_crypt_type() {
        let (u, f) = encrypted_test_data();
        let mut c = VideoMessageEventContent::encrypted("foo".to_owned(), f.clone());
        c.url = Some(u.clone());
        let mut info = VideoInfo::default();
        info.thumbnail_url = Some(u);
        info.thumbnail_file = Some(Box::new(f));
        c.info = Some(Box::new(info));
        assert!(matches!(c.file(), Some(MediaType::Encrypted(_))));
        assert!(matches!(c.thumbnail(), Some(MediaType::Encrypted(_))));
    }

    #[test]
    fn test_location_content_prefer_crypt_type() {
        let (u, f) = encrypted_test_data();
        let mut c = LocationMessageEventContent::new("foo".to_owned(), "27,37".to_owned());
        let mut info = LocationInfo::default();
        info.thumbnail_url = Some(u);
        info.thumbnail_file = Some(Box::new(f));
        c.info = Some(Box::new(info));
        assert!(matches!(c.thumbnail(), Some(MediaType::Encrypted(_))));
    }
}
