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

use std::{collections::HashMap, sync::Arc};

use eyeball_im::{ObservableVector, VectorSubscriberBatchedStream};

use eyeball_im::Vector;
use futures_util::{StreamExt, pin_mut};
use matrix_sdk::executor::AbortOnDrop;
use matrix_sdk_common::executor::spawn;
use ruma::OwnedRoomId;
use tokio::sync::Mutex as AsyncMutex;
use tracing::{error, trace};

use crate::room_list_service::RoomListDynamicEntriesController;
use crate::room_list_service::filters::{BoxedFilterFn, new_filter_all, new_filter_identifiers};
use crate::{
    RoomListService,
    room_list_service::{RoomListItem, filters::new_filter_non_left},
    spaces::SpaceService,
};

#[derive(Debug, Clone)]
pub enum GroupedRoomListItem {
    Room(RoomListItem),
    Space(RoomListItem, Option<RoomListItem>),
}

struct RoomListState {
    raw_rooms: Vector<RoomListItem>,
    rooms: ObservableVector<GroupedRoomListItem>,
    room_list_update_handle: Option<AbortOnDrop<()>>,
    room_list_controller: Option<RoomListDynamicEntriesController>,
    descendants: Option<Vec<OwnedRoomId>>,
    grouping_enabled: bool,
}

pub struct GroupedRoomListService {
    room_list_service: Arc<AsyncMutex<Arc<RoomListService>>>,
    space_service: Arc<AsyncMutex<Arc<SpaceService>>>,
    space_id: Option<OwnedRoomId>,
    room_list_state: Arc<AsyncMutex<RoomListState>>,
}

impl GroupedRoomListService {
    pub fn new(
        room_list_service: Arc<RoomListService>,
        space_service: Arc<SpaceService>,
        space_id: Option<OwnedRoomId>,
    ) -> Self {
        let room_list_state = RoomListState {
            raw_rooms: Vector::<RoomListItem>::new(),
            rooms: ObservableVector::new(),
            room_list_update_handle: None,
            room_list_controller: None,
            descendants: None,
            grouping_enabled: true,
        };

        Self {
            room_list_service: Arc::new(AsyncMutex::new(room_list_service)),
            space_service: Arc::new(AsyncMutex::new(space_service)),
            space_id,
            room_list_state: Arc::new(AsyncMutex::new(room_list_state)),
        }
    }

    /// Setup a room list entries observer which receives diffs, updates the
    /// stored raw rooms and then proceeds to update the grouped room list
    /// following the algorithm described in [`Self::update_rooms`].
    pub async fn setup(&self) {
        let room_list_service = Arc::clone(&self.room_list_service);
        let space_service = Arc::clone(&self.space_service);

        let handle = spawn({
            let space_id = self.space_id.clone();
            let room_list_state = Arc::clone(&self.room_list_state);

            async move {
                let room_list_service = room_list_service.lock().await;
                let room_list = room_list_service.all_rooms().await.unwrap();
                drop(room_list_service);

                // TODO: Add the latest event sorter feature flag thingie
                let (raw_stream, room_list_controller) =
                    room_list.entries_with_dynamic_adapters_with(usize::MAX, false);
                room_list_controller.set_filter(Box::new(new_filter_non_left()));

                {
                    let mut room_list_state = room_list_state.lock().await;
                    room_list_state.room_list_controller = Some(room_list_controller);
                }

                pin_mut!(raw_stream);

                while let Some(diffs) = raw_stream.next().await {
                    trace!("Received stream update");

                    let mut room_list_state = room_list_state.lock().await;
                    for diff in diffs {
                        diff.apply(&mut room_list_state.raw_rooms);
                    }

                    Self::update_rooms(
                        &space_id,
                        &*space_service.lock().await,
                        &mut room_list_state,
                    )
                    .await;
                }
            }
        });

        self.room_list_state.lock().await.room_list_update_handle = Some(AbortOnDrop::new(handle));
    }

    /// Subscribes to room list updates.
    pub async fn subscribe_to_room_updates(
        &self,
    ) -> (Vector<GroupedRoomListItem>, VectorSubscriberBatchedStream<GroupedRoomListItem>) {
        self.room_list_state.lock().await.rooms.subscribe().into_values_and_batched_stream()
    }

    // TODO This probably needs a rewrite, it's sort of nasty
    pub async fn set_filter(&self, filter: Option<BoxedFilterFn>) -> bool {
        let mut room_list_state = self.room_list_state.lock().await;

        if let Some(filter) = filter {
            trace!("Setting room list filter");

            let filter = if let Some(descendants) = &room_list_state.descendants {
                Box::new(new_filter_all(vec![
                    Box::new(new_filter_identifiers(descendants.clone())),
                    filter,
                ]))
            } else {
                filter
            };

            room_list_state.grouping_enabled = false;

            if let Some(controller) = &room_list_state.room_list_controller {
                controller.set_filter(Box::new(new_filter_all(vec![filter])));
                true
            } else {
                false
            }
        } else {
            trace!("Clearing room list filter");

            let base_filter = new_filter_non_left();

            room_list_state.grouping_enabled = true;

            if let Some(controller) = &room_list_state.room_list_controller {
                controller.set_filter(Box::new(base_filter));
                true
            } else {
                false
            }
        }
    }

    /// First, some terminology:
    /// * Rooms mean any type of rooms (space, DM etc.) unless specified
    ///   otherwise
    /// * Top level rooms are rooms that don't have a parent
    /// * Decendants are rooms that stem from the same root parent
    /// * Direct descendants are rooms that have a direct link to the given
    ///   parent
    /// * Preview rooms are rooms that appear first in their respective space's
    ///   descendant list based on the underlying room list's sorting
    ///   configuration.
    ///
    /// The way this algoritm works is the following. Every time the underlying
    /// room list updates (i.e. raw_rooms):
    /// - the space service is used to retrieve the direct decendants of this
    ///   instance's space or, if on the root level, all top level rooms.
    ///   We find the top level non-space rooms by starting from the
    ///   full room list and filtering out rooms that appear as a descendant of
    ///   any of the top level space rooms. Orphan rooms are irrelevant outside of
    ///   top level.
    /// - While iterating through the direct spaces we also build a map of each
    ///   space's descendants which we later use to tell if a room is a candidate
    ///   for being the space's preview room.
    /// - With those in hand it's now time to iterate the raw room list and
    ///   build the grouped room list. For each room in raw rooms:
    ///     * if it's a top level non-space room, add it to the list
    ///     * if it's a space room with no descendants, add it to the list
    ///     * otherwise, check the space's descendants map to see if the room
    ///       appears in that list. If it doesn't, then keep going.
    ///       If it does, then check if this is the first time we're encountering
    ///       a room for that particular space. If it is, congratulations, we
    ///       found the preview room for that space. If it isn't then we can just
    ///       skip it.
    /// - Once the grouped room list is built, update the observable vector
    ///   and publish the diffs and wait for the next iteration.
    async fn update_rooms(
        space_id: &Option<OwnedRoomId>,
        space_service: &SpaceService,
        room_list_state: &mut RoomListState,
    ) {
        trace!(
            "Raw rooms {:?}",
            room_list_state.raw_rooms.iter().map(|r| r.cached_display_name()).collect::<Vec<_>>()
        );

        if !room_list_state.grouping_enabled {
            trace!("Grouping disabled, using the raw room list");
            room_list_state.rooms.clear();
            room_list_state.rooms.append(
                room_list_state.raw_rooms.iter().cloned().map(GroupedRoomListItem::Room).collect(),
            );
            return;
        }

        // Make sure the graph is built and stored if not yet
        // TODO: Figure out if there's a better way to do this
        space_service.joined_spaces().await;

        // And if not on the top level (i.e. space_id = None), fetch all
        // descendants so they can be used later for filtering.
        if let Some(space_id) = space_id {
            match space_service.descendants_of_space(space_id).await {
                Ok(descendants) => {
                    room_list_state.descendants = Some(descendants);
                }
                Err(e) => {
                    error!("Failed to get descendants of space {:?}: {:?}", space_id, e);
                }
            }
        }

        let direct_descendants = space_service.direct_descendants_of_space(space_id).await.unwrap();
        trace!("Direct descendants {:?}", direct_descendants);

        // If on the top level, compute the top level non-space rooms by taking
        // all the rooms and retaining only the ones that don't appear in the
        // list of descendants for any of the top level space rooms.
        let mut orphan_rooms = if space_id.is_none() {
            room_list_state.raw_rooms.iter().map(|r| r.room_id().to_owned()).collect::<Vec<_>>()
        } else {
            Vec::new()
        };

        // For each top level space room fetch all its descendants, from any
        // level, all the way down, and adjust the top level non-space room list
        // accordingly
        let mut all_descendants = HashMap::<OwnedRoomId, Vec<OwnedRoomId>>::new();
        for top_level_room_id in &direct_descendants {
            if let Ok(descendants) =
                space_service.descendants_of_space(top_level_room_id.as_ref()).await
            {
                // Remove the descendants from the orphaned rooms list
                orphan_rooms.retain(|r| !descendants.contains(r) && r != top_level_room_id);

                all_descendants.insert(top_level_room_id.clone(), descendants);
            } else {
                error!("Failed to get descendants of space {:?}", top_level_room_id);
            }
        }

        trace!("Space descendants: {:?}", all_descendants);
        trace!("Orphaned rooms: {:?}", orphan_rooms);

        // Use this to remember that we already found the top most room for a
        // given space and that we should ignore all the rest.
        // Keep in mind that the same room might be present in multiple spaces.
        let mut latest_room_for_space = HashMap::new();

        // Iterate raw_rooms and build the grouped room list version
        let mut room_list = Vec::new();
        for room in room_list_state.raw_rooms.iter() {
            // If the room is an orphan OR if it's a top level non-space room
            // that's a direct descendant, keep it in place
            if orphan_rooms.contains(&room.room_id().to_owned())
                || !room.is_space() && direct_descendants.contains(&room.room_id().to_owned())
            {
                room_list.push(GroupedRoomListItem::Room(room.clone()));
            }
            // Similarly, if the room is a top level space with no descendants
            // keep it in place
            else if room.is_space()
                && direct_descendants.contains(&room.room_id().to_owned())
                && all_descendants.get(&room.room_id().to_owned()).is_none_or(|v| v.is_empty())
            {
                room_list.push(GroupedRoomListItem::Space(room.clone(), None));
            } else {
                for space_id in all_descendants.keys() {
                    if all_descendants[space_id].contains(&room.room_id().to_owned()) {
                        // The first room we find is the correct one
                        if !latest_room_for_space.contains_key(space_id) {
                            latest_room_for_space.insert(space_id.clone(), room.room_id());

                            if let Some(space) =
                                room_list_state.raw_rooms.iter().find(|r| r.room_id() == space_id)
                            {
                                room_list.push(GroupedRoomListItem::Space(
                                    space.clone(),
                                    Some(room.clone()),
                                ));
                            } else {
                                error!(
                                    "Failed finding top space with ID {} in room list",
                                    space_id
                                );
                            }
                        }
                    }
                }
            }
        }

        let grouped_display_names = &room_list
            .iter()
            .map(|r| match r {
                GroupedRoomListItem::Space(space, room) => {
                    format!(
                        "(S:{:?}, R:{:?})",
                        space.name().unwrap_or(space.room_id().to_string()),
                        room.as_ref().map(|r| r.name().unwrap_or(r.room_id().to_string()))
                    )
                }
                GroupedRoomListItem::Room(room) => {
                    format!("R:{:?}", room.name().unwrap_or(room.room_id().to_string()))
                }
            })
            .collect::<Vec<_>>();

        trace!("Grouped room list: [{:?}]", grouped_display_names);

        // Finally update the observable vector in one go
        room_list_state.rooms.clear();
        room_list_state.rooms.append(Vector::from_iter(room_list.into_iter()));
    }
}

#[cfg(test)]
mod tests {
    use assert_matches2::assert_let;
    use eyeball_im::VectorDiff;
    use matrix_sdk::test_utils::mocks::MatrixMockServer;
    use matrix_sdk_test::{JoinedRoomBuilder, async_test, event_factory::EventFactory};
    use ruma::{RoomId, RoomVersionId, owned_room_id, room_id};

    use super::*;

    #[async_test]
    async fn test_new() {
        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;
        let user_id = client.user_id().unwrap();
        let factory = EventFactory::new();

        server.mock_room_state_encryption().plain().mount().await;

        let room_list_service = RoomListService::new(client.clone()).await.unwrap();
        let space_service = SpaceService::new(client.clone());

        let grouped_room_list_service =
            GroupedRoomListService::new(Arc::new(room_list_service), Arc::new(space_service), None);

        grouped_room_list_service.setup().await;

        // S1
        //  S1.1
        //  S1.2
        //   R1.2.1
        server
            .sync_room(
                &client,
                JoinedRoomBuilder::new(room_id!("!S1:example.org"))
                    .add_state_event(factory.create(user_id, RoomVersionId::V1).with_space_type())
                    .add_state_event(
                        factory
                            .space_child(
                                owned_room_id!("!S1:example.org"),
                                owned_room_id!("!S1.1:example.org"),
                            )
                            .sender(user_id),
                    )
                    .add_state_event(
                        factory
                            .space_child(
                                owned_room_id!("!S1:example.org"),
                                owned_room_id!("!S1.2:example.org"),
                            )
                            .sender(user_id),
                    ),
            )
            .await;

        server
            .sync_room(
                &client,
                JoinedRoomBuilder::new(room_id!("!S1.1:example.org"))
                    .add_state_event(factory.create(user_id, RoomVersionId::V1).with_space_type())
                    .add_state_event(
                        factory
                            .space_parent(
                                owned_room_id!("!S1:example.org"),
                                owned_room_id!("!S1.1:example.org"),
                            )
                            .sender(user_id),
                    ),
            )
            .await;

        server
            .sync_room(
                &client,
                JoinedRoomBuilder::new(room_id!("!S1.2:example.org"))
                    .add_state_event(factory.create(user_id, RoomVersionId::V1).with_space_type())
                    .add_state_event(
                        factory
                            .space_child(
                                owned_room_id!("!S1.2:example.org"),
                                owned_room_id!("!R1.2.1:example.org"),
                            )
                            .sender(user_id),
                    ),
            )
            .await;

        server
            .sync_room(
                &client,
                JoinedRoomBuilder::new(room_id!("!R1.2.1:example.org"))
                    .add_state_event(factory.create(user_id, RoomVersionId::V1).with_space_type()),
            )
            .await;

        let (initial_values, mut subscription) =
            grouped_room_list_service.subscribe_to_room_updates().await;

        assert_eq!(
            initial_values
                .iter()
                .map(|r| match r {
                    GroupedRoomListItem::Room(room) => (room.room_id(), None),
                    GroupedRoomListItem::Space(space, room) => {
                        (space.room_id(), room.as_ref().map(|r| r.room_id()))
                    }
                })
                .collect::<Vec<(&RoomId, Option<&RoomId>)>>(),
            vec![(room_id!("!S1:example.org"), Some(room_id!("!S1.1:example.org")))]
        );

        // S1
        //  S1.1
        //  S1.2
        //   R1.2.1
        // S2
        //  R2.1
        // R3

        server
            .sync_room(
                &client,
                JoinedRoomBuilder::new(room_id!("!S2:example.org"))
                    .add_state_event(factory.create(user_id, RoomVersionId::V1).with_space_type())
                    .add_state_event(
                        factory
                            .space_child(
                                owned_room_id!("!S2:example.org"),
                                owned_room_id!("!R2.1:example.org"),
                            )
                            .sender(user_id),
                    ),
            )
            .await;

        server
            .sync_room(
                &client,
                JoinedRoomBuilder::new(room_id!("!R2.1:example.org"))
                    .add_state_event(factory.create(user_id, RoomVersionId::V1)),
            )
            .await;

        server
            .sync_room(
                &client,
                JoinedRoomBuilder::new(room_id!("!R3:example.org"))
                    .add_state_event(factory.create(user_id, RoomVersionId::V1)),
            )
            .await;

        let updates = subscription.next().await.unwrap();
        assert_let!(VectorDiff::Reset { values } = &updates[0]);

        assert_eq!(
            values
                .iter()
                .map(|r| match r {
                    GroupedRoomListItem::Room(room) => (room.room_id(), None),
                    GroupedRoomListItem::Space(space, room) => {
                        (space.room_id(), room.as_ref().map(|r| r.room_id()))
                    }
                })
                .collect::<Vec<(&RoomId, Option<&RoomId>)>>(),
            vec![
                (room_id!("!S1:example.org"), Some(room_id!("!S1.1:example.org"))),
                (room_id!("!S2:example.org"), Some(room_id!("!R2.1:example.org"))),
                (room_id!("!R3:example.org"), None)
            ]
        );
    }
}
