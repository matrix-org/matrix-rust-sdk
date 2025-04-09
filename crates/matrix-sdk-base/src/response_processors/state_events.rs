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
    collections::{BTreeMap, BTreeSet},
    iter,
};

use ruma::{
    events::{
        room::member::MembershipState, AnyStrippedStateEvent, AnySyncStateEvent,
        AnySyncTimelineEvent,
    },
    serde::Raw,
    OwnedUserId,
};
use serde::Deserialize;
use tracing::{instrument, warn};

use super::{profiles, Context};
use crate::{
    store::{ambiguity_map::AmbiguityCache, Result as StoreResult},
    RoomInfo,
};

/// Collect sync state events and map them with their deserialized form.
pub fn collect_sync(
    _context: &mut Context,
    raw_events: &[Raw<AnySyncStateEvent>],
) -> (Vec<Raw<AnySyncStateEvent>>, Vec<AnySyncStateEvent>) {
    collect(raw_events)
}

/// Collect stripped state events and map them with their deserialized form.
pub fn collect_stripped(
    _context: &mut Context,
    raw_events: &[Raw<AnyStrippedStateEvent>],
) -> (Vec<Raw<AnyStrippedStateEvent>>, Vec<AnyStrippedStateEvent>) {
    collect(raw_events)
}

/// Collect sync timeline event, filter the sync state events, and map them with
/// their deserialized form.
pub fn collect_sync_from_timeline(
    _context: &mut Context,
    raw_events: &[Raw<AnySyncTimelineEvent>],
) -> (Vec<Raw<AnySyncStateEvent>>, Vec<AnySyncStateEvent>) {
    collect(raw_events.into_iter().filter_map(|raw_event| {
        // State events have a `state_key` field.
        match raw_event.get_field::<&str>("state_key") {
            Ok(Some(_)) => Some(raw_event.cast_ref()),
            _ => None,
        }
    }))
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

/// Dispatch the state events and return the new users for this room.
///
/// `raw_events` and `events` must be generated from [`collect_sync`]. Events
/// must be exactly the same list of events that are in raw_events, but
/// deserialised. We demand them here to avoid deserialising multiple times.
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
