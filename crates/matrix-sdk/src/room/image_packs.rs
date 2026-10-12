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

//! Support for [image packs]. An image pack is a collection of custom
//! emoticons and stickers.
//!
//! [image packs]: https://spec.matrix.org/v1.19/client-server-api/#image-packs

use std::collections::{HashMap, HashSet};

use matrix_sdk_base::deserialized_responses::{RawSyncOrStrippedState, SyncOrStrippedState};
use ruma::{
    OwnedRoomId,
    api::client::state::send_state_event,
    events::{SyncStateEvent, room::image_pack::RoomImagePackEventContent},
};
use tracing::{debug, info};

use crate::{Error, Result, Room, RoomState};

/// An image pack defined in a room, by an `m.room.image_pack` state event.
#[derive(Clone, Debug)]
pub struct ImagePack {
    /// The ID of the room where the image pack is defined.
    pub room_id: OwnedRoomId,

    /// The state key of the `m.room.image_pack` state event, which uniquely
    /// identifies the image pack within the room.
    pub state_key: String,

    /// The content of the image pack.
    pub content: RoomImagePackEventContent,
}

/// Where an image pack returned by [`Room::available_image_packs()`] comes
/// from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ImagePackSource {
    /// The image pack is enabled globally, in the user's `m.image_pack.rooms`
    /// account data.
    Global,

    /// The image pack is defined in the room itself.
    Room,
}

/// An image pack that is available in a room, with its source.
#[derive(Clone, Debug)]
pub struct AvailableImagePack {
    /// The image pack.
    pub pack: ImagePack,

    /// Where the image pack comes from.
    pub source: ImagePackSource,
}

/// Whether the given string is a valid image pack shortcode, according to the
/// [grammar] in the spec.
///
/// A valid shortcode is between 1 and 100 bytes long and only contains ASCII
/// letters, digits, `-` and `_`.
///
/// [grammar]: https://spec.matrix.org/v1.19/client-server-api/#shortcode-grammar
pub fn is_valid_shortcode(shortcode: &str) -> bool {
    (1..=100).contains(&shortcode.len())
        && shortcode.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

impl Room {
    /// Get all the image packs defined in this room, from storage.
    ///
    /// This method ignores image packs that are redacted or that fail to
    /// deserialize. The results are in no particular order.
    pub async fn image_packs(&self) -> Result<Vec<ImagePack>> {
        Ok(self
            .get_state_events_static::<RoomImagePackEventContent>()
            .await?
            .into_iter()
            .filter_map(|raw| self.deserialize_image_pack(raw))
            .collect())
    }

    /// Get the image pack with the given state key in this room, from storage.
    ///
    /// Returns `None` if there is no such image pack, or if it is redacted or
    /// fails to deserialize.
    pub async fn image_pack(&self, state_key: &str) -> Result<Option<ImagePack>> {
        Ok(self
            .get_state_event_static_for_key::<RoomImagePackEventContent, _>(state_key)
            .await?
            .and_then(|raw| self.deserialize_image_pack(raw)))
    }

    /// Create or replace the image pack with the given state key in this room.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidImagePackShortcode`] if one of the images of the
    /// pack has a shortcode that does not match the [grammar] in the spec.
    ///
    /// [grammar]: https://spec.matrix.org/v1.19/client-server-api/#shortcode-grammar
    pub async fn set_image_pack(
        &self,
        state_key: &str,
        content: RoomImagePackEventContent,
    ) -> Result<send_state_event::v3::Response> {
        if let Some(shortcode) = content.images.keys().find(|s| !is_valid_shortcode(s)) {
            return Err(Error::InvalidImagePackShortcode(shortcode.clone()));
        }

        self.send_state_event_for_key(state_key, content).await
    }

    /// Get all the image packs that are available to the user in this room,
    /// from storage.
    ///
    /// This method returns the image packs in the order recommended by the
    /// [spec]:
    ///
    /// 1. Image packs enabled globally, in the user's `m.image_pack.rooms`
    ///    account data. This method skips packs in rooms that the user did not
    ///    join.
    /// 2. Image packs defined in this room.
    ///
    /// Within each source, this method sorts image packs by room ID and state
    /// key, so that the order is stable. It returns an image pack only once,
    /// with the first source where it found the pack.
    ///
    /// [spec]: https://spec.matrix.org/v1.19/client-server-api/#image-pack-source-priority
    pub async fn available_image_packs(&self) -> Result<Vec<AvailableImagePack>> {
        let mut packs = Vec::new();
        let mut seen = HashSet::new();

        let mut push = |pack: ImagePack, source: ImagePackSource| {
            if seen.insert((pack.room_id.clone(), pack.state_key.clone())) {
                packs.push(AvailableImagePack { pack, source });
            }
        };

        // 1. Image packs enabled globally.
        let image_pack_rooms = self.client.account().image_pack_rooms().await?;

        for (room_id, state_keys) in image_pack_rooms.rooms {
            let Some(room) =
                self.client.get_room(&room_id).filter(|room| room.state() == RoomState::Joined)
            else {
                debug!(
                    ?room_id,
                    "Skipping globally enabled image packs of a room that isn't joined"
                );
                continue;
            };

            // Load all the packs of the room at once, rather than one by one.
            let mut room_packs: HashMap<String, ImagePack> = room
                .image_packs()
                .await?
                .into_iter()
                .map(|pack| (pack.state_key.clone(), pack))
                .collect();

            for state_key in state_keys.into_keys() {
                match room_packs.remove(&state_key) {
                    Some(pack) => push(pack, ImagePackSource::Global),
                    None => {
                        debug!(?room_id, state_key, "Globally enabled image pack not found");
                    }
                }
            }
        }

        // 2. Image packs of this room.
        let mut room_packs = self.image_packs().await?;
        room_packs.sort_by(|a, b| a.state_key.cmp(&b.state_key));

        for pack in room_packs {
            push(pack, ImagePackSource::Room);
        }

        Ok(packs)
    }

    fn deserialize_image_pack(
        &self,
        raw: RawSyncOrStrippedState<RoomImagePackEventContent>,
    ) -> Option<ImagePack> {
        let (state_key, content) = match raw.deserialize() {
            Ok(SyncOrStrippedState::Sync(SyncStateEvent::Original(e))) => (e.state_key, e.content),
            Ok(SyncOrStrippedState::Sync(SyncStateEvent::Redacted(_))) => return None,
            Ok(SyncOrStrippedState::Stripped(e)) => {
                let mut content = RoomImagePackEventContent::new(e.content.images?);
                content.pack = e.content.pack;
                (e.state_key, content)
            }
            Err(e) => {
                info!(room_id = ?self.room_id(), "Could not deserialize m.room.image_pack: {e}");
                return None;
            }
        };

        Some(ImagePack { room_id: self.room_id().to_owned(), state_key, content })
    }
}

#[cfg(test)]
mod tests {
    use super::is_valid_shortcode;

    #[test]
    fn test_is_valid_shortcode() {
        assert!(is_valid_shortcode("cat_wave"));
        assert!(is_valid_shortcode("Cat-Wave_2"));
        assert!(is_valid_shortcode(&"a".repeat(100)));

        assert!(!is_valid_shortcode(""));
        assert!(!is_valid_shortcode(&"a".repeat(101)));
        assert!(!is_valid_shortcode("cat:wave"));
        assert!(!is_valid_shortcode("cat/wave"));
        assert!(!is_valid_shortcode("cat wave"));
        assert!(!is_valid_shortcode("chat_é"));
    }
}
