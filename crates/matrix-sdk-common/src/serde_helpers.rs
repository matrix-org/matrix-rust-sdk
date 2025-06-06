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

use ruma::{
    events::{relation::BundledThread, AnyMessageLikeEvent, AnySyncTimelineEvent},
    serde::Raw,
    OwnedEventId, UInt,
};
use serde::Deserialize;

use crate::deserialized_responses::{ThreadSummary, ThreadSummaryStatus};

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

#[allow(missing_debug_implementations)]
#[derive(Deserialize)]
struct Relations {
    #[serde(rename = "m.thread")]
    thread: Option<Box<BundledThread>>,
}

#[allow(missing_debug_implementations)]
#[derive(Deserialize)]
struct Unsigned {
    #[serde(rename = "m.relations")]
    relations: Option<Relations>,
}

/// Try to extract a bundled thread summary of a timeline event, if available.
pub fn extract_bundled_thread_summary(
    event: &Raw<AnySyncTimelineEvent>,
) -> (ThreadSummaryStatus, Option<Raw<AnyMessageLikeEvent>>) {
    match event.get_field::<Unsigned>("unsigned") {
        Ok(Some(Unsigned { relations: Some(Relations { thread: Some(bundled_thread) }) })) => {
            // Take the count from the bundled thread summary, if available. If it can't be
            // converted to a `u64`, we use `UInt::MAX` as a fallback, as this is unlikely
            // to happen to have that many events in real-world threads.
            let count = bundled_thread.count.try_into().unwrap_or(UInt::MAX.try_into().unwrap());

            let latest_reply =
                bundled_thread.latest_event.get_field::<OwnedEventId>("event_id").ok().flatten();

            (
                ThreadSummaryStatus::Some(ThreadSummary { num_replies: count, latest_reply }),
                Some(bundled_thread.latest_event),
            )
        }
        Ok(_) => (ThreadSummaryStatus::None, None),
        Err(_) => (ThreadSummaryStatus::Unknown, None),
    }
}

#[cfg(test)]
mod tests {
    use assert_matches::assert_matches;
    use ruma::{event_id, serde::Raw};
    use serde_json::json;

    use super::extract_thread_root;
    use crate::{
        deserialized_responses::{ThreadSummary, ThreadSummaryStatus},
        serde_helpers::extract_bundled_thread_summary,
    };

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

    #[test]
    fn test_extract_bundled_thread_summary() {
        // When there's a bundled thread summary, we can extract it.
        let event = Raw::new(&json!({
            "event_id": "$eid:example.com",
            "type": "m.room.message",
            "sender": "@alice:example.com",
            "origin_server_ts": 42,
            "content": {
                "body": "Hello, world!",
            },
            "unsigned": {
                "m.relations": {
                    "m.thread": {
                        "latest_event": {
                            "event_id": "$latest_event:example.com",
                            "type": "m.room.message",
                            "sender": "@bob:example.com",
                            "origin_server_ts": 42,
                            "content": {
                                "body": "Hello to you too!",
                            }
                        },
                        "count": 2,
                        "current_user_participated": true,
                    }
                }
            }
        }))
        .unwrap()
        .cast();

        assert_matches!(
            extract_bundled_thread_summary(&event),
            (ThreadSummaryStatus::Some(ThreadSummary { .. }), Some(..))
        );

        // When there's a bundled thread summary, we can assert it with certainty.
        let event = Raw::new(&json!({
            "event_id": "$eid:example.com",
            "type": "m.room.message",
            "sender": "@alice:example.com",
            "origin_server_ts": 42,
        }))
        .unwrap()
        .cast();

        assert_matches!(extract_bundled_thread_summary(&event), (ThreadSummaryStatus::None, None));

        // When there's a bundled replace, we can assert there's no thread summary.
        let event = Raw::new(&json!({
            "event_id": "$eid:example.com",
            "type": "m.room.message",
            "sender": "@alice:example.com",
            "origin_server_ts": 42,
            "content": {
                "body": "Bonjour, monde!",
            },
            "unsigned": {
                "m.relations": {
                    "m.replace":
                    {
                        "event_id": "$update:example.com",
                        "type": "m.room.message",
                        "sender": "@alice:example.com",
                        "origin_server_ts": 43,
                        "content": {
                            "body": "* Hello, world!",
                        }
                    },
                }
            }
        }))
        .unwrap()
        .cast();

        assert_matches!(extract_bundled_thread_summary(&event), (ThreadSummaryStatus::None, None));

        // When the bundled thread summary is malformed, we return
        // `ThreadSummaryStatus::Unknown`.
        let event = Raw::new(&json!({
            "event_id": "$eid:example.com",
            "type": "m.room.message",
            "sender": "@alice:example.com",
            "origin_server_ts": 42,
            "unsigned": {
                "m.relations": {
                    "m.thread": {
                        // Missing `latest_event` field.
                    }
                }
            }
        }))
        .unwrap()
        .cast();

        assert_matches!(
            extract_bundled_thread_summary(&event),
            (ThreadSummaryStatus::Unknown, None)
        );
    }
}
