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

use as_variant::as_variant;
use ruma::{events::AnySyncTimelineEvent, serde::Raw, OwnedEventId};
use thiserror::Error;

mod serde_helpers {
    use ruma::events::relation::BundledThread;
    use serde::Deserialize;

    #[derive(Deserialize)]
    pub struct Relations {
        #[serde(rename = "m.thread")]
        pub thread: Option<Box<BundledThread>>,
    }

    #[derive(Deserialize)]
    pub struct Unsigned {
        #[serde(rename = "m.relations")]
        pub relations: Option<Relations>,
    }
}

#[derive(Debug, Error)]
pub enum EventCacheThreadError {
    #[error("Deserialization error when extracting thread summary from the event: {0}")]
    SummaryDeserialization(String),
}

/// A thread summary, as computed locally.
#[derive(Clone, Debug)]
pub struct ThreadSummary {
    /// Number of events in the thread.
    ///
    /// TODO: make this the "meaningful" count, for some definition of
    /// "meaningful".
    pub count: usize,

    /// The latest event in the thread.
    ///
    /// TODO: make this the "meaningful" latest event, for some definition of
    /// "meaningful".
    pub latest_event: OwnedEventId,

    /// Did the current user participate in the thread?
    ///
    /// TODO: this is the server data at this point, try to include some of the
    /// logic behind the participation model.
    pub current_user_participated: bool,

    /// Is this thread summary potentially outdated?
    ///
    /// If so, it will be refreshed in the background, and a new thread summary
    /// update will be emitted as soon as it's correct.
    pub maybe_outdated: bool,
}

impl ThreadSummary {
    /// Extract the thread summary from an event's bundled aggregation.
    ///
    /// Will return `Ok(None)` if the event doesn't represent a thread, an error
    /// if the deserialization failed at some point, or the thread summary
    /// otherwise.
    fn extract_from_bundled(
        root: &Raw<AnySyncTimelineEvent>,
    ) -> Result<Option<Self>, EventCacheThreadError> {
        let unsigned = root
            .get_field::<serde_helpers::Unsigned>("unsigned")
            .map_err(|err| EventCacheThreadError::SummaryDeserialization(err.to_string()))?;

        let Some(bundled_thread) = unsigned.and_then(|u| u.relations?.thread) else {
            // No thread information, return early.
            return Ok(None);
        };

        let count = bundled_thread.count.try_into().map_err(|_| {
            EventCacheThreadError::SummaryDeserialization(
                "overflow for count of events in thread".to_owned(),
            )
        })?;

        let latest_event = bundled_thread
            .latest_event
            .get_field::<OwnedEventId>("event_id")
            .map_err(|err| {
                EventCacheThreadError::SummaryDeserialization(format!(
                    "unable to deserialize an event id in a thread summary: {err}",
                ))
            })?
            .ok_or_else(|| {
                EventCacheThreadError::SummaryDeserialization(
                    "missing event id in the latest thread event of a thread summary".to_owned(),
                )
            })?;

        Ok(Some(ThreadSummary {
            count,
            latest_event,
            current_user_participated: bundled_thread.current_user_participated,
            maybe_outdated: true,
        }))
    }
}

#[cfg(test)]
mod tests {
    use std::ops::Not as _;

    use assert_matches::assert_matches;
    use matrix_sdk_test::{event_factory::EventFactory, ALICE};
    use ruma::{
        event_id,
        events::{relation::BundledThread, BundledMessageLikeRelations},
        room_id, uint,
    };

    use super::ThreadSummary;
    use crate::event_cache::room::threads::EventCacheThreadError;

    #[test]
    fn test_extract_thread_summary() {
        let room_id = room_id!("!room:example.org");
        let f = EventFactory::new().sender(&ALICE).room(room_id);

        let latest_event_id = event_id!("$latest");
        let latest_event = f.text_msg("hello to you too!").event_id(latest_event_id).into_raw();
        let bundled_thread = BundledThread::new(latest_event, uint!(3), false);
        let mut relations = BundledMessageLikeRelations::new();
        relations.thread = Some(Box::new(bundled_thread));

        let root = f.text_msg("hello world!").bundled_relations(relations).into_raw();

        let extracted_summary = ThreadSummary::extract_from_bundled(&root)
            .expect("extracting works")
            .expect("summary found");

        assert_eq!(extracted_summary.count, 3);
        assert!(extracted_summary.current_user_participated.not());
        assert_eq!(extracted_summary.latest_event, latest_event_id);

        // Summaries are assumed to be outdated when extracted the first time.
        assert!(extracted_summary.maybe_outdated);
    }

    #[test]
    fn test_extract_invalid_thread_summary() {
        let room_id = room_id!("!room:example.org");
        let f = EventFactory::new().sender(&ALICE).room(room_id);

        // The latest event doesn't have an event id: this will cause an error while
        // extracting the thread summary.
        let latest_event = f.text_msg("hello to you too!").no_event_id().into_raw();
        let bundled_thread = BundledThread::new(latest_event, uint!(3), false);
        let mut relations = BundledMessageLikeRelations::new();
        relations.thread = Some(Box::new(bundled_thread));

        let root = f.text_msg("hello world!").bundled_relations(relations).into_raw();

        let err = ThreadSummary::extract_from_bundled(&root).expect_err("extracting didn't work");
        assert_matches!(err, EventCacheThreadError::SummaryDeserialization(_));
    }
}
