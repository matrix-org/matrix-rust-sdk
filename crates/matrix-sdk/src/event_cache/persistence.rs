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

use std::collections::{HashMap, HashSet};

use matrix_sdk_base::{
    deserialized_responses::TimelineEventKind,
    event_cache::{Event, Gap, store::EventCacheStoreLockGuard},
    executor::spawn,
    linked_chunk::{ChunkMetadata, LinkedChunkId, OwnedLinkedChunkId, Update},
};
use ruma::serde::Raw;
use tokio::sync::broadcast::Sender;
use tracing::trace;

use super::{EventCacheError, Result, caches::room::RoomEventCacheLinkedChunkUpdate};

/// Load a linked chunk's full metadata, making sure the chunks are
/// according to their their links.
///
/// Returns `None` if there's no such linked chunk in the store, or an
/// error if the linked chunk is malformed.
pub(super) async fn load_linked_chunk_metadata(
    store_guard: &EventCacheStoreLockGuard,
    linked_chunk_id: LinkedChunkId<'_>,
) -> Result<Option<Vec<ChunkMetadata>>> {
    let mut all_chunks = store_guard
        .load_all_chunks_metadata(linked_chunk_id)
        .await
        .map_err(EventCacheError::from)?;

    if all_chunks.is_empty() {
        // There are no chunks, so there's nothing to do.
        return Ok(None);
    }

    // Transform the vector into a hashmap, for quick lookup of the predecessors.
    let chunk_map: HashMap<_, _> = all_chunks.iter().map(|meta| (meta.identifier, meta)).collect();

    // Find a last chunk.
    let mut iter = all_chunks.iter().filter(|meta| meta.next.is_none());
    let Some(last) = iter.next() else {
        return Err(EventCacheError::InvalidLinkedChunkMetadata {
            details: "no last chunk found".to_owned(),
        });
    };

    // There must at most one last chunk.
    if let Some(other_last) = iter.next() {
        return Err(EventCacheError::InvalidLinkedChunkMetadata {
            details: format!(
                "chunks {} and {} both claim to be last chunks",
                last.identifier.index(),
                other_last.identifier.index()
            ),
        });
    }

    // Rewind the chain back to the first chunk, and do some checks at the same
    // time.
    let mut seen = HashSet::new();
    let mut current = last;
    loop {
        // If we've already seen this chunk, there's a cycle somewhere.
        if !seen.insert(current.identifier) {
            return Err(EventCacheError::InvalidLinkedChunkMetadata {
                details: format!(
                    "cycle detected in linked chunk at {}",
                    current.identifier.index()
                ),
            });
        }

        let Some(prev_id) = current.previous else {
            // If there's no previous chunk, we're done.
            if seen.len() != all_chunks.len() {
                return Err(EventCacheError::InvalidLinkedChunkMetadata {
                    details: format!(
                        "linked chunk likely has multiple components: {} chunks seen through the chain of predecessors, but {} expected",
                        seen.len(),
                        all_chunks.len()
                    ),
                });
            }
            break;
        };

        // If the previous chunk is not in the map, then it's unknown
        // and missing.
        let Some(pred_meta) = chunk_map.get(&prev_id) else {
            return Err(EventCacheError::InvalidLinkedChunkMetadata {
                details: format!(
                    "missing predecessor {} chunk for {}",
                    prev_id.index(),
                    current.identifier.index()
                ),
            });
        };

        // If the previous chunk isn't connected to the next, then the link is invalid.
        if pred_meta.next != Some(current.identifier) {
            return Err(EventCacheError::InvalidLinkedChunkMetadata {
                details: format!(
                    "chunk {}'s next ({:?}) doesn't match the current chunk ({})",
                    pred_meta.identifier.index(),
                    pred_meta.next.map(|chunk_id| chunk_id.index()),
                    current.identifier.index()
                ),
            });
        }

        current = *pred_meta;
    }

    // At this point, `current` is the identifier of the first chunk.
    //
    // Reorder the resulting vector, by going through the chain of `next` links, and
    // swapping items into their final position.
    //
    // Invariant in this loop: all items in [0..i[ are in their final, correct
    // position.
    let mut current = current.identifier;

    for i in 0..all_chunks.len() {
        // Find the target metadata.
        let j = all_chunks
            .iter()
            .rev()
            .position(|meta| meta.identifier == current)
            .map(|j| all_chunks.len() - 1 - j)
            .expect("the target chunk must be present in the metadata");

        if i != j {
            all_chunks.swap(i, j);
        }

        if let Some(next) = all_chunks[i].next {
            current = next;
        }
    }

    Ok(Some(all_chunks))
}

/// Propagate linked chunk updates to the store and to the linked chunk update
/// observers.
pub(super) async fn send_updates_to_store(
    store: &EventCacheStoreLockGuard,
    linked_chunk_id: OwnedLinkedChunkId,
    linked_chunk_update_sender: &Sender<RoomEventCacheLinkedChunkUpdate>,
    mut updates: Vec<Update<Event, Gap>>,
) -> Result<()> {
    if updates.is_empty() {
        return Ok(());
    }

    // Strip relations from updates which insert or replace items.
    //
    // The reason we're doing this, is that consumers of the event cache might look
    // into bundled relations, and assume they're up to date. If we were to keep
    // the relations in the events, when storing them, then it could be that
    // they become outdated (as soon as a new relation comes over sync), so we'd
    // need to update the bundled relations in this case, which would
    // have a non-negligible cost, as we'd need to look up related events for each
    // forwarded to a listener.
    //
    // As a result, we choose to strip bundled relations from events when we forward
    // them to the store, and consumers have to explicitly ask for relations.
    for update in updates.iter_mut() {
        match update {
            Update::PushItems { items, .. } => strip_relations_from_events(items),
            Update::ReplaceItem { item, .. } => strip_relations_from_event(item),
            // Other update kinds don't involve adding new events.
            Update::NewItemsChunk { .. }
            | Update::NewGapChunk { .. }
            | Update::RemoveChunk(_)
            | Update::RemoveItem { .. }
            | Update::DetachLastItems { .. }
            | Update::StartReattachItems
            | Update::EndReattachItems
            | Update::Clear => {}
        }
    }

    // Spawn a task to make sure that all the changes are effectively forwarded to
    // the store, even if the call to this method gets aborted.
    //
    // The store cross-process locking involves an actual mutex, which ensures that
    // storing updates happens in the expected order.

    let store = store.clone();
    let cloned_updates = updates.clone();
    let cloned_linked_chunk_id = linked_chunk_id.clone();

    spawn(async move {
        trace!(updates = ?cloned_updates, "sending linked chunk updates to the store");

        store.handle_linked_chunk_updates(cloned_linked_chunk_id.as_ref(), cloned_updates).await?;
        trace!("linked chunk updates applied");

        Result::Ok(())
    })
    .await
    .expect("joining failed")?;

    // Forward that the store got updated to observers.
    let _ = linked_chunk_update_sender
        .send(RoomEventCacheLinkedChunkUpdate { linked_chunk_id, updates });

    Ok(())
}

/// Strips the bundled relations from a collection of events.
fn strip_relations_from_events(items: &mut [Event]) {
    for ev in items.iter_mut() {
        strip_relations_from_event(ev);
    }
}

/// Strips the bundled relations from an event, if they were present.
fn strip_relations_from_event(ev: &mut Event) {
    match &mut ev.kind {
        TimelineEventKind::Decrypted(decrypted) => {
            // Remove all information about encryption info for
            // the bundled events.
            decrypted.unsigned_encryption_info = None;

            // Remove the `unsigned`/`m.relations` field, if needs be.
            strip_relations_if_present(&mut decrypted.event);
        }

        TimelineEventKind::UnableToDecrypt { event, .. }
        | TimelineEventKind::PlainText { event } => {
            strip_relations_if_present(event);
        }
    }
}

/// Removes the bundled relations from an event, if they were present.
///
/// Only replaces the present if it contained bundled relations.
fn strip_relations_if_present<T>(event: &mut Raw<T>) {
    // We're going to get rid of the `unsigned`/`m.relations` field, if it's
    // present.
    // Use a closure that returns an option so we can quickly short-circuit.
    let mut closure = || -> Option<()> {
        let mut val: serde_json::Value = event.deserialize_as().ok()?;
        let unsigned = val.get_mut("unsigned")?;
        let unsigned_obj = unsigned.as_object_mut()?;
        if unsigned_obj.remove("m.relations").is_some() {
            *event = Raw::new(&val).ok()?.cast_unchecked();
        }
        None
    };
    let _ = closure();
}
