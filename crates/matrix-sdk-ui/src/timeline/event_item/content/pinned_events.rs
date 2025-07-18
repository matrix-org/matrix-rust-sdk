// Copyright 2024 The Matrix.org Foundation C.I.C.
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

use std::collections::HashSet;

use ruma::{
    OwnedEventId,
    events::{FullStateEventContent, room::pinned_events::RoomPinnedEventsEventContent},
};

#[derive(Clone, Debug)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
/// The type of change between the previous and current pinned events.
pub enum RoomPinnedEventsChange {
    /// Only new event ids were added.
    Added,
    /// Only event ids were removed.
    Removed,
    /// Some change other than only adding or only removing ids happened.
    Changed,
}

impl From<&FullStateEventContent<RoomPinnedEventsEventContent>> for RoomPinnedEventsChange {
    fn from(value: &FullStateEventContent<RoomPinnedEventsEventContent>) -> Self {
        match value {
            FullStateEventContent::Original { content, prev_content } => {
                if let Some(prev_content) = prev_content {
                    let mut new_pinned: HashSet<&OwnedEventId> =
                        HashSet::from_iter(&content.pinned);
                    if let Some(old_pinned) = &prev_content.pinned {
                        let mut still_pinned: HashSet<&OwnedEventId> =
                            HashSet::from_iter(old_pinned);

                        // Newly added elements will be kept in new_pinned, previous ones in
                        // still_pinned instead
                        still_pinned.retain(|item| new_pinned.remove(item));

                        let added = !new_pinned.is_empty();
                        let removed = still_pinned.len() < old_pinned.len();
                        if added && removed {
                            RoomPinnedEventsChange::Changed
                        } else if added {
                            RoomPinnedEventsChange::Added
                        } else if removed {
                            RoomPinnedEventsChange::Removed
                        } else {
                            // Any other case
                            RoomPinnedEventsChange::Changed
                        }
                    } else {
                        // We don't know the previous state, so let's assume a generic change
                        RoomPinnedEventsChange::Changed
                    }
                } else {
                    // If there is no previous content we can assume the first pinned event id was
                    // just added
                    RoomPinnedEventsChange::Added
                }
            }
            FullStateEventContent::Redacted(_) => RoomPinnedEventsChange::Changed,
        }
    }
}

#[cfg(test)]
mod tests {
    use assert_matches::assert_matches;
    use ruma::{
        events::{
            FullStateEventContent,
            room::pinned_events::{
                PossiblyRedactedRoomPinnedEventsEventContent, RedactedRoomPinnedEventsEventContent,
                RoomPinnedEventsEventContent,
            },
        },
        owned_event_id,
        serde::Raw,
    };
    use serde_json::json;

    use crate::timeline::event_item::content::pinned_events::RoomPinnedEventsChange;

    #[test]
    fn redacted_pinned_events_content_has_generic_changes() {
        let content = FullStateEventContent::Redacted(RedactedRoomPinnedEventsEventContent::new());
        let ret: RoomPinnedEventsChange = (&content).into();
        assert_matches!(ret, RoomPinnedEventsChange::Changed);
    }

    #[test]
    fn pinned_events_content_with_no_prev_content_returns_added() {
        let content = FullStateEventContent::Original {
            content: RoomPinnedEventsEventContent::new(vec![owned_event_id!("$1")]),
            prev_content: None,
        };
        let ret: RoomPinnedEventsChange = (&content).into();
        assert_matches!(ret, RoomPinnedEventsChange::Added);
    }

    #[test]
    fn pinned_events_content_with_added_ids_returns_added() {
        // This is the only way I found to create the PossiblyRedacted content
        let prev_content = possibly_redacted_content(Vec::new());
        let content = FullStateEventContent::Original {
            content: RoomPinnedEventsEventContent::new(vec![owned_event_id!("$1")]),
            prev_content,
        };
        let ret: RoomPinnedEventsChange = (&content).into();
        assert_matches!(ret, RoomPinnedEventsChange::Added);
    }

    #[test]
    fn pinned_events_content_with_removed_ids_returns_removed() {
        // This is the only way I found to create the PossiblyRedacted content
        let prev_content = possibly_redacted_content(vec!["$1"]);
        let content = FullStateEventContent::Original {
            content: RoomPinnedEventsEventContent::new(Vec::new()),
            prev_content,
        };
        let ret: RoomPinnedEventsChange = (&content).into();
        assert_matches!(ret, RoomPinnedEventsChange::Removed);
    }

    #[test]
    fn pinned_events_content_with_added_and_removed_ids_returns_changed() {
        // This is the only way I found to create the PossiblyRedacted content
        let prev_content = possibly_redacted_content(vec!["$1"]);
        let content = FullStateEventContent::Original {
            content: RoomPinnedEventsEventContent::new(vec![owned_event_id!("$2")]),
            prev_content,
        };
        let ret: RoomPinnedEventsChange = (&content).into();
        assert_matches!(ret, RoomPinnedEventsChange::Changed);
    }

    #[test]
    fn pinned_events_content_with_changed_order_returns_changed() {
        // This is the only way I found to create the PossiblyRedacted content
        let prev_content = possibly_redacted_content(vec!["$1", "$2"]);
        let content = FullStateEventContent::Original {
            content: RoomPinnedEventsEventContent::new(vec![
                owned_event_id!("$2"),
                owned_event_id!("$1"),
            ]),
            prev_content,
        };
        let ret: RoomPinnedEventsChange = (&content).into();
        assert_matches!(ret, RoomPinnedEventsChange::Changed);
    }

    #[test]
    fn pinned_events_content_with_no_changes_returns_changed() {
        // Returning Changed is counter-intuitive, but it makes no sense to display in
        // the timeline 'UserFoo didn't change anything in the pinned events'

        // This is the only way I found to create the PossiblyRedacted content
        let prev_content = possibly_redacted_content(vec!["$1", "$2"]);
        let content = FullStateEventContent::Original {
            content: RoomPinnedEventsEventContent::new(vec![
                owned_event_id!("$1"),
                owned_event_id!("$2"),
            ]),
            prev_content,
        };
        let ret: RoomPinnedEventsChange = (&content).into();
        assert_matches!(ret, RoomPinnedEventsChange::Changed);
    }

    fn possibly_redacted_content(
        ids: Vec<&str>,
    ) -> Option<PossiblyRedactedRoomPinnedEventsEventContent> {
        // This is the only way I found to create the PossiblyRedacted content
        Raw::new(&json!({
            "pinned": ids,
        }))
        .unwrap()
        .cast_unchecked::<PossiblyRedactedRoomPinnedEventsEventContent>()
        .deserialize()
        .ok()
    }
}
