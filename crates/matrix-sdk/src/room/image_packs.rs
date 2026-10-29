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

use std::collections::{BTreeMap, HashMap, HashSet};

use matrix_sdk_base::{
    StateChanges,
    deserialized_responses::{RawSyncOrStrippedState, SyncOrStrippedState},
};
use ruma::{
    OwnedRoomId, OwnedUserId, RoomId, UserId,
    api::client::state::{
        get_state_event_for_key::{self, v3::StateEventFormat},
        get_state_events, send_state_event,
    },
    assign,
    events::{
        AnyStateEvent, StateEventType, SyncStateEvent,
        room::image_pack::RoomImagePackEventContent,
        space::{child::SpaceChildEventContent, parent::SpaceParentEventContent},
    },
    serde::Raw,
};
use tracing::{debug, info};

use crate::{Error, Result, Room, RoomState};

/// The maximum number of canonical spaces to walk up when looking for image
/// packs of the canonical space hierarchy of a room.
const MAX_CANONICAL_SPACE_DEPTH: usize = 10;

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

    /// The image pack is defined in the canonical space hierarchy of the room.
    Space {
        /// The ID of the space where the image pack is defined.
        space_id: OwnedRoomId,
    },
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
    /// 3. Image packs defined in the canonical space of this room, then in the
    ///    canonical space of that space, and so on. This method only uses
    ///    spaces that the user joined.
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

        // 3. Image packs of the canonical space hierarchy.
        for space in self.canonical_space_chain().await? {
            let space_id = space.room_id().to_owned();
            let mut space_packs = space.image_packs().await?;
            space_packs.sort_by(|a, b| a.state_key.cmp(&b.state_key));

            for pack in space_packs {
                push(pack, ImagePackSource::Space { space_id: space_id.clone() });
            }
        }

        Ok(packs)
    }

    /// Fetch from the server the image packs of other rooms that
    /// [`Room::available_image_packs()`] needs, and save them to storage.
    ///
    /// The image packs of this room must already be in storage, for example
    /// through the room subscription of the room list service. This method
    /// fetches:
    ///
    /// - The image packs enabled globally, in the user's `m.image_pack.rooms`
    ///   account data. This method fetches each pack individually.
    /// - The image packs of the canonical space hierarchy of this room. This
    ///   method fetches the full state of each space, because no endpoint
    ///   fetches all the state events of a given type.
    ///
    /// Call this before [`Room::available_image_packs()`], for example when you
    /// open an image pack picker. If the image packs of a room fail to fetch,
    /// this method skips that room and fetches the others.
    ///
    /// If a room removes an image pack, this method does not remove the pack
    /// from storage.
    pub async fn load_image_packs(&self) -> Result<()> {
        let mut changes = StateChanges::default();

        // 1. Image packs enabled globally.
        let image_pack_rooms = self.client.account().image_pack_rooms().await?;

        for (room_id, state_keys) in image_pack_rooms.rooms {
            if !self.client.get_room(&room_id).is_some_and(|room| room.state() == RoomState::Joined)
            {
                continue;
            }

            for state_key in state_keys.into_keys() {
                let request = assign!(
                    get_state_event_for_key::v3::Request::new(
                        room_id.clone(),
                        StateEventType::RoomImagePack,
                        state_key.clone(),
                    ),
                    { format: StateEventFormat::Event }
                );

                match self.client.send(request).await {
                    Ok(response) => {
                        add_image_pack_event(&mut changes, &room_id, response.into_event());
                    }
                    Err(e) => {
                        info!(
                            ?room_id,
                            state_key, "Could not load globally enabled image pack: {e}"
                        );
                    }
                }
            }
        }

        // 2. Image packs of the canonical space hierarchy.
        for space in self.canonical_space_chain().await? {
            let space_id = space.room_id();
            let request = get_state_events::v3::Request::new(space_id.to_owned());

            match self.client.send(request).await {
                Ok(response) => {
                    for event in response.room_state {
                        add_image_pack_event(&mut changes, space_id, event);
                    }
                }
                Err(e) => {
                    info!(?space_id, "Could not load the image packs of a space: {e}");
                }
            }
        }

        if !changes.state.is_empty() {
            self.client.state_store().save_changes(&changes).await?;
        }

        Ok(())
    }

    /// Get the canonical space hierarchy of this room, from storage.
    ///
    /// Returns the canonical parent space of this room, then the canonical
    /// parent space of that space, and so on. The walk stops at the first space
    /// that the user did not join, when it finds a cycle, or after
    /// [`MAX_CANONICAL_SPACE_DEPTH`] spaces.
    async fn canonical_space_chain(&self) -> Result<Vec<Room>> {
        let mut spaces = Vec::new();
        let mut visited = HashSet::from([self.room_id().to_owned()]);
        let mut current = self.clone();

        for _ in 0..MAX_CANONICAL_SPACE_DEPTH {
            let Some(space) = current.canonical_parent_space().await? else {
                break;
            };

            if !visited.insert(space.room_id().to_owned()) {
                debug!(space_id = ?space.room_id(), "Cycle detected in the canonical space hierarchy");
                break;
            }

            if space.state() != RoomState::Joined {
                break;
            }

            spaces.push(space.clone());
            current = space;
        }

        Ok(spaces)
    }

    /// Get the canonical parent space of this room, from storage.
    ///
    /// This method only uses parents with a verified relationship. A
    /// relationship is verified if the parent has a reciprocal `m.space.child`
    /// event. It is also verified if the sender of the `m.space.parent` event
    /// has the power to send `m.space.child` events in the parent. This
    /// prevents a room member from adding the image packs of any space to this
    /// room.
    ///
    /// If more than one verified parent is marked as canonical, this method
    /// returns the one with the lowest room ID. This makes the result
    /// deterministic.
    ///
    /// Unlike [`Room::parent_spaces()`], this method never fetches the members
    /// of a parent from the server. It compares the power of the sender only
    /// with the power levels of the parent. If the power levels of a parent are
    /// not in storage, the relationship is not verified.
    async fn canonical_parent_space(&self) -> Result<Option<Room>> {
        // Map the ID of each canonical parent to the sender of its
        // `m.space.parent` event. A `BTreeMap` lets us check parents in
        // ascending room ID order and stop at the first verified one.
        let canonical_parents: BTreeMap<OwnedRoomId, OwnedUserId> = self
            .get_state_events_static::<SpaceParentEventContent>()
            .await?
            .into_iter()
            .filter_map(|raw| match raw.deserialize() {
                Ok(SyncOrStrippedState::Sync(SyncStateEvent::Original(e)))
                    if e.content.canonical =>
                {
                    Some((e.state_key, e.sender))
                }
                Ok(SyncOrStrippedState::Stripped(e)) if e.content.canonical => {
                    Some((e.state_key, e.sender))
                }
                Ok(_) => None,
                Err(e) => {
                    info!(room_id = ?self.room_id(), "Could not deserialize m.space.parent: {e}");
                    None
                }
            })
            .collect();

        for (space_id, sender) in canonical_parents {
            let Some(space) = self.client.get_room(&space_id) else {
                // We are not in the space, so we cannot verify the
                // relationship.
                continue;
            };

            if self.is_verified_child_of(&space, &sender).await? {
                return Ok(Some(space));
            }

            debug!(
                room_id = ?self.room_id(), ?space_id,
                "Ignoring unverified canonical parent space"
            );
        }

        Ok(None)
    }

    /// Whether the relationship between this room and the given parent space,
    /// declared by `sender` in an `m.space.parent` event, can be verified from
    /// storage.
    async fn is_verified_child_of(&self, space: &Room, sender: &UserId) -> Result<bool> {
        // A valid (non-redacted) `m.space.child` event in the parent pointing
        // to this room makes the relationship reciprocal. A redacted event is
        // missing the `via` key, which invalidates that end of the
        // relationship.
        if let Some(raw) = space
            .get_state_event_static_for_key::<SpaceChildEventContent, _>(self.room_id())
            .await?
        {
            match raw.deserialize() {
                Ok(SyncOrStrippedState::Sync(SyncStateEvent::Original(_))) => return Ok(true),
                Ok(_) => {}
                Err(e) => {
                    info!(
                        room_id = ?self.room_id(), space_id = ?space.room_id(),
                        "Could not deserialize m.space.child: {e}"
                    );
                }
            }
        }

        // Otherwise, check whether the sender has the power to send an
        // `m.space.child` event in the parent.
        Ok(space
            .power_levels()
            .await
            .is_ok_and(|pl| pl.user_can_send_state(sender, StateEventType::SpaceChild)))
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

/// Add the given state event to `changes` if it is an `m.room.image_pack`
/// event, so that it is saved to storage.
///
/// This function ignores events without an event ID. A server that does not
/// support `format=event` for a single state event returns only its content.
fn add_image_pack_event(changes: &mut StateChanges, room_id: &RoomId, event: Raw<AnyStateEvent>) {
    let Ok(Some(event_type)) = event.get_field::<String>("type") else {
        return;
    };
    if event_type != StateEventType::RoomImagePack.to_string() {
        return;
    }

    let (Ok(Some(state_key)), Ok(Some(_))) =
        (event.get_field::<String>("state_key"), event.get_field::<String>("event_id"))
    else {
        info!(?room_id, "Ignoring m.room.image_pack event without a state key or an event ID");
        return;
    };

    changes
        .state
        .entry(room_id.to_owned())
        .or_default()
        .entry(StateEventType::RoomImagePack)
        .or_default()
        .insert(state_key, event.cast_unchecked());
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
