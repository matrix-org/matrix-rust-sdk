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
//! The `SpaceService` is an UI oriented, high-level interface for working with
//! [Matrix Spaces](https://spec.matrix.org/latest/client-server-api/#spaces).
//! It provides methods to retrieve joined spaces, subscribe
//! to updates, and navigate space hierarchies.
//!
//! It consists of 3 main components:
//! - `SpaceService`: The main service for managing spaces. It
//! - `SpaceGraph`: An utility that maps the `m.space.parent` and
//!   `m.space.child` fields into a graph structure, removing cycles and
//!   providing access to top level parents.
//! - `SpaceRoomList`: A component for retrieving a space's children rooms and
//!   their details.

use std::{cmp::Ordering, collections::HashMap, sync::Arc};

use eyeball_im::{ObservableVector, VectorSubscriberBatchedStream};
use futures_util::pin_mut;
use imbl::Vector;
use itertools::Itertools;
use matrix_sdk::{
    Client, Error as SDKError, deserialized_responses::SyncOrStrippedState, executor::AbortOnDrop,
};
use matrix_sdk_common::executor::spawn;
use ruma::{
    OwnedRoomId, RoomId,
    events::{
        self, SyncStateEvent,
        space::{child::SpaceChildEventContent, parent::SpaceParentEventContent},
    },
};
use thiserror::Error;
use tokio::sync::Mutex as AsyncMutex;
use tracing::error;

use crate::spaces::{graph::SpaceGraph, leave::LeaveSpaceHandle};
pub use crate::spaces::{room::SpaceRoom, room_list::SpaceRoomList};

pub mod graph;
pub mod leave;
pub mod room;
pub mod room_list;

/// Possible [`SpaceService`] errors.
#[derive(Debug, Error)]
pub enum Error {
    /// The requested room was not found.
    #[error("Room `{0}` not found")]
    RoomNotFound(OwnedRoomId),

    /// Failed to leave a space.
    #[error("Failed to leave space")]
    LeaveSpace(SDKError),

    /// Failed to load members.
    #[error("Failed to load members")]
    LoadRoomMembers(SDKError),
}

struct SpaceState {
    graph: SpaceGraph,
    joined_rooms: ObservableVector<SpaceRoom>,
}

/// The main entry point into the Spaces facilities.
///
/// The spaces service is responsible for retrieving one's joined rooms,
/// building a graph out of their `m.space.parent` and `m.space.child` state
/// events, and providing access to the top-level spaces and their children.
///
/// # Examples
///
/// ```no_run
/// use futures_util::StreamExt;
/// use matrix_sdk::Client;
/// use matrix_sdk_ui::spaces::SpaceService;
/// use ruma::owned_room_id;
///
/// # async {
/// # let client: Client = todo!();
/// let space_service = SpaceService::new(client.clone());
///
/// // Get a list of all the joined spaces
/// let joined_spaces = space_service.joined_spaces().await;
///
/// // And subscribe to changes on them
/// // `initial_values` is equal to `joined_spaces` if nothing changed meanwhile
/// let (initial_values, stream) =
///     space_service.subscribe_to_joined_spaces().await;
///
/// while let Some(diffs) = stream.next().await {
///     println!("Received joined spaces updates: {diffs:?}");
/// }
///
/// // Get a list of all the rooms in a particular space
/// let room_list = space_service
///     .space_room_list(owned_room_id!("!some_space:example.org"))
///     .await;
///
/// // Which can be used to retrieve information about the children rooms
/// let children = room_list.rooms();
/// # anyhow::Ok(()) };
/// ```
pub struct SpaceService {
    client: Client,

    space_state: Arc<AsyncMutex<SpaceState>>,

    room_update_handle: AsyncMutex<Option<AbortOnDrop<()>>>,
}

impl SpaceService {
    /// Creates a new `SpaceService` instance.
    pub fn new(client: Client) -> Self {
        Self {
            client,
            space_state: Arc::new(AsyncMutex::new(SpaceState {
                graph: SpaceGraph::new(),
                joined_rooms: ObservableVector::new(),
            })),
            room_update_handle: AsyncMutex::new(None),
        }
    }

    /// Subscribes to updates on the joined spaces list. If space rooms are
    /// joined or left, the stream will yield diffs that reflect the changes.
    pub async fn subscribe_to_joined_spaces(
        &self,
    ) -> (Vector<SpaceRoom>, VectorSubscriberBatchedStream<SpaceRoom>) {
        let mut room_update_handle = self.room_update_handle.lock().await;

        if room_update_handle.is_none() {
            let client = self.client.clone();
            let space_state = Arc::clone(&self.space_state);
            let all_room_updates_receiver = self.client.subscribe_to_all_room_updates();

            *room_update_handle = Some(AbortOnDrop::new(spawn(async move {
                pin_mut!(all_room_updates_receiver);

                loop {
                    match all_room_updates_receiver.recv().await {
                        Ok(updates) => {
                            if updates.is_empty() {
                                continue;
                            }

                            let (spaces, graph) = Self::joined_spaces_for(&client).await;
                            Self::update_joined_spaces_if_needed(
                                Vector::from(spaces),
                                graph,
                                &space_state,
                            )
                            .await;
                        }
                        Err(err) => {
                            error!("error when listening to room updates: {err}");
                        }
                    }
                }
            })));

            // Make sure to also update the currently joined spaces for the initial values.
            let (spaces, graph) = Self::joined_spaces_for(&self.client).await;
            Self::update_joined_spaces_if_needed(Vector::from(spaces), graph, &self.space_state)
                .await;
        }

        self.space_state.lock().await.joined_rooms.subscribe().into_values_and_batched_stream()
    }

    /// Returns a list of all the top-level joined spaces. It will eagerly
    /// compute the latest version and also notify subscribers if there were
    /// any changes.
    pub async fn joined_spaces(&self) -> Vec<SpaceRoom> {
        let (spaces, graph) = Self::joined_spaces_for(&self.client).await;

        Self::update_joined_spaces_if_needed(
            Vector::from(spaces.clone()),
            graph,
            &self.space_state,
        )
        .await;

        spaces
    }

    /// Returns a `SpaceRoomList` for the given space ID.
    pub async fn space_room_list(&self, space_id: OwnedRoomId) -> SpaceRoomList {
        SpaceRoomList::new(self.client.clone(), space_id).await
    }

    /// Start a space leave process returning a [`LeaveSpaceHandle`] from which
    /// rooms can be retrieved in reversed BFS order starting from the requested
    /// `space_id` graph node. If the room is unknown then an error will be
    /// returned.
    ///
    /// Once the rooms to be left are chosen the handle can be used to leave
    /// them.
    pub async fn leave_space(&self, space_id: &RoomId) -> Result<LeaveSpaceHandle, Error> {
        let space_state = self.space_state.lock().await;

        if !space_state.graph.has_node(space_id) {
            return Err(Error::RoomNotFound(space_id.to_owned()));
        }

        let room_ids = space_state.graph.flattened_bottom_up_subtree(space_id);

        let handle = LeaveSpaceHandle::new(self.client.clone(), room_ids).await;

        Ok(handle)
    }

    async fn update_joined_spaces_if_needed(
        new_spaces: Vector<SpaceRoom>,
        new_graph: SpaceGraph,
        space_state: &Arc<AsyncMutex<SpaceState>>,
    ) {
        let mut space_state = space_state.lock().await;

        if new_spaces != space_state.joined_rooms.clone() {
            space_state.joined_rooms.clear();
            space_state.joined_rooms.append(new_spaces);
        }

        space_state.graph = new_graph;
    }

    async fn joined_spaces_for(client: &Client) -> (Vec<SpaceRoom>, SpaceGraph) {
        let joined_spaces = client.joined_space_rooms();

        // Build a graph to hold the parent-child relations
        let mut graph = SpaceGraph::new();

        // Iterate over all joined spaces and populate the graph with edges based
        // on `m.space.parent` and `m.space.child` state events.
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
                .filter_map(|child_event| match child_event.deserialize() {
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

        // Remove cycles from the graph. This is important because they are not
        // enforced backend side.
        graph.remove_cycles();

        let root_nodes = graph.root_nodes();

        // Proceed with filtering to the top level spaces, sorting them by their
        // (optional) order field (as defined in MSC3230) and then mapping them
        // to `SpaceRoom`s.
        let top_level_spaces = joined_spaces
            .iter()
            .filter(|room| root_nodes.contains(&room.room_id()))
            .collect::<Vec<_>>();

        let mut top_level_space_order = HashMap::new();
        for space in &top_level_spaces {
            if let Ok(Some(raw_event)) =
                space.account_data_static::<events::space_order::SpaceOrderEventContent>().await
                && let Ok(event) = raw_event.deserialize()
            {
                top_level_space_order.insert(space.room_id().to_owned(), event.content.order);
            }
        }

        let top_level_spaces = top_level_spaces
            .iter()
            .sorted_by(|a, b| {
                // MSC3230: lexicographically by `order` and then by room ID
                match (
                    top_level_space_order.get(a.room_id()),
                    top_level_space_order.get(b.room_id()),
                ) {
                    (Some(a_order), Some(b_order)) => {
                        a_order.cmp(b_order).then(a.room_id().cmp(b.room_id()))
                    }
                    (Some(_), None) => Ordering::Less,
                    (None, Some(_)) => Ordering::Greater,
                    (None, None) => a.room_id().cmp(b.room_id()),
                }
            })
            .map(|room| {
                SpaceRoom::new_from_known(room, graph.children_of(room.room_id()).len() as u64)
            })
            .collect();

        (top_level_spaces, graph)
    }
}

#[cfg(test)]
mod tests {
    use assert_matches2::assert_let;
    use eyeball_im::VectorDiff;
    use futures_util::{StreamExt, pin_mut};
    use matrix_sdk::{room::ParentSpace, test_utils::mocks::MatrixMockServer};
    use matrix_sdk_test::{
        JoinedRoomBuilder, LeftRoomBuilder, RoomAccountDataTestEvent, async_test,
        event_factory::EventFactory,
    };
    use ruma::{RoomVersionId, UserId, owned_room_id, room_id};
    use serde_json::json;
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

        let (initial_values, joined_spaces_subscriber) =
            space_service.subscribe_to_joined_spaces().await;
        pin_mut!(joined_spaces_subscriber);
        assert_pending!(joined_spaces_subscriber);

        assert_eq!(
            initial_values,
            vec![SpaceRoom::new_from_known(&client.get_room(first_space_id).unwrap(), 0)].into()
        );

        assert_eq!(
            space_service.joined_spaces().await,
            vec![SpaceRoom::new_from_known(&client.get_room(first_space_id).unwrap(), 0)]
        );

        // And the stream is still pending as the initial values were
        // already set.
        assert_pending!(joined_spaces_subscriber);

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
                SpaceRoom::new_from_known(&client.get_room(first_space_id).unwrap(), 0),
                SpaceRoom::new_from_known(&client.get_room(second_space_id).unwrap(), 1)
            ]
        );

        assert_next_eq!(
            joined_spaces_subscriber,
            vec![
                VectorDiff::Clear,
                VectorDiff::Append {
                    values: vec![
                        SpaceRoom::new_from_known(&client.get_room(first_space_id).unwrap(), 0),
                        SpaceRoom::new_from_known(&client.get_room(second_space_id).unwrap(), 1)
                    ]
                    .into()
                },
            ]
        );

        server.sync_room(&client, LeftRoomBuilder::new(second_space_id)).await;

        // and when one is left
        assert_next_eq!(
            joined_spaces_subscriber,
            vec![
                VectorDiff::Clear,
                VectorDiff::Append {
                    values: vec![SpaceRoom::new_from_known(
                        &client.get_room(first_space_id).unwrap(),
                        0
                    )]
                    .into()
                },
            ]
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
            vec![SpaceRoom::new_from_known(&client.get_room(first_space_id).unwrap(), 0)]
        );
    }

    #[async_test]
    async fn test_top_level_space_order() {
        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;

        server.mock_room_state_encryption().plain().mount().await;

        add_space_rooms_with(
            vec![
                (room_id!("!2:a.b"), Some("2")),
                (room_id!("!4:a.b"), None),
                (room_id!("!3:a.b"), None),
                (room_id!("!1:a.b"), Some("1")),
            ],
            &client,
            &server,
            &EventFactory::new(),
            client.user_id().unwrap(),
        )
        .await;

        let space_service = SpaceService::new(client.clone());

        // Space with an `order` field set should come first in lexicographic
        // order and rest sorted by room ID.
        assert_eq!(
            space_service.joined_spaces().await,
            vec![
                SpaceRoom::new_from_known(&client.get_room(room_id!("!1:a.b")).unwrap(), 0),
                SpaceRoom::new_from_known(&client.get_room(room_id!("!2:a.b")).unwrap(), 0),
                SpaceRoom::new_from_known(&client.get_room(room_id!("!3:a.b")).unwrap(), 0),
                SpaceRoom::new_from_known(&client.get_room(room_id!("!4:a.b")).unwrap(), 0),
            ]
        );
    }

    async fn add_space_rooms_with(
        rooms: Vec<(&RoomId, Option<&str>)>,
        client: &Client,
        server: &MatrixMockServer,
        factory: &EventFactory,
        user_id: &UserId,
    ) {
        for (room_id, order) in rooms {
            let mut builder = JoinedRoomBuilder::new(room_id)
                .add_state_event(factory.create(user_id, RoomVersionId::V1).with_space_type());

            if let Some(order) = order {
                builder = builder.add_account_data(RoomAccountDataTestEvent::Custom(json!({
                    "type": "m.space_order",
                      "content": {
                        "order": order
                      }
                })));
            }

            server.sync_room(client, builder).await;
        }
    }
}
