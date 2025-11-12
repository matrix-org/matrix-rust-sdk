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
use matrix_sdk::{executor::AbortOnDrop, locks::Mutex};
use matrix_sdk_common::executor::spawn;
use ruma::OwnedRoomId;
use tokio::sync::Mutex as AsyncMutex;
use tracing::error;

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

pub struct GroupedRoomListService {
    room_list_service: Arc<AsyncMutex<Arc<RoomListService>>>,
    space_service: Arc<AsyncMutex<Arc<SpaceService>>>,
    space_id: Option<OwnedRoomId>,

    raw_rooms: Arc<AsyncMutex<Vector<RoomListItem>>>,
    rooms: Arc<AsyncMutex<ObservableVector<GroupedRoomListItem>>>,

    room_list_update_handle: Mutex<Option<AbortOnDrop<()>>>,
}

impl GroupedRoomListService {
    pub fn new(
        room_list_service: Arc<RoomListService>,
        space_service: Arc<SpaceService>,
        space_id: Option<OwnedRoomId>,
    ) -> Self {
        Self {
            room_list_service: Arc::new(AsyncMutex::new(room_list_service)),
            space_service: Arc::new(AsyncMutex::new(space_service)),
            space_id,
            raw_rooms: Arc::new(AsyncMutex::new(Vector::<RoomListItem>::new())),
            rooms: Arc::new(AsyncMutex::new(ObservableVector::new())),
            room_list_update_handle: Mutex::new(None),
        }
    }

    /// Subscribes to room list updates.
    pub async fn subscribe_to_room_updates(
        &self,
    ) -> (Vector<GroupedRoomListItem>, VectorSubscriberBatchedStream<GroupedRoomListItem>) {
        self.rooms.lock().await.subscribe().into_values_and_batched_stream()
    }

    // Resubscribe and reset everything when the space hierarchy changes?
    // Inform the user somehow? Reset the navigation stack?
    pub async fn subscribe(&self) {
        let room_list_service = Arc::clone(&self.room_list_service);
        let space_service = Arc::clone(&self.space_service);

        *self.raw_rooms.lock().await = Vector::<RoomListItem>::new();
        *self.rooms.lock().await = ObservableVector::new();

        let handle = spawn({
            let space_id = self.space_id.clone();
            let raw_rooms = Arc::clone(&self.raw_rooms);
            let rooms = Arc::clone(&self.rooms);

            async move {
                let room_list = room_list_service.lock().await.all_rooms().await.unwrap();

                // Add the latest event sorter feature flag thingie
                let (raw_stream, room_list_controller) =
                    room_list.entries_with_dynamic_adapters_with(usize::MAX, false);
                room_list_controller.set_filter(Box::new(new_filter_non_left()));

                pin_mut!(raw_stream);

                while let Some(diffs) = raw_stream.next().await {
                    let mut raw_rooms = raw_rooms.lock().await;

                    println!("Stefan: got stream update");
                    for diff in diffs {
                        diff.apply(&mut raw_rooms);
                    }

                    Self::update_rooms(
                        &space_id,
                        space_service.clone(),
                        rooms.clone(),
                        raw_rooms.clone(),
                    )
                    .await;
                }
            }
        });

        *self.room_list_update_handle.lock() = Some(AbortOnDrop::new(handle));
    }

    /// First some terminology:
    /// Top level rooms are rooms that don't have a parent
    /// Decendants are all rooms or spaces that stem from the same root parent
    /// Top level descendants are rooms or spaces that have a direct link to the given parent
    async fn update_rooms(
        space_id: &Option<OwnedRoomId>,
        space_service: Arc<AsyncMutex<Arc<SpaceService>>>,
        rooms: Arc<AsyncMutex<ObservableVector<GroupedRoomListItem>>>,
        raw_rooms: Vector<RoomListItem>,
    ) {
        println!(
            "Stefan: Raw rooms {:?}",
            raw_rooms.iter().map(|r| r.room_id()).collect::<Vec<_>>()
        );

        let space_service = space_service.lock().await;

        // Make sure the graph is built and stored if not yet
        space_service.joined_spaces().await;

        let top_level_descendants =
            space_service.top_level_descendants_of_space(space_id).await.unwrap();

        println!("Stefan: Top level rooms {:?}", top_level_descendants);

        // If on the top level compute the orphan rooms by taking all the rooms
        // and retaining only the ones that don't appear in the list of
        // descendants
        let mut orphan_rooms = if space_id.is_none() {
            raw_rooms.iter().map(|r| r.room_id().to_owned()).collect::<Vec<_>>()
        } else {
            Vec::new()
        };

        // For each top level room fetch all its descendants, from any level all
        // the way down and adjust the orphan rooms list accordingly
        let mut all_descendants = HashMap::<OwnedRoomId, Vec<OwnedRoomId>>::new();
        for top_level_room_id in &top_level_descendants {
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

        println!("Stefan: Space descendants: {:?}", all_descendants);
        println!("Stefan: Orphaned rooms: {:?}", orphan_rooms);

        // Use this to remember that we already found the top most room for a
        // given space and that we should ignore all the rest
        let mut latest_room_for_space = HashMap::new();

        // Iterate raw_rooms and build the grouped room list version
        let mut room_list = Vec::new();
        for room in raw_rooms.iter() {
            // If the room is an orphan keep it in place
            if orphan_rooms.contains(&room.room_id().to_owned()) {
                room_list.push(GroupedRoomListItem::Room(room.clone()));
            }
            // Similarly, if the room is a top level space with no descendants
            // keep it in place
            else if room.is_space()
                && top_level_descendants.contains(&room.room_id().to_owned())
                && all_descendants.get(&room.room_id().to_owned()).is_none_or(|v| v.is_empty())
            {
                room_list.push(GroupedRoomListItem::Space(room.clone(), None));
            } else {
                for space_id in all_descendants.keys() {
                    if all_descendants[space_id].contains(&room.room_id().to_owned()) {
                        // The first room we find is the correct one
                        if !latest_room_for_space.contains_key(space_id) {
                            latest_room_for_space.insert(space_id.clone(), room.room_id());

                            if let Some(space) = raw_rooms.iter().find(|r| r.room_id() == space_id)
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

        println!("Stefan: Grouped room list: {:?}", room_list);

        let mut rooms = rooms.lock().await;
        rooms.clear();
        rooms.append(Vector::from_iter(room_list.into_iter().map(|r| r)));
    }
}

#[cfg(test)]
mod tests {
    use matrix_sdk::test_utils::mocks::MatrixMockServer;
    use matrix_sdk_test::{JoinedRoomBuilder, async_test, event_factory::EventFactory};
    use ruma::{RoomVersionId, owned_room_id, room_id};

    use super::*;

    #[async_test]
    async fn test_new() {
        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;
        let user_id = client.user_id().unwrap();
        let factory = EventFactory::new();

        server.mock_room_state_encryption().plain().mount().await;

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

        let room_list_service = RoomListService::new(client.clone()).await.unwrap();
        let space_service = SpaceService::new(client.clone());

        let grouped_room_list_service = GroupedRoomListService::new(
            Arc::new(room_list_service),
            Arc::new(space_service),
            Some(owned_room_id!("!S1:example.org")),
        );

        grouped_room_list_service.subscribe().await;

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

        let (initial_values, mut _subscription) =
            grouped_room_list_service.subscribe_to_room_updates().await;

        // subscription.next().await;
    }
}
