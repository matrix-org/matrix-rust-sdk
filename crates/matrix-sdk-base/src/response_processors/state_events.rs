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

use ruma::{events::AnySyncStateEvent, serde::Raw};
use serde::Deserialize;
use tracing::warn;

use super::Context;

/// Collect [`AnySyncStateEvent`].
pub mod sync {
    use std::{
        collections::{BTreeMap, BTreeSet},
        iter,
    };

    use ruma::{
        events::{room::member::MembershipState, AnySyncTimelineEvent},
        OwnedUserId,
    };
    use tracing::instrument;

    use super::{super::profiles, AnySyncStateEvent, Context, Raw};
    use crate::{
        store::{ambiguity_map::AmbiguityCache, Result as StoreResult},
        RoomInfo,
    };

    /// Collect [`AnySyncStateEvent`] to [`AnySyncStateEvent`].
    pub fn collect(
        _context: &mut Context,
        raw_events: &[Raw<AnySyncStateEvent>],
    ) -> (Vec<Raw<AnySyncStateEvent>>, Vec<AnySyncStateEvent>) {
        super::collect(raw_events)
    }

    /// Collect [`AnySyncTimelineEvent`] to [`AnySyncStateEvent`].
    ///
    /// A [`AnySyncTimelineEvent`] can represent either message-like events or
    /// state events. The message-like events are filtered out.
    pub fn collect_from_timeline(
        _context: &mut Context,
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

    /// Dispatch the sync state events and return the new users for this room.
    ///
    /// `raw_events` and `events` must be generated from [`collect`].
    /// Events must be exactly the same list of events that are in
    /// `raw_events`, but deserialised. We demand them here to avoid
    /// deserialising multiple times.
    #[instrument(skip_all, fields(room_id = ?room_info.room_id))]
    pub async fn dispatch_and_get_new_users(
        context: &mut Context,
        (raw_events, events): (&[Raw<AnySyncStateEvent>], &[AnySyncStateEvent]),
        room_info: &mut RoomInfo,
        ambiguity_cache: &mut AmbiguityCache,
    ) -> StoreResult<BTreeSet<OwnedUserId>> {
        let mut user_ids = BTreeSet::new();

        if raw_events.is_empty() {
            return Ok(user_ids);
        }

        let mut state_events = BTreeMap::new();

        for (raw_event, event) in iter::zip(raw_events, events) {
            room_info.handle_state_event(event);

            if let AnySyncStateEvent::RoomMember(member) = event {
                ambiguity_cache
                    .handle_event(&context.state_changes, &room_info.room_id, member)
                    .await?;

                match member.membership() {
                    MembershipState::Join | MembershipState::Invite => {
                        user_ids.insert(member.state_key().to_owned());
                    }
                    _ => (),
                }

                profiles::upsert_or_delete(context, &room_info.room_id, member);
            }

            state_events
                .entry(event.event_type())
                .or_insert_with(BTreeMap::new)
                .insert(event.state_key().to_owned(), raw_event.clone());
        }

        context.state_changes.state.insert(room_info.room_id.clone(), state_events);

        Ok(user_ids)
    }
}

/// Collect [`AnyStrippedStateEvent`].
pub mod stripped {
    use std::{collections::BTreeMap, iter};

    use ruma::{
        events::AnyStrippedStateEvent,
        push::{Action, Ruleset},
        OwnedRoomId,
    };
    use tracing::instrument;

    use super::{super::timeline, Context, Raw};
    use crate::{
        deserialized_responses::RawAnySyncOrStrippedTimelineEvent, store::BaseStateStore,
        sync::Notification, Result, Room, RoomInfo,
    };

    /// Collect [`AnyStrippedStateEvent`] to [`AnyStrippedStateEvent`].
    pub fn collect(
        _context: &mut Context,
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
    /// * `push_rules` - The push rules for this room.
    /// * `changes` - The accumulated list of changes to apply once the
    ///   processing is finished.
    /// * `notifications` - Notifications to post for the current room.
    /// * `state_store` — The state store.
    #[instrument(skip_all, fields(room_id = ?room_info.room_id))]
    pub(crate) async fn dispatch_invite_or_knock(
        context: &mut Context,
        (raw_events, events): (&[Raw<AnyStrippedStateEvent>], &[AnyStrippedStateEvent]),
        room: &Room,
        room_info: &mut RoomInfo,
        push_rules: &Ruleset,
        notifications: &mut BTreeMap<OwnedRoomId, Vec<Notification>>,
        state_store: &BaseStateStore,
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
        if let Some(push_context) =
            timeline::get_push_room_context(context, room, room_info, state_store).await?
        {
            // Check every event again for notification.
            for event in state_events.values().flat_map(|map| map.values()) {
                let actions = push_rules.get_actions(event, &push_context);

                if actions.iter().any(Action::should_notify) {
                    notifications.entry(room.room_id().to_owned()).or_default().push(
                        Notification {
                            actions: actions.to_owned(),
                            event: RawAnySyncOrStrippedTimelineEvent::Stripped(event.clone()),
                        },
                    );
                }
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
