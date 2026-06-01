// Copyright 2026 The Matrix.org Foundation C.I.C.
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

use std::collections::HashMap;

use matrix_sdk_base::{
    serde_helpers::{extract_redaction_target, extract_relation, extract_thread_root},
    sync::Timeline,
};
use ruma::{OwnedEventId, events::relation::RelationType, room_version_rules::RedactionRules};

use super::{
    super::{Result, states::StateLockReadGuard},
    room::RoomEventCacheState,
    thread::ThreadEventCache,
};

pub fn aggregate_timeline_for_room(timeline: Timeline) -> Timeline {
    timeline
}

pub async fn aggregate_timeline_for_threads(
    timeline: &Timeline,
    existing_threads: &HashMap<OwnedEventId, ThreadEventCache>,
    room_event_cache: StateLockReadGuard<'_, RoomEventCacheState>,
    redaction_rules: &RedactionRules,
) -> Result<HashMap<OwnedEventId, Timeline>> {
    let mut new_events_by_thread = HashMap::new();

    let default_timeline = || Timeline {
        limited: timeline.limited,
        prev_batch: timeline.prev_batch.clone(),
        events: Vec::new(),
    };

    // Look for in-thread events, i.e. events that are part of threads.
    for (nth, event) in timeline.events.iter().enumerate() {
        match extract_relation(event.raw()) {
            // Ohh, this event relates to another event!
            Some((relation_type, related_event_id)) => match relation_type {
                // `related_event` represents a thread root.
                RelationType::Thread => {
                    new_events_by_thread
                        .entry(related_event_id)
                        .or_insert_with(default_timeline)
                        .events
                        .push(event.clone());
                }

                // `event` represents an annotation (e.g. reactions), a replacement (an edit), a
                // reference or something custom. Let's see if the `related_event_id` is an
                // in-thread event.
                RelationType::Annotation
                | RelationType::Replacement
                | RelationType::Reference
                | _ => {
                    // First, look for the related event in `timeline` backwards.
                    if let Some(thread_root) = match timeline.events[..nth]
                        .iter()
                        .rev()
                        .find(|event| event.event_id() == Some(&related_event_id))
                    {
                        // The related event has been found in the `timeline`! Extract its thread
                        // root.
                        Some(related_event) => extract_thread_root(related_event.raw()),

                        // Not in `timeline`, okay, look for the related event in the `room` as it
                        // knows about all the events, and then extract its thread root.
                        None => room_event_cache.find_event(&related_event_id).await?.and_then(
                            |(_location, related_event)| extract_thread_root(related_event.raw()),
                        ),
                    } {
                        new_events_by_thread
                            .entry(thread_root)
                            .or_insert_with(default_timeline)
                            .events
                            .push(event.clone());
                    }
                }
            },

            // No explicit relation, okay, but it can still be related to a thread!
            None => {
                // We previously found events that are part of a thread, but we didn't see the
                // thread root yet. And guess what? This might be this event!
                if let Some(event_id) = event.event_id()
                    && existing_threads.contains_key(event_id)
                {
                    new_events_by_thread
                        .entry(event_id.to_owned())
                        .or_insert_with(default_timeline)
                        .events
                        .push(event.clone());
                }
                // Otherwise, this event might be a redaction that applies to a thread.
                else if let Some(redaction_target) =
                    extract_redaction_target(event.raw(), redaction_rules)
                    && room_event_cache.find_event(&redaction_target).await?.is_some()
                {
                    // The redacted event exists (in the room, because it contains _all_ the
                    // events) **but** the event has been redacted (in
                    // the room). It's no more possible to extract its
                    // thread root (because this information has been removed).
                    //
                    // But we need to know if the event is part of a thread to apply the
                    // redaction in the thread too. No other choice than
                    // doing a full search…

                    let mut associated_thread_root = None;

                    for (thread_root, thread) in existing_threads {
                        if thread.find_event(&redaction_target).await?.is_some() {
                            associated_thread_root = Some(thread_root.clone());
                            break;
                        }
                    }

                    // We've found the thread owning the event being redacted!
                    if let Some(thread_root) = associated_thread_root {
                        new_events_by_thread
                            .entry(thread_root)
                            .or_insert_with(default_timeline)
                            .events
                            .push(event.clone());
                    }
                }
            }
        }
    }

    Ok(new_events_by_thread)
}

pub fn aggregate_timeline_for_pinned_events(
    timeline: &Timeline,
    pinned_event_ids: &[OwnedEventId],
    redaction_rules: &RedactionRules,
) -> Timeline {
    let mut new_timeline = Timeline {
        limited: timeline.limited,
        prev_batch: timeline.prev_batch.clone(),
        events: Vec::new(),
    };

    // No events are pinned? The `Timeline` must be empty.
    if pinned_event_ids.is_empty() {
        return new_timeline;
    }

    // Look for events that relate to pinned events. We already know the
    // pinned-events, we don't need to look for them. We are only interested by
    // related events.
    for event in &timeline.events {
        match extract_relation(event.raw()) {
            // Ohh, this event relates to another event!
            Some((relation_type, related_event_id)) => match relation_type {
                // `event` relates to a thread: not what we want.
                RelationType::Thread => {}

                // `event` represents an annotation (e.g. reactions), a replacement (an edit), a
                // reference or something custom. Let's see if the `related_event_id` is a
                // pinned-event.
                RelationType::Annotation
                | RelationType::Replacement
                | RelationType::Reference
                | _ => {
                    if pinned_event_ids.contains(&related_event_id) {
                        new_timeline.events.push(event.clone());
                    }
                }
            },

            // No explicit relation, but it can be a redaction of a pinned-event!
            None => {
                if let Some(redaction_target) =
                    extract_redaction_target(event.raw(), redaction_rules)
                    && pinned_event_ids.contains(&redaction_target)
                {
                    new_timeline.events.push(event.clone());
                }
            }
        }
    }

    new_timeline
}
