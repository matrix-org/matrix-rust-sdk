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

use std::{
    borrow::Cow,
    collections::{BTreeMap, BTreeSet},
    ops::Not,
};

use ruma::{events::AnySyncStateEvent, serde::Raw, OwnedRoomId};
use serde::Deserialize;
use tracing::warn;

use super::Context;
use crate::{store::BaseStateStore, sync::RoomUpdates, Error, Result};

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
    use tracing::instrument;

    use super::{super::profiles, AnySyncStateEvent, Context, Raw};
    use crate::{
        store::{ambiguity_map::AmbiguityCache, Result as StoreResult},
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
    ) -> StoreResult<()>
    where
        U: NewUsers,
    {
        for (raw_event, event) in iter::zip(raw_events, events) {
            room_info.handle_state_event(event);

            if let AnySyncStateEvent::RoomMember(member) = event {
                dispatch_room_member(
                    context,
                    &room_info.room_id,
                    member,
                    ambiguity_cache,
                    new_users,
                )
                .await?;
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

/// Check room upgrades (`m.room.tombstone` and `m.room.create`) aren't creating
/// a loop, or a merger, or a splitter. Basically, it checks consistency between
/// predecessors and successors, and that a loop isn't created.
pub fn check_room_upgrades(
    context: &mut Context,
    room_updates: &RoomUpdates,
    state_store: &BaseStateStore,
) -> Result<()> {
    #[derive(Clone, Debug)]
    struct Links {
        predecessor: Option<OwnedRoomId>,
        successor: Option<OwnedRoomId>,
    }

    /// Get links for a particular room.
    ///
    /// If the links are found in `room_updates`, this function returns
    /// `Some(Cow::Borrowed(_))`, if the links are found in `state_store`, this
    /// functions returns `Some(Cow::Owned(_))`, otherwise it returns `None`.
    fn get_links_for<'a>(
        room_id: &OwnedRoomId,
        room_updates: &'a BTreeMap<OwnedRoomId, Links>,
        state_store: &'a BaseStateStore,
    ) -> Option<Cow<'a, Links>> {
        room_updates.get(room_id).map(Cow::Borrowed).or_else(|| {
            state_store
                .room(room_id)
                .map(|room| Links {
                    predecessor: room.predecessor_room().map(|predecessor| predecessor.room_id),
                    successor: room.successor_room().map(|successor| successor.room_id),
                })
                .map(Cow::Owned)
        })
    }

    let mut room_updates = room_updates
        .iter_all_room_ids()
        // Ignore all rooms that have no predecessor **and** no successor.
        .filter_map(|room_id| {
            let room_info = context.state_changes.room_infos.get(room_id)?;

            let links = Links {
                predecessor: room_info.base_info.create.as_ref().and_then(|create_event| {
                    Some(create_event.as_original()?.content.predecessor.as_ref()?.room_id.clone())
                }),
                successor: room_info.base_info.tombstone.as_ref().and_then(|tombstone_event| {
                    Some(tombstone_event.as_original()?.content.replacement_room.clone())
                }),
            };

            (links.predecessor.is_some() || links.successor.is_some())
                .then_some((room_info.room_id.clone(), links))
        })
        .collect::<BTreeMap<_, _>>();

    // Check that for each room A, if its successor is B, then A is the predecessor
    // of B. It also works if A = B, which is not a problem for the moment.
    for (room, Links { predecessor, successor }) in &room_updates {
        // `room` has a predecessor. Check that this predecessor has `room` as a
        // successor.
        if let Some(predecessor_room_id) = predecessor {
            // The predecessor room does exist.
            // Otherwise, if it doesn't exist, maybe the sync has not catch up this room in
            // particular. In this case, let's continue.
            if let Some(predecessor_room) =
                get_links_for(predecessor_room_id, &room_updates, state_store)
            {
                if matches!(&predecessor_room.successor, Some(successor_of_the_predecessor) if successor_of_the_predecessor == room).not() {
                    return Err(Error::RoomUpgradeInvalidPredecessor {
                        room: room.clone(),
                        expected_predecessor: predecessor_room_id.clone(),
                        successor_of_the_predecessor: predecessor_room.successor.clone()
                    });
                }
            }
        }

        // `room` has a successor. Check that this successor has `room` as a
        // predecessor.
        if let Some(successor_room_id) = successor {
            // The successor room does exist.
            // Otherwise, if it doesn't exist, maybe the sync has not catch up this room in
            // particular. In this case, let's continue.
            if let Some(successor_room) =
                get_links_for(successor_room_id, &room_updates, state_store)
            {
                if matches!(&successor_room.predecessor, Some(predecessor_of_the_successor) if predecessor_of_the_successor == room).not() {
                    return Err(Error::RoomUpgradeInvalidSuccessor {
                        room: room.clone(),
                        expected_successor: successor_room_id.clone(),
                        predecessor_of_the_successor: successor_room.predecessor.clone()
                    });
                }
            }
        }
    }

    // Now we can iterate over room and see if a loop is formed.
    let mut already_seen = BTreeSet::new();

    // Loop over each `room_updates`. And for each one of them, loop over the
    // predecessors to find a loop.
    while let Some((mut room, Links { mut predecessor, .. })) = room_updates.pop_first() {
        // Clear `already_seen`. It's a memory optimisation: we don't need to reallocate
        // it for each `room_updates`.
        already_seen.clear();

        loop {
            if already_seen.contains(&room) {
                return Err(Error::RoomUpgradeIsLooping { room_in_path: room });
            }

            already_seen.insert(room);

            let Some(predecessor_) = predecessor else {
                // No predecessor. Stop looking.
                break;
            };

            (room, predecessor) = match get_links_for(&predecessor_, &room_updates, state_store) {
                // The links are coming from `room_updates`.
                Some(Cow::Borrowed(_)) => {
                    // Remove the predecessor from `room_updates`, so that it's not iterated
                    // twice (once by this `loop`, a second time by the
                    // upper `while` loop).
                    let links = room_updates.remove(&predecessor_).unwrap();

                    (predecessor_, links.predecessor)
                }

                // The links are coming from `state_store`.
                Some(Cow::Owned(predecessor)) => (predecessor_, predecessor.predecessor),

                // The links are not found.
                None => {
                    break;
                }
            };
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use assert_matches::assert_matches;
    use matrix_sdk_test::{
        async_test, event_factory::EventFactory, JoinedRoomBuilder, StateTestEvent,
        SyncResponseBuilder, DEFAULT_TEST_ROOM_ID,
    };
    use ruma::{event_id, room_id, user_id, RoomVersionId};

    use crate::{test_utils::logged_in_base_client, Error};

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

        // Room 0 <-> room 1.
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

        // Room 1 <-> room 2.
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

        // Create all rooms in a misordered way to see if `check_room_upgrades` will
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

            // But the algorithm in `check_room_upgrades` works nicely.
            assert!(client.receive_sync_response(response).await.is_ok());
        }
    }

    #[async_test]
    async fn test_check_room_upgrades_shortest_invalid_successor() {
        let sender = user_id!("@mnt_io:matrix.org");
        let event_factory = EventFactory::new().sender(sender);
        let mut response_builder = SyncResponseBuilder::new();
        let room_id_0 = room_id!("!r0");

        let client = logged_in_base_client(None).await;

        // Room 0 -> room 0.
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

            assert_matches!(
                client.receive_sync_response(response).await,
                Err(Error::RoomUpgradeInvalidSuccessor { room, expected_successor, predecessor_of_the_successor, }) => {
                    assert_eq!(room, room_id_0);
                    assert_eq!(expected_successor, room_id_0);
                    assert!(predecessor_of_the_successor.is_none());
                }
            );
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

        // Room 0 <-> room 1.
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

        // Room 1 <-> room 2 -> room 0.
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

            assert_matches!(
                client.receive_sync_response(response).await,
                Err(Error::RoomUpgradeInvalidSuccessor { room, expected_successor, predecessor_of_the_successor }) => {
                    assert_eq!(room, room_id_2);
                    assert_eq!(expected_successor, room_id_0);
                    assert!(predecessor_of_the_successor.is_none());
                }
            );
        }
    }

    #[async_test]
    async fn test_check_room_upgrades_shortest_invalid_predecessor() {
        let sender = user_id!("@mnt_io:matrix.org");
        let event_factory = EventFactory::new().sender(sender);
        let mut response_builder = SyncResponseBuilder::new();
        let room_id_0 = room_id!("!r0");

        let client = logged_in_base_client(None).await;

        // Room 0 <- room 0.
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

            assert_matches!(
                client.receive_sync_response(response).await,
                Err(Error::RoomUpgradeInvalidPredecessor { room, expected_predecessor, successor_of_the_predecessor, }) => {
                    assert_eq!(room, room_id_0);
                    assert_eq!(expected_predecessor, room_id_0);
                    assert!(successor_of_the_predecessor.is_none());
                }
            );
        }
    }

    #[async_test]
    async fn test_check_room_upgrades_shortest_loop() {
        let sender = user_id!("@mnt_io:matrix.org");
        let event_factory = EventFactory::new().sender(sender);
        let mut response_builder = SyncResponseBuilder::new();
        let room_id_0 = room_id!("!r0");

        let client = logged_in_base_client(None).await;

        // Room 0 <-> room 0.
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

            assert_matches!(
                client.receive_sync_response(response).await,
                Err(Error::RoomUpgradeIsLooping { room_in_path }) => {
                    assert_eq!(room_in_path, room_id_0);
                }
            );
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

        // Room 0 <-> room 1 <-> room 2 <-> room 0.
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

            assert_matches!(
                client.receive_sync_response(response).await,
                Err(Error::RoomUpgradeIsLooping { room_in_path }) => {
                    assert_eq!(room_in_path, room_id_0);
                }
            );
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
