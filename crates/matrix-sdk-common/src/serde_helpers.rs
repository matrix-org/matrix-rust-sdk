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
    MilliSecondsSinceUnixEpoch, OwnedEventId,
    events::{
        AnyMessageLikeEventContent, AnySyncMessageLikeEvent, AnySyncTimelineEvent,
        relation::BundledThread,
    },
    serde::Raw,
};
use serde::Deserialize;

use crate::deserialized_responses::{ThreadSummary, ThreadSummaryStatus};

#[derive(Deserialize)]
enum RelationsType {
    #[serde(rename = "m.thread")]
    Thread,
    #[serde(rename = "m.replace")]
    Edit,
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

/// Try to extract the thread root from an event's content, if provided.
///
/// The thread root is the field located at `m.relates_to`.`event_id`,
/// if the field at `m.relates_to`.`rel_type` is `m.thread`.
///
/// Returns `None` if we couldn't find a thread root, or if there was an issue
/// during deserialization.
pub fn extract_thread_root_from_content(
    content: Raw<AnyMessageLikeEventContent>,
) -> Option<OwnedEventId> {
    let relates_to = content.deserialize_as_unchecked::<SimplifiedContent>().ok()?.relates_to?;
    match relates_to.rel_type {
        RelationsType::Thread => relates_to.event_id,
        RelationsType::Edit => None,
    }
}

/// Try to extract the thread root from a timeline event, if provided.
///
/// The thread root is the field located at `content`.`m.relates_to`.`event_id`,
/// if the field at `content`.`m.relates_to`.`rel_type` is `m.thread`.
///
/// Returns `None` if we couldn't find a thread root, or if there was an issue
/// during deserialization.
pub fn extract_thread_root(event: &Raw<AnySyncTimelineEvent>) -> Option<OwnedEventId> {
    extract_thread_root_from_content(event.get_field("content").ok().flatten()?)
}

/// Try to extract the target of an edit event, from a raw timeline event, if
/// provided.
///
/// The target event is the field located at
/// `content`.`m.relates_to`.`event_id`, if the field at
/// `content`.`m.relates_to`.`rel_type` is `m.replace`.
///
/// Returns `None` if we couldn't find it, or if there was an issue
/// during deserialization.
pub fn extract_edit_target(event: &Raw<AnySyncTimelineEvent>) -> Option<OwnedEventId> {
    let relates_to = event.get_field::<SimplifiedContent>("content").ok().flatten()?.relates_to?;
    match relates_to.rel_type {
        RelationsType::Edit => relates_to.event_id,
        RelationsType::Thread => None,
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
) -> (ThreadSummaryStatus, Option<Raw<AnySyncMessageLikeEvent>>) {
    match event.get_field::<Unsigned>("unsigned") {
        Ok(Some(Unsigned { relations: Some(Relations { thread: Some(bundled_thread) }) })) => {
            // Take the count from the bundled thread summary, if available. If it can't be
            // converted to a `u32`, we use `u32::MAX` as a fallback, as this is unlikely
            // to happen to have that many events in real-world threads.
            let count = bundled_thread.count.try_into().unwrap_or(u32::MAX);

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

/// Try to extract the `origin_server_ts`, if available.
///
/// If the value is larger than `max_value`, it becomes `max_value`. This is
/// necessary to prevent against user-forged value pretending an event is coming
/// from the future.
pub fn extract_timestamp(
    event: &Raw<AnySyncTimelineEvent>,
    max_value: MilliSecondsSinceUnixEpoch,
) -> Option<MilliSecondsSinceUnixEpoch> {
    let mut origin_server_ts = event.get_field("origin_server_ts").ok().flatten()?;

    if origin_server_ts > max_value {
        origin_server_ts = max_value;
    }

    Some(origin_server_ts)
}

#[cfg(test)]
mod tests {
    use assert_matches::assert_matches;
    use ruma::{UInt, event_id};
    use serde_json::json;

    use super::{
        MilliSecondsSinceUnixEpoch, Raw, extract_bundled_thread_summary, extract_thread_root,
        extract_timestamp,
    };
    use crate::deserialized_responses::{ThreadSummary, ThreadSummaryStatus};

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
        .cast_unchecked();

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
        .cast_unchecked();

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
        .cast_unchecked();

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
        .cast_unchecked();

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
        .cast_unchecked();

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
        .cast_unchecked();

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
        .cast_unchecked();

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
        .cast_unchecked();

        assert_matches!(
            extract_bundled_thread_summary(&event),
            (ThreadSummaryStatus::Unknown, None)
        );
    }

    #[test]
    fn test_extract_timestamp() {
        let event = Raw::new(&json!({
            "event_id": "$ev0",
            "type": "m.room.message",
            "sender": "@mnt_io:matrix.org",
            "origin_server_ts": 42,
            "content": {
                "body": "Le gras, c'est la vie",
            }
        }))
        .unwrap()
        .cast_unchecked();

        let timestamp = extract_timestamp(&event, MilliSecondsSinceUnixEpoch(UInt::from(100u32)));

        assert_eq!(timestamp, Some(MilliSecondsSinceUnixEpoch(UInt::from(42u32))));
    }

    #[test]
    fn test_extract_timestamp_no_origin_server_ts() {
        let event = Raw::new(&json!({
            "event_id": "$ev0",
            "type": "m.room.message",
            "sender": "@mnt_io:matrix.org",
            "content": {
                "body": "Le gras, c'est la vie",
            }
        }))
        .unwrap()
        .cast_unchecked();

        let timestamp = extract_timestamp(&event, MilliSecondsSinceUnixEpoch(UInt::from(100u32)));

        assert!(timestamp.is_none());
    }

    #[test]
    fn test_extract_timestamp_invalid_origin_server_ts() {
        let event = Raw::new(&json!({
            "event_id": "$ev0",
            "type": "m.room.message",
            "sender": "@mnt_io:matrix.org",
            "origin_server_ts": "saucisse",
            "content": {
                "body": "Le gras, c'est la vie",
            }
        }))
        .unwrap()
        .cast_unchecked();

        let timestamp = extract_timestamp(&event, MilliSecondsSinceUnixEpoch(UInt::from(100u32)));

        assert!(timestamp.is_none());
    }

    #[test]
    fn test_extract_timestamp_malicious_origin_server_ts() {
        let event = Raw::new(&json!({
            "event_id": "$ev0",
            "type": "m.room.message",
            "sender": "@mnt_io:matrix.org",
            "origin_server_ts": 101,
            "content": {
                "body": "Le gras, c'est la vie",
            }
        }))
        .unwrap()
        .cast_unchecked();

        let timestamp = extract_timestamp(&event, MilliSecondsSinceUnixEpoch(UInt::from(100u32)));

        assert_eq!(timestamp, Some(MilliSecondsSinceUnixEpoch(UInt::from(100u32))));
    }
}
