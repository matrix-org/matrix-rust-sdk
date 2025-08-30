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

use matrix_sdk_base::deserialized_responses::TimelineEvent;
use ruma::{
    events::{room::message::Relation, AnySyncMessageLikeEvent},
    EventId,
};
use tracing::warn;

use crate::event_cache::RoomEventCache;

/// Given an event id this function returns the most recent edit on said event
/// or the event itself if there are no edits.
async fn get_most_recent_edit(
    cache: &RoomEventCache,
    original: &EventId,
) -> Option<AnySyncMessageLikeEvent> {
    use ruma::events::{relation::RelationType, AnySyncTimelineEvent};

    let Some((original_ev, related)) =
        cache.find_event_with_relations(original, Some(vec![RelationType::Replacement])).await
    else {
        warn!("Couldn't find relations for {}", original);
        return None;
    };

    match related.last().unwrap_or(&original_ev).raw().deserialize() {
        Ok(AnySyncTimelineEvent::MessageLike(latest)) => Some(latest),
        _ => None,
    }
}

/// Given an event, if it is a message then we return its latest edit else we
/// return the event.
async fn handle_possible_edits(
    cache: &RoomEventCache,
    event: &AnySyncMessageLikeEvent,
) -> Option<AnySyncMessageLikeEvent> {
    match event {
        AnySyncMessageLikeEvent::RoomMessage(ev) => {
            let original_ev_id = ev.as_original().map(|ev| match &ev.content.relates_to {
                Some(Relation::Replacement(replaces)) => &replaces.event_id,
                _ => event.event_id(),
            })?;

            get_most_recent_edit(cache, original_ev_id).await
        }
        _ => Some(event.clone()),
    }
}

/// Prepare a [`TimelineEvent`] into a [`AnySyncMessageLikeEvent`] for search
/// indexing.
pub(crate) async fn parse_timeline_event(
    cache: &RoomEventCache,
    event: &TimelineEvent,
) -> Option<AnySyncMessageLikeEvent> {
    use ruma::events::AnySyncTimelineEvent;

    if event.kind.is_utd() {
        return None;
    }

    match event.raw().deserialize() {
        Ok(event) => match event {
            AnySyncTimelineEvent::MessageLike(event) => handle_possible_edits(cache, &event).await,
            AnySyncTimelineEvent::State(_) => None,
        },

        Err(e) => {
            warn!("failed to parse event: {e:?}");
            None
        }
    }
}
