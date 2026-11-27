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

use std::collections::BTreeMap;

use itertools::Itertools;
use matrix_sdk::{
    Client, Result, Room, RoomState,
    deserialized_responses::{RawSyncOrStrippedState, SyncOrStrippedState},
};
use ruma::{
    OwnedRoomId,
    events::{
        SyncStateEvent,
        image_pack::rooms::{ImagePackRoomsEventContent, RoomImagePackMeta},
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
    /// Whether the user enabled the pack globally, through their
    /// `m.image_pack.rooms` account data.
    pub is_enabled_globally: bool,
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
/// The packs of the same room are sorted by state key. A pack that would appear
/// more than once is only returned the first time.
/// Packs without images are skipped, see [`room_image_packs()`] to manage them.
///
/// All the packs are read from the local state of their rooms, which the
/// [`RoomListService`](crate::RoomListService) requests.
pub async fn available_image_packs(
    room: &Room,
    space_service: &SpaceService,
) -> Result<Vec<ImagePack>> {
    let client = room.client();
    let enabled_packs = enabled_packs(&client).await?;
    let mut packs = Vec::new();

    for (room_id, state_keys) in &enabled_packs {
        let Some(pack_room) =
            client.get_room(room_id).filter(|room| room.state() == RoomState::Joined)
        else {
            continue;
        };

        let events = pack_room.get_state_events_for_keys_static(state_keys.keys()).await?;
        packs.extend(image_packs_from(&pack_room, events, &enabled_packs));
    }

    packs.extend(image_packs_from(room, room.get_state_events_static().await?, &enabled_packs));

    for space_id in space_service.joined_ancestors_of(room.room_id()).await {
        if let Some(space) = client.get_room(&space_id) {
            let events = space.get_state_events_static().await?;
            packs.extend(image_packs_from(&space, events, &enabled_packs));
        }
    }

    Ok(packs
        .into_iter()
        // Packs without images have nothing to offer.
        .filter(|pack| !pack.content.images.is_empty())
        // Keep only the first occurrence of each pack, identified by its room
        // and state key.
        .unique_by(|pack| (pack.room_id.clone(), pack.state_key.clone()))
        .collect())
}

/// Get all the image packs defined in a room's state, including the ones
/// without images, to manage them.
///
/// They are sorted by state key.
pub async fn room_image_packs(room: &Room) -> Result<Vec<ImagePack>> {
    let enabled_packs = enabled_packs(&room.client()).await?;
    Ok(image_packs_from(room, room.get_state_events_static().await?, &enabled_packs).collect())
}

/// Get the packs that the user enabled globally through their
/// `m.image_pack.rooms` account data, by room and state key.
async fn enabled_packs(
    client: &Client,
) -> Result<BTreeMap<OwnedRoomId, BTreeMap<String, RoomImagePackMeta>>> {
    let Some(raw) = client.account().account_data::<ImagePackRoomsEventContent>().await? else {
        return Ok(BTreeMap::new());
    };

    // Any client can write this account data, so a malformed one is ignored
    // rather than hiding the packs of the room and its spaces too.
    Ok(raw.deserialize().map(|content| content.rooms).unwrap_or_else(|error| {
        warn!("Ignoring malformed m.image_pack.rooms account data: {error}");
        Default::default()
    }))
}

/// Turn a room's `m.room.image_pack` state events into image packs.
fn image_packs_from(
    room: &Room,
    events: Vec<RawSyncOrStrippedState<RoomImagePackEventContent>>,
    enabled_packs: &BTreeMap<OwnedRoomId, BTreeMap<String, RoomImagePackMeta>>,
) -> impl Iterator<Item = ImagePack> {
    let enabled_state_keys = enabled_packs.get(room.room_id());

    events
        .into_iter()
        .filter_map(move |raw| match raw.deserialize() {
            Ok(SyncOrStrippedState::Sync(SyncStateEvent::Original(mut event))) => {
                let is_enabled_globally = enabled_state_keys
                    .is_some_and(|state_keys| state_keys.contains_key(&event.state_key));

                // Use the room's name and avatar for the pack's missing ones.
                let meta = &mut event.content.pack;
                meta.display_name = meta.display_name.take().or_else(|| room.name());
                meta.avatar_url = meta.avatar_url.take().or_else(|| room.avatar_url());

                Some(ImagePack {
                    room_id: room.room_id().to_owned(),
                    state_key: event.state_key,
                    content: event.content,
                    is_enabled_globally,
                })
            }
            // Packs are deleted by sending an empty content, which fails to
            // deserialize, so they're skipped here along with the redacted ones.
            _ => None,
        })
        // The store returns state events in no particular order.
        .sorted_by(|a, b| a.state_key.cmp(&b.state_key))
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use ruma::{events::room::image_pack::PackUsage, owned_room_id};

    use super::{ImagePack, RoomImagePackEventContent};

    fn pack(usage: &[PackUsage]) -> ImagePack {
        let mut content = RoomImagePackEventContent::new(BTreeMap::new());
        content.pack.usage = BTreeSet::from_iter(usage.iter().cloned());
        ImagePack {
            room_id: owned_room_id!("!room:localhost"),
            state_key: String::new(),
            content,
            is_enabled_globally: false,
        }
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
