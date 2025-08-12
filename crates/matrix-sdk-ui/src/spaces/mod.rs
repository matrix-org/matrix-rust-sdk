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
use matrix_sdk::{Client, deserialized_responses::SyncOrStrippedState, locks::Mutex};
use matrix_sdk_common::executor::{JoinHandle, spawn};
use ruma::{
    OwnedRoomId,
    events::{
        SyncStateEvent,
        space::{child::SpaceChildEventContent, parent::SpaceParentEventContent},
    },
};
use tracing::error;

use crate::spaces::graph::SpaceGraph;
pub use crate::spaces::{room::SpaceRoom, room_list::SpaceRoomList};

pub mod graph;
pub mod room;
pub mod room_list;

pub struct SpaceService {
    client: Client,

    joined_spaces: SharedObservable<Vec<SpaceRoom>>,

    room_update_handle: Mutex<Option<JoinHandle<()>>>,
}

impl Drop for SpaceService {
    fn drop(&mut self) {
        if let Some(handle) = &*self.room_update_handle.lock() {
            handle.abort();
        }
    }
}

impl SpaceService {
    pub fn new(client: Client) -> Self {
        Self {
            client,
            joined_spaces: SharedObservable::new(Vec::new()),
            room_update_handle: Mutex::new(None),
        }
    }

    pub fn subscribe_to_joined_spaces(&self) -> Subscriber<Vec<SpaceRoom>> {
        if self.room_update_handle.lock().is_none() {
            let client_clone = self.client.clone();
            let joined_spaces_clone = self.joined_spaces.clone();
            let all_room_updates_receiver = self.client.subscribe_to_all_room_updates();

            *self.room_update_handle.lock() = Some(spawn(async move {
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
            }));
        }

        self.joined_spaces.subscribe()
    }

    pub async fn joined_spaces(&self) -> Vec<SpaceRoom> {
        let spaces = Self::joined_spaces_for(&self.client).await;

        if spaces != self.joined_spaces.get() {
            self.joined_spaces.set(spaces.clone());
        }

        spaces
    }

    pub fn space_room_list(&self, space_id: OwnedRoomId) -> SpaceRoomList {
        SpaceRoomList::new(self.client.clone(), space_id)
    }

    async fn joined_spaces_for(client: &Client) -> Vec<SpaceRoom> {
        let joined_spaces = client
            .joined_rooms()
            .into_iter()
            .filter_map(|room| room.is_space().then_some(room))
            .collect::<Vec<_>>();

        let mut graph = SpaceGraph::new();

        for space in joined_spaces.iter() {
            graph.add_node(space.room_id().to_owned());

            if let Ok(parents) = space.get_state_events_static::<SpaceParentEventContent>().await {
                parents.into_iter()
                .flat_map(|parent_event| match parent_event.deserialize() {
                    Ok(SyncOrStrippedState::Sync(SyncStateEvent::Original(e))) => {
                        Some(e.state_key)
                    }
                    Ok(SyncOrStrippedState::Sync(SyncStateEvent::Redacted(_))) => None,
                    Ok(SyncOrStrippedState::Stripped(e)) => Some(e.state_key),
                    Err(e) => {
                        error!(room_id = ?space.room_id(), "Could not deserialize m.space.parent: {e}");
                        None
                    }
                }).for_each(|parent| graph.add_edge(parent, space.room_id().to_owned()));
            } else {
                error!(room_id = ?space.room_id(), "Could not get m.space.parent events");
            }

            if let Ok(children) = space.get_state_events_static::<SpaceChildEventContent>().await {
                children.into_iter()
                .flat_map(|child_event| match child_event.deserialize() {
                    Ok(SyncOrStrippedState::Sync(SyncStateEvent::Original(e))) => {
                        Some(e.state_key)
                    }
                    Ok(SyncOrStrippedState::Sync(SyncStateEvent::Redacted(_))) => None,
                    Ok(SyncOrStrippedState::Stripped(e)) => Some(e.state_key),
                    Err(e) => {
                        error!(room_id = ?space.room_id(), "Could not deserialize m.space.child: {e}");
                        None
                    }
                }).for_each(|child| graph.add_edge(space.room_id().to_owned(), child));
            } else {
                error!(room_id = ?space.room_id(), "Could not get m.space.child events");
            }
        }

        graph.remove_cycles();

        let root_notes = graph.root_nodes();

        joined_spaces
            .iter()
            .flat_map(|room| {
                let room_id = room.room_id().to_owned();

                if root_notes.contains(&&room_id) {
                    Some(SpaceRoom::new_from_known(
                        room.clone(),
                        graph.children_of(&room_id).len() as u64,
                    ))
                } else {
                    None
                }
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use assert_matches2::assert_let;
    use futures_util::{StreamExt, pin_mut};
    use matrix_sdk::{room::ParentSpace, test_utils::mocks::MatrixMockServer};
    use matrix_sdk_test::{
        JoinedRoomBuilder, LeftRoomBuilder, async_test, event_factory::EventFactory,
    };
    use ruma::{RoomVersionId, owned_room_id, room_id};
    use stream_assert::{assert_next_eq, assert_pending};

    use super::*;

    #[async_test]
    async fn test_spaces_hierarchy() {
        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;
        let user_id = client.user_id().unwrap();
        let space_service = SpaceService::new(client.clone());
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
            space_service
                .joined_spaces()
                .await
                .iter()
                .map(|s| s.room_id.to_owned())
                .collect::<Vec<_>>(),
            vec![parent_space_id]
        );

        // and it has 2 children
        assert_eq!(
            space_service
                .joined_spaces()
                .await
                .iter()
                .map(|s| s.children_count)
                .collect::<Vec<_>>(),
            vec![2]
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

        let space_service = SpaceService::new(client.clone());

        let joined_spaces_subscriber = space_service.subscribe_to_joined_spaces();
        pin_mut!(joined_spaces_subscriber);
        assert_pending!(joined_spaces_subscriber);

        assert_eq!(
            space_service.joined_spaces().await,
            vec![SpaceRoom::new_from_known(client.get_room(first_space_id).unwrap(), 0)]
        );

        // Join the second space

        server
            .sync_room(
                &client,
                JoinedRoomBuilder::new(second_space_id)
                    .add_state_event(factory.create(user_id, RoomVersionId::V1).with_space_type())
                    .add_state_event(
                        factory
                            .space_child(
                                second_space_id.to_owned(),
                                owned_room_id!("!child:example.org"),
                            )
                            .sender(user_id),
                    ),
            )
            .await;

        // And expect the list to update
        assert_eq!(
            space_service.joined_spaces().await,
            vec![
                SpaceRoom::new_from_known(client.get_room(first_space_id).unwrap(), 0),
                SpaceRoom::new_from_known(client.get_room(second_space_id).unwrap(), 1)
            ]
        );

        // The subscriber yields new results when a space is joined
        assert_next_eq!(
            joined_spaces_subscriber,
            vec![
                SpaceRoom::new_from_known(client.get_room(first_space_id).unwrap(), 0),
                SpaceRoom::new_from_known(client.get_room(second_space_id).unwrap(), 1)
            ]
        );

        server.sync_room(&client, LeftRoomBuilder::new(second_space_id)).await;

        // and when one is left
        assert_next_eq!(
            joined_spaces_subscriber,
            vec![SpaceRoom::new_from_known(client.get_room(first_space_id).unwrap(), 0)]
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
            space_service.joined_spaces().await,
            vec![SpaceRoom::new_from_known(client.get_room(first_space_id).unwrap(), 0)]
        );
    }
}
