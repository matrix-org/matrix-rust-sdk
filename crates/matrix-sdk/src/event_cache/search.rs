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
use matrix_sdk_search::index::RoomIndexOperation;
use ruma::{
    EventId,
    events::{
        AnySyncMessageLikeEvent, AnySyncTimelineEvent,
        room::{
            message::{OriginalSyncRoomMessageEvent, Relation, SyncRoomMessageEvent},
            redaction::SyncRoomRedactionEvent,
        },
    },
    room_version_rules::RedactionRules,
};
use tracing::warn;

use crate::event_cache::RoomEventCache;

/// Given an event id this function returns the most recent edit on said event
/// or the event itself if there are no edits.
async fn get_most_recent_edit(
    cache: &RoomEventCache,
    original: &EventId,
) -> Option<OriginalSyncRoomMessageEvent> {
    use ruma::events::{AnySyncTimelineEvent, relation::RelationType};

    let Some((original_ev, related)) =
        cache.find_event_with_relations(original, Some(vec![RelationType::Replacement])).await
    else {
        warn!("Couldn't find relations for {}", original);
        return None;
    };

    match related.last().unwrap_or(&original_ev).raw().deserialize() {
        Ok(AnySyncTimelineEvent::MessageLike(AnySyncMessageLikeEvent::RoomMessage(latest))) => {
            latest.as_original().cloned()
        }
        _ => None,
    }
}

/// If the given [`OriginalSyncRoomMessageEvent`] is an edit we make an
/// [`RoomIndexOperation::Edit`] with the new most recent version of the
/// original.
async fn handle_possible_edit(
    event: &OriginalSyncRoomMessageEvent,
    cache: &RoomEventCache,
) -> Option<RoomIndexOperation> {
    if let Some(Relation::Replacement(replacement_data)) = &event.content.relates_to {
        if let Some(recent) = get_most_recent_edit(cache, &replacement_data.event_id).await {
            return Some(RoomIndexOperation::Edit(replacement_data.event_id.clone(), recent));
        } else {
            return Some(RoomIndexOperation::Noop);
        }
    }
    None
}

/// Return a [`RoomIndexOperation::Edit`] or [`RoomIndexOperation::Add`]
/// depending on the message.
async fn handle_room_message(
    event: SyncRoomMessageEvent,
    cache: &RoomEventCache,
) -> Option<RoomIndexOperation> {
    if let Some(event) = event.as_original() {
        return handle_possible_edit(event, cache).await.or(get_most_recent_edit(
            cache,
            &event.event_id,
        )
        .await
        .map(RoomIndexOperation::Add));
    }
    None
}

/// Return a [`RoomIndexOperation::Edit`] or [`RoomIndexOperation::Remove`]
/// depending on the message.
async fn handle_room_redaction(
    event: SyncRoomRedactionEvent,
    cache: &RoomEventCache,
    rules: &RedactionRules,
) -> Option<RoomIndexOperation> {
    if let Some(redacted_event_id) = event.redacts(rules)
        && let Some(redacted_event) = cache.find_event(redacted_event_id).await
        && let Ok(AnySyncTimelineEvent::MessageLike(AnySyncMessageLikeEvent::RoomMessage(
            redacted_event,
        ))) = redacted_event.raw().deserialize()
        && let Some(redacted_event) = redacted_event.as_original()
    {
        return handle_possible_edit(redacted_event, cache)
            .await
            .or(Some(RoomIndexOperation::Remove(redacted_event.event_id.clone())));
    }
    None
}

/// Prepare a [`TimelineEvent`] into a [`RoomIndexOperation`] for search
/// indexing.
pub(crate) async fn parse_timeline_event(
    cache: &RoomEventCache,
    event: &TimelineEvent,
    redaction_rules: &RedactionRules,
) -> Option<RoomIndexOperation> {
    use ruma::events::AnySyncTimelineEvent;

    if event.kind.is_utd() {
        return None;
    }

    match event.raw().deserialize() {
        Ok(event) => match event {
            AnySyncTimelineEvent::MessageLike(event) => match event {
                AnySyncMessageLikeEvent::RoomMessage(event) => {
                    handle_room_message(event, cache).await
                }
                AnySyncMessageLikeEvent::RoomRedaction(event) => {
                    handle_room_redaction(event, cache, redaction_rules).await
                }
                _ => None,
            },
            AnySyncTimelineEvent::State(_) => None,
        },

        Err(e) => {
            warn!("failed to parse event: {e:?}");
            None
        }
    }
}
