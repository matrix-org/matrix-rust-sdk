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

use as_variant::as_variant;
use futures_util::future::try_join_all;
use matrix_sdk_base::{event_cache::Event, linked_chunk::LinkedChunk};
use ruma::{
    api::client::threads::get_threads::v1::IncludeThreads,
    events::{
        relation::RelationType::{self},
        AnyMessageLikeEvent, AnySyncTimelineEvent,
    },
    serde::Raw,
    OwnedEventId,
};
use tokio::sync::broadcast::{self, Sender};
use tracing::{instrument, trace, warn};

use crate::{
    room::{IncludeRelations, ListThreadsOptions, RelationsOptions, WeakRoom},
    Room,
};

#[derive(Debug, thiserror::Error)]
pub enum EventCacheThreadError {
    #[error("Couldn't list all threads for room: {0}")]
    ListThreadError(crate::Error),

    #[error("Couldn't backfill a thread: {0}")]
    BackfillThread(crate::Error),
}

/// A thread summary, as computed locally.
#[derive(Clone)]
pub struct ThreadSummary {
    /// Number of meaningful events in the thread.
    pub count: usize,

    /// The latest event in the thread.
    pub latest_event: OwnedEventId,

    /// Is this thread summary potentially outdated?
    pub maybe_outdated: bool,
}

impl ThreadSummary {
    /// Extract the thread summary from an event's bundled aggregation.
    ///
    /// Will return `None` if the event doesn't represent a thread.
    pub fn extract_from_bundled(
        root: &Raw<AnySyncTimelineEvent>,
    ) -> Option<(Self, LatestThreadEvent)> {
        let ev = root.deserialize().ok()?;
        let ev = as_variant!(ev, AnySyncTimelineEvent::MessageLike)?;
        let bundled_thread = ev.relations().thread?;

        let summary = ThreadSummary {
            count: bundled_thread.count.try_into().ok()?,
            latest_event: bundled_thread
                .latest_event
                .get_field::<OwnedEventId>("event_id")
                .ok()??,
            maybe_outdated: true,
        };

        Some((summary, LatestThreadEvent(bundled_thread.latest_event)))
    }
}

pub struct LatestThreadEvent(Raw<AnyMessageLikeEvent>);

#[derive(Clone, Debug, Default)]
struct ThreadGap {
    prev_batch: Option<String>,
    next_batch: Option<String>,
}

type ThreadLinkedChunk = LinkedChunk<32, Event, ThreadGap>;

/// A tiny event cache for a given thread in a given room.
pub struct ThreadEventCache {
    events: ThreadLinkedChunk,

    /// A thread summary.
    ///
    /// Optional because after a clear (upon gappy sync), we don't have a
    /// summary anymore, and we'll refetch it soon.
    summary: Option<ThreadSummary>,
}

impl ThreadEventCache {
    pub fn clear(&mut self) {
        self.events.clear();
        self.summary = None;
    }
}

type ThreadRoot = OwnedEventId;

/// A cache for all the threads in a room.
pub struct RoomThreadCache {
    room: WeakRoom,

    threads: BTreeMap<ThreadRoot, ThreadEventCache>,

    summary_sender: Sender<(OwnedEventId, ThreadSummary)>,
}

impl RoomThreadCache {
    pub fn new(room: WeakRoom) -> Self {
        Self { room, threads: BTreeMap::new(), summary_sender: broadcast::channel(16).0 }
    }

    /// A method to initialize a room's thread if it's empty.
    pub async fn init_if_empty(&mut self) -> Result<(), EventCacheThreadError> {
        if !self.threads.is_empty() {
            return Ok(());
        }
        self.update_all_threads().await
    }

    /// Force updating all the threads.
    ///
    /// This must be done initially to prefill a list of recent threads (if
    /// needs be), or after we've run into a gappy sync.
    ///
    /// TODO: clear all the `ThreadEventCache` after a gappy sync + call this
    /// method.
    pub async fn update_all_threads(&mut self) -> Result<(), EventCacheThreadError> {
        // Find all the threads in this room.
        let Some(room) = self.room.get() else {
            // Client is shutting down; abort.
            return Ok(());
        };

        let mut threads_to_refresh = Vec::new();
        let roots = self.list_all_threads_internal(&room, None).await?;

        for root in roots {
            let Some(root_id) = root.event_id() else {
                warn!("Skipping thread root without event id");
                continue;
            };

            // TODO: do something with the latest event
            let new_summary = ThreadSummary::extract_from_bundled(root.raw());

            // If we knew about the thread,
            if let Some(thread) = self.threads.get_mut(&root_id) {
                // And the latest event matches the one we know about, based on an event id
                // comparison,
                if thread.summary.as_ref().map(|s| &s.latest_event)
                    == new_summary.as_ref().map(|summary| &summary.0.latest_event)
                {
                    // We don't have anything to do.
                } else {
                    // Otherwise, the thread may be outdated; let observers know.
                    if let Some(summary) = thread.summary.as_mut() {
                        summary.maybe_outdated = true;
                        let _ = self.summary_sender.send((root_id.to_owned(), summary.clone()));
                    }
                    threads_to_refresh.push(root_id.to_owned());
                }
            } else {
                threads_to_refresh.push(root_id.to_owned());
            }
        }

        let promises =
            threads_to_refresh.into_iter().map(|root| Self::backfill_thread(room.clone(), root));

        let results = try_join_all(promises).await?;

        for (root, events) in results {
            let thread_cache = self.threads.entry(root).or_insert_with(|| {
                // TODO: it's stooped to recompute the summary
                // TODO: is it safe to unwrap here?
                let (summary, _latest_event) =
                    ThreadSummary::extract_from_bundled(events.first().unwrap().raw()).unwrap();

                ThreadEventCache { events: ThreadLinkedChunk::new(), summary: Some(summary) }
            });

            if let Some(summary) = thread_cache.summary.as_mut() {
                summary.maybe_outdated = false;
            }
            thread_cache.events.clear();
            thread_cache.events.push_items_back(events);
            // TODO propagate updates
        }

        Ok(())
    }

    pub async fn list_all_threads(&mut self) -> Result<Vec<Event>, EventCacheThreadError> {
        let Some(room) = self.room.get() else {
            // Client is shutting down; abort.
            return Ok(vec![]);
        };
        self.list_all_threads_internal(&room, None).await
    }

    // TODO: implement proper pagination instead of `max_iterations`.
    #[instrument(skip_all, fields(room = %room.room_id()))]
    async fn list_all_threads_internal(
        &mut self,
        room: &Room,
        max_iterations: Option<usize>,
    ) -> Result<Vec<Event>, EventCacheThreadError> {
        let mut from = None;
        let mut roots = Vec::new();

        // TODO: stop as soon as we know all the threads in the answer. This is valid if
        // the list of returned threads is somewhat deterministic. Don't think too hard
        // about threads in gaps.

        let _timer_guard = matrix_sdk_common::timer!(tracing::Level::TRACE, "list_all_threads");

        let mut num_iter = 0;
        loop {
            num_iter += 1;
            trace!(iteration = num_iter, "Listing threads.");
            let opts = ListThreadsOptions {
                from: from.clone(),
                include_threads: IncludeThreads::All,
                ..Default::default()
            };
            let response =
                room.list_threads(opts).await.map_err(EventCacheThreadError::ListThreadError)?;
            roots.extend(response.chunk);

            if response.prev_batch_token.is_none() {
                break;
            }
            from = response.prev_batch_token;

            if let Some(max_iterations) = max_iterations {
                if num_iter >= max_iterations {
                    warn!("Thread listing took too long; aborting. This is a temporary behavior.");
                    break;
                }
            }
        }

        trace!("Listing threads completed in {num_iter} iterations");

        Ok(roots)
    }

    async fn backfill_thread(
        room: Room,
        root: OwnedEventId,
    ) -> Result<(OwnedEventId, Vec<Event>), EventCacheThreadError> {
        // Get all the events in the thread.
        // TODO: until the ones we know about.

        // TODO: proper pagination. In the meanwhile, this limits the maximum number of
        // iterations one is allowed to do.
        let mut i = 30;

        let mut from = None;
        let mut events = Vec::new();

        let mut opts = RelationsOptions::default();
        opts.include_relations = IncludeRelations::RelationsOfType(RelationType::Thread);

        // TODO: experiment with this: pretty sure recursing is the right choice, so as
        // to be able to order the thread related events against each other.
        opts.recurse = true;

        loop {
            opts.from = from;

            let response = room
                .relations(root.clone(), opts.clone())
                .await
                .map_err(EventCacheThreadError::BackfillThread)?;

            // Note: the results should be ordered in the reverse ordering.
            events.extend(response.chunk);

            if response.prev_batch_token.is_none() {
                break;
            }
            from = response.prev_batch_token;

            i -= 1;
            if i == 0 {
                warn!("Thread backfill took too long; aborting. This is a temporary behavior.");
                break;
            }
        }

        Ok((root, events))
    }
}
