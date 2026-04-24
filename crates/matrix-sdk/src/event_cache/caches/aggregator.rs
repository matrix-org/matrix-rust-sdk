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
    serde_helpers::{extract_edit_target, extract_thread_root},
    sync::Timeline,
};
use ruma::OwnedEventId;

use super::{super::Result, room::RoomEventCacheStateLockReadGuard, thread::ThreadEventCache};

pub fn aggregate_timeline_for_room(timeline: Timeline) -> Timeline {
    timeline
}

pub async fn aggregate_timeline_for_threads(
    timeline: &Timeline,
    existing_threads: &HashMap<OwnedEventId, ThreadEventCache>,
    room_event_cache: RoomEventCacheStateLockReadGuard<'_>,
) -> Result<HashMap<OwnedEventId, Timeline>> {
    let mut new_events_by_thread = HashMap::new();

    let default_timeline = || Timeline {
        limited: timeline.limited,
        prev_batch: timeline.prev_batch.clone(),
        events: Vec::new(),
    };

    for event in &timeline.events {
        // This event is part of a thread.
        if let Some(thread_root) = extract_thread_root(event.raw()) {
            new_events_by_thread
                .entry(thread_root)
                .or_insert_with(default_timeline)
                .events
                .push(event.clone());
        }
        // This event is the root of a thread.
        else if let Some(event_id) = event.event_id()
            && existing_threads.contains_key(&event_id)
        {
            new_events_by_thread
                .entry(event_id)
                .or_insert_with(default_timeline)
                .events
                .push(event.clone());
        }

        // This event is an edit that may apply to a thread..
        if let Some(edit_target) = extract_edit_target(event.raw()) {
            // This event is known and part of a thread.
            if let Some((_location, edit_target_event)) =
                room_event_cache.find_event(&edit_target).await?
                && let Some(thread_root) = extract_thread_root(edit_target_event.raw())
            {
                new_events_by_thread.entry(thread_root).or_insert_with(default_timeline);
            }
        }
    }

    Ok(new_events_by_thread)
}
