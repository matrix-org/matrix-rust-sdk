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

use std::collections::BTreeSet;

use ruma::{
    events::{
        room::{create::RoomCreateEventContent, tombstone::RoomTombstoneEventContent},
        AnySyncStateEvent, SyncStateEvent,
    },
    serde::Raw,
    RoomId,
};
use serde::Deserialize;
use tracing::warn;

use super::Context;
use crate::store::BaseStateStore;

/// Collect [`AnySyncStateEvent`].
pub mod sync {
    use std::{collections::BTreeSet, iter};

    use ruma::{
        events::{
            room::member::{MembershipState, RoomMemberEventContent},
            AnySyncTimelineEvent, SyncStateEvent,
        },
        OwnedUserId, RoomId, UserId,
    };
    use tracing::{error, instrument};

    use super::{super::profiles, AnySyncStateEvent, Context, Raw};
    use crate::{
        store::{ambiguity_map::AmbiguityCache, BaseStateStore, Result as StoreResult},
        RoomInfo,
    };

    /// Collect [`AnySyncStateEvent`] to [`AnySyncStateEvent`].
    pub fn collect(
        raw_events: &[Raw<AnySyncStateEvent>],
    ) -> (Vec<Raw<AnySyncStateEvent>>, Vec<AnySyncStateEvent>) {
        super::collect(raw_events)
    }

    /// Collect [`AnySyncTimelineEvent`] to [`AnySyncStateEvent`].
    ///
    /// A [`AnySyncTimelineEvent`] can represent either message-like events or
    /// state events. The message-like events are filtered out.
    pub fn collect_from_timeline(
        raw_events: &[Raw<AnySyncTimelineEvent>],
    ) -> (Vec<Raw<AnySyncStateEvent>>, Vec<AnySyncStateEvent>) {
        super::collect(raw_events.iter().filter_map(|raw_event| {
            // Only state events have a `state_key` field.
            match raw_event.get_field::<&str>("state_key") {
                Ok(Some(_)) => Some(raw_event.cast_ref()),
                _ => None,
            }
        }))
    }

    /// Dispatch the sync state events.
    ///
    /// `raw_events` and `events` must be generated from [`collect`].
    /// Events must be exactly the same list of events that are in
    /// `raw_events`, but deserialised. We demand them here to avoid
    /// deserialising multiple times.
    ///
    /// The `new_users` mutable reference allows to collect the new users for
    /// this room.
    #[instrument(skip_all, fields(room_id = ?room_info.room_id))]
    pub async fn dispatch<U>(
        context: &mut Context,
        (raw_events, events): (&[Raw<AnySyncStateEvent>], &[AnySyncStateEvent]),
        room_info: &mut RoomInfo,
        ambiguity_cache: &mut AmbiguityCache,
        new_users: &mut U,
        state_store: &BaseStateStore,
    ) -> StoreResult<()>
    where
        U: NewUsers,
    {
        for (raw_event, event) in iter::zip(raw_events, events) {
            match event {
                AnySyncStateEvent::RoomMember(member) => {
                    room_info.handle_state_event(event);

                    dispatch_room_member(
                        context,
                        &room_info.room_id,
                        member,
                        ambiguity_cache,
                        new_users,
                    )
                    .await?;
                }

                AnySyncStateEvent::RoomCreate(create) => {
                    if super::is_create_event_valid(
                        context,
                        room_info.room_id(),
                        create,
                        state_store,
                    ) {
                        room_info.handle_state_event(event);
                    } else {
                        error!(
                            room_id = ?room_info.room_id(),
                            ?create,
                            "`m.create.tombstone` event is invalid, it creates a loop"
                        );

                        // Do not add the event to `room_info`.
                        // Do not add the event to `context.state_changes.state`.
                        continue;
                    }
                }

                AnySyncStateEvent::RoomTombstone(tombstone) => {
                    if super::is_tombstone_event_valid(
                        context,
                        room_info.room_id(),
                        tombstone,
                        state_store,
                    ) {
                        room_info.handle_state_event(event);
                    } else {
                        error!(
                            room_id = ?room_info.room_id(),
                            ?tombstone,
                            "`m.room.tombstone` event is invalid, it creates a loop"
                        );

                        // Do not add the event to `room_info`.
                        // Do not add the event to `context.state_changes.state`.
                        continue;
                    }
                }

                _ => {
                    room_info.handle_state_event(event);
                }
            }

            context
                .state_changes
                .state
                .entry(room_info.room_id.to_owned())
                .or_default()
                .entry(event.event_type())
                .or_default()
                .insert(event.state_key().to_owned(), raw_event.clone());
        }

        Ok(())
    }

    /// Dispatch a [`RoomMemberEventContent>`] state event.
    async fn dispatch_room_member<U>(
        context: &mut Context,
        room_id: &RoomId,
        event: &SyncStateEvent<RoomMemberEventContent>,
        ambiguity_cache: &mut AmbiguityCache,
        new_users: &mut U,
    ) -> StoreResult<()>
    where
        U: NewUsers,
    {
        ambiguity_cache.handle_event(&context.state_changes, room_id, event).await?;

        match event.membership() {
            MembershipState::Join | MembershipState::Invite => {
                new_users.insert(event.state_key());
            }
            _ => (),
        }

        profiles::upsert_or_delete(context, room_id, event);

        Ok(())
    }

    /// A trait to collect new users in [`dispatch`].
    trait NewUsers {
        /// Insert a new user in the collection of new users.
        fn insert(&mut self, user_id: &UserId);
    }

    impl NewUsers for BTreeSet<OwnedUserId> {
        fn insert(&mut self, user_id: &UserId) {
            self.insert(user_id.to_owned());
        }
    }

    impl NewUsers for () {
        fn insert(&mut self, _user_id: &UserId) {}
    }
}

/// Collect [`AnyStrippedStateEvent`].
pub mod stripped {
    use std::{collections::BTreeMap, iter};

    use ruma::{events::AnyStrippedStateEvent, push::Action};
    use tracing::instrument;

    use super::{
        super::{notification, timeline},
        Context, Raw,
    };
    use crate::{Result, Room, RoomInfo};

    /// Collect [`AnyStrippedStateEvent`] to [`AnyStrippedStateEvent`].
    pub fn collect(
        raw_events: &[Raw<AnyStrippedStateEvent>],
    ) -> (Vec<Raw<AnyStrippedStateEvent>>, Vec<AnyStrippedStateEvent>) {
        super::collect(raw_events)
    }

    /// Dispatch the stripped state events.
    ///
    /// `raw_events` and `events` must be generated from [`collect`].
    /// Events must be exactly the same list of events that are in
    /// `raw_events`, but deserialised. We demand them here to avoid
    /// deserialising multiple times.
    ///
    /// Dispatch the stripped state events in `invite_state` or `knock_state`,
    /// modifying the room's info and posting notifications as needed.
    ///
    /// * `raw_events` and `events` - The contents of `invite_state` in the form
    ///   of list of pairs of raw stripped state events with their deserialized
    ///   counterpart.
    /// * `room` - The [`Room`] to modify.
    /// * `room_info` - The current room's info.
    /// * `notifications` - Notifications to post for the current room.
    #[instrument(skip_all, fields(room_id = ?room_info.room_id))]
    pub(crate) async fn dispatch_invite_or_knock(
        context: &mut Context,
        (raw_events, events): (&[Raw<AnyStrippedStateEvent>], &[AnyStrippedStateEvent]),
        room: &Room,
        room_info: &mut RoomInfo,
        mut notification: notification::Notification<'_>,
    ) -> Result<()> {
        let mut state_events = BTreeMap::new();

        for (raw_event, event) in iter::zip(raw_events, events) {
            room_info.handle_stripped_state_event(event);
            state_events
                .entry(event.event_type())
                .or_insert_with(BTreeMap::new)
                .insert(event.state_key().to_owned(), raw_event.clone());
        }

        context
            .state_changes
            .stripped_state
            .insert(room_info.room_id().to_owned(), state_events.clone());

        // We need to check for notifications after we have handled all state
        // events, to make sure we have the full push context.
        if let Some(push_condition_room_ctx) =
            timeline::get_push_room_context(context, room, room_info, notification.state_store)
                .await?
        {
            let room_id = room.room_id();

            // Check every event again for notification.
            for event in state_events.values().flat_map(|map| map.values()) {
                notification.push_notification_from_event_if(
                    room_id,
                    &push_condition_room_ctx,
                    event,
                    Action::should_notify,
                );
            }
        }

        Ok(())
    }
}

fn collect<'a, I, T>(raw_events: I) -> (Vec<Raw<T>>, Vec<T>)
where
    I: IntoIterator<Item = &'a Raw<T>>,
    T: Deserialize<'a> + 'a,
{
    raw_events
        .into_iter()
        .filter_map(|raw_event| match raw_event.deserialize() {
            Ok(event) => Some((raw_event.clone(), event)),
            Err(e) => {
                warn!("Couldn't deserialize stripped state event: {e}");
                None
            }
        })
        .unzip()
}

/// Check if `m.room.create` isn't creating a loop of rooms.
pub fn is_create_event_valid(
    context: &mut Context,
    room_id: &RoomId,
    event: &SyncStateEvent<RoomCreateEventContent>,
    state_store: &BaseStateStore,
) -> bool {
    let mut already_seen = BTreeSet::new();
    already_seen.insert(room_id.to_owned());

    let Some(mut predecessor_room_id) = event
        .as_original()
        .and_then(|event| Some(event.content.predecessor.as_ref()?.room_id.clone()))
    else {
        // `true` means no problem. No predecessor = no problem here.
        return true;
    };

    loop {
        // We must check immediately if the `predecessor_room_id` is in `already_seen`
        // in case of a room is created and marks itself as its predecessor in a single
        // sync.
        if already_seen.contains(AsRef::<RoomId>::as_ref(&predecessor_room_id)) {
            // Ahhh, there is a loop with `m.room.create` events!
            return false;
        }

        already_seen.insert(predecessor_room_id.clone());

        // Where is the predecessor room? Check in `room_infos` and then in
        // `state_store`.
        let Some(next_predecessor_room_id) = context
            .state_changes
            .room_infos
            .get(&predecessor_room_id)
            .and_then(|room_info| Some(room_info.create()?.predecessor.as_ref()?.room_id.clone()))
            .or_else(|| {
                state_store
                    .room(&predecessor_room_id)
                    .and_then(|room| Some(room.predecessor_room()?.room_id))
            })
        else {
            // No more predecessor found. Everything seems alright. No loop.
            break;
        };

        predecessor_room_id = next_predecessor_room_id;
    }

    true
}

/// Check if `m.room.tombstone` isn't creating a loop of rooms.
pub fn is_tombstone_event_valid(
    context: &mut Context,
    room_id: &RoomId,
    event: &SyncStateEvent<RoomTombstoneEventContent>,
    state_store: &BaseStateStore,
) -> bool {
    let mut already_seen = BTreeSet::new();
    already_seen.insert(room_id.to_owned());

    let Some(mut successor_room_id) =
        event.as_original().map(|event| event.content.replacement_room.clone())
    else {
        // `true` means no problem. No successor = no problem here.
        return true;
    };

    loop {
        // We must check immediately if the `successor_room_id` is in `already_seen` in
        // case of a room is created and tombstones itself in a single sync.
        if already_seen.contains(AsRef::<RoomId>::as_ref(&successor_room_id)) {
            // Ahhh, there is a loop with `m.room.tombstone` events!
            return false;
        }

        already_seen.insert(successor_room_id.clone());

        // Where is the successor room? Check in `room_infos` and then in `state_store`.
        let Some(next_successor_room_id) = context
            .state_changes
            .room_infos
            .get(&successor_room_id)
            .and_then(|room_info| Some(room_info.tombstone()?.replacement_room.clone()))
            .or_else(|| {
                state_store
                    .room(&successor_room_id)
                    .and_then(|room| Some(room.successor_room()?.room_id))
            })
        else {
            // No more successor found. Everything seems alright. No loop.
            break;
        };

        successor_room_id = next_successor_room_id;
    }

    true
}

#[cfg(test)]
mod tests {
    use matrix_sdk_test::{
        async_test, event_factory::EventFactory, JoinedRoomBuilder, StateTestEvent,
        SyncResponseBuilder, DEFAULT_TEST_ROOM_ID,
    };
    use ruma::{event_id, room_id, user_id, RoomVersionId};

    use crate::test_utils::logged_in_base_client;

    #[async_test]
    async fn test_not_possible_to_overwrite_m_room_create() {
        let sender = user_id!("@mnt_io:matrix.org");
        let event_factory = EventFactory::new().sender(sender);
        let mut response_builder = SyncResponseBuilder::new();
        let room_id_0 = room_id!("!r0");
        let room_id_1 = room_id!("!r1");
        let room_id_2 = room_id!("!r2");

        let client = logged_in_base_client(None).await;

        // Create room 0 with 2 `m.room.create` events.
        // Create room 1 with 1 `m.room.create` event.
        // Create room 2 with 0 `m.room.create` event.
        {
            let response = response_builder
                .add_joined_room(
                    JoinedRoomBuilder::new(room_id_0)
                        .add_timeline_event(
                            event_factory.create(sender, RoomVersionId::try_from("42").unwrap()),
                        )
                        .add_timeline_event(
                            event_factory.create(sender, RoomVersionId::try_from("43").unwrap()),
                        ),
                )
                .add_joined_room(JoinedRoomBuilder::new(room_id_1).add_timeline_event(
                    event_factory.create(sender, RoomVersionId::try_from("44").unwrap()),
                ))
                .add_joined_room(JoinedRoomBuilder::new(room_id_2))
                .build_sync_response();

            assert!(client.receive_sync_response(response).await.is_ok());

            // Room 0
            // the second `m.room.create` has been ignored!
            assert_eq!(
                client.get_room(room_id_0).unwrap().create_content().unwrap().room_version.as_str(),
                "42"
            );
            // Room 1
            assert_eq!(
                client.get_room(room_id_1).unwrap().create_content().unwrap().room_version.as_str(),
                "44"
            );
            // Room 2
            assert!(client.get_room(room_id_2).unwrap().create_content().is_none());
        }

        // Room 0 receives a new `m.room.create` event.
        // Room 1 receives a new `m.room.create` event.
        // Room 2 receives its first `m.room.create` event.
        {
            let response = response_builder
                .add_joined_room(JoinedRoomBuilder::new(room_id_0).add_timeline_event(
                    event_factory.create(sender, RoomVersionId::try_from("45").unwrap()),
                ))
                .add_joined_room(JoinedRoomBuilder::new(room_id_1).add_timeline_event(
                    event_factory.create(sender, RoomVersionId::try_from("46").unwrap()),
                ))
                .add_joined_room(JoinedRoomBuilder::new(room_id_2).add_timeline_event(
                    event_factory.create(sender, RoomVersionId::try_from("47").unwrap()),
                ))
                .build_sync_response();

            assert!(client.receive_sync_response(response).await.is_ok());

            // Room 0
            // the third `m.room.create` has been ignored!
            assert_eq!(
                client.get_room(room_id_0).unwrap().create_content().unwrap().room_version.as_str(),
                "42"
            );
            // Room 1
            // the second `m.room.create` has been ignored!
            assert_eq!(
                client.get_room(room_id_1).unwrap().create_content().unwrap().room_version.as_str(),
                "44"
            );
            // Room 2
            assert_eq!(
                client.get_room(room_id_2).unwrap().create_content().unwrap().room_version.as_str(),
                "47"
            );
        }
    }

    #[async_test]
    async fn test_check_room_upgrades_no_newly_tombstoned_rooms() {
        let client = logged_in_base_client(None).await;

        // Create a new room, no tombstone, no anything.
        {
            let response = SyncResponseBuilder::new()
                .add_joined_room(JoinedRoomBuilder::new(room_id!("!r0")))
                .build_sync_response();

            assert!(client.receive_sync_response(response).await.is_ok());
        }
    }

    #[async_test]
    async fn test_check_room_upgrades_no_error() {
        let sender = user_id!("@mnt_io:matrix.org");
        let event_factory = EventFactory::new().sender(sender);
        let mut response_builder = SyncResponseBuilder::new();
        let room_id_0 = room_id!("!r0");
        let room_id_1 = room_id!("!r1");
        let room_id_2 = room_id!("!r2");

        let client = logged_in_base_client(None).await;

        // Room 0.
        {
            let response = response_builder
                .add_joined_room(JoinedRoomBuilder::new(room_id_0).add_timeline_event(
                    // Room 0 has no predecessor.
                    event_factory.create(sender, RoomVersionId::try_from("41").unwrap()),
                ))
                .build_sync_response();

            assert!(client.receive_sync_response(response).await.is_ok());

            let room_0 = client.get_room(room_id_0).unwrap();

            assert!(room_0.predecessor_room().is_none());
            assert!(room_0.successor_room().is_none());
        }

        // Room 0 and room 1.
        {
            let tombstone_event_id = event_id!("$ev0");
            let response = response_builder
                .add_joined_room(JoinedRoomBuilder::new(room_id_0).add_timeline_event(
                    // Successor of room 0 is room 1.
                    event_factory.room_tombstone("hello", room_id_1).event_id(tombstone_event_id),
                ))
                .add_joined_room(
                    JoinedRoomBuilder::new(room_id_1).add_timeline_event(
                        // Predecessor of room 1 is room 0.
                        event_factory
                            .create(sender, RoomVersionId::try_from("42").unwrap())
                            .predecessor(room_id_0, tombstone_event_id),
                    ),
                )
                .build_sync_response();

            assert!(client.receive_sync_response(response).await.is_ok());

            let room_0 = client.get_room(room_id_0).unwrap();

            assert!(room_0.predecessor_room().is_none(), "room 0 must not have a predecessor");
            assert_eq!(
                room_0.successor_room().expect("room 0 must have a successor").room_id,
                room_id_1,
                "room 0 does not have the expected successor",
            );

            let room_1 = client.get_room(room_id_1).unwrap();

            assert_eq!(
                room_1.predecessor_room().expect("room 1 must have a predecessor").room_id,
                room_id_0,
                "room 1 does not have the expected predecessor",
            );
            assert!(room_1.successor_room().is_none(), "room 1 must not have a successor");
        }

        // Room 1 and room 2.
        {
            let tombstone_event_id = event_id!("$ev1");
            let response = response_builder
                .add_joined_room(JoinedRoomBuilder::new(room_id_1).add_timeline_event(
                    // Successor of room 1 is room 2.
                    event_factory.room_tombstone("hello", room_id_2).event_id(tombstone_event_id),
                ))
                .add_joined_room(
                    JoinedRoomBuilder::new(room_id_2).add_timeline_event(
                        // Predecessor of room 2 is room 1.
                        event_factory
                            .create(sender, RoomVersionId::try_from("43").unwrap())
                            .predecessor(room_id_1, tombstone_event_id),
                    ),
                )
                .build_sync_response();

            assert!(client.receive_sync_response(response).await.is_ok());

            let room_1 = client.get_room(room_id_1).unwrap();

            assert_eq!(
                room_1.predecessor_room().expect("room 1 must have a predecessor").room_id,
                room_id_0,
                "room 1 does not have the expected predecessor",
            );
            assert_eq!(
                room_1.successor_room().expect("room 1 must have a successor").room_id,
                room_id_2,
                "room 1 does not have the expected successor",
            );

            let room_2 = client.get_room(room_id_2).unwrap();

            assert_eq!(
                room_2.predecessor_room().expect("room 2 must have a predecessor").room_id,
                room_id_1,
                "room 2 does not have the expected predecessor",
            );
            assert!(room_2.successor_room().is_none(), "room 2 must not have a successor");
        }
    }

    #[async_test]
    async fn test_check_room_upgrades_no_loop_within_misordered_rooms() {
        let sender = user_id!("@mnt_io:matrix.org");
        let event_factory = EventFactory::new().sender(sender);
        let mut response_builder = SyncResponseBuilder::new();
        // The room IDs are important because `SyncResponseBuilder` stores them in a
        // `HashMap`, so they are going to be “shuffled”.
        let room_id_0 = room_id!("!r1");
        let room_id_1 = room_id!("!r0");
        let room_id_2 = room_id!("!r2");

        let client = logged_in_base_client(None).await;

        // Create all rooms in a misordered way to see if `check_tombstone` will
        // re-order them appropriately.
        {
            let response = response_builder
                // Room 0
                .add_joined_room(
                    JoinedRoomBuilder::new(room_id_0)
                        .add_timeline_event(
                            // No predecessor for room 0.
                            event_factory.create(sender, RoomVersionId::try_from("41").unwrap()),
                        )
                        .add_timeline_event(
                            // Successor of room 0 is room 1.
                            event_factory
                                .room_tombstone("hello", room_id_1)
                                .event_id(event_id!("$ev0")),
                        ),
                )
                // Room 1
                .add_joined_room(
                    JoinedRoomBuilder::new(room_id_1)
                        .add_timeline_event(
                            // Predecessor of room 1 is room 0.
                            event_factory
                                .create(sender, RoomVersionId::try_from("42").unwrap())
                                .predecessor(room_id_0, event_id!("$ev0")),
                        )
                        .add_timeline_event(
                            // Successor of room 1 is room 2.
                            event_factory
                                .room_tombstone("hello", room_id_2)
                                .event_id(event_id!("$ev1")),
                        ),
                )
                // Room 2
                .add_joined_room(
                    JoinedRoomBuilder::new(room_id_2).add_timeline_event(
                        // Predecessor of room 2 is room 1.
                        event_factory
                            .create(sender, RoomVersionId::try_from("43").unwrap())
                            .predecessor(room_id_1, event_id!("$ev1")),
                    ),
                )
                .build_sync_response();

            // At this point, we can check that `response` contains misordered room updates.
            {
                let mut rooms = response.rooms.join.keys();

                // Room 1 is before room 0!
                assert_eq!(rooms.next().unwrap(), room_id_1);
                assert_eq!(rooms.next().unwrap(), room_id_0);
                assert_eq!(rooms.next().unwrap(), room_id_2);
                assert!(rooms.next().is_none());
            }

            // But the algorithm to detect invalid states works nicely.
            assert!(client.receive_sync_response(response).await.is_ok());

            let room_0 = client.get_room(room_id_0).unwrap();

            assert!(room_0.predecessor_room().is_none(), "room 0 must not have a predecessor");
            assert_eq!(
                room_0.successor_room().expect("room 0 must have a successor").room_id,
                room_id_1,
                "room 0 does not have the expected successor",
            );

            let room_1 = client.get_room(room_id_1).unwrap();

            assert_eq!(
                room_1.predecessor_room().expect("room 1 must have a predecessor").room_id,
                room_id_0,
                "room 1 does not have the expected predecessor",
            );
            assert_eq!(
                room_1.successor_room().expect("room 1 must have a successor").room_id,
                room_id_2,
                "room 1 does not have the expected successor",
            );

            let room_2 = client.get_room(room_id_2).unwrap();

            assert_eq!(
                room_2.predecessor_room().expect("room 2 must have a predecessor").room_id,
                room_id_1,
                "room 2 does not have the expected predecessor",
            );
            assert!(room_2.successor_room().is_none(), "room 2 must not have a successor");
        }
    }

    #[async_test]
    async fn test_check_room_upgrades_shortest_invalid_successor() {
        let sender = user_id!("@mnt_io:matrix.org");
        let event_factory = EventFactory::new().sender(sender);
        let mut response_builder = SyncResponseBuilder::new();
        let room_id_0 = room_id!("!r0");

        let client = logged_in_base_client(None).await;

        // Room 0.
        {
            let tombstone_event_id = event_id!("$ev0");
            let response = response_builder
                .add_joined_room(
                    // Successor of room 0 is room 0.
                    // No predecessor.
                    JoinedRoomBuilder::new(room_id_0).add_timeline_event(
                        event_factory
                            .room_tombstone("hello", room_id_0)
                            .event_id(tombstone_event_id),
                    ),
                )
                .build_sync_response();

            // The sync doesn't fail but…
            assert!(client.receive_sync_response(response).await.is_ok());

            // … the state event has not been saved.
            let room_0 = client.get_room(room_id_0).unwrap();

            assert!(room_0.predecessor_room().is_none(), "room 0 must not have a predecessor");
            assert!(room_0.successor_room().is_none(), "room 0 must not have a successor");
        }
    }

    #[async_test]
    async fn test_check_room_upgrades_invalid_successor() {
        let sender = user_id!("@mnt_io:matrix.org");
        let event_factory = EventFactory::new().sender(sender);
        let mut response_builder = SyncResponseBuilder::new();
        let room_id_0 = room_id!("!r0");
        let room_id_1 = room_id!("!r1");
        let room_id_2 = room_id!("!r2");

        let client = logged_in_base_client(None).await;

        // Room 0 and room 1.
        {
            let tombstone_event_id = event_id!("$ev0");
            let response = response_builder
                .add_joined_room(JoinedRoomBuilder::new(room_id_0).add_timeline_event(
                    // Successor of room 0 is room 1.
                    event_factory.room_tombstone("hello", room_id_1).event_id(tombstone_event_id),
                ))
                .add_joined_room(
                    JoinedRoomBuilder::new(room_id_1).add_timeline_event(
                        // Predecessor of room 1 is room 0.
                        event_factory
                            .create(sender, RoomVersionId::try_from("42").unwrap())
                            .predecessor(room_id_0, tombstone_event_id),
                    ),
                )
                .build_sync_response();

            assert!(client.receive_sync_response(response).await.is_ok());

            let room_0 = client.get_room(room_id_0).unwrap();

            assert!(room_0.predecessor_room().is_none(), "room 0 must not have a predecessor");
            assert_eq!(
                room_0.successor_room().expect("room 0 must have a successor").room_id,
                room_id_1,
                "room 0 does not have the expected successor",
            );

            let room_1 = client.get_room(room_id_1).unwrap();

            assert_eq!(
                room_1.predecessor_room().expect("room 1 must have a predecessor").room_id,
                room_id_0,
                "room 1 does not have the expected predecessor",
            );
            assert!(room_1.successor_room().is_none(), "room 1 must not have a successor");
        }

        // Room 1, room 2 and room 0.
        {
            let tombstone_event_id = event_id!("$ev1");
            let response = response_builder
                .add_joined_room(JoinedRoomBuilder::new(room_id_1).add_timeline_event(
                    // Successor of room 1 is room 2.
                    event_factory.room_tombstone("hello", room_id_2).event_id(tombstone_event_id),
                ))
                .add_joined_room(
                    JoinedRoomBuilder::new(room_id_2)
                        .add_timeline_event(
                            // Predecessor of room 2 is room 1.
                            event_factory
                                .create(sender, RoomVersionId::try_from("43").unwrap())
                                .predecessor(room_id_1, tombstone_event_id),
                        )
                        .add_timeline_event(
                            // Successor of room 2 is room 0.
                            event_factory
                                .room_tombstone("hehe", room_id_0)
                                .event_id(event_id!("$ev_foo")),
                        ),
                )
                .build_sync_response();

            // The sync doesn't fail but…
            assert!(client.receive_sync_response(response).await.is_ok());

            // … the state event for `room_id_2` has not been saved.
            let room_0 = client.get_room(room_id_0).unwrap();

            assert!(room_0.predecessor_room().is_none(), "room 0 must not have a predecessor");
            assert_eq!(
                room_0.successor_room().expect("room 0 must have a successor").room_id,
                room_id_1,
                "room 0 does not have the expected successor",
            );

            let room_1 = client.get_room(room_id_1).unwrap();

            assert_eq!(
                room_1.predecessor_room().expect("room 1 must have a predecessor").room_id,
                room_id_0,
                "room 1 does not have the expected predecessor",
            );
            assert_eq!(
                room_1.successor_room().expect("room 1 must have a successor").room_id,
                room_id_2,
                "room 1 does not have the expected successor",
            );

            let room_2 = client.get_room(room_id_2).unwrap();

            assert_eq!(
                room_2.predecessor_room().expect("room 2 must have a predecessor").room_id,
                room_id_1,
                "room 2 does not have the expected predecessor",
            );
            // this state event is missing because it creates a loop
            assert!(room_2.successor_room().is_none(), "room 2 must not have a successor",);
        }
    }

    #[async_test]
    async fn test_check_room_upgrades_shortest_invalid_predecessor() {
        let sender = user_id!("@mnt_io:matrix.org");
        let event_factory = EventFactory::new().sender(sender);
        let mut response_builder = SyncResponseBuilder::new();
        let room_id_0 = room_id!("!r0");

        let client = logged_in_base_client(None).await;

        // Room 0.
        {
            let tombstone_event_id = event_id!("$ev0");
            let response = response_builder
                .add_joined_room(
                    // Predecessor of room 0 is room 0.
                    // No successor.
                    JoinedRoomBuilder::new(room_id_0).add_timeline_event(
                        event_factory
                            .create(sender, RoomVersionId::try_from("42").unwrap())
                            .predecessor(room_id_0, tombstone_event_id)
                            .event_id(tombstone_event_id),
                    ),
                )
                .build_sync_response();

            // The sync doesn't fail but…
            assert!(client.receive_sync_response(response).await.is_ok());

            // … the state event has not been saved.
            let room_0 = client.get_room(room_id_0).unwrap();

            assert!(room_0.predecessor_room().is_none(), "room 0 must not have a predecessor");
            assert!(room_0.successor_room().is_none(), "room 0 must not have a successor");
        }
    }

    #[async_test]
    async fn test_check_room_upgrades_shortest_loop() {
        let sender = user_id!("@mnt_io:matrix.org");
        let event_factory = EventFactory::new().sender(sender);
        let mut response_builder = SyncResponseBuilder::new();
        let room_id_0 = room_id!("!r0");

        let client = logged_in_base_client(None).await;

        // Room 0.
        {
            let tombstone_event_id = event_id!("$ev0");
            let response = response_builder
                .add_joined_room(
                    JoinedRoomBuilder::new(room_id_0)
                        .add_timeline_event(
                            // Successor of room 0 is room 0
                            event_factory
                                .room_tombstone("hello", room_id_0)
                                .event_id(tombstone_event_id),
                        )
                        .add_timeline_event(
                            // Predecessor of room 0 is room 0
                            event_factory
                                .create(sender, RoomVersionId::try_from("42").unwrap())
                                .predecessor(room_id_0, tombstone_event_id),
                        ),
                )
                .build_sync_response();

            // The sync doesn't fail but…
            assert!(client.receive_sync_response(response).await.is_ok());

            // … the state event has not been saved.
            let room_0 = client.get_room(room_id_0).unwrap();

            assert!(room_0.predecessor_room().is_none(), "room 0 must not have a predecessor");
            assert!(room_0.successor_room().is_none(), "room 0 must not have a successor");
        }
    }

    #[async_test]
    async fn test_check_room_upgrades_loop() {
        let sender = user_id!("@mnt_io:matrix.org");
        let event_factory = EventFactory::new().sender(sender);
        let mut response_builder = SyncResponseBuilder::new();
        let room_id_0 = room_id!("!r0");
        let room_id_1 = room_id!("!r1");
        let room_id_2 = room_id!("!r2");

        let client = logged_in_base_client(None).await;

        // Room 0, room 1 and room 2.
        //
        // Doing that in one sync, it's the only way to create such loop (otherwise it
        // implies overwriting the `m.room.create` event, or not setting it first, then
        // setting it later… anyway, it works in one sync)
        {
            let response = response_builder
                .add_joined_room(
                    JoinedRoomBuilder::new(room_id_0)
                        .add_timeline_event(
                            // Predecessor of room 0 is room 2
                            event_factory
                                .create(sender, RoomVersionId::try_from("42").unwrap())
                                .predecessor(room_id_2, event_id!("$ev2")),
                        )
                        .add_timeline_event(
                            // Successor of room 0 is room 1
                            event_factory
                                .room_tombstone("hello", room_id_1)
                                .event_id(event_id!("$ev0")),
                        ),
                )
                .add_joined_room(
                    JoinedRoomBuilder::new(room_id_1)
                        .add_timeline_event(
                            // Predecessor of room 1 is room 0
                            event_factory
                                .create(sender, RoomVersionId::try_from("43").unwrap())
                                .predecessor(room_id_0, event_id!("$ev0")),
                        )
                        .add_timeline_event(
                            // Successor of room 1 is room 2
                            event_factory
                                .room_tombstone("hello", room_id_2)
                                .event_id(event_id!("$ev1")),
                        ),
                )
                .add_joined_room(
                    JoinedRoomBuilder::new(room_id_2)
                        .add_timeline_event(
                            // Predecessor of room 2 is room 1
                            event_factory
                                .create(sender, RoomVersionId::try_from("44").unwrap())
                                .predecessor(room_id_1, event_id!("$ev1")),
                        )
                        .add_timeline_event(
                            // Successor of room 2 is room 0
                            event_factory
                                .room_tombstone("hello", room_id_0)
                                .event_id(event_id!("$ev2")),
                        ),
                )
                .build_sync_response();

            // The sync doesn't fail but…
            assert!(client.receive_sync_response(response).await.is_ok());

            // … the state event for room 2 -> room 0 has not been saved, but room 0 <- room
            // 2 has been saved.
            let room_0 = client.get_room(room_id_0).unwrap();

            assert_eq!(
                room_0.predecessor_room().expect("room 0 must have a predecessor").room_id,
                room_id_2,
                "room 0 does not have the expected predecessor"
            );
            assert_eq!(
                room_0.successor_room().expect("room 0 must have a successor").room_id,
                room_id_1,
                "room 0 does not have the expected successor",
            );

            let room_1 = client.get_room(room_id_1).unwrap();

            assert_eq!(
                room_1.predecessor_room().expect("room 1 must have a predecessor").room_id,
                room_id_0,
                "room 1 does not have the expected predecessor",
            );
            assert_eq!(
                room_1.successor_room().expect("room 1 must have a successor").room_id,
                room_id_2,
                "room 1 does not have the expected successor",
            );

            let room_2 = client.get_room(room_id_2).unwrap();

            // this state event is missing because it creates a loop
            assert!(room_2.predecessor_room().is_none(), "room 2 must not have a predecessor");
            assert!(room_2.successor_room().is_none(), "room 2 must not have a successor",);
        }
    }

    #[async_test]
    async fn test_state_events_after_sync() {
        // Given a room
        let user_id = user_id!("@u:u.to");

        let client = logged_in_base_client(Some(user_id)).await;
        let mut sync_builder = SyncResponseBuilder::new();

        let room_name = EventFactory::new()
            .sender(user_id)
            .room_topic("this is the test topic in the timeline")
            .event_id(event_id!("$2"))
            .into_raw_sync();

        let response = sync_builder
            .add_joined_room(
                JoinedRoomBuilder::new(&DEFAULT_TEST_ROOM_ID)
                    .add_timeline_event(room_name)
                    .add_state_event(StateTestEvent::PowerLevels),
            )
            .build_sync_response();
        client.receive_sync_response(response).await.unwrap();

        let room = client.get_room(&DEFAULT_TEST_ROOM_ID).expect("Just-created room not found!");

        // ensure that we have the power levels
        assert!(room.power_levels().await.is_ok());

        // ensure that we have the topic
        assert_eq!(room.topic().unwrap(), "this is the test topic in the timeline");
    }
}
