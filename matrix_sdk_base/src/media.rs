//! Common types for [media content](https://matrix.org/docs/spec/client_server/r0.6.1#id66).

use matrix_sdk_common::{
    api::r0::media::get_content_thumbnail::Method, events::room::EncryptedFile,
    identifiers::MxcUri, UInt,
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
    Uri(MxcUri),

    /// An encrypted media content.
    Encrypted(EncryptedFile),
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
