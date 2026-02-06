// Copyright 2025 The Matrix.org Foundation C.I.C.
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

use std::ops::Not;

use ruma::{OwnedRoomId, events::room::tombstone::RoomTombstoneEventContent};

use super::Room;

impl Room {
    /// Has the room been tombstoned.
    ///
    /// A room is tombstoned if it has received a [`m.room.tombstone`] state
    /// event; see [`Room::tombstone_content`].
    ///
    /// [`m.room.tombstone`]: https://spec.matrix.org/v1.14/client-server-api/#mroomtombstone
    pub fn is_tombstoned(&self) -> bool {
        self.info.read().base_info.tombstone.is_some()
    }

    /// Get the [`m.room.tombstone`] state event's content of this room if one
    /// has been received.
    ///
    /// Also see [`Room::is_tombstoned`] to check if the [`m.room.tombstone`]
    /// event has been received. It's faster than using this method.
    ///
    /// [`m.room.tombstone`]: https://spec.matrix.org/v1.14/client-server-api/#mroomtombstone
    pub fn tombstone_content(&self) -> Option<RoomTombstoneEventContent> {
        self.info.read().tombstone().cloned()
    }

    /// If this room is tombstoned, return the “reference” to the successor room
    /// —i.e. the room replacing this one.
    ///
    /// A room is tombstoned if it has received a [`m.room.tombstone`] state
    /// event; see [`Room::tombstone_content`].
    ///
    /// [`m.room.tombstone`]: https://spec.matrix.org/v1.14/client-server-api/#mroomtombstone
    pub fn successor_room(&self) -> Option<SuccessorRoom> {
        self.tombstone_content().map(|tombstone_event| SuccessorRoom {
            room_id: tombstone_event.replacement_room,
            reason: tombstone_event.body.is_empty().not().then_some(tombstone_event.body),
        })
    }

    /// If this room is the successor of a tombstoned room, return the
    /// “reference” to the predecessor room.
    ///
    /// A room is tombstoned if it has received a [`m.room.tombstone`] state
    /// event; see [`Room::tombstone_content`].
    ///
    /// To determine if a room is the successor of a tombstoned room, the
    /// [`m.room.create`] must have been received, **with** a `predecessor`
    /// field. See [`Room::create_content`].
    ///
    /// [`m.room.tombstone`]: https://spec.matrix.org/v1.14/client-server-api/#mroomtombstone
    /// [`m.room.create`]: https://spec.matrix.org/v1.14/client-server-api/#mroomcreate
    pub fn predecessor_room(&self) -> Option<PredecessorRoom> {
        self.create_content()
            .and_then(|content_event| content_event.predecessor)
            .map(|predecessor| PredecessorRoom { room_id: predecessor.room_id })
    }
}

/// When a room A is tombstoned, it is replaced by a room B. The room A is the
/// predecessor of B, and B is the successor of A. This type holds information
/// about the successor room. See [`Room::successor_room`].
///
/// A room is tombstoned if it has received a [`m.room.tombstone`] state event.
///
/// [`m.room.tombstone`]: https://spec.matrix.org/v1.14/client-server-api/#mroomtombstone
#[derive(Debug)]
pub struct SuccessorRoom {
    /// The ID of the next room replacing this (tombstoned) room.
    pub room_id: OwnedRoomId,

    /// The reason why the room has been tombstoned.
    pub reason: Option<String>,
}

/// When a room A is tombstoned, it is replaced by a room B. The room A is the
/// predecessor of B, and B is the successor of A. This type holds information
/// about the predecessor room. See [`Room::predecessor_room`].
///
/// To know the predecessor of a room, the [`m.room.create`] state event must
/// have been received.
///
/// [`m.room.create`]: https://spec.matrix.org/v1.14/client-server-api/#mroomcreate
#[derive(Debug)]
pub struct PredecessorRoom {
    /// The ID of the old room.
    pub room_id: OwnedRoomId,
}

#[cfg(test)]
mod tests {
    use std::ops::Not;

    use assert_matches::assert_matches;
    use matrix_sdk_test::{
        JoinedRoomBuilder, SyncResponseBuilder, async_test, event_factory::EventFactory,
    };
    use ruma::{RoomVersionId, room_id, user_id};

    use crate::{RoomState, test_utils::logged_in_base_client};

    #[async_test]
    async fn test_no_successor_room() {
        let client = logged_in_base_client(None).await;
        let room = client.get_or_create_room(room_id!("!r0"), RoomState::Joined);

        assert!(room.is_tombstoned().not());
        assert!(room.tombstone_content().is_none());
        assert!(room.successor_room().is_none());
    }

    #[async_test]
    async fn test_successor_room() {
        let client = logged_in_base_client(None).await;
        let sender = user_id!("@mnt_io:matrix.org");
        let room_id = room_id!("!r0");
        let successor_room_id = room_id!("!r1");
        let room = client.get_or_create_room(room_id, RoomState::Joined);

        let mut sync_builder = SyncResponseBuilder::new();
        let response = sync_builder
            .add_joined_room(
                JoinedRoomBuilder::new(room_id).add_timeline_event(
                    EventFactory::new()
                        .sender(sender)
                        .room_tombstone("traces of you", successor_room_id),
                ),
            )
            .build_sync_response();

        client.receive_sync_response(response).await.unwrap();

        assert!(room.is_tombstoned());
        assert!(room.tombstone_content().is_some());
        assert_matches!(room.successor_room(), Some(successor_room) => {
            assert_eq!(successor_room.room_id, successor_room_id);
            assert_matches!(successor_room.reason, Some(reason) => {
                assert_eq!(reason, "traces of you");
            });
        });
    }

    #[async_test]
    async fn test_successor_room_no_reason() {
        let client = logged_in_base_client(None).await;
        let sender = user_id!("@mnt_io:matrix.org");
        let room_id = room_id!("!r0");
        let successor_room_id = room_id!("!r1");
        let room = client.get_or_create_room(room_id, RoomState::Joined);

        let mut sync_builder = SyncResponseBuilder::new();
        let response = sync_builder
            .add_joined_room(JoinedRoomBuilder::new(room_id).add_timeline_event(
                EventFactory::new().sender(sender).room_tombstone(
                    // An empty reason will result in `None` in `SuccessorRoom::reason`.
                    "",
                    successor_room_id,
                ),
            ))
            .build_sync_response();

        client.receive_sync_response(response).await.unwrap();

        assert!(room.is_tombstoned());
        assert!(room.tombstone_content().is_some());
        assert_matches!(room.successor_room(), Some(successor_room) => {
            assert_eq!(successor_room.room_id, successor_room_id);
            assert!(successor_room.reason.is_none());
        });
    }

    #[async_test]
    async fn test_no_predecessor_room() {
        let client = logged_in_base_client(None).await;
        let room = client.get_or_create_room(room_id!("!r0"), RoomState::Joined);

        assert!(room.create_content().is_none());
        assert!(room.predecessor_room().is_none());
    }

    #[async_test]
    async fn test_no_predecessor_room_with_create_event() {
        let client = logged_in_base_client(None).await;
        let sender = user_id!("@mnt_io:matrix.org");
        let room_id = room_id!("!r1");
        let room = client.get_or_create_room(room_id, RoomState::Joined);

        let mut sync_builder = SyncResponseBuilder::new();
        let response = sync_builder
            .add_joined_room(
                JoinedRoomBuilder::new(room_id).add_timeline_event(
                    EventFactory::new()
                        .create(sender, RoomVersionId::V11)
                        // No `predecessor` field!
                        .no_predecessor()
                        .into_raw_sync(),
                ),
            )
            .build_sync_response();

        client.receive_sync_response(response).await.unwrap();

        assert!(room.create_content().is_some());
        assert!(room.predecessor_room().is_none());
    }

    #[async_test]
    async fn test_predecessor_room() {
        let client = logged_in_base_client(None).await;
        let sender = user_id!("@mnt_io:matrix.org");
        let room_id = room_id!("!r1");
        let predecessor_room_id = room_id!("!r0");
        let room = client.get_or_create_room(room_id, RoomState::Joined);

        let mut sync_builder = SyncResponseBuilder::new();
        let response = sync_builder
            .add_joined_room(
                JoinedRoomBuilder::new(room_id).add_timeline_event(
                    EventFactory::new()
                        .create(sender, RoomVersionId::V11)
                        .predecessor(predecessor_room_id)
                        .into_raw_sync(),
                ),
            )
            .build_sync_response();

        client.receive_sync_response(response).await.unwrap();

        assert!(room.create_content().is_some());
        assert_matches!(room.predecessor_room(), Some(predecessor_room) => {
            assert_eq!(predecessor_room.room_id, predecessor_room_id);
        });
    }
}
