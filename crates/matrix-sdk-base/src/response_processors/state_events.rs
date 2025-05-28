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

use std::collections::{BTreeMap, BTreeSet};

use ruma::{events::AnySyncStateEvent, serde::Raw};
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
/// a loop or a merger.
///
/// Please see [`Error::InconsistentTombstonedRooms`] to learn more about the
/// kind of invalid states it can detect.
pub fn check_tombstone(
    context: &mut Context,
    room_updates: &RoomUpdates,
    state_store: &BaseStateStore,
) -> Result<()> {
    let mut room_updates = room_updates
        .iter_all_room_ids()
        // Ignore rooms with no `RoomInfo`s.
        .filter_map(|room_id| context.state_changes.room_infos.get(room_id))
        .map(|room_info| {
            (
                room_info.room_id.clone(),
                room_info.base_info.tombstone.as_ref().and_then(|tombstone_event| {
                    Some(tombstone_event.as_original()?.content.replacement_room.clone())
                }),
            )
        })
        .collect::<BTreeMap<_, _>>();

    let mut already_seen = BTreeSet::new();

    while let Some((mut room_id, mut maybe_replacement_room_id)) = room_updates.pop_first() {
        loop {
            if already_seen.contains(&room_id) {
                return Err(Error::InconsistentTombstonedRooms { room_in_path: room_id });
            }

            // First time we see this room: let's remember it.
            already_seen.insert(room_id);

            let Some(replacement_room_id) = maybe_replacement_room_id else {
                // No replacement room. Stop looking for one.
                break;
            };

            // Look up for the replacement room in the `room_updates`.
            let (next_room_id, next_replacement_room_id) = if let Some(next_replacement_room_id) =
                room_updates.remove(&replacement_room_id)
            {
                (replacement_room_id, next_replacement_room_id)
            }
            // Look for for the replacement room in the known rooms.
            else if let Some(replacement_room) = state_store.room(&replacement_room_id) {
                (
                    replacement_room_id,
                    replacement_room.successor_room().map(|successor_room| successor_room.room_id),
                )
            } else {
                break;
            };

            room_id = next_room_id;
            maybe_replacement_room_id = next_replacement_room_id;
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
    async fn test_check_tombstone_no_newly_tombstoned_rooms() {
        let client = logged_in_base_client(None).await;

        // Create a new room, no tombstone, no anything.
        {
            let response = SyncResponseBuilder::new()
                .add_joined_room(JoinedRoomBuilder::new(room_id!("!r0")))
                .build_sync_response();

            assert!(client.receive_sync_response(response).await.is_ok());
        }
    }

    /// This test is ensuring [`check_tombstone`] is not rejecting normal room
    /// upgrades not forming a loop or a merger.
    #[async_test]
    async fn test_check_tombstone_no_error() {
        let sender = user_id!("@mnt_io:matrix.org");
        let event_factory = EventFactory::new().sender(sender);
        let mut response_builder = SyncResponseBuilder::new();
        let room_id_0 = room_id!("!r0");
        let room_id_1 = room_id!("!r1");
        let room_id_2 = room_id!("!r2");

        let client = logged_in_base_client(None).await;

        // Create room 0.
        {
            let response = response_builder
                .add_joined_room(JoinedRoomBuilder::new(room_id_0))
                .build_sync_response();

            assert!(client.receive_sync_response(response).await.is_ok());

            let room_0 = client.get_room(room_id_0).unwrap();

            assert!(room_0.predecessor_room().is_none());
            assert!(room_0.successor_room().is_none());
        }

        // Room 0 is being tombstoned by room 1.
        {
            let tombstone_event_id = event_id!("$ev0");
            let response = response_builder
                // tombstoning room 0 with room 1
                .add_joined_room(
                    JoinedRoomBuilder::new(room_id_0).add_timeline_event(
                        event_factory
                            .room_tombstone("hello", room_id_1)
                            .room(room_id_0)
                            .event_id(tombstone_event_id),
                    ),
                )
                // creating room 1, tombstoning room 0
                .add_joined_room(
                    JoinedRoomBuilder::new(room_id_1).add_timeline_event(
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

        // Room 1 is being tombstoned by room 2.
        {
            let tombstone_event_id = event_id!("$ev1");
            let response = response_builder
                // tombstoning room 1 with room 2
                .add_joined_room(
                    JoinedRoomBuilder::new(room_id_1).add_timeline_event(
                        event_factory
                            .room_tombstone("hello", room_id_2)
                            .room(room_id_1)
                            .event_id(tombstone_event_id),
                    ),
                )
                // creating room 2, tombstoning room 1
                .add_joined_room(
                    JoinedRoomBuilder::new(room_id_2).add_timeline_event(
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

    /// This test is ensuring [`check_tombstone`] detects the smallest loop
    /// possible in tombstone: room 0 is tombstoning itself and is replacing
    /// itself.
    ///
    /// ```text
    /// m.room.tombstone
    /// replaced by room 0
    /// ┌──────────────┐
    /// │              │
    /// │  ┌────────┐  │
    /// └──┤ room 0 ◄──┘
    ///    └────────┘
    /// ```
    #[async_test]
    async fn test_check_tombstone_shortest_loop() {
        let sender = user_id!("@mnt_io:matrix.org");
        let event_factory = EventFactory::new().sender(sender);
        let mut response_builder = SyncResponseBuilder::new();
        let room_id_0 = room_id!("!r0");

        let client = logged_in_base_client(None).await;

        // Create room 0.
        {
            let response = response_builder
                .add_joined_room(JoinedRoomBuilder::new(room_id_0))
                .build_sync_response();

            assert!(client.receive_sync_response(response).await.is_ok());

            let room_0 = client.get_room(room_id_0).unwrap();

            assert!(room_0.predecessor_room().is_none());
            assert!(room_0.successor_room().is_none());
        }

        // Room 0 is being tombstoned by room 0.
        {
            let tombstone_event_id = event_id!("$ev0");
            let response = response_builder
                // tombstoning room 0 with room 0
                .add_joined_room(
                    JoinedRoomBuilder::new(room_id_0).add_timeline_event(
                        event_factory
                            .room_tombstone("hello", room_id_0)
                            .room(room_id_0)
                            .event_id(tombstone_event_id),
                    ),
                )
                .build_sync_response();

            assert_matches!(
                client.receive_sync_response(response).await,
                Err(Error::InconsistentTombstonedRooms { room_in_path }) => {
                    assert_eq!(room_in_path, room_id_0);
                }
            );
        }
    }

    /// This test is ensuring [`check_tombstone`] detects loops involving the
    /// state store and the room updates.
    ///
    /// ```text
    ///     m.room.tombstone     m.room.tombstone
    ///     replaced by room 1   replaced by room 2
    ///     ┌─────────────────┐  ┌─────────────────┐
    ///     │                 │  │                 │
    /// ┌────────┐         ┌──▼──┴──┐         ┌────▼───┐
    /// │ room 0 │         │ room 1 │         │ room 2 │
    /// └───▲────┘         └────────┘         └────┬───┘
    ///     │                                      │
    ///     └──────────────────────────────────────┘
    ///                 m.room.tomsbone
    ///                 replaced by room 0
    /// ```
    #[async_test]
    async fn test_check_tombstone_loop() {
        let sender = user_id!("@mnt_io:matrix.org");
        let event_factory = EventFactory::new().sender(sender);
        let mut response_builder = SyncResponseBuilder::new();
        let room_id_0 = room_id!("!r0");
        let room_id_1 = room_id!("!r1");
        let room_id_2 = room_id!("!r2");

        let client = logged_in_base_client(None).await;

        // Create room 0.
        {
            let response = response_builder
                .add_joined_room(JoinedRoomBuilder::new(room_id_0))
                .build_sync_response();

            assert!(client.receive_sync_response(response).await.is_ok());

            let room_0 = client.get_room(room_id_0).unwrap();

            assert!(room_0.predecessor_room().is_none());
            assert!(room_0.successor_room().is_none());
        }

        // Room 0 is being tombstoned by room 1.
        {
            let tombstone_event_id = event_id!("$ev0");
            let response = response_builder
                // tombstoning room 0 with room 1
                .add_joined_room(
                    JoinedRoomBuilder::new(room_id_0).add_timeline_event(
                        event_factory
                            .room_tombstone("hello", room_id_1)
                            .room(room_id_0)
                            .event_id(tombstone_event_id),
                    ),
                )
                // creating room 1, tombstoning room 0
                .add_joined_room(
                    JoinedRoomBuilder::new(room_id_1).add_timeline_event(
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

        // Room 1 is being tombstoned by room 2, but room 2 is tombstoned by room 0!
        {
            let tombstone_event_id = event_id!("$ev1");
            let response = response_builder
                // tombstoning room 1 with room 2
                .add_joined_room(
                    JoinedRoomBuilder::new(room_id_1).add_timeline_event(
                        event_factory
                            .room_tombstone("hello", room_id_2)
                            .room(room_id_1)
                            .event_id(tombstone_event_id),
                    ),
                )
                // creating room 2, tombstoning room 1, but also tombstoned by room 0
                .add_joined_room(
                    JoinedRoomBuilder::new(room_id_2)
                        .add_timeline_event(
                            event_factory
                                .create(sender, RoomVersionId::try_from("43").unwrap())
                                .predecessor(room_id_1, tombstone_event_id),
                        )
                        .add_timeline_event(
                            event_factory
                                .room_tombstone("hehe", room_id_0)
                                .room(room_id_2)
                                .event_id(event_id!("$ev_foo")),
                        ),
                )
                .build_sync_response();

            assert_matches!(
                client.receive_sync_response(response).await,
                Err(Error::InconsistentTombstonedRooms { room_in_path }) => {
                    assert_eq!(room_in_path, room_id_1);
                }
            );
        }
    }

    /// This test is ensuring [`check_tombstone`] detects a merger pattern in
    /// tombstoned room, i.e. when two rooms are being replaced by the same
    /// room.
    ///
    /// ```text
    ///      m.room.tombstone
    ///      replaced by room 2
    ///      ┌──────────────┐
    ///      │              │
    /// ┌────┴───┐          │
    /// │ room 0 │          │
    /// └────────┘     ┌────▼───┐
    ///                │ room 2 │
    /// ┌────────┐     └────▲───┘
    /// │ room 1 │          │
    /// └────┬───┘          │
    ///      │              │
    ///      └──────────────┘
    ///      m.room.tombstone
    ///      replaced by room 2
    /// ```
    #[async_test]
    async fn test_check_tombstone_merger() {
        let sender = user_id!("@mnt_io:matrix.org");
        let event_factory = EventFactory::new().sender(sender);
        let mut response_builder = SyncResponseBuilder::new();
        let room_id_0 = room_id!("!r0");
        let room_id_1 = room_id!("!r1");
        let room_id_2 = room_id!("!r2");

        let client = logged_in_base_client(None).await;

        // Create room 0 and room 1.
        {
            let response = response_builder
                .add_joined_room(JoinedRoomBuilder::new(room_id_0))
                .add_joined_room(JoinedRoomBuilder::new(room_id_1))
                .build_sync_response();

            assert!(client.receive_sync_response(response).await.is_ok());

            let room_0 = client.get_room(room_id_0).unwrap();

            assert!(room_0.predecessor_room().is_none());
            assert!(room_0.successor_room().is_none());

            let room_1 = client.get_room(room_id_1).unwrap();

            assert!(room_1.predecessor_room().is_none());
            assert!(room_1.successor_room().is_none());
        }

        // Room 0 and room 1 are both being tombstoned by room 2.
        {
            let tombstone_event_id_for_room_0 = event_id!("$ev0");
            let tombstone_event_id_for_room_1 = event_id!("$ev1");
            let response = response_builder
                // tombstoning room 0 with room 2
                .add_joined_room(
                    JoinedRoomBuilder::new(room_id_0).add_timeline_event(
                        event_factory
                            .room_tombstone("hello", room_id_2)
                            .room(room_id_0)
                            .event_id(tombstone_event_id_for_room_0),
                    ),
                )
                // tombstoning room 1 with room 2
                .add_joined_room(
                    JoinedRoomBuilder::new(room_id_1).add_timeline_event(
                        event_factory
                            .room_tombstone("hello", room_id_2)
                            .room(room_id_1)
                            .event_id(tombstone_event_id_for_room_1),
                    ),
                )
                // creating room 2, tombstoning room 0 (it's not possible to succeed to 2 rooms)
                .add_joined_room(
                    JoinedRoomBuilder::new(room_id_2).add_timeline_event(
                        event_factory
                            .create(sender, RoomVersionId::try_from("42").unwrap())
                            .predecessor(room_id_0, tombstone_event_id_for_room_0),
                    ),
                )
                .build_sync_response();

            assert_matches!(
                client.receive_sync_response(response).await,
                Err(Error::InconsistentTombstonedRooms { room_in_path }) => {
                    assert_eq!(room_in_path, room_id_2);
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
