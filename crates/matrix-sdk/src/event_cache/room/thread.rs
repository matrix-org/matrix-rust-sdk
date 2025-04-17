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
    api::client::threads::{self, get_threads::v1::IncludeThreads},
    events::{
        relation::RelationType::{self, Thread},
        AnyMessageLikeEvent, AnySyncTimelineEvent, AnyTimelineEvent,
    },
    serde::Raw,
    OwnedEventId,
};
use tokio::sync::broadcast::{self, Sender};
use tracing::warn;

use crate::{room::WeakRoom, Room};

#[derive(Debug, thiserror::Error)]
pub enum EventCacheThreadError {
    #[error("Couldn't list all threads for room: {0}")]
    ListThreadError(crate::Error),

    #[error("Couldn't backfill a thread: {0}")]
    BackfillThread(crate::Error),
}

#[derive(Clone)]
pub struct ThreadSummary {
    /// Number of meaningful events in the thread.
    pub count: usize,

    /// The latest event in the thread.
    pub latest_event: OwnedEventId,

    /// Is this thread summary potentially outdated?
    pub maybe_outdated: bool,
}

#[derive(Clone, Debug, Default)]
struct ThreadGap {
    prev_batch: Option<String>,
    next_batch: Option<String>,
}

type ThreadLinkedChunk = LinkedChunk<32, Event, ThreadGap>;

pub struct ThreadEventCache {
    events: ThreadLinkedChunk,
    summary: ThreadSummary,
}

type ThreadRoot = OwnedEventId;

pub struct ThreadyRoomer {
    room: WeakRoom,

    threads: BTreeMap<ThreadRoot, ThreadEventCache>,

    summary_sender: Sender<(OwnedEventId, ThreadSummary)>,
}

impl ThreadyRoomer {
    pub fn new(room: WeakRoom) -> Self {
        Self { room, threads: BTreeMap::new(), summary_sender: broadcast::channel(16).0 }
    }

    pub async fn init_if_empty(&mut self) -> Result<(), EventCacheThreadError> {
        if !self.threads.is_empty() {
            return Ok(());
        }
        self.update_all_threads().await
    }

    pub async fn update_all_threads(&mut self) -> Result<(), EventCacheThreadError> {
        // Find all the threasd in this room.
        let Some(room) = self.room.get() else {
            // Client is shutting down; abort.
            return Ok(());
        };

        let mut threads_to_refresh = Vec::new();
        let roots = self.list_all_threads(&room).await?;

        for root in roots {
            let Some(root_id) = root.event_id() else {
                warn!("Skipping thread root without event id");
                continue;
            };

            // TODO: do something with the latest event
            let new_summary = Self::extract_summary_from_bundled(root.raw());

            // If we knew about the thread,
            if let Some(thread) = self.threads.get_mut(&root_id) {
                // And the latest event matches the one we know about,
                if Some(&thread.summary.latest_event)
                    == new_summary.as_ref().map(|summary| &summary.0.latest_event)
                {
                    // We don't have anything to do.
                } else {
                    // Otherwise, the thread may be outdated; let observers know.
                    thread.summary.maybe_outdated = true;
                    let _ = self.summary_sender.send((root_id.to_owned(), thread.summary.clone()));
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
                    Self::extract_summary_from_bundled(events.first().unwrap().raw()).unwrap();

                ThreadEventCache { events: ThreadLinkedChunk::new(), summary }
            });

            thread_cache.summary.maybe_outdated = false;
            thread_cache.events.clear();
            thread_cache.events.push_items_back(events);
            // TODO propagate updates
        }

        Ok(())
    }

    async fn list_all_threads(&mut self, room: &Room) -> Result<Vec<Event>, EventCacheThreadError> {
        let mut from = None;
        let mut roots = Vec::new();

        // TODO: stop as soon as we know all the threads from the answer.

        // TODO: proper pagination.
        let mut i = 30;

        loop {
            let (roots_batch, next_batch) = room
                .list_threads(IncludeThreads::All, from)
                .await
                .map_err(EventCacheThreadError::ListThreadError)?;
            roots.extend(roots_batch);

            if next_batch.is_none() {
                break;
            }
            from = next_batch;

            i -= 1;
            if i == 0 {
                warn!("Thread listing took too long; aborting. This is a temporary behavior.");
                break;
            }
        }

        Ok(roots)
    }

    fn extract_summary_from_bundled(
        root: &Raw<AnySyncTimelineEvent>,
    ) -> Option<(ThreadSummary, Raw<AnyMessageLikeEvent>)> {
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

        Some((summary, bundled_thread.latest_event))
    }

    async fn backfill_thread(
        room: Room,
        root: OwnedEventId,
    ) -> Result<(OwnedEventId, Vec<Event>), EventCacheThreadError> {
        // Get all the events in the thread.
        // TODO: until the ones we know about.

        // TODO: proper pagination.
        let mut i = 30;

        let mut from = None;
        let mut events = Vec::new();

        loop {
            let (events_batch, next_batch) = room
                .relations_with_rel_type(root.clone(), RelationType::Thread, from)
                .await
                .map_err(EventCacheThreadError::BackfillThread)?;

            events.extend(events_batch);

            if next_batch.is_none() {
                break;
            }
            from = next_batch;

            i -= 1;
            if i == 0 {
                warn!("Thread backfill took too long; aborting. This is a temporary behavior.");
                break;
            }
        }

        Ok((root, events))
    }
}
