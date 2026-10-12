// Copyright 2026 The Matrix.org Foundation C.I.C.
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

//! Bindings for [image packs]. An image pack is a collection of custom
//! emoticons and stickers.
//!
//! [image packs]: https://spec.matrix.org/v1.19/client-server-api/#image-packs

use std::collections::{BTreeMap, HashMap, btree_map::Entry};

use matrix_sdk::room::image_packs::{
    AvailableImagePack as SdkAvailableImagePack, ImagePack as SdkImagePack,
    ImagePackSource as SdkImagePackSource,
};
use matrix_sdk_common::{SendOutsideWasm, SyncOutsideWasm};
use ruma::{
    OwnedMxcUri,
    events::{
        image_pack::rooms::ImagePackRoomsEventContent,
        room::{
            ImageInfo as RumaImageInfo,
            image_pack::{
                ImagePackImage as RumaImagePackImage, ImagePackMeta, PackUsage,
                RoomImagePackEventContent,
            },
        },
    },
};
use tracing::warn;

use crate::{error::ClientError, ruma::ImageInfo};

/// Whether the given string is a valid image pack shortcode, according to the
/// grammar in the spec.
#[matrix_sdk_ffi_macros::export]
pub fn is_valid_image_pack_shortcode(shortcode: String) -> bool {
    matrix_sdk::room::image_packs::is_valid_shortcode(&shortcode)
}

/// The intended usage of an image pack.
#[derive(Clone, uniffi::Enum)]
pub enum ImagePackUsage {
    /// The images are intended to be sent inline in messages.
    Emoticon,
    /// The images are intended to be sent as standalone sticker events.
    Sticker,
    /// An unknown usage.
    Custom { value: String },
}

impl From<&PackUsage> for ImagePackUsage {
    fn from(value: &PackUsage) -> Self {
        match value {
            PackUsage::Emoticon => Self::Emoticon,
            PackUsage::Sticker => Self::Sticker,
            other => Self::Custom { value: other.as_str().to_owned() },
        }
    }
}

impl From<ImagePackUsage> for PackUsage {
    fn from(value: ImagePackUsage) -> Self {
        match value {
            ImagePackUsage::Emoticon => Self::Emoticon,
            ImagePackUsage::Sticker => Self::Sticker,
            ImagePackUsage::Custom { value } => value.into(),
        }
    }
}

/// An image in an image pack.
#[derive(Clone, uniffi::Record)]
pub struct ImagePackImage {
    /// The shortcode of the image, which identifies it in the pack.
    pub shortcode: String,
    /// The `mxc://` URI of the image.
    pub url: String,
    /// A textual representation or description of the image.
    ///
    /// If absent, clients use the shortcode.
    pub body: Option<String>,
    /// The info of the image.
    pub info: Option<ImageInfo>,
}

/// The content of an image pack.
#[derive(Clone, uniffi::Record)]
pub struct ImagePackContent {
    /// The display name of the pack.
    ///
    /// If absent, clients use the name of the room of the pack.
    pub display_name: Option<String>,
    /// The `mxc://` URI of the avatar of the pack.
    ///
    /// If absent, clients use the avatar of the room of the pack.
    pub avatar_url: Option<String>,
    /// The intended usages of the pack.
    ///
    /// If empty, the pack has all usages.
    pub usage: Vec<ImagePackUsage>,
    /// An attribution for the pack, to credit the original author for
    /// example.
    pub attribution: Option<String>,
    /// The images of the pack.
    pub images: Vec<ImagePackImage>,
}

impl From<RoomImagePackEventContent> for ImagePackContent {
    fn from(value: RoomImagePackEventContent) -> Self {
        Self {
            display_name: value.pack.display_name,
            avatar_url: value.pack.avatar_url.map(|url| url.to_string()),
            usage: value.pack.usage.iter().map(Into::into).collect(),
            attribution: value.pack.attribution,
            images: value
                .images
                .into_iter()
                .map(|(shortcode, image)| ImagePackImage {
                    shortcode,
                    url: image.url.to_string(),
                    body: image.body,
                    info: image.info.as_ref().and_then(image_info_from_ruma),
                })
                .collect(),
        }
    }
}

/// Convert the info of an image to its FFI counterpart.
///
/// If the thumbnail source of the image is not valid, this function drops only
/// the thumbnail source. This keeps the rest of the info if the app sends the
/// pack back.
fn image_info_from_ruma(info: &RumaImageInfo) -> Option<ImageInfo> {
    ImageInfo::try_from(info)
        .or_else(|error| {
            warn!("Dropping invalid thumbnail source of image pack image: {error}");
            let mut info = info.clone();
            info.thumbnail_source = None;
            ImageInfo::try_from(&info)
        })
        .ok()
}

/// Parse an `mxc://` URI. Return an error if the URI is not valid.
fn parse_mxc_uri(uri: String) -> Result<OwnedMxcUri, ClientError> {
    let uri = OwnedMxcUri::from(uri);
    uri.validate().map_err(|error| {
        ClientError::from_str(format!("invalid MXC URI `{uri}`: {error}"), None)
    })?;
    Ok(uri)
}

impl TryFrom<ImagePackContent> for RoomImagePackEventContent {
    type Error = ClientError;

    fn try_from(value: ImagePackContent) -> Result<Self, Self::Error> {
        value.into_ruma(None)
    }
}

impl ImagePackContent {
    /// Convert this content to its Ruma counterpart.
    ///
    /// The FFI types do not model all the fields of the info of an image. They
    /// also drop thumbnail sources that are not valid. If `existing` is the
    /// current content of the pack, this method copies these fields from it for
    /// images with the same URL. This way, the new pack keeps the data that
    /// other clients wrote.
    pub(crate) fn into_ruma(
        self,
        existing: Option<&RoomImagePackEventContent>,
    ) -> Result<RoomImagePackEventContent, ClientError> {
        let mut images = BTreeMap::new();

        for image in self.images {
            let url = parse_mxc_uri(image.url)?;
            let existing_info = existing
                .and_then(|content| content.images.get(&image.shortcode))
                .filter(|existing| existing.url == url)
                .and_then(|existing| existing.info.as_ref());

            let mut ruma_image = RumaImagePackImage::new(url);
            ruma_image.body = image.body;
            ruma_image.info = image.info.map(|info| image_info_to_ruma(info, existing_info));

            match images.entry(image.shortcode) {
                Entry::Vacant(entry) => {
                    entry.insert(ruma_image);
                }
                Entry::Occupied(entry) => {
                    return Err(ClientError::from_str(
                        format!("duplicate image pack shortcode `{}`", entry.key()),
                        None,
                    ));
                }
            }
        }

        let mut pack = ImagePackMeta::new();
        pack.display_name = self.display_name;
        pack.avatar_url = self.avatar_url.map(parse_mxc_uri).transpose()?;
        pack.usage = self.usage.into_iter().map(Into::into).collect();
        pack.attribution = self.attribution;

        let mut content = RoomImagePackEventContent::new(images);
        content.pack = pack;
        Ok(content)
    }
}

/// Convert the info of an image from its FFI counterpart.
///
/// The FFI type cannot represent some fields. This function copies these fields
/// from the `existing` info of the image.
fn image_info_to_ruma(info: ImageInfo, existing: Option<&RumaImageInfo>) -> RumaImageInfo {
    let mut info = RumaImageInfo::from(info);

    if let Some(existing) = existing {
        info.thumbhash = existing.thumbhash.clone();

        // `image_info_from_ruma()` drops a thumbnail source that is not valid.
        // So the app did not see it and did not remove it on purpose.
        if info.thumbnail_source.is_none() && ImageInfo::try_from(existing).is_err() {
            info.thumbnail_source = existing.thumbnail_source.clone();
        }
    }

    info
}

/// An image pack defined in a room.
#[derive(Clone, uniffi::Record)]
pub struct ImagePack {
    /// The ID of the room where the pack is defined.
    pub room_id: String,
    /// The state key of the pack, which identifies it within the room.
    pub state_key: String,
    /// The content of the pack.
    pub content: ImagePackContent,
}

impl From<SdkImagePack> for ImagePack {
    fn from(value: SdkImagePack) -> Self {
        Self {
            room_id: value.room_id.to_string(),
            state_key: value.state_key,
            content: value.content.into(),
        }
    }
}

/// Where an available image pack comes from.
#[derive(Clone, uniffi::Enum)]
pub enum ImagePackSource {
    /// The pack is enabled globally by the user.
    Global,
    /// The pack is defined in the room itself.
    Room,
    /// The pack is defined in the canonical space hierarchy of the room.
    Space { space_id: String },
}

impl From<SdkImagePackSource> for ImagePackSource {
    fn from(value: SdkImagePackSource) -> Self {
        match value {
            SdkImagePackSource::Global => Self::Global,
            SdkImagePackSource::Room => Self::Room,
            SdkImagePackSource::Space { space_id } => {
                Self::Space { space_id: space_id.to_string() }
            }
        }
    }
}

/// An image pack available in a room, with its source.
#[derive(Clone, uniffi::Record)]
pub struct AvailableImagePack {
    /// The image pack.
    pub pack: ImagePack,
    /// Where the image pack comes from.
    pub source: ImagePackSource,
}

impl From<SdkAvailableImagePack> for AvailableImagePack {
    fn from(value: SdkAvailableImagePack) -> Self {
        Self { pack: value.pack.into(), source: value.source.into() }
    }
}

/// Convert the content of an `m.image_pack.rooms` event to a map of room ID to
/// the state keys of the globally enabled packs in that room.
pub(crate) fn image_pack_rooms_to_map(
    content: ImagePackRoomsEventContent,
) -> HashMap<String, Vec<String>> {
    content
        .rooms
        .into_iter()
        .map(|(room_id, packs)| (room_id.to_string(), packs.into_keys().collect()))
        .collect()
}

/// A listener for changes of the globally enabled image packs.
#[matrix_sdk_ffi_macros::export(callback_interface)]
pub trait ImagePackRoomsListener: SyncOutsideWasm + SendOutsideWasm {
    /// Called with a map of room ID to the state keys of the globally enabled
    /// packs in that room.
    fn on_change(&self, rooms: HashMap<String, Vec<String>>);
}

#[cfg(test)]
mod tests {
    use ruma::{
        events::room::{
            ImageInfo as RumaImageInfo, MediaSource,
            image_pack::{ImagePackImage as RumaImagePackImage, RoomImagePackEventContent},
        },
        owned_mxc_uri,
    };

    use super::{ImagePackContent, ImagePackImage};
    use crate::ruma::ImageInfo;

    fn image(shortcode: &str, url: &str) -> ImagePackImage {
        ImagePackImage {
            shortcode: shortcode.to_owned(),
            url: url.to_owned(),
            body: None,
            info: None,
        }
    }

    fn content(images: Vec<ImagePackImage>, avatar_url: Option<&str>) -> ImagePackContent {
        ImagePackContent {
            display_name: Some("Cats".to_owned()),
            avatar_url: avatar_url.map(ToOwned::to_owned),
            usage: vec![],
            attribution: None,
            images,
        }
    }

    #[test]
    fn test_image_pack_content_to_ruma() {
        let ruma_content = RoomImagePackEventContent::try_from(content(
            vec![image("cat", "mxc://example.org/cat"), image("dog", "mxc://example.org/dog")],
            Some("mxc://example.org/avatar"),
        ))
        .unwrap();

        assert_eq!(ruma_content.images.len(), 2);
        assert_eq!(ruma_content.images["cat"].url, "mxc://example.org/cat");
        assert_eq!(ruma_content.pack.display_name.as_deref(), Some("Cats"));
        assert_eq!(ruma_content.pack.avatar_url.unwrap(), "mxc://example.org/avatar");
    }

    #[test]
    fn test_image_pack_content_rejects_duplicate_shortcodes() {
        let result = RoomImagePackEventContent::try_from(content(
            vec![image("cat", "mxc://example.org/cat"), image("cat", "mxc://example.org/cat2")],
            None,
        ));
        assert!(result.is_err());
    }

    #[test]
    fn test_image_pack_content_rejects_invalid_urls() {
        let result = RoomImagePackEventContent::try_from(content(
            vec![image("cat", "https://example.org/cat.png")],
            None,
        ));
        assert!(result.is_err());

        let result = RoomImagePackEventContent::try_from(content(
            vec![image("cat", "mxc://example.org/cat")],
            Some("https://example.org/avatar.png"),
        ));
        assert!(result.is_err());
    }

    #[test]
    fn test_image_info_kept_with_invalid_thumbnail_source() {
        let mut info = RumaImageInfo::new();
        info.mimetype = Some("image/png".to_owned());
        info.thumbnail_source = Some(MediaSource::Plain("not-an-mxc-uri".into()));

        let mut ruma_image = RumaImagePackImage::new(owned_mxc_uri!("mxc://example.org/cat"));
        ruma_image.info = Some(info);

        let ffi_content = ImagePackContent::from(RoomImagePackEventContent::new(
            [("cat".to_owned(), ruma_image)].into(),
        ));

        let info = ffi_content.images[0].info.as_ref().unwrap();
        assert_eq!(info.mimetype.as_deref(), Some("image/png"));
        assert!(info.thumbnail_source.is_none());
    }

    #[test]
    fn test_image_pack_content_round_trip_keeps_existing_info() {
        let mut info = RumaImageInfo::new();
        info.mimetype = Some("image/png".to_owned());
        info.thumbnail_source = Some(MediaSource::Plain("not-an-mxc-uri".into()));
        info.thumbhash = Some(ruma::serde::Base64::new(vec![1, 2, 3]));

        let mut ruma_image = RumaImagePackImage::new(owned_mxc_uri!("mxc://example.org/cat"));
        ruma_image.info = Some(info);
        let existing = RoomImagePackEventContent::new([("cat".to_owned(), ruma_image)].into());

        let mut ffi_content = ImagePackContent::from(existing.clone());
        ffi_content.images[0].info.as_mut().unwrap().mimetype = Some("image/webp".to_owned());
        ffi_content.images.push(image("dog", "mxc://example.org/dog"));
        ffi_content.images[1].info = Some(ImageInfo {
            height: None,
            width: None,
            mimetype: None,
            size: None,
            thumbnail_info: None,
            thumbnail_source: None,
            blurhash: None,
            is_animated: None,
        });

        let ruma_content = ffi_content.into_ruma(Some(&existing)).unwrap();

        let cat_info = ruma_content.images["cat"].info.as_ref().unwrap();
        assert_eq!(cat_info.mimetype.as_deref(), Some("image/webp"));
        assert!(cat_info.thumbhash.is_some());
        assert!(cat_info.thumbnail_source.is_some());

        let dog_info = ruma_content.images["dog"].info.as_ref().unwrap();
        assert!(dog_info.thumbhash.is_none());
        assert!(dog_info.thumbnail_source.is_none());
    }
}
