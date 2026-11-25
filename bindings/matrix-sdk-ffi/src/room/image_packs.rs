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

use std::sync::Arc;

use matrix_sdk_ui::image_packs::{ImagePack as SdkImagePack, available_image_packs};

use super::Room;
use crate::{error::ClientError, ruma::ImageInfo, spaces::SpaceService};

/// An image pack, holding custom emoticons and/or stickers.
#[derive(Clone, uniffi::Record)]
pub struct ImagePack {
    /// The room the pack is defined in.
    pub room_id: String,
    /// The state key of the pack's state event, identifying it within its
    /// room.
    pub state_key: String,
    /// The name to display for the pack: its own display name if it has one,
    /// otherwise the name of its room. `None` only if neither has a name.
    pub display_name: Option<String>,
    /// The `mxc://` URL of the avatar to display for the pack: its own avatar
    /// if it has one, otherwise the avatar of its room. `None` only if neither
    /// has an avatar.
    pub avatar_url: Option<String>,
    /// Whether the pack's images can be used as inline emoticons.
    pub is_emoticon_pack: bool,
    /// Whether the pack's images can be sent as stickers.
    pub is_sticker_pack: bool,
    /// Whether the user enabled the pack globally, making it usable in all
    /// the rooms.
    pub is_enabled_globally: bool,
    /// An attribution for the pack, crediting its original author or source
    /// for example.
    pub attribution: Option<String>,
    /// The pack's images.
    pub images: Vec<ImagePackImage>,
}

/// An image of an [`ImagePack`].
#[derive(Clone, uniffi::Record)]
pub struct ImagePackImage {
    /// The shortcode of the image, unique within its pack.
    pub shortcode: String,
    /// The `mxc://` URL of the image.
    pub url: String,
    /// A textual description of the image, falling back to its shortcode as
    /// the spec recommends. To be used as the `alt` of emoticons and the
    /// `body` of stickers.
    pub body: String,
    /// Metadata about the image, to be used as the `info` of the `m.sticker`
    /// events it's sent as.
    pub info: Option<ImageInfo>,
}

impl From<SdkImagePack> for ImagePack {
    fn from(pack: SdkImagePack) -> Self {
        let is_emoticon_pack = pack.is_emoticon_pack();
        let is_sticker_pack = pack.is_sticker_pack();
        let meta = pack.content.pack;

        Self {
            room_id: pack.room_id.to_string(),
            state_key: pack.state_key,
            is_emoticon_pack,
            is_sticker_pack,
            is_enabled_globally: pack.is_enabled_globally,
            display_name: meta.display_name,
            avatar_url: meta.avatar_url.map(|url| url.to_string()),
            attribution: meta.attribution,
            images: pack
                .content
                .images
                .into_iter()
                .map(|(shortcode, image)| ImagePackImage {
                    body: image.body.unwrap_or_else(|| shortcode.clone()),
                    shortcode,
                    url: image.url.to_string(),
                    info: image.info.as_ref().and_then(|info| info.try_into().ok()),
                })
                .collect(),
        }
    }
}

#[matrix_sdk_ffi_macros::export]
impl Room {
    /// Get the image packs that can be used in this room.
    ///
    /// The packs the user enabled globally come first, then the packs defined
    /// in this room, then the packs of the joined spaces it belongs to.
    pub async fn image_packs(
        &self,
        space_service: Arc<SpaceService>,
    ) -> Result<Vec<ImagePack>, ClientError> {
        Ok(available_image_packs(&self.inner, &space_service.inner)
            .await?
            .into_iter()
            .map(Into::into)
            .collect())
    }
}
