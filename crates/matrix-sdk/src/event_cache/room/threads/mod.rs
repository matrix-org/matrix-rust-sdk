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

use std::collections::BTreeMap;

use matrix_sdk_base::{
    deserialized_responses::TimelineEvent, event_cache::Gap, linked_chunk::LinkedChunk,
};
use ruma::{events::AnySyncTimelineEvent, serde::Raw, OwnedEventId};
use thiserror::Error;
use tracing::warn;

mod serde_helpers;

/// All the information and events related to a single thread.
#[derive(Debug)]
pub struct ThreadEventCache {
    /// Current, always up-to-date thread summary.
    summary: ThreadSummary,

    /// The events which belong in a thread, including the root.
    events: LinkedChunk<16, TimelineEvent, Gap>,
}

/// A model of all the threads in a room.
pub struct RoomThreads {
    threads: BTreeMap<OwnedEventId, ThreadEventCache>,
}

impl RoomThreads {
    pub fn new() -> Self {
        // TODO: persistent storage, reload threads from the database, etc.
        Self { threads: BTreeMap::new() }
    }

    /// Get all the event ids from a given thread, sorted in sync order, if it's
    /// known.
    #[cfg(test)]
    fn thread_event_ids(&self, root_event_id: &ruma::EventId) -> Option<Vec<OwnedEventId>> {
        self.threads
            .get(root_event_id)
            .map(|thread| thread.events.items().filter_map(|(_, item)| item.event_id()).collect())
    }

    /// Handle live events to include in the thread cache.
    ///
    /// `is_gappy` must be true if, and only if, the event cache concluded that
    /// there *might* be some missing events in the timeline.
    pub(super) fn handle_live(
        &mut self,
        events: Vec<TimelineEvent>,
        is_gappy: bool,
    ) -> Result<BTreeMap<OwnedEventId, ThreadSummary>, EventCacheThreadError> {
        let mut updates = BTreeMap::new();

        if is_gappy {
            // There's a gap in the sync. This means there *might* be a gap in *some*
            // thread (or no gaps anywhere), but we don't have a per-thread back-pagination
            // token that we could use to backfill threads. We cannot decide which threads
            // have missing events, so we unfortunately have to reset *all* of them.
            //
            // TODO: optionally we could start a back-pagination automatically, before
            // resetting, since some gappy syncs are expected to be spurious (i.e. the first
            // batch of backpaginated events would include all the known events).
            for (root, thread) in &mut self.threads {
                // A note: we intentionally don't reset any information related to the thread in
                // here, as we expect that this will be a temporary state. In particular, the
                // count may be incorrect until the caller paginated the full
                // thread.
                //
                // XXX: we could use /context (with no previous/after events) to get a proper,
                // up-to-date summary, but that may not scale nicely with the number of threads
                // in the room.
                thread.summary.maybe_outdated = true;
                updates.insert(root.clone(), thread.summary.clone());

                // Reset all the linked chunks though, and make sure to not push the new events,
                // so that a future attempt at back-paginating the thread will
                // effectively start from the end.
                //
                // TODO: when persisted, unload all the chunks and add an "empty" (i.e.
                // tokenless) gap to the linked chunk (without adding any sync
                // events), so that a pagination must start from the end of the
                // thread.
                thread.events.clear();
            }
        } else {
            // Process each event, which may be either a thread root, a thread reply, or
            // none of these.
            for event in events {
                if let Some((root, summary)) = self.on_new_sync_event(event)? {
                    // We have a new thread summary to emit.
                    updates.insert(root, summary);
                }
            }
        }

        Ok(updates)
    }

    /// Given a new sync event, process it and update the thread cache.
    ///
    /// If the sync event was the root of a thread we didn't know about, or part
    /// of a thread, this will return a thread summary update (along with
    /// the thread root id), to be forwarded to observers.
    fn on_new_sync_event(
        &mut self,
        event: TimelineEvent,
    ) -> Result<Option<(OwnedEventId, ThreadSummary)>, EventCacheThreadError> {
        let raw = event.raw();

        let Some(event_id) = raw.get_field::<OwnedEventId>("event_id").ok().flatten() else {
            // No event id, nothing to do.
            warn!("event had not event id, ignoring");
            return Ok(None);
        };

        // Try to look if it's a new thread root. Since we only create a thread summary
        // if we didn't know about the thread, check for that first, to avoid
        // deserializing for nothing.
        if !self.threads.contains_key(&event_id) {
            // XXX: is the bundled thread latest event stored decrypted, if the event was
            // encrypted?
            if let Some(summary) = ThreadSummary::extract_from_bundled(raw)? {
                let mut events = LinkedChunk::new_with_update_history();

                // If it had a bundled thread summary, then this event is the thread root. Push
                // it to the back of the (previously empty, just created) events chunk.
                events.push_items_back([event]);

                // TODO: save the bundled latest event into the event cache, if provided!

                self.threads.insert(
                    event_id.clone(),
                    ThreadEventCache { summary: summary.clone(), events },
                );

                return Ok(Some((event_id, summary)));
            }
        }

        // Alternatively, try to see if this is event is part of a thread itself.
        let Some(thread_root) = raw
            .get_field::<serde_helpers::SimplifiedContent>("content")
            .ok()
            .flatten()
            .and_then(|content| content.thread_root())
        else {
            // No thread relationship.
            return Ok(None);
        };

        // This event is part of a thread, so we'll update both the thread summary and
        // the thread linked chunk.

        if let Some(thread_cache) = self.threads.get_mut(&thread_root) {
            // This thread was already known; update the summary and the events.
            let mut increase_count = true;

            // Look for the received event in the linked chunk, so we deduplicate it if we
            // must.
            if let Some(pos) =
                thread_cache.events.item_position(|item| item.event_id() == event.event_id())
            {
                // The event was already known in the thread, so we will remove it… unless it
                // was at the same position where we'd push it back, in which case we'll do
                // nothing.
                if let Some((last_pos, _)) = thread_cache.events.ritems().next() {
                    if last_pos == pos {
                        // No need to remove and reinsert the item at the same position; the
                        // summary doesn't change. We're done here!
                        return Ok(None);
                    }
                }

                // We found the event, at a position different from the end; remove it.
                thread_cache
                    .events
                    .remove_item_at(pos)
                    .expect("we could remove the item we just found");

                increase_count = false;
            }

            // The event was either not known, or it's been deduplicated: append it to the
            // back, since we're receiving it from sync.
            thread_cache.events.push_items_back([event]);

            let summary = &mut thread_cache.summary;
            summary.latest_event = event_id;
            if increase_count {
                // If the event was not known, increase the count.
                summary.count += 1;
            }

            // TODO: we can update the current_user_participated field too,
            // based on the event's sender / mentions etc.
            //summary.current_user_participated = maybe?

            // Note: the event is saved by the main linked chunk, so we don't need to save
            // it into the persisted storage here.

            return Ok(Some((thread_root, summary.clone())));
        }

        // The thread was unknown; add a stub summary.
        let summary = ThreadSummary {
            // The root + this event.
            count: 2,
            // This is the final event.
            latest_event: event_id,
            // TODO: we can do better than that, based on the sender.
            current_user_participated: false,
            // This thread is likely outdated.
            maybe_outdated: true,
        };

        // Create a stub linked chunk with with this event.
        // XXX: i think this will get problematic as soon as it's back-paginated? if the
        // pagination returned only one event (this one), then we'd think we've
        // backfilled the entire thread… Maybe it should remain empty?
        let mut events = LinkedChunk::new_with_update_history();
        events.push_items_back([event]);

        self.threads
            .insert(thread_root.clone(), ThreadEventCache { summary: summary.clone(), events });

        Ok(Some((thread_root, summary)))
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

        // TODO: we can do better than this, if the bundled latest event contains a
        // mention in an encrypted room, for example.
        let current_user_participated = bundled_thread.current_user_participated;

        // The thread summary isn't outdated when it's just been extracted.
        let maybe_outdated = false;

        Ok(Some(ThreadSummary { count, latest_event, current_user_participated, maybe_outdated }))
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
        assert!(extracted_summary.maybe_outdated.not());
    }

    #[test]
    fn test_extract_no_thread_summary() {
        let room_id = room_id!("!room:example.org");
        let f = EventFactory::new().sender(&ALICE).room(room_id);

        let root = f.text_msg("hello world!").into_raw();

        // If there's no thread summary, it can be extracted without trouble.
        let extracted_summary =
            ThreadSummary::extract_from_bundled(&root).expect("extracting works");
        assert_matches!(extracted_summary, None);

        // If there's a bundled relation, but it's not a thread, we can still extract
        // without failing.
        let mut relations = BundledMessageLikeRelations::new();
        relations.replace = Some(Box::new(f.text_msg("bonjour monde !").into_raw()));

        let root = f.text_msg("hello world!").bundled_relations(relations).into_raw();
        let extracted_summary =
            ThreadSummary::extract_from_bundled(&root).expect("extracting works");
        assert_matches!(extracted_summary, None);
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
            .into_event();

        let mut room_threads = RoomThreads::new();

        // We find a new summary the first time we process this event.
        let (thread_root, summary) = room_threads
            .on_new_sync_event(root.clone())
            .expect("event processing works")
            .expect("summary found");

        assert_eq!(thread_root, root_event_id);

        assert_eq!(summary.count, 3);
        assert!(summary.current_user_participated.not());
        assert_eq!(summary.latest_event, latest_event_id);
        assert!(summary.maybe_outdated.not());

        // But if we already knew about this thread, we won't emit a new thread summary
        // update.
        let summary_update = room_threads.on_new_sync_event(root).expect("event processing works");
        assert_matches!(summary_update, None);
    }

    #[test]
    fn test_thread_summary_updated_on_new_thread_response() {
        let room_id = room_id!("!room:example.org");
        let f = EventFactory::new().sender(&ALICE).room(room_id);

        let root_event_id = event_id!("$root");
        let prev_thread_event_id = event_id!("$thread_prev");
        let thread_event_id = event_id!("$thread");
        let event = f
            .text_msg("hello to you too!")
            .in_thread(root_event_id, prev_thread_event_id)
            .event_id(thread_event_id)
            .into_event();

        let mut room_threads = RoomThreads::new();

        // We get a summary for the thread root the first time we process this event.
        let (found_root, summary) = room_threads
            .on_new_sync_event(event.clone())
            .expect("event processing works")
            .expect("summary found");

        assert_eq!(found_root, root_event_id);

        assert_eq!(summary.count, 2);
        assert!(summary.current_user_participated.not());
        assert_eq!(&summary.latest_event, thread_event_id);
        assert!(summary.maybe_outdated);

        // If we process it a second time, the summary isn't updated.
        let summary = room_threads.on_new_sync_event(event).expect("event processing works");
        assert_matches!(summary, None);

        // On subsequent events, we will update the thread summary accordingly.
        let thread_event_id2 = event_id!("$thread2");
        let event = f
            .text_msg("bonjour !")
            .in_thread(root_event_id, thread_event_id)
            .event_id(thread_event_id2)
            .into_event();

        let (found_root, summary) = room_threads
            .on_new_sync_event(event)
            .expect("event processing works")
            .expect("summary found");

        assert_eq!(found_root, root_event_id);

        assert_eq!(summary.count, 3);
        assert!(summary.current_user_participated.not());
        assert_eq!(&summary.latest_event, thread_event_id2);
        assert!(summary.maybe_outdated);
    }

    #[test]
    fn test_on_new_sync_event_noop() {
        // When an event doesn't have any relevant thread information, no summary is
        // extracted.
        let room_id = room_id!("!room:example.org");
        let f = EventFactory::new().sender(&ALICE).room(room_id);

        let replied_to_event_id = event_id!("$replied_to");
        let event_id = event_id!("$thread");
        let event = f
            .text_msg("hello to you too!")
            .reply_to(replied_to_event_id)
            .event_id(event_id)
            .into_event();

        let mut room_threads = RoomThreads::new();

        let summary = room_threads.on_new_sync_event(event).expect("event processing works");
        assert_matches!(summary, None);
    }

    #[test]
    fn test_basic_handle_live() {
        // When the sync is gappy, we reset all the threads.
        let room_id = room_id!("!room:example.org");
        let f = EventFactory::new().sender(&ALICE).room(room_id);

        let thread0 = event_id!("$thread0");

        // Thread 0's event 0, etc.
        let t0eid0 = event_id!("$t0eid0");
        let t0eid1 = event_id!("$t0edi1");
        let t0eid2 = event_id!("$t0eid2");

        let t0e1 =
            f.text_msg("hello world!").event_id(t0eid1).in_thread(thread0, t0eid0).into_event();
        let t0e2 = f
            .text_msg("hello to you too!")
            .event_id(t0eid2)
            .in_thread(thread0, t0eid1)
            .into_event();

        // Another thread in the same room.
        let thread1 = event_id!("$thread1");
        let t1eid1 = event_id!("$t1eid1");

        // Reply in thread 1.
        let t1e1 =
            f.text_msg("thread reply").event_id(t1eid1).in_thread(thread1, thread1).into_event();

        // Root for thread 1, including the latest bundled event.
        let mut relations = BundledMessageLikeRelations::new();
        relations.thread =
            Some(Box::new(BundledThread::new(t1e1.raw().clone().cast(), uint!(2), false)));
        let t1e0 = f.text_msg("root").event_id(thread1).bundled_relations(relations).into_event();

        let mut room_threads = RoomThreads::new();

        // When we receive a non-gappy sync, all threads are correctly updated.
        let is_gappy = false;
        let updates = room_threads
            .handle_live(vec![t1e0, t0e1, t1e1, t0e2], is_gappy)
            .expect("event processing works");

        assert_eq!(updates.len(), 2);

        let thread0_summary = updates.get(thread0).expect("thread 0 summary found");

        // 2 explicit thread events + implicit root.
        assert_eq!(thread0_summary.count, 3);

        // The thread is marked as potentially outdated, since we didn't observe the
        // thread root with its bundled summary.
        assert!(thread0_summary.maybe_outdated);

        // But the real chunk only contains the two events, not the root.
        assert_eq!(
            room_threads.thread_event_ids(thread0).expect("thread 0 events found"),
            vec![t0eid1, t0eid2]
        );

        let thread1_summary = updates.get(thread1).expect("thread 1 summary found");
        // XXX Interesting issue here: the thread summary was correct because it
        // included the latest event, but now we've counted it twice…
        assert_eq!(thread1_summary.count, 2 + 1);
        assert!(thread1_summary.maybe_outdated.not());

        // The chunk does count the two events.
        assert_eq!(
            room_threads.thread_event_ids(thread1).expect("thread 1 events found"),
            vec![thread1, t1eid1]
        );

        // But after a gappy sync, all threads are reset, and we receive summary updates
        // for each thread. Thread events are not pushed back, in this case.
        let is_gappy = true;

        let t0eid3 = event_id!("$t0eid3");
        let t0e3 = f
            .text_msg("gappy hello world!")
            .event_id(t0eid3)
            .in_thread(thread0, t0eid2)
            .into_event();

        let updates =
            room_threads.handle_live(vec![t0e3], is_gappy).expect("event processing works");

        assert_eq!(updates.len(), 2);

        let thread0_summary = updates.get(thread0).expect("thread 0 summary found");
        // Still the same number of events in the summary…
        assert_eq!(thread0_summary.count, 2 + 1);
        // And now the thread summary is marked as outdated.
        assert!(thread0_summary.maybe_outdated);
        // …but the linked chunk is now empty!
        assert!(room_threads.thread_event_ids(thread0).expect("thread 0 events found").is_empty());

        let thread1_summary = updates.get(thread1).expect("thread 1 summary found");
        // Still the same number of events in the summary…
        assert_eq!(thread1_summary.count, 3);
        // And now the thread summary is marked as outdated.
        assert!(thread1_summary.maybe_outdated);
        // …but the linked chunk is now empty!
        assert!(room_threads.thread_event_ids(thread1).expect("thread 1 events found").is_empty());
    }

    #[test]
    fn test_handle_live_deduplication() {
        // When we sync events that are duplicated, they end up being deduplicated.
        let room_id = room_id!("!room:example.org");
        let f = EventFactory::new().sender(&ALICE).room(room_id);

        let thread = event_id!("$thread");

        let eid0 = event_id!("$eid0");
        let eid1 = event_id!("$edi1");
        let eid2 = event_id!("$eid2");

        let event1 = f.text_msg("hello world!").event_id(eid1).in_thread(thread, eid0).into_event();
        let event2 =
            f.text_msg("hello to you too!").event_id(eid2).in_thread(thread, eid1).into_event();

        let mut room_threads = RoomThreads::new();

        // When we receive a non-gappy sync, all threads are correctly updated, even if
        // an event has been duplicated.
        let is_gappy = false;

        {
            let updates = room_threads
                .handle_live(vec![event1.clone(), event2.clone()], is_gappy)
                .expect("event processing works");

            assert_eq!(updates.len(), 1);

            let thread0_summary = updates.get(thread).expect("thread summary found");
            // 2 explicit thread events + implicit root.
            assert_eq!(thread0_summary.count, 3);
            assert_eq!(thread0_summary.latest_event, eid2);

            // Sanity check: the linked chunk contains the two events.
            assert_eq!(
                room_threads.thread_event_ids(thread).expect("thread events found"),
                vec![eid1, eid2]
            );
        }

        // Then event2 is sync'd again, for some reason: in this case, it's a noop.
        {
            let updates =
                room_threads.handle_live(vec![event2], is_gappy).expect("event processing works");

            // No new thread summary is emitted, because the event was already known, and
            // deduplicated at the same position where it would've been reinserted.
            assert!(updates.is_empty());

            // Sanity check: the linked chunk still contains the two events in the same
            // order.
            assert_eq!(
                room_threads.thread_event_ids(thread).expect("thread events found"),
                vec![eid1, eid2]
            );
        }

        // Then event1 is sync'd again, but this time at the end of the thread. It's
        // supposedly impossible to happen in practice, but we want to test
        // deduplication, in the spirit of this test.
        {
            let updates =
                room_threads.handle_live(vec![event1], is_gappy).expect("event processing works");

            assert_eq!(updates.len(), 1);

            let summary = updates.get(thread).expect("thread summary found");
            // Still 2 explicit thread events + implicit root.
            assert_eq!(summary.count, 3);
            // But now the latest event has been updated to eid1.
            assert_eq!(summary.latest_event, eid1);

            // The chunk contains event2, *then* event1, since that's the final order from
            // the syncs.
            assert_eq!(
                room_threads.thread_event_ids(thread).expect("thread events found"),
                vec![eid2, eid1]
            );
        }
    }
}
