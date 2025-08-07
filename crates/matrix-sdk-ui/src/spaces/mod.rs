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
// See the License for that specific language governing permissions and
// limitations under the License.

//! High level interfaces for working with Spaces
//!
//! See [`SpaceService`] for details.

use eyeball::{SharedObservable, Subscriber};
use futures_util::pin_mut;
use matrix_sdk::Client;
use ruma::OwnedRoomId;
use tokio::task::JoinHandle;
use tokio_stream::StreamExt;
use tracing::error;

pub use crate::spaces::{room::SpaceServiceRoom, room_list::SpaceServiceRoomList};

pub mod room;
pub mod room_list;

pub struct SpaceService {
    client: Client,

    joined_spaces: SharedObservable<Vec<SpaceServiceRoom>>,

    room_update_handle: JoinHandle<()>,
}

impl Drop for SpaceService {
    fn drop(&mut self) {
        self.room_update_handle.abort();
    }
}

impl SpaceService {
    pub async fn new(client: Client) -> Self {
        let joined_spaces = SharedObservable::new(Vec::new());

        joined_spaces.set(Self::joined_spaces_for(&client).await);

        let client_clone = client.clone();
        let joined_spaces_clone = joined_spaces.clone();
        let all_room_updates_receiver = client.subscribe_to_all_room_updates();

        let handle = tokio::spawn(async move {
            pin_mut!(all_room_updates_receiver);

            loop {
                match all_room_updates_receiver.recv().await {
                    Ok(_) => {
                        let new_spaces = Self::joined_spaces_for(&client_clone).await;
                        if new_spaces != joined_spaces_clone.get() {
                            joined_spaces_clone.set(new_spaces);
                        }
                    }
                    Err(err) => {
                        error!("error when listening to room updates: {err}");
                    }
                }
            }
        });

        Self { client, joined_spaces, room_update_handle: handle }
    }

    pub fn subscribe_to_joined_spaces(&self) -> Subscriber<Vec<SpaceServiceRoom>> {
        self.joined_spaces.subscribe()
    }

    pub fn joined_spaces(&self) -> Vec<SpaceServiceRoom> {
        self.joined_spaces.get()
    }

    pub fn space_room_list(&self, space_id: OwnedRoomId) -> SpaceServiceRoomList {
        SpaceServiceRoomList::new(self.client.clone(), space_id)
    }

    async fn joined_spaces_for(client: &Client) -> Vec<SpaceServiceRoom> {
        let joined_spaces = client
            .joined_rooms()
            .into_iter()
            .filter_map(|room| if room.is_space() { Some(room) } else { None })
            .collect::<Vec<_>>();

        let top_level_spaces: Vec<SpaceServiceRoom> = tokio_stream::iter(joined_spaces)
            .then(|room| async move {
                let ok = if let Ok(parents) = room.parent_spaces().await {
                    pin_mut!(parents);
                    parents.any(|p| p.is_ok()).await == false
                } else {
                    false
                };
                (room, ok)
            })
            .filter(|(_, ok)| *ok)
            .map(|(x, _)| SpaceServiceRoom::new_from_known(x))
            .collect()
            .await;

        top_level_spaces
    }
}

#[cfg(test)]
mod tests {
    use assert_matches2::assert_let;
    use matrix_sdk::{room::ParentSpace, test_utils::mocks::MatrixMockServer};
    use matrix_sdk_test::{
        JoinedRoomBuilder, LeftRoomBuilder, async_test, event_factory::EventFactory,
    };
    use ruma::{RoomVersionId, room_id};
    use tokio_stream::StreamExt;

    use super::*;

    use futures_util::pin_mut;

    use stream_assert::{assert_next_eq, assert_pending};

    #[async_test]
    async fn test_spaces_hierarchy() {
        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;
        let user_id = client.user_id().unwrap();
        let space_service = SpaceService::new(client.clone()).await;
        let factory = EventFactory::new();

        server.mock_room_state_encryption().plain().mount().await;

        // Given one parent space with 2 children spaces

        let parent_space_id = room_id!("!parent_space:example.org");
        let child_space_id_1 = room_id!("!child_space_1:example.org");
        let child_space_id_2 = room_id!("!child_space_2:example.org");

        server
            .sync_room(
                &client,
                JoinedRoomBuilder::new(child_space_id_1)
                    .add_state_event(factory.create(user_id, RoomVersionId::V1).with_space_type())
                    .add_state_event(
                        factory
                            .space_parent(parent_space_id.to_owned(), child_space_id_1.to_owned())
                            .sender(user_id),
                    ),
            )
            .await;

        server
            .sync_room(
                &client,
                JoinedRoomBuilder::new(child_space_id_2)
                    .add_state_event(factory.create(user_id, RoomVersionId::V1).with_space_type())
                    .add_state_event(
                        factory
                            .space_parent(parent_space_id.to_owned(), child_space_id_2.to_owned())
                            .sender(user_id),
                    ),
            )
            .await;
        server
            .sync_room(
                &client,
                JoinedRoomBuilder::new(parent_space_id)
                    .add_state_event(factory.create(user_id, RoomVersionId::V1).with_space_type())
                    .add_state_event(
                        factory
                            .space_child(parent_space_id.to_owned(), child_space_id_1.to_owned())
                            .sender(user_id),
                    )
                    .add_state_event(
                        factory
                            .space_child(parent_space_id.to_owned(), child_space_id_2.to_owned())
                            .sender(user_id),
                    ),
            )
            .await;

        // Only the parent space is returned
        assert_eq!(
            space_service.joined_spaces().iter().map(|s| s.room_id.to_owned()).collect::<Vec<_>>(),
            vec![parent_space_id]
        );

        let parent_space = client.get_room(parent_space_id).unwrap();
        assert!(parent_space.is_space());

        // And the parent space and the two child spaces are linked

        let spaces: Vec<ParentSpace> = client
            .get_room(child_space_id_1)
            .unwrap()
            .parent_spaces()
            .await
            .unwrap()
            .map(Result::unwrap)
            .collect()
            .await;

        assert_let!(ParentSpace::Reciprocal(parent) = spaces.first().unwrap());
        assert_eq!(parent.room_id(), parent_space.room_id());

        let spaces: Vec<ParentSpace> = client
            .get_room(child_space_id_2)
            .unwrap()
            .parent_spaces()
            .await
            .unwrap()
            .map(Result::unwrap)
            .collect()
            .await;

        assert_let!(ParentSpace::Reciprocal(parent) = spaces.last().unwrap());
        assert_eq!(parent.room_id(), parent_space.room_id());
    }

    #[async_test]
    async fn test_joined_spaces_updates() {
        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;
        let user_id = client.user_id().unwrap();
        let factory = EventFactory::new();

        server.mock_room_state_encryption().plain().mount().await;

        let first_space_id = room_id!("!first_space:example.org");
        let second_space_id = room_id!("!second_space:example.org");

        // Join the first space
        server
            .sync_room(
                &client,
                JoinedRoomBuilder::new(first_space_id)
                    .add_state_event(factory.create(user_id, RoomVersionId::V1).with_space_type()),
            )
            .await;

        // Build the `SpaceService` and expect the room to show up with no updates
        // pending

        let space_service = SpaceService::new(client.clone()).await;

        let joined_spaces_subscriber = space_service.subscribe_to_joined_spaces();
        pin_mut!(joined_spaces_subscriber);
        assert_pending!(joined_spaces_subscriber);

        assert_eq!(
            space_service.joined_spaces(),
            vec![SpaceServiceRoom::new_from_known(client.get_room(first_space_id).unwrap())]
        );

        // Join the second space

        server
            .sync_room(
                &client,
                JoinedRoomBuilder::new(second_space_id)
                    .add_state_event(factory.create(user_id, RoomVersionId::V1).with_space_type()),
            )
            .await;

        // And expect the list to update
        assert_eq!(
            space_service.joined_spaces(),
            vec![
                SpaceServiceRoom::new_from_known(client.get_room(first_space_id).unwrap()),
                SpaceServiceRoom::new_from_known(client.get_room(second_space_id).unwrap())
            ]
        );

        // The subscriber yields new results when a space is joined
        assert_next_eq!(
            joined_spaces_subscriber,
            vec![
                SpaceServiceRoom::new_from_known(client.get_room(first_space_id).unwrap()),
                SpaceServiceRoom::new_from_known(client.get_room(second_space_id).unwrap())
            ]
        );

        server.sync_room(&client, LeftRoomBuilder::new(second_space_id)).await;

        // and when one is left
        assert_next_eq!(
            joined_spaces_subscriber,
            vec![SpaceServiceRoom::new_from_known(client.get_room(first_space_id).unwrap())]
        );

        // but it doesn't when a non-space room gets joined
        server
            .sync_room(
                &client,
                JoinedRoomBuilder::new(room_id!("!room:example.org"))
                    .add_state_event(factory.create(user_id, RoomVersionId::V1)),
            )
            .await;

        // and the subscriber doesn't yield any updates
        assert_pending!(joined_spaces_subscriber);
        assert_eq!(
            space_service.joined_spaces(),
            vec![SpaceServiceRoom::new_from_known(client.get_room(first_space_id).unwrap())]
        );
    }
}
