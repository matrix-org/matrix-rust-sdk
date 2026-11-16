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

//! [Image packs], which hold the custom emoticons and stickers that can be
//! used in a room.
//!
//! [Image packs]: https://spec.matrix.org/v1.19/client-server-api/#image-packs

use itertools::Itertools;
use matrix_sdk::{
    Result, Room, RoomState,
    deserialized_responses::{RawSyncOrStrippedState, SyncOrStrippedState},
};
use ruma::{
    OwnedRoomId,
    events::{
        SyncStateEvent,
        image_pack::rooms::ImagePackRoomsEventContent,
        room::image_pack::{PackUsage, RoomImagePackEventContent},
    },
};
use tracing::warn;

use crate::spaces::SpaceService;

/// An image pack, defined by an `m.room.image_pack` state event.
#[derive(Clone, Debug)]
pub struct ImagePack {
    /// The room the pack is defined in.
    pub room_id: OwnedRoomId,
    /// The state key of the pack's state event, identifying it within its
    /// room.
    pub state_key: String,
    /// The pack's images and metadata.
    ///
    /// If the pack doesn't have a display name or an avatar, those of its room
    /// are used, as the spec recommends.
    pub content: RoomImagePackEventContent,
}

impl ImagePack {
    /// Whether the pack's images can be used as inline emoticons.
    pub fn is_emoticon_pack(&self) -> bool {
        self.is_usable_as(PackUsage::Emoticon)
    }

    /// Whether the pack's images can be sent as stickers.
    pub fn is_sticker_pack(&self) -> bool {
        self.is_usable_as(PackUsage::Sticker)
    }

    /// Packs without a usage can be used for everything.
    fn is_usable_as(&self, usage: PackUsage) -> bool {
        let usages = &self.content.pack.usage;
        usages.is_empty() || usages.contains(&usage)
    }
}

/// Get the image packs that can be used in a room.
///
/// They are returned in the order in which the spec recommends presenting
/// them:
///
/// 1. The packs that the user enabled globally through their
///    `m.image_pack.rooms` account data. The ones of rooms the user isn't in
///    are skipped, as their images can't be accessed.
/// 2. The packs defined in the room's state.
/// 3. The packs defined in the state of the joined spaces the room belongs to,
///    closest spaces first.
///
/// A pack that would appear more than once is only returned the first time.
/// Packs without images are skipped.
///
/// All the packs are read from the local state of their rooms, which the
/// [`RoomListService`](crate::RoomListService) requests.
pub async fn image_packs(room: &Room, space_service: &SpaceService) -> Result<Vec<ImagePack>> {
    let client = room.client();
    let mut packs = Vec::new();

    if let Some(raw) = client.account().account_data::<ImagePackRoomsEventContent>().await? {
        // Any client can write this account data, so a malformed one is ignored
        // rather than hiding the packs of the room and its spaces too.
        let rooms = raw.deserialize().map(|content| content.rooms).unwrap_or_else(|error| {
            warn!("Ignoring malformed m.image_pack.rooms account data: {error}");
            Default::default()
        });

        for (room_id, state_keys) in rooms {
            let Some(pack_room) =
                client.get_room(&room_id).filter(|room| room.state() == RoomState::Joined)
            else {
                continue;
            };

            let events = pack_room.get_state_events_for_keys_static(state_keys.keys()).await?;
            packs.extend(image_packs_from(&pack_room, events));
        }
    }

    packs.extend(image_packs_from(room, room.get_state_events_static().await?));

    for space_id in space_service.joined_ancestors_of(room.room_id()).await {
        if let Some(space) = client.get_room(&space_id) {
            packs.extend(image_packs_from(&space, space.get_state_events_static().await?));
        }
    }

    // Keep only the first occurrence of each pack, identified by its room and
    // state key.
    Ok(packs.into_iter().unique_by(|pack| (pack.room_id.clone(), pack.state_key.clone())).collect())
}

/// Turn a room's `m.room.image_pack` state events into image packs.
fn image_packs_from(
    room: &Room,
    events: Vec<RawSyncOrStrippedState<RoomImagePackEventContent>>,
) -> impl Iterator<Item = ImagePack> {
    events.into_iter().filter_map(|raw| match raw.deserialize() {
        // Packs without images have nothing to offer.
        Ok(SyncOrStrippedState::Sync(SyncStateEvent::Original(mut event)))
            if !event.content.images.is_empty() =>
        {
            // Use the room's name and avatar for the pack's missing ones.
            let meta = &mut event.content.pack;
            meta.display_name = meta.display_name.take().or_else(|| room.name());
            meta.avatar_url = meta.avatar_url.take().or_else(|| room.avatar_url());

            Some(ImagePack {
                room_id: room.room_id().to_owned(),
                state_key: event.state_key,
                content: event.content,
            })
        }
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use ruma::{events::room::image_pack::PackUsage, owned_room_id};

    use super::{ImagePack, RoomImagePackEventContent};

    fn pack(usage: &[PackUsage]) -> ImagePack {
        let mut content = RoomImagePackEventContent::new(BTreeMap::new());
        content.pack.usage = BTreeSet::from_iter(usage.iter().cloned());
        ImagePack { room_id: owned_room_id!("!room:localhost"), state_key: String::new(), content }
    }

    #[test]
    fn test_usage() {
        // A pack without a usage can be used for everything.
        let pack_without_usage = pack(&[]);
        assert!(pack_without_usage.is_emoticon_pack());
        assert!(pack_without_usage.is_sticker_pack());

        // Otherwise, it can only be used as it says.
        let sticker_pack = pack(&[PackUsage::Sticker]);
        assert!(!sticker_pack.is_emoticon_pack());
        assert!(sticker_pack.is_sticker_pack());
    }
}
