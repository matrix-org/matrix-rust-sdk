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

use matrix_sdk_base::{
    deserialized_responses::TimelineEventKind,
    event_cache::{Event, Gap, store::EventCacheStoreLockGuard},
    executor::spawn,
    linked_chunk::{OwnedLinkedChunkId, Update},
};
use ruma::serde::Raw;
use tokio::sync::broadcast::Sender;
use tracing::trace;

use crate::event_cache::{Result, RoomEventCacheLinkedChunkUpdate};

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
