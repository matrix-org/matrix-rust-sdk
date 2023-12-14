//! Common types for [media content](https://matrix.org/docs/spec/client_server/r0.6.1#id66).

use ruma::{
    api::client::media::get_content_thumbnail::v3::Method,
    events::{
        room::{
            message::{
                AudioMessageEventContent, FileMessageEventContent, ImageMessageEventContent,
                LocationMessageEventContent, VideoMessageEventContent,
            },
            MediaSource,
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
        format!("{}{UNIQUE_SEPARATOR}{}x{}", self.method, self.width, self.height)
    }
}

impl UniqueKey for MediaSource {
    fn unique_key(&self) -> String {
        match self {
            Self::Plain(uri) => uri.to_string(),
            Self::Encrypted(file) => file.url.to_string(),
        }
    }
}

/// A request for media data.
#[derive(Clone, Debug)]
pub struct MediaRequest {
    /// The source of the media file.
    pub source: MediaSource,

    /// The requested format of the media data.
    pub format: MediaFormat,
}

impl MediaRequest {
    /// Get the [`MxcUri`] from `Self`.
    pub fn uri(&self) -> &MxcUri {
        match &self.source {
            MediaSource::Plain(url) => url.as_ref(),
            MediaSource::Encrypted(file) => file.url.as_ref(),
        }
    }
}

impl UniqueKey for MediaRequest {
    fn unique_key(&self) -> String {
        format!("{}{UNIQUE_SEPARATOR}{}", self.source.unique_key(), self.format.unique_key())
    }
}

/// Trait for media event content.
pub trait MediaEventContent {
    /// Get the source of the file for `Self`.
    ///
    /// Returns `None` if `Self` has no file.
    fn source(&self) -> Option<MediaSource>;

    /// Get the source of the thumbnail for `Self`.
    ///
    /// Returns `None` if `Self` has no thumbnail.
    fn thumbnail_source(&self) -> Option<MediaSource>;
}

impl MediaEventContent for StickerEventContent {
    fn source(&self) -> Option<MediaSource> {
        Some(MediaSource::Plain(self.url.clone()))
    }

    fn thumbnail_source(&self) -> Option<MediaSource> {
        None
    }
}

impl MediaEventContent for AudioMessageEventContent {
    fn source(&self) -> Option<MediaSource> {
        Some(self.source.clone())
    }

    fn thumbnail_source(&self) -> Option<MediaSource> {
        None
    }
}

impl MediaEventContent for FileMessageEventContent {
    fn source(&self) -> Option<MediaSource> {
        Some(self.source.clone())
    }

    fn thumbnail_source(&self) -> Option<MediaSource> {
        self.info.as_ref()?.thumbnail_source.clone()
    }
}

impl MediaEventContent for ImageMessageEventContent {
    fn source(&self) -> Option<MediaSource> {
        Some(self.source.clone())
    }

    fn thumbnail_source(&self) -> Option<MediaSource> {
        self.info
            .as_ref()
            .and_then(|info| info.thumbnail_source.clone())
            .or_else(|| Some(self.source.clone()))
    }
}

impl MediaEventContent for VideoMessageEventContent {
    fn source(&self) -> Option<MediaSource> {
        Some(self.source.clone())
    }

    fn thumbnail_source(&self) -> Option<MediaSource> {
        self.info
            .as_ref()
            .and_then(|info| info.thumbnail_source.clone())
            .or_else(|| Some(self.source.clone()))
    }
}

impl MediaEventContent for LocationMessageEventContent {
    fn source(&self) -> Option<MediaSource> {
        None
    }

    fn thumbnail_source(&self) -> Option<MediaSource> {
        self.info.as_ref()?.thumbnail_source.clone()
    }
}

#[cfg(test)]
mod tests {
    use ruma::{
        events::room::{EncryptedFile, JsonWebKey},
        mxc_uri,
    };
    use serde_json::json;

    use super::*;

    #[test]
    fn test_media_request_url() {
        let mxc_uri = mxc_uri!("mxc://homeserver/media");

        let plain = MediaRequest {
            source: MediaSource::Plain(mxc_uri.to_owned()),
            format: MediaFormat::File,
        };

        assert_eq!(plain.uri(), mxc_uri);

        let file = MediaRequest {
            source: MediaSource::Encrypted(Box::new(
                serde_json::from_value(json!({
                    "url": mxc_uri,
                    "key": {
                        "kty": "oct",
                        "key_ops": ["encrypt", "decrypt"],
                        "alg": "A256CTR",
                        "k": "b50ACIv6LMn9AfMCFD1POJI_UAFWIclxAN1kWrEO2X8",
                        "ext": true,
                    },
                    "iv": "AK1wyzigZtQAAAABAAAAKK",
                    "hashes": {
                        "sha256": "foobar",
                    },
                    "v": "v2",
                }))
                .unwrap(),
            )),
            format: MediaFormat::File,
        };

        assert_eq!(file.uri(), mxc_uri);
    }
}
