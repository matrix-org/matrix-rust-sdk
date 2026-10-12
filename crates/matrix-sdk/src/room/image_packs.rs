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

use matrix_sdk_base::deserialized_responses::{RawSyncOrStrippedState, SyncOrStrippedState};
use ruma::{
    OwnedRoomId,
    api::client::state::send_state_event,
    events::{SyncStateEvent, room::image_pack::RoomImagePackEventContent},
};
use tracing::info;

use crate::{Error, Result, Room};

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
