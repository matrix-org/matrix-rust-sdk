// Copyright 2025 Kévin Commaille
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

//! Media store and common types for [media content](https://matrix.org/docs/spec/client_server/r0.6.1#id66).

pub mod store;

use ruma::{
    MxcUri, UInt,
    api::client::media::get_content_thumbnail::v3::Method,
    events::{
        room::{
            MediaSource,
            message::{
                AudioMessageEventContent, FileMessageEventContent, ImageMessageEventContent,
                LocationMessageEventContent, VideoMessageEventContent,
            },
        },
        sticker::StickerEventContent,
    },
};
use serde::{Deserialize, Serialize};

const UNIQUE_SEPARATOR: &str = "_";

/// A trait to uniquely identify values of the same type.
pub trait UniqueKey {
    /// A string that uniquely identifies `Self` compared to other values of
    /// the same type.
    fn unique_key(&self) -> String;
}

/// The requested format of a media file.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum MediaFormat {
    /// The file that was uploaded.
    File,

    /// A thumbnail of the file that was uploaded.
    Thumbnail(MediaThumbnailSettings),
}

impl UniqueKey for MediaFormat {
    fn unique_key(&self) -> String {
        match self {
            Self::File => "file".into(),
            Self::Thumbnail(settings) => settings.unique_key(),
        }
    }
}

/// The desired settings of a media thumbnail.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MediaThumbnailSettings {
    /// The desired resizing method.
    pub method: Method,

    /// The desired width of the thumbnail. The actual thumbnail may not match
    /// the size specified.
    pub width: UInt,

    /// The desired height of the thumbnail. The actual thumbnail may not match
    /// the size specified.
    pub height: UInt,

    /// If we want to request an animated thumbnail from the homeserver.
    ///
    /// If it is `true`, the server should return an animated thumbnail if
    /// the media supports it.
    ///
    /// Defaults to `false`.
    pub animated: bool,
}

impl MediaThumbnailSettings {
    /// Constructs a new `MediaThumbnailSettings` with the given method, width
    /// and height.
    ///
    /// Requests a non-animated thumbnail by default.
    pub fn with_method(method: Method, width: UInt, height: UInt) -> Self {
        Self { method, width, height, animated: false }
    }

    /// Constructs a new `MediaThumbnailSettings` with the given width and
    /// height.
    ///
    /// Requests scaling, and a non-animated thumbnail.
    pub fn new(width: UInt, height: UInt) -> Self {
        Self { method: Method::Scale, width, height, animated: false }
    }
}

impl UniqueKey for MediaThumbnailSettings {
    fn unique_key(&self) -> String {
        let mut key = format!("{}{UNIQUE_SEPARATOR}{}x{}", self.method, self.width, self.height);

        if self.animated {
            key.push_str(UNIQUE_SEPARATOR);
            key.push_str("animated");
        }

        key
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

/// Parameters for a request for retrieve media data.
///
/// This is used as a key in the media cache too.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MediaRequestParameters {
    /// The source of the media file.
    pub source: MediaSource,

    /// The requested format of the media data.
    pub format: MediaFormat,
}

impl MediaRequestParameters {
    /// Get the [`MxcUri`] from `Self`.
    pub fn uri(&self) -> &MxcUri {
        match &self.source {
            MediaSource::Plain(url) => url.as_ref(),
            MediaSource::Encrypted(file) => file.url.as_ref(),
        }
    }
}

impl UniqueKey for MediaRequestParameters {
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
        Some(MediaSource::from(self.source.clone()))
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
    use ruma::mxc_uri;
    use serde_json::json;

    use super::*;

    #[test]
    fn test_media_request_url() {
        let mxc_uri = mxc_uri!("mxc://homeserver/media");

        let plain = MediaRequestParameters {
            source: MediaSource::Plain(mxc_uri.to_owned()),
            format: MediaFormat::File,
        };

        assert_eq!(plain.uri(), mxc_uri);

        let file = MediaRequestParameters {
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
