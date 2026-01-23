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
    Client, Error as SDKError, Room, deserialized_responses::SyncOrStrippedState,
    executor::AbortOnDrop,
};
use matrix_sdk_common::executor::spawn;
use ruma::{
    OwnedRoomId, RoomId,
    events::{
        self, StateEventType, SyncStateEvent,
        space::{child::SpaceChildEventContent, parent::SpaceParentEventContent},
    },
};
use thiserror::Error;
use tokio::sync::Mutex as AsyncMutex;
use tracing::{error, warn};

use crate::spaces::{graph::SpaceGraph, leave::LeaveSpaceHandle, room::SpaceRoomChildState};
pub use crate::spaces::{room::SpaceRoom, room_list::SpaceRoomList};

pub mod graph;
pub mod leave;
pub mod room;
pub mod room_list;

/// Possible [`SpaceService`] errors.
#[derive(Debug, Error)]
pub enum Error {
    /// The user ID was not available from the client.
    #[error("User ID not available from client")]
    UserIdNotFound,

    /// The requested room was not found.
    #[error("Room `{0}` not found")]
    RoomNotFound(OwnedRoomId),

    /// The space parent/child state was missing.
    #[error("Missing `{0}` for `{1}`")]
    MissingState(StateEventType, OwnedRoomId),

    /// Failed to set the expected m.space.parent or m.space.child state events.
    #[error("Failed updating space parent/child relationship")]
    UpdateRelationship(SDKError),

    /// Failed to leave a space.
    #[error("Failed to leave space")]
    LeaveSpace(SDKError),

    /// Failed to load members.
    #[error("Failed to load members")]
    LoadRoomMembers(SDKError),
}

struct SpaceState {
    graph: SpaceGraph,
    top_level_joined_spaces: ObservableVector<SpaceRoom>,
    space_filters: ObservableVector<SpaceFilter>,
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
/// let space_service = SpaceService::new(client.clone()).await;
///
/// // Get a list of all the joined spaces
/// let joined_spaces = space_service.top_level_joined_spaces().await;
///
/// // And subscribe to changes on them
/// // `initial_values` is equal to `top_level_joined_spaces` if nothing changed meanwhile
/// let (initial_values, stream) =
///     space_service.subscribe_to_top_level_joined_spaces().await;
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

    _room_update_handle: AsyncMutex<AbortOnDrop<()>>,
}

impl SpaceService {
    /// Creates a new `SpaceService` instance.
    pub async fn new(client: Client) -> Self {
        let space_state = Arc::new(AsyncMutex::new(SpaceState {
            graph: SpaceGraph::new(),
            top_level_joined_spaces: ObservableVector::new(),
            space_filters: ObservableVector::new(),
        }));

        let room_update_handle = spawn({
            let client = client.clone();
            let space_state = Arc::clone(&space_state);
            let all_room_updates_receiver = client.subscribe_to_all_room_updates();

            async move {
                pin_mut!(all_room_updates_receiver);

                loop {
                    match all_room_updates_receiver.recv().await {
                        Ok(updates) => {
                            if updates.is_empty() {
                                continue;
                            }

                            let (spaces, filters, graph) = Self::build_space_state(&client).await;
                            Self::update_space_state_if_needed(
                                Vector::from(spaces),
                                Vector::from(filters),
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
            }
        });

        // Make sure to also update the currently joined spaces for the initial values.
        let (spaces, filters, graph) = Self::build_space_state(&client).await;
        Self::update_space_state_if_needed(
            Vector::from(spaces),
            Vector::from(filters),
            graph,
            &space_state,
        )
        .await;

        Self {
            client,
            space_state,
            _room_update_handle: AsyncMutex::new(AbortOnDrop::new(room_update_handle)),
        }
    }

    /// Subscribes to updates on the joined spaces list. If space rooms are
    /// joined or left, the stream will yield diffs that reflect the changes.
    pub async fn subscribe_to_top_level_joined_spaces(
        &self,
    ) -> (Vector<SpaceRoom>, VectorSubscriberBatchedStream<SpaceRoom>) {
        self.space_state
            .lock()
            .await
            .top_level_joined_spaces
            .subscribe()
            .into_values_and_batched_stream()
    }

    /// Returns a list of all the top-level joined spaces. It will eagerly
    /// compute the latest version and also notify subscribers if there were
    /// any changes.
    pub async fn top_level_joined_spaces(&self) -> Vec<SpaceRoom> {
        let (top_level_joined_spaces, filters, graph) = Self::build_space_state(&self.client).await;

        Self::update_space_state_if_needed(
            Vector::from(top_level_joined_spaces.clone()),
            Vector::from(filters),
            graph,
            &self.space_state,
        )
        .await;

        top_level_joined_spaces
    }

    /// Space filters provide access to a custom subset of the space graph that
    /// can be used in tandem with the [`crate::RoomListService`] to narrow
    /// down the presented rooms. A [`crate::room_list_service::RoomList`]'s
    /// [`crate::room_list_service::RoomListDynamicEntriesController`] can take
    /// a filter, which in this case can be a
    /// [`crate::room_list_service::filters::new_filter_identifiers`]
    /// pointing to the space descendants retrieved from the filters.
    ///
    /// They are limited to the first 2 levels of the graph, with the first
    /// level only containing direct descendants while the second holds the rest
    /// of them recursively.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use futures_util::StreamExt;
    /// use matrix_sdk::Client;
    /// use matrix_sdk_ui::{
    ///     room_list_service::{RoomListService, filters},
    ///     spaces::SpaceService,
    /// };
    /// use ruma::owned_room_id;
    ///
    /// # async {
    /// # let client: Client = todo!();
    /// let space_service = SpaceService::new(client.clone()).await;
    /// let room_list_service = RoomListService::new(client.clone()).await?;
    ///
    /// // Get the list of filters derived from the space hierarchy.
    /// let space_filters = space_service.space_filters().await;
    /// // Pick a filter/space
    /// let space_filter = space_filters.first().unwrap();
    ///
    /// // Create a room list stream and a controller that accepts filters.
    /// let all_rooms = room_list_service.all_rooms().await?;
    /// let (_, controller) = all_rooms.entries_with_dynamic_adapters(25);
    ///
    /// // Apply an identifiers filter built from the space filter descendants.
    /// controller.set_filter(Box::new(filters::new_filter_identifiers(
    ///     space_filter.descendants.clone(),
    /// )));
    ///
    /// # anyhow::Ok(()) };
    /// ```
    pub async fn space_filters(&self) -> Vec<SpaceFilter> {
        let (top_level_joined_spaces, filters, graph) = Self::build_space_state(&self.client).await;

        Self::update_space_state_if_needed(
            Vector::from(top_level_joined_spaces),
            Vector::from(filters.clone()),
            graph,
            &self.space_state,
        )
        .await;

        filters
    }

    /// Subscribe to changes or updates to the space filters.
    pub async fn subscribe_to_space_filters(
        &self,
    ) -> (Vector<SpaceFilter>, VectorSubscriberBatchedStream<SpaceFilter>) {
        self.space_state.lock().await.space_filters.subscribe().into_values_and_batched_stream()
    }

    /// Returns a flattened list containing all the spaces where the user has
    /// permission to send `m.space.child` state events.
    ///
    /// Note: Unlike [`Self::top_level_joined_spaces()`], this method does not
    /// recompute graph, nor does it notify subscribers about changes.
    pub async fn editable_spaces(&self) -> Vec<SpaceRoom> {
        let Some(user_id) = self.client.user_id() else {
            return vec![];
        };

        let graph = &self.space_state.lock().await.graph;
        let rooms = self.client.joined_space_rooms();

        let mut editable_spaces = Vec::new();
        for room in &rooms {
            if let Ok(power_levels) = room.power_levels().await
                && power_levels.user_can_send_state(user_id, StateEventType::SpaceChild)
            {
                let room_id = room.room_id();
                editable_spaces
                    .push(SpaceRoom::new_from_known(room, graph.children_of(room_id).len() as u64));
            }
        }

        editable_spaces
    }

    /// Returns a `SpaceRoomList` for the given space ID.
    pub async fn space_room_list(&self, space_id: OwnedRoomId) -> SpaceRoomList {
        SpaceRoomList::new(self.client.clone(), space_id).await
    }

    /// Returns all known direct-parents of a given space room ID.
    pub async fn joined_parents_of_child(&self, child_id: &RoomId) -> Vec<SpaceRoom> {
        let graph = &self.space_state.lock().await.graph;

        graph
            .parents_of(child_id)
            .into_iter()
            .filter_map(|parent_id| self.client.get_room(parent_id))
            .map(|room| {
                SpaceRoom::new_from_known(&room, graph.children_of(room.room_id()).len() as u64)
            })
            .collect()
    }

    /// Returns the corresponding `SpaceRoom` for the given room ID, or `None`
    /// if it isn't known.
    pub async fn get_space_room(&self, room_id: &RoomId) -> Option<SpaceRoom> {
        let graph = &self.space_state.lock().await.graph;

        if graph.has_node(room_id)
            && let Some(room) = self.client.get_room(room_id)
        {
            Some(SpaceRoom::new_from_known(&room, graph.children_of(room.room_id()).len() as u64))
        } else {
            None
        }
    }

    pub async fn add_child_to_space(
        &self,
        child_id: OwnedRoomId,
        space_id: OwnedRoomId,
    ) -> Result<(), Error> {
        let user_id = self.client.user_id().ok_or(Error::UserIdNotFound)?;
        let space_room =
            self.client.get_room(&space_id).ok_or(Error::RoomNotFound(space_id.to_owned()))?;
        let child_room =
            self.client.get_room(&child_id).ok_or(Error::RoomNotFound(child_id.to_owned()))?;
        let child_power_levels = child_room
            .power_levels()
            .await
            .map_err(|error| Error::UpdateRelationship(matrix_sdk::Error::from(error)))?;

        // Add the child to the space.
        let child_route = child_room.route().await.map_err(Error::UpdateRelationship)?;
        space_room
            .send_state_event_for_key(&child_id, SpaceChildEventContent::new(child_route))
            .await
            .map_err(Error::UpdateRelationship)?;

        // Add the space as parent of the child if allowed.
        if child_power_levels.user_can_send_state(user_id, StateEventType::SpaceParent) {
            let parent_route = space_room.route().await.map_err(Error::UpdateRelationship)?;
            child_room
                .send_state_event_for_key(&space_id, SpaceParentEventContent::new(parent_route))
                .await
                .map_err(Error::UpdateRelationship)?;
        } else {
            warn!("The current user doesn't have permission to set the child's parent.");
        }

        Ok(())
    }

    pub async fn remove_child_from_space(
        &self,
        child_id: OwnedRoomId,
        space_id: OwnedRoomId,
    ) -> Result<(), Error> {
        let space_room =
            self.client.get_room(&space_id).ok_or(Error::RoomNotFound(space_id.to_owned()))?;

        if let Ok(Some(_)) =
            space_room.get_state_event_static_for_key::<SpaceChildEventContent, _>(&child_id).await
        {
            // Redacting state is a "weird" thing to do, so send {} instead.
            // https://github.com/matrix-org/matrix-spec/issues/2252
            //
            // Specifically, "The redaction of the state doesn't participate in state
            // resolution so behaves quite differently from e.g. sending an empty form of
            // that state events".
            space_room
                .send_state_event_raw("m.space.child", child_id.as_str(), serde_json::json!({}))
                .await
                .map_err(Error::UpdateRelationship)?;
        } else {
            warn!("A space child event wasn't found on the parent, ignoring.");
        }

        if let Some(child_room) = self.client.get_room(&child_id) {
            if let Ok(Some(_)) = child_room
                .get_state_event_static_for_key::<SpaceParentEventContent, _>(&space_id)
                .await
            {
                // Same as the comment above.
                child_room
                    .send_state_event_raw(
                        "m.space.parent",
                        space_id.as_str(),
                        serde_json::json!({}),
                    )
                    .await
                    .map_err(Error::UpdateRelationship)?;
            } else {
                warn!("A space parent event wasn't found on the child, ignoring.");
            }
        } else {
            warn!("The child room is unknown, skipping m.space.parent removal.");
        }

        Ok(())
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

    async fn update_space_state_if_needed(
        new_spaces: Vector<SpaceRoom>,
        new_filters: Vector<SpaceFilter>,
        new_graph: SpaceGraph,
        space_state: &Arc<AsyncMutex<SpaceState>>,
    ) {
        let mut space_state = space_state.lock().await;

        if new_spaces != space_state.top_level_joined_spaces.clone() {
            space_state.top_level_joined_spaces.clear();
            space_state.top_level_joined_spaces.append(new_spaces);
        }

        if new_filters != space_state.space_filters.clone() {
            space_state.space_filters.clear();
            space_state.space_filters.append(new_filters);
        }

        space_state.graph = new_graph;
    }

    async fn build_space_state(client: &Client) -> (Vec<SpaceRoom>, Vec<SpaceFilter>, SpaceGraph) {
        let joined_spaces = client.joined_space_rooms();

        // Build a graph to hold the parent-child relations
        let mut graph = SpaceGraph::new();

        // And also store `m.space.child` ordering info for later use
        let mut space_child_states = HashMap::<OwnedRoomId, SpaceRoomChildState>::new();

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
                        space_child_states.insert(
                            e.state_key.to_owned(),
                            SpaceRoomChildState {
                                order: e.content.order.clone(),
                                origin_server_ts: e.origin_server_ts,
                            },
                        );

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
        let top_level_space_rooms = joined_spaces
            .iter()
            .filter(|room| root_nodes.contains(&room.room_id()))
            .collect::<Vec<_>>();

        let mut top_level_space_order = HashMap::new();
        for space in &top_level_space_rooms {
            if let Ok(Some(raw_event)) =
                space.account_data_static::<events::space_order::SpaceOrderEventContent>().await
                && let Ok(event) = raw_event.deserialize()
            {
                top_level_space_order.insert(space.room_id().to_owned(), event.content.order);
            }
        }

        let top_level_space_rooms = top_level_space_rooms
            .into_iter()
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
            .collect::<Vec<_>>();

        let top_level_spaces = top_level_space_rooms
            .iter()
            .map(|room| {
                SpaceRoom::new_from_known(room, graph.children_of(room.room_id()).len() as u64)
            })
            .collect();

        let space_filters =
            Self::build_space_filters(client, &graph, top_level_space_rooms, space_child_states);

        (top_level_spaces, space_filters, graph)
    }

    /// Build the 2 levels required for space filters.
    /// As per product requirements, the first level space filters only include
    /// direct descendants while second level ones contain *all* descendants.
    ///
    /// The sorting mechanism is different between first level spaces/filters
    /// and second level ones so while the former are already sorted at this
    /// point the latter need to be manually taken care of here though the use
    /// of the collected `m.space.child` state event details.
    fn build_space_filters(
        client: &Client,
        graph: &SpaceGraph,
        top_level_space_rooms: Vec<&Room>,
        space_child_states: HashMap<OwnedRoomId, SpaceRoomChildState>,
    ) -> Vec<SpaceFilter> {
        let mut filters = Vec::new();
        for top_level_space in top_level_space_rooms {
            let children = graph
                .children_of(top_level_space.room_id())
                .into_iter()
                .map(|id| id.to_owned())
                .collect::<Vec<_>>();

            filters.push(SpaceFilter {
                space_room: SpaceRoom::new_from_known(top_level_space, children.len() as u64),
                level: 0,
                descendants: children.clone(),
            });

            filters.append(
                &mut children
                    .iter()
                    .filter_map(|id| client.get_room(id))
                    .filter(|room| room.is_space())
                    .map(|room| {
                        SpaceRoom::new_from_known(
                            &room,
                            graph.children_of(room.room_id()).len() as u64,
                        )
                    })
                    .sorted_by(|a, b| {
                        let a_state = space_child_states.get(&a.room_id).cloned();
                        let b_state = space_child_states.get(&b.room_id).cloned();

                        SpaceRoom::compare_rooms(a, b, a_state, b_state)
                    })
                    .map(|space_room| {
                        let descendants = graph.flattened_bottom_up_subtree(&space_room.room_id);

                        SpaceFilter { space_room, level: 1, descendants }
                    })
                    .collect::<Vec<_>>(),
            );
        }

        filters
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct SpaceFilter {
    /// The underlying [`SpaceRoom`]
    pub space_room: SpaceRoom,

    /// The level of the space filter in the tree/hierarchy.
    /// At this point in time the filters are limited to the first 2 levels.
    pub level: u8,

    /// The room identifiers of the descendants of this space.
    /// For top level spaces (level 0) these will be direct descendants while
    /// for first level spaces they will be all other descendants, recursively.
    pub descendants: Vec<OwnedRoomId>,
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use assert_matches2::assert_let;
    use eyeball_im::VectorDiff;
    use futures_util::{StreamExt, pin_mut};
    use matrix_sdk::{room::ParentSpace, test_utils::mocks::MatrixMockServer};
    use matrix_sdk_test::{
        JoinedRoomBuilder, LeftRoomBuilder, RoomAccountDataTestEvent, async_test,
        event_factory::EventFactory,
    };
    use ruma::{
        MilliSecondsSinceUnixEpoch, RoomVersionId, UserId, event_id, owned_room_id, room_id,
        serde::Raw,
    };
    use serde_json::json;
    use stream_assert::{assert_next_eq, assert_pending};

    use super::*;

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

        add_space_rooms(
            vec![
                MockSpaceRoomParameters {
                    room_id: child_space_id_1,
                    order: None,
                    parents: vec![parent_space_id],
                    children: vec![],
                    power_level: None,
                },
                MockSpaceRoomParameters {
                    room_id: child_space_id_2,
                    order: None,
                    parents: vec![parent_space_id],
                    children: vec![],
                    power_level: None,
                },
                MockSpaceRoomParameters {
                    room_id: parent_space_id,
                    order: None,
                    parents: vec![],
                    children: vec![child_space_id_1, child_space_id_2],
                    power_level: None,
                },
            ],
            &client,
            &server,
            &factory,
            user_id,
        )
        .await;

        // Only the parent space is returned
        assert_eq!(
            space_service
                .top_level_joined_spaces()
                .await
                .iter()
                .map(|s| s.room_id.to_owned())
                .collect::<Vec<_>>(),
            vec![parent_space_id]
        );

        // and it has 2 children
        assert_eq!(
            space_service
                .top_level_joined_spaces()
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

        let space_service = SpaceService::new(client.clone()).await;

        let (initial_values, joined_spaces_subscriber) =
            space_service.subscribe_to_top_level_joined_spaces().await;
        pin_mut!(joined_spaces_subscriber);
        assert_pending!(joined_spaces_subscriber);

        assert_eq!(
            initial_values,
            vec![SpaceRoom::new_from_known(&client.get_room(first_space_id).unwrap(), 0)].into()
        );

        assert_eq!(
            space_service.top_level_joined_spaces().await,
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
            space_service.top_level_joined_spaces().await,
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
            space_service.top_level_joined_spaces().await,
            vec![SpaceRoom::new_from_known(&client.get_room(first_space_id).unwrap(), 0)]
        );
    }

    #[async_test]
    async fn test_space_filters() {
        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;

        server.mock_room_state_encryption().plain().mount().await;

        add_space_rooms(
            vec![
                MockSpaceRoomParameters {
                    room_id: room_id!("!1:a.b"),
                    order: None,
                    parents: vec![],
                    children: vec![],
                    power_level: None,
                },
                MockSpaceRoomParameters {
                    room_id: room_id!("!1.2:a.b"),
                    order: None,
                    parents: vec![room_id!("!1:a.b")],
                    children: vec![],
                    power_level: None,
                },
                MockSpaceRoomParameters {
                    room_id: room_id!("!1.2.3:a.b"),
                    order: None,
                    parents: vec![room_id!("!1.2:a.b")],
                    children: vec![],
                    power_level: None,
                },
                MockSpaceRoomParameters {
                    room_id: room_id!("!1.2.3.4:a.b"),
                    order: None,
                    parents: vec![room_id!("!1.2.3:a.b")],
                    children: vec![],
                    power_level: None,
                },
            ],
            &client,
            &server,
            &EventFactory::new(),
            client.user_id().unwrap(),
        )
        .await;

        let space_service = SpaceService::new(client.clone()).await;

        let filters = space_service.space_filters().await;
        assert_eq!(filters.len(), 2);
        assert_eq!(filters[0].space_room.room_id, room_id!("!1:a.b"));
        assert_eq!(filters[0].level, 0);
        assert_eq!(filters[0].descendants.len(), 1); //
        assert_eq!(filters[1].space_room.room_id, room_id!("!1.2:a.b"));
        assert_eq!(filters[1].level, 1);
        assert_eq!(filters[1].descendants.len(), 3);

        let (initial_values, space_filters_subscriber) =
            space_service.subscribe_to_space_filters().await;
        pin_mut!(space_filters_subscriber);
        assert_pending!(space_filters_subscriber);

        assert_eq!(initial_values, filters.into());

        add_space_rooms(
            vec![MockSpaceRoomParameters {
                room_id: room_id!("!1.2.3.4.5:a.b"),
                order: None,
                parents: vec![room_id!("!1.2.3.4:a.b")],
                children: vec![],
                power_level: None,
            }],
            &client,
            &server,
            &EventFactory::new(),
            client.user_id().unwrap(),
        )
        .await;

        space_filters_subscriber.next().await;

        let filters = space_service.space_filters().await;
        assert_eq!(filters[0].descendants.len(), 1);
        assert_eq!(filters[1].descendants.len(), 4);
    }

    #[async_test]
    async fn test_top_level_space_order() {
        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;

        server.mock_room_state_encryption().plain().mount().await;

        add_space_rooms(
            vec![
                MockSpaceRoomParameters {
                    room_id: room_id!("!2:a.b"),
                    order: Some("2"),
                    parents: vec![],
                    children: vec![],
                    power_level: None,
                },
                MockSpaceRoomParameters {
                    room_id: room_id!("!4:a.b"),
                    order: None,
                    parents: vec![],
                    children: vec![],
                    power_level: None,
                },
                MockSpaceRoomParameters {
                    room_id: room_id!("!3:a.b"),
                    order: None,
                    parents: vec![],
                    children: vec![],
                    power_level: None,
                },
                MockSpaceRoomParameters {
                    room_id: room_id!("!1:a.b"),
                    order: Some("1"),
                    parents: vec![],
                    children: vec![],
                    power_level: None,
                },
            ],
            &client,
            &server,
            &EventFactory::new(),
            client.user_id().unwrap(),
        )
        .await;

        let space_service = SpaceService::new(client.clone()).await;

        // Space with an `order` field set should come first in lexicographic
        // order and rest sorted by room ID.
        assert_eq!(
            space_service.top_level_joined_spaces().await,
            vec![
                SpaceRoom::new_from_known(&client.get_room(room_id!("!1:a.b")).unwrap(), 0),
                SpaceRoom::new_from_known(&client.get_room(room_id!("!2:a.b")).unwrap(), 0),
                SpaceRoom::new_from_known(&client.get_room(room_id!("!3:a.b")).unwrap(), 0),
                SpaceRoom::new_from_known(&client.get_room(room_id!("!4:a.b")).unwrap(), 0),
            ]
        );
    }

    #[async_test]
    async fn test_editable_spaces() {
        // Given a space hierarchy where the user is admin of some spaces and subspaces.
        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;
        let user_id = client.user_id().unwrap();
        let factory = EventFactory::new();

        server.mock_room_state_encryption().plain().mount().await;

        let admin_space_id = room_id!("!admin_space:example.org");
        let admin_subspace_id = room_id!("!admin_subspace:example.org");
        let regular_space_id = room_id!("!regular_space:example.org");
        let regular_subspace_id = room_id!("!regular_subspace:example.org");

        add_space_rooms(
            vec![
                MockSpaceRoomParameters {
                    room_id: admin_space_id,
                    order: None,
                    parents: vec![],
                    children: vec![regular_subspace_id],
                    power_level: Some(100),
                },
                MockSpaceRoomParameters {
                    room_id: admin_subspace_id,
                    order: None,
                    parents: vec![regular_space_id],
                    children: vec![],
                    power_level: Some(100),
                },
                MockSpaceRoomParameters {
                    room_id: regular_space_id,
                    order: None,
                    parents: vec![],
                    children: vec![admin_subspace_id],
                    power_level: Some(0),
                },
                MockSpaceRoomParameters {
                    room_id: regular_subspace_id,
                    order: None,
                    parents: vec![admin_space_id],
                    children: vec![],
                    power_level: Some(0),
                },
            ],
            &client,
            &server,
            &factory,
            user_id,
        )
        .await;

        let space_service = SpaceService::new(client.clone()).await;

        // When retrieving all editable joined spaces.
        let editable_spaces = space_service.editable_spaces().await;

        // Then only the spaces where the user is admin are returned.
        assert_eq!(
            editable_spaces.iter().map(|room| room.room_id.to_owned()).collect::<Vec<_>>(),
            vec![admin_space_id.to_owned(), admin_subspace_id.to_owned()]
        );
    }

    #[async_test]
    async fn test_joined_parents_of_child() {
        // Given a space with three parent spaces, two of which are joined.
        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;
        let user_id = client.user_id().unwrap();
        let factory = EventFactory::new();

        server.mock_room_state_encryption().plain().mount().await;

        let parent_space_id_1 = room_id!("!parent_space_1:example.org");
        let parent_space_id_2 = room_id!("!parent_space_2:example.org");
        let unknown_parent_space_id = room_id!("!unknown_parent_space:example.org");
        let child_space_id = room_id!("!child_space:example.org");

        add_space_rooms(
            vec![
                MockSpaceRoomParameters {
                    room_id: child_space_id,
                    order: None,
                    parents: vec![parent_space_id_1, parent_space_id_2, unknown_parent_space_id],
                    children: vec![],
                    power_level: None,
                },
                MockSpaceRoomParameters {
                    room_id: parent_space_id_1,
                    order: None,
                    parents: vec![],
                    children: vec![child_space_id],
                    power_level: None,
                },
                MockSpaceRoomParameters {
                    room_id: parent_space_id_2,
                    order: None,
                    parents: vec![],
                    children: vec![child_space_id],
                    power_level: None,
                },
            ],
            &client,
            &server,
            &factory,
            user_id,
        )
        .await;

        let space_service = SpaceService::new(client.clone()).await;

        // When retrieving the joined parents of the child space
        let parents = space_service.joined_parents_of_child(child_space_id).await;

        // Then both parent spaces are returned
        assert_eq!(
            parents.iter().map(|space| space.room_id.to_owned()).collect::<Vec<_>>(),
            vec![parent_space_id_1, parent_space_id_2]
        );
    }

    #[async_test]
    async fn test_get_space_room_for_id() {
        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;
        let user_id = client.user_id().unwrap();
        let factory = EventFactory::new();

        server.mock_room_state_encryption().plain().mount().await;

        let space_id = room_id!("!single_space:example.org");

        add_space_rooms(
            vec![MockSpaceRoomParameters {
                room_id: space_id,
                order: None,
                parents: vec![],
                children: vec![],
                power_level: None,
            }],
            &client,
            &server,
            &factory,
            user_id,
        )
        .await;

        let space_service = SpaceService::new(client.clone()).await;

        let found = space_service.get_space_room(space_id).await;
        assert!(found.is_some());

        let expected = SpaceRoom::new_from_known(&client.get_room(space_id).unwrap(), 0);
        assert_eq!(found.unwrap(), expected);
    }

    #[async_test]
    async fn test_add_child_to_space() {
        // Given a space and child room where the user is admin of both.
        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;
        let user_id = client.user_id().unwrap();
        let factory = EventFactory::new();

        server.mock_room_state_encryption().plain().mount().await;

        let space_child_event_id = event_id!("$1");
        let space_parent_event_id = event_id!("$2");
        server.mock_set_space_child().ok(space_child_event_id.to_owned()).expect(1).mount().await;
        server.mock_set_space_parent().ok(space_parent_event_id.to_owned()).expect(1).mount().await;

        let space_id = room_id!("!my_space:example.org");
        let child_id = room_id!("!my_child:example.org");

        add_space_rooms(
            vec![
                MockSpaceRoomParameters {
                    room_id: space_id,
                    order: None,
                    parents: vec![],
                    children: vec![],
                    power_level: Some(100),
                },
                MockSpaceRoomParameters {
                    room_id: child_id,
                    order: None,
                    parents: vec![],
                    children: vec![],
                    power_level: Some(100),
                },
            ],
            &client,
            &server,
            &factory,
            user_id,
        )
        .await;

        let space_service = SpaceService::new(client.clone()).await;

        // When adding the child to the space.
        let result =
            space_service.add_child_to_space(child_id.to_owned(), space_id.to_owned()).await;

        // Then both space child and parent events are set successfully.
        assert!(result.is_ok());
    }

    #[async_test]
    async fn test_add_child_to_space_without_space_admin() {
        // Given a space and child room where the user is a regular member of both.
        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;
        let user_id = client.user_id().unwrap();
        let factory = EventFactory::new();

        server.mock_room_state_encryption().plain().mount().await;

        server.mock_set_space_child().unauthorized().expect(1).mount().await;
        server.mock_set_space_parent().unauthorized().expect(0).mount().await;

        let space_id = room_id!("!my_space:example.org");
        let child_id = room_id!("!my_child:example.org");

        add_space_rooms(
            vec![
                MockSpaceRoomParameters {
                    room_id: space_id,
                    order: None,
                    parents: vec![],
                    children: vec![],
                    power_level: Some(0),
                },
                MockSpaceRoomParameters {
                    room_id: child_id,
                    order: None,
                    parents: vec![],
                    children: vec![],
                    power_level: Some(0),
                },
            ],
            &client,
            &server,
            &factory,
            user_id,
        )
        .await;

        let space_service = SpaceService::new(client.clone()).await;

        // When adding the child to the space.
        let result =
            space_service.add_child_to_space(child_id.to_owned(), space_id.to_owned()).await;

        // Then the operation fails when trying to set the space child event and the
        // parent event is not attempted.
        assert!(result.is_err());
    }

    #[async_test]
    async fn test_add_child_to_space_without_child_admin() {
        // Given a space and child room where the user is admin of the space but not of
        // the child.
        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;
        let user_id = client.user_id().unwrap();
        let factory = EventFactory::new();

        server.mock_room_state_encryption().plain().mount().await;

        let space_child_event_id = event_id!("$1");
        server.mock_set_space_child().ok(space_child_event_id.to_owned()).expect(1).mount().await;
        server.mock_set_space_parent().unauthorized().expect(0).mount().await;

        let space_id = room_id!("!my_space:example.org");
        let child_id = room_id!("!my_child:example.org");

        add_space_rooms(
            vec![
                MockSpaceRoomParameters {
                    room_id: space_id,
                    order: None,
                    parents: vec![],
                    children: vec![],
                    power_level: Some(100),
                },
                MockSpaceRoomParameters {
                    room_id: child_id,
                    order: None,
                    parents: vec![],
                    children: vec![],
                    power_level: Some(0),
                },
            ],
            &client,
            &server,
            &factory,
            user_id,
        )
        .await;

        let space_service = SpaceService::new(client.clone()).await;

        // When adding the child to the space.
        let result =
            space_service.add_child_to_space(child_id.to_owned(), space_id.to_owned()).await;

        error!("result: {:?}", result);
        // Then the operation succeeds in setting the space child event and the parent
        // event is not attempted.
        assert!(result.is_ok());
    }

    #[async_test]
    async fn test_remove_child_from_space() {
        // Given a space and child room where the user is admin of both.
        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;
        let user_id = client.user_id().unwrap();
        let factory = EventFactory::new();

        server.mock_room_state_encryption().plain().mount().await;

        let space_child_event_id = event_id!("$1");
        let space_parent_event_id = event_id!("$2");
        server.mock_set_space_child().ok(space_child_event_id.to_owned()).expect(1).mount().await;
        server.mock_set_space_parent().ok(space_parent_event_id.to_owned()).expect(1).mount().await;

        let parent_id = room_id!("!parent_space:example.org");
        let child_id = room_id!("!child_space:example.org");

        add_space_rooms(
            vec![
                MockSpaceRoomParameters {
                    room_id: parent_id,
                    order: None,
                    parents: vec![],
                    children: vec![child_id],
                    power_level: None,
                },
                MockSpaceRoomParameters {
                    room_id: child_id,
                    order: None,
                    parents: vec![parent_id],
                    children: vec![],
                    power_level: None,
                },
            ],
            &client,
            &server,
            &factory,
            user_id,
        )
        .await;

        let space_service = SpaceService::new(client.clone()).await;

        // When removing the child from the space.
        let result =
            space_service.remove_child_from_space(child_id.to_owned(), parent_id.to_owned()).await;

        // Then both space child and parent events are removed successfully.
        assert!(result.is_ok());
    }

    #[async_test]
    async fn test_remove_child_from_space_without_parent_event() {
        // Given a space with a child where the m.space.parent event wasn't set.
        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;
        let user_id = client.user_id().unwrap();
        let factory = EventFactory::new();

        server.mock_room_state_encryption().plain().mount().await;

        let space_child_event_id = event_id!("$1");
        server.mock_set_space_child().ok(space_child_event_id.to_owned()).expect(1).mount().await;
        server.mock_set_space_parent().unauthorized().expect(0).mount().await;

        let parent_id = room_id!("!parent_space:example.org");
        let child_id = room_id!("!child_space:example.org");

        add_space_rooms(
            vec![
                MockSpaceRoomParameters {
                    room_id: parent_id,
                    order: None,
                    parents: vec![],
                    children: vec![child_id],
                    power_level: None,
                },
                MockSpaceRoomParameters {
                    room_id: child_id,
                    order: None,
                    parents: vec![],
                    children: vec![],
                    power_level: None,
                },
            ],
            &client,
            &server,
            &factory,
            user_id,
        )
        .await;

        let space_service = SpaceService::new(client.clone()).await;

        // When removing the child from the space.
        let result =
            space_service.remove_child_from_space(child_id.to_owned(), parent_id.to_owned()).await;

        // Then the child event is removed successfully and the parent event removal is
        // not attempted.
        assert!(result.is_ok());
    }

    #[async_test]
    async fn test_remove_child_from_space_without_child_event() {
        // Given a space with a child where the space's m.space.child event wasn't set.
        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;
        let user_id = client.user_id().unwrap();
        let factory = EventFactory::new();

        server.mock_room_state_encryption().plain().mount().await;

        let space_parent_event_id = event_id!("$2");
        server.mock_set_space_child().unauthorized().expect(0).mount().await;
        server.mock_set_space_parent().ok(space_parent_event_id.to_owned()).expect(1).mount().await;

        let parent_id = room_id!("!parent_space:example.org");
        let child_id = room_id!("!child_space:example.org");

        add_space_rooms(
            vec![
                MockSpaceRoomParameters {
                    room_id: parent_id,
                    order: None,
                    parents: vec![],
                    children: vec![],
                    power_level: None,
                },
                MockSpaceRoomParameters {
                    room_id: child_id,
                    order: None,
                    parents: vec![parent_id],
                    children: vec![],
                    power_level: None,
                },
            ],
            &client,
            &server,
            &factory,
            user_id,
        )
        .await;

        let space_service = SpaceService::new(client.clone()).await;

        // When removing the child from the space.
        let result =
            space_service.remove_child_from_space(child_id.to_owned(), parent_id.to_owned()).await;

        // Then the parent event is removed successfully and the child event removal is
        // not attempted.
        assert!(result.is_ok());
    }

    #[async_test]
    async fn test_remove_unknown_child_from_space() {
        // Given a space with a child room that is unknown (not in the client store).
        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;
        let user_id = client.user_id().unwrap();
        let factory = EventFactory::new();

        server.mock_room_state_encryption().plain().mount().await;

        let space_child_event_id = event_id!("$1");
        server.mock_set_space_child().ok(space_child_event_id.to_owned()).expect(1).mount().await;
        // The parent event should not be attempted since the child room is unknown.
        server.mock_set_space_parent().unauthorized().expect(0).mount().await;

        let parent_id = room_id!("!parent_space:example.org");
        let unknown_child_id = room_id!("!unknown_child:example.org");

        // Only add the parent space, not the child room.
        add_space_rooms(
            vec![MockSpaceRoomParameters {
                room_id: parent_id,
                order: None,
                parents: vec![],
                children: vec![unknown_child_id],
                power_level: None,
            }],
            &client,
            &server,
            &factory,
            user_id,
        )
        .await;

        // Verify that the child room is indeed unknown.
        assert!(client.get_room(unknown_child_id).is_none());

        let space_service = SpaceService::new(client.clone()).await;

        // When removing the unknown child from the space.
        let result = space_service
            .remove_child_from_space(unknown_child_id.to_owned(), parent_id.to_owned())
            .await;

        // Then the operation succeeds: the child event is removed from the space,
        // and the parent event removal is skipped since the child room is unknown.
        assert!(result.is_ok());
    }

    async fn add_space_rooms(
        rooms: Vec<MockSpaceRoomParameters>,
        client: &Client,
        server: &MatrixMockServer,
        factory: &EventFactory,
        user_id: &UserId,
    ) {
        for parameters in rooms {
            let mut builder = JoinedRoomBuilder::new(parameters.room_id)
                .add_state_event(factory.create(user_id, RoomVersionId::V1).with_space_type());

            if let Some(order) = parameters.order {
                builder = builder.add_account_data(RoomAccountDataTestEvent::Custom(json!({
                    "type": "m.space_order",
                      "content": {
                        "order": order
                      }
                })));
            }

            for parent_id in parameters.parents {
                builder = builder.add_state_event(
                    factory
                        .space_parent(parent_id.to_owned(), parameters.room_id.to_owned())
                        .sender(user_id),
                );
            }

            for child_id in parameters.children {
                builder = builder.add_state_event(
                    factory
                        .space_child(parameters.room_id.to_owned(), child_id.to_owned())
                        .sender(user_id),
                );
            }

            if let Some(power_level) = parameters.power_level {
                let mut power_levels = BTreeMap::from([(user_id.to_owned(), power_level.into())]);

                builder = builder.add_state_event(
                    factory.power_levels(&mut power_levels).state_key("").sender(user_id),
                );
            }

            server.sync_room(client, builder).await;
        }
    }

    struct MockSpaceRoomParameters {
        room_id: &'static RoomId,
        order: Option<&'static str>,
        parents: Vec<&'static RoomId>,
        children: Vec<&'static RoomId>,
        power_level: Option<i32>,
    }

    #[async_test]
    async fn test_space_child_updates() {
        // Test child updates received via sync.
        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;
        let user_id = client.user_id().unwrap();
        let factory = EventFactory::new();

        server.mock_room_state_encryption().plain().mount().await;

        let space_id = room_id!("!space:localhost");
        let first_child_id = room_id!("!first_child:localhost");
        let second_child_id = room_id!("!second_child:localhost");

        // The space is joined.
        server
            .sync_room(
                &client,
                JoinedRoomBuilder::new(space_id)
                    .add_state_event(factory.create(user_id, RoomVersionId::V11).with_space_type()),
            )
            .await;

        // Build the `SpaceService` and expect the room to show up with no updates
        // pending
        let space_service = SpaceService::new(client.clone()).await;

        let (initial_values, joined_spaces_subscriber) =
            space_service.subscribe_to_top_level_joined_spaces().await;
        pin_mut!(joined_spaces_subscriber);
        assert_pending!(joined_spaces_subscriber);

        assert_eq!(
            initial_values,
            vec![SpaceRoom::new_from_known(&client.get_room(space_id).unwrap(), 0)].into()
        );

        assert_eq!(
            space_service.top_level_joined_spaces().await,
            vec![SpaceRoom::new_from_known(&client.get_room(space_id).unwrap(), 0)]
        );

        // Two children are added.
        server
            .sync_room(
                &client,
                JoinedRoomBuilder::new(space_id)
                    .add_state_event(
                        factory
                            .space_child(space_id.to_owned(), first_child_id.to_owned())
                            .sender(user_id),
                    )
                    .add_state_event(
                        factory
                            .space_child(space_id.to_owned(), second_child_id.to_owned())
                            .sender(user_id),
                    ),
            )
            .await;

        // And expect the list to update.
        assert_eq!(
            space_service.top_level_joined_spaces().await,
            vec![SpaceRoom::new_from_known(&client.get_room(space_id).unwrap(), 2)]
        );
        assert_next_eq!(
            joined_spaces_subscriber,
            vec![
                VectorDiff::Clear,
                VectorDiff::Append {
                    values: vec![SpaceRoom::new_from_known(&client.get_room(space_id).unwrap(), 2)]
                        .into()
                },
            ]
        );

        // Then remove a child by replacing the state event with an empty one.
        server
            .sync_room(
                &client,
                JoinedRoomBuilder::new(space_id).add_state_bulk([Raw::new(&json!({
                    "content": {},
                    "type": "m.space.child",
                    "event_id": "$cancelsecondchild",
                    "origin_server_ts": MilliSecondsSinceUnixEpoch::now(),
                    "sender": user_id,
                    "state_key": second_child_id,
                }))
                .unwrap()
                .cast_unchecked()]),
            )
            .await;

        // And expect the list to update.
        assert_eq!(
            space_service.top_level_joined_spaces().await,
            vec![SpaceRoom::new_from_known(&client.get_room(space_id).unwrap(), 1)]
        );
        assert_next_eq!(
            joined_spaces_subscriber,
            vec![
                VectorDiff::Clear,
                VectorDiff::Append {
                    values: vec![SpaceRoom::new_from_known(&client.get_room(space_id).unwrap(), 1)]
                        .into()
                },
            ]
        );
    }
}
