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

//! A collection of serde helpers to avoid having to deserialize an entire event
//! to access some fields.

use ruma::{events::AnySyncTimelineEvent, serde::Raw, OwnedEventId};
use serde::Deserialize;

#[derive(Deserialize)]
enum RelationsType {
    #[serde(rename = "m.thread")]
    Thread,
}

#[derive(Deserialize)]
struct RelatesTo {
    #[serde(rename = "rel_type")]
    rel_type: RelationsType,
    #[serde(rename = "event_id")]
    event_id: Option<OwnedEventId>,
}

#[allow(missing_debug_implementations)]
#[derive(Deserialize)]
struct SimplifiedContent {
    #[serde(rename = "m.relates_to")]
    relates_to: Option<RelatesTo>,
}

/// Try to extract the thread root from a timeline event, if provided.
///
/// The thread root is the field located at `content`.`m.relates_to`.`event_id`,
/// if the field at `content`.`m.relates_to`.`rel_type` is `m.thread`.
///
/// Returns `None` if we couldn't find a thread root, or if there was an issue
/// during deserialization.
pub fn extract_thread_root(event: &Raw<AnySyncTimelineEvent>) -> Option<OwnedEventId> {
    let relates_to = event.get_field::<SimplifiedContent>("content").ok().flatten()?.relates_to?;
    match relates_to.rel_type {
        RelationsType::Thread => relates_to.event_id,
    }
}

#[cfg(test)]
mod tests {
    use assert_matches::assert_matches;
    use ruma::{event_id, serde::Raw};
    use serde_json::json;

    use super::extract_thread_root;

    #[test]
    fn test_extract_thread_root() {
        // No event factory in this crate :( There would be a dependency cycle with the
        // `matrix-sdk-test` crate if we tried to use it here.

        // We can extract the thread root from a regular message that contains one.
        let thread_root = event_id!("$thread_root_event_id:example.com");
        let event = Raw::new(&json!({
            "event_id": "$eid:example.com",
            "type": "m.room.message",
            "sender": "@alice:example.com",
            "origin_server_ts": 42,
            "content": {
                "body": "Hello, world!",
                "m.relates_to": {
                    "rel_type": "m.thread",
                    "event_id": thread_root,
                }
            }
        }))
        .unwrap()
        .cast();

        let observed_thread_root = extract_thread_root(&event);
        assert_eq!(observed_thread_root.as_deref(), Some(thread_root));

        // If the event doesn't have a content for some reason (redacted), it returns
        // None.
        let event = Raw::new(&json!({
            "event_id": "$eid:example.com",
            "type": "m.room.message",
            "sender": "@alice:example.com",
            "origin_server_ts": 42,
        }))
        .unwrap()
        .cast();

        let observed_thread_root = extract_thread_root(&event);
        assert_matches!(observed_thread_root, None);

        // If the event has a content but with no `m.relates_to` field, it returns None.
        let event = Raw::new(&json!({
            "event_id": "$eid:example.com",
            "type": "m.room.message",
            "sender": "@alice:example.com",
            "origin_server_ts": 42,
            "content": {
                "body": "Hello, world!",
            }
        }))
        .unwrap()
        .cast();

        let observed_thread_root = extract_thread_root(&event);
        assert_matches!(observed_thread_root, None);

        // If the event has a relation, but it's not a thread reply, it returns None.
        let event = Raw::new(&json!({
            "event_id": "$eid:example.com",
            "type": "m.room.message",
            "sender": "@alice:example.com",
            "origin_server_ts": 42,
            "content": {
                "body": "Hello, world!",
                "m.relates_to": {
                    "rel_type": "m.reference",
                    "event_id": "$referenced_event_id:example.com",
                }
            }
        }))
        .unwrap()
        .cast();

        let observed_thread_root = extract_thread_root(&event);
        assert_matches!(observed_thread_root, None);
    }
}
