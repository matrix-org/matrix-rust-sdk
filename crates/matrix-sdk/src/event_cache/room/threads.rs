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

use std::collections::{BTreeMap, HashSet};

use ruma::{events::AnySyncTimelineEvent, serde::Raw, OwnedEventId, OwnedRoomId};
use thiserror::Error;

mod serde_helpers {
    use ruma::{events::relation::BundledThread, OwnedEventId};
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

    #[derive(Deserialize)]
    pub struct SimplifiedContent {
        #[serde(rename = "m.relates_to")]
        relates_to: Option<RelatesTo>,
    }

    impl SimplifiedContent {
        pub fn thread_root(self) -> Option<OwnedEventId> {
            self.relates_to?.event_id
        }
    }
}

/// All the information and events related to a single thread.
#[derive(Debug)]
pub struct ThreadEventCache {
    /// Current, always up-to-date thread summary.
    summary: ThreadSummary,

    /// The events which belong in a thread, including the root.
    ///
    /// TODO: replace with a linked chunk!
    events: HashSet<OwnedEventId>,
}

/// A model of all the threads in a room.
pub struct RoomThreads {
    room_id: OwnedRoomId,

    threads: BTreeMap<OwnedEventId, ThreadEventCache>,
}

impl RoomThreads {
    pub fn new(room_id: OwnedRoomId) -> Self {
        // TODO: persistent storage, reload threads from the database, etc.
        Self { room_id, threads: BTreeMap::new() }
    }

    /// Given a new sync event, process it and update the thread cache.
    ///
    /// If the sync event was the root of a thread we didn't know about, or part
    /// of a thread, this will return a thread summary update (along with
    /// the thread root id), to be forwarded to observers.
    fn on_new_sync_event(
        &mut self,
        raw: &Raw<AnySyncTimelineEvent>,
    ) -> Result<Option<(OwnedEventId, ThreadSummary)>, EventCacheThreadError> {
        let Some(event_id) = raw.get_field::<OwnedEventId>("event_id").ok().flatten() else {
            // No event id, nothing to do.
            return Ok(None);
        };

        // Try to look if it's a new thread root. Since we only create a thread summary
        // if we didn't know about the thread, check for that first, to avoid
        // deserializing for nothing.
        if !self.threads.contains_key(&event_id) {
            if let Some(summary) = ThreadSummary::extract_from_bundled(raw)? {
                self.threads.insert(
                    event_id.clone(),
                    ThreadEventCache {
                        summary: summary.clone(),
                        events: HashSet::from_iter([event_id.clone()]),
                    },
                );
                return Ok(Some((event_id, summary)));
            }
        }

        // Alternatively, try to see if this is event is part of a thread itself.
        if let Some(thread_root) = raw
            .get_field::<serde_helpers::SimplifiedContent>("content")
            .ok()
            .flatten()
            .and_then(|content| content.thread_root())
        {
            if let Some(thread_cache) = self.threads.get_mut(&thread_root) {
                if thread_cache.events.insert(event_id.clone()) {
                    let summary = &mut thread_cache.summary;
                    summary.count = thread_cache.events.len();
                    summary.latest_event = event_id;
                    // TODO: we can update the current_user_participated field too,
                    // based on the sender.
                    //summary.current_user_participated = maybe?

                    return Ok(Some((thread_root, summary.clone())));
                }

                // The event was already known in the thread: no summary update.
                return Ok(None);
            }

            // The thread was unknown; add a stub summary.
            let summary = ThreadSummary {
                // The root + this event.
                count: 2,
                // This is the final event.
                latest_event: event_id.clone(),
                // TODO: we can do better than that, based on the sender.
                current_user_participated: false,
                // This thread is likely outdated.
                maybe_outdated: true,
            };

            self.threads.insert(
                thread_root.clone(),
                ThreadEventCache {
                    summary: summary.clone(),
                    events: HashSet::from_iter([thread_root.clone(), event_id.clone()]),
                },
            );

            Ok(Some((thread_root, summary)))
        } else {
            Ok(None)
        }
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

    use super::{RoomThreads, ThreadSummary};
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

    #[test]
    fn test_thread_summary_updated_on_new_thread_root() {
        let room_id = room_id!("!room:example.org");
        let f = EventFactory::new().sender(&ALICE).room(room_id);

        let latest_event_id = event_id!("$latest");
        let latest_event = f.text_msg("hello to you too!").event_id(latest_event_id).into_raw();
        let bundled_thread = BundledThread::new(latest_event, uint!(3), false);
        let mut relations = BundledMessageLikeRelations::new();
        relations.thread = Some(Box::new(bundled_thread));

        let root_event_id = event_id!("$root");
        let root = f
            .text_msg("hello world!")
            .bundled_relations(relations)
            .event_id(root_event_id)
            .into_raw();

        let mut room_threads = RoomThreads::new(room_id.to_owned());

        // We find a new summary the first time we process this event.
        let (thread_root, summary) = room_threads
            .on_new_sync_event(&root)
            .expect("event processing works")
            .expect("summary found");

        assert_eq!(thread_root, root_event_id);

        assert_eq!(summary.count, 3);
        assert!(summary.current_user_participated.not());
        assert_eq!(summary.latest_event, latest_event_id);
        assert!(summary.maybe_outdated);

        // But if we already knew about this thread, we won't emit a new thread summary
        // update.
        let summary_update = room_threads.on_new_sync_event(&root).expect("event processing works");
        assert_matches!(summary_update, None);
    }

    #[test]
    fn test_thread_summary_updated_on_new_thread_response() {
        let room_id = room_id!("!room:example.org");
        let f = EventFactory::new().sender(&ALICE).room(room_id);

        let root_event_id = event_id!("$root");
        let thread_event_id = event_id!("$thread");
        let event = f
            .text_msg("hello to you too!")
            .in_thread(root_event_id, thread_event_id)
            .event_id(thread_event_id)
            .into_raw();

        let mut room_threads = RoomThreads::new(room_id.to_owned());

        // We get a summary for the thread root the first time we process this event.
        let (found_root, summary) = room_threads
            .on_new_sync_event(&event)
            .expect("event processing works")
            .expect("summary found");

        assert_eq!(found_root, root_event_id);

        assert_eq!(summary.count, 2);
        assert!(summary.current_user_participated.not());
        assert_eq!(&summary.latest_event, thread_event_id);
        assert!(summary.maybe_outdated);

        // If we process it a second time, the summary isn't updated.
        let summary = room_threads.on_new_sync_event(&event).expect("event processing works");
        assert_matches!(summary, None);

        // On subsequent events, we will update the thread summary accordingly.
        let thread_event_id2 = event_id!("$thread2");
        let event = f
            .text_msg("bonjour !")
            .in_thread(root_event_id, thread_event_id2)
            .event_id(thread_event_id2)
            .into_raw();

        let (found_root, summary) = room_threads
            .on_new_sync_event(&event)
            .expect("event processing works")
            .expect("summary found");

        assert_eq!(found_root, root_event_id);

        assert_eq!(summary.count, 3);
        assert!(summary.current_user_participated.not());
        assert_eq!(&summary.latest_event, thread_event_id2);
        assert!(summary.maybe_outdated);
    }
}
