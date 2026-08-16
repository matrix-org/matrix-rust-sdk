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

#![cfg(not(target_family = "wasm"))] // Uses the cross-process lock, so needs time support.

use std::sync::Arc;

use assert_matches::assert_matches;
use eyeball_im::VectorDiff;
use futures_util::FutureExt as _;
use matrix_sdk_base::{
    RoomState,
    event_cache::{
        Gap,
        store::{EventCacheStore as _, MemoryStore},
    },
    linked_chunk::{ChunkIdentifier, LinkedChunkId, Position, Update},
    store::StoreConfig,
};
use matrix_sdk_common::cross_process_lock::CrossProcessLockConfig;
use matrix_sdk_test::{
    ALICE, JoinedRoomBuilder, async_test,
    event_factory::{EventBuilder, EventFactory},
};
use ruma::{
    EventId, OwnedEventId, RoomId, event_id, events::room::message::RoomMessageEventContent,
    owned_mxc_uri, room_id,
};

use super::{MessageTypesCacheUpdate, MessageTypesEventCache};
use crate::{
    Client, assert_let_timeout,
    event_cache::{EventsOrigin, RoomEventCache},
    test_utils::mocks::{MatrixMockServer, RoomMessagesResponseTemplate},
};

const IMAGE: &str = "m.image";

fn ids(events: &[matrix_sdk_base::event_cache::Event]) -> Vec<OwnedEventId> {
    events.iter().map(|event| event.event_id().unwrap().to_owned()).collect()
}

fn image(f: &EventFactory, event_id: &EventId) -> matrix_sdk_base::event_cache::Event {
    f.image(format!("{event_id}.png"), owned_mxc_uri!("mxc://example.org/img"))
        .sender(*ALICE)
        .event_id(event_id)
        .into_event()
}

fn text(f: &EventFactory, event_id: &EventId) -> matrix_sdk_base::event_cache::Event {
    f.text_msg("hello").sender(*ALICE).event_id(event_id).into_event()
}

fn image_builder(f: &EventFactory, event_id: &EventId) -> EventBuilder<RoomMessageEventContent> {
    f.image(format!("{event_id}.png"), owned_mxc_uri!("mxc://example.org/img"))
        .sender(*ALICE)
        .event_id(event_id)
}

fn text_builder(f: &EventFactory, event_id: &EventId) -> EventBuilder<RoomMessageEventContent> {
    f.text_msg("hello").sender(*ALICE).event_id(event_id)
}

/// Prefill a memory store with, in timeline order:
///
/// `[$img1] [gap "g1"] [$txt1, $img2] [$txt2] [gap "g2"] [$img3, $txt3]`
///
/// with the chunk identifiers deliberately out of timeline order (5, 3, 4, 2,
/// 1, 0), so that a correct ordering can only come from the links.
async fn prefill_store(store: &MemoryStore, room_id: &RoomId, f: &EventFactory) {
    store
        .handle_linked_chunk_updates(
            LinkedChunkId::Room(room_id),
            vec![
                Update::NewItemsChunk { previous: None, new: ChunkIdentifier::new(5), next: None },
                Update::PushItems {
                    at: Position::new(ChunkIdentifier::new(5), 0),
                    items: vec![image(f, event_id!("$img1"))],
                },
                Update::NewGapChunk {
                    previous: Some(ChunkIdentifier::new(5)),
                    new: ChunkIdentifier::new(3),
                    next: None,
                    gap: Gap { token: "g1".to_owned() },
                },
                Update::NewItemsChunk {
                    previous: Some(ChunkIdentifier::new(3)),
                    new: ChunkIdentifier::new(4),
                    next: None,
                },
                Update::PushItems {
                    at: Position::new(ChunkIdentifier::new(4), 0),
                    items: vec![text(f, event_id!("$txt1")), image(f, event_id!("$img2"))],
                },
                Update::NewItemsChunk {
                    previous: Some(ChunkIdentifier::new(4)),
                    new: ChunkIdentifier::new(2),
                    next: None,
                },
                Update::PushItems {
                    at: Position::new(ChunkIdentifier::new(2), 0),
                    items: vec![text(f, event_id!("$txt2"))],
                },
                Update::NewGapChunk {
                    previous: Some(ChunkIdentifier::new(2)),
                    new: ChunkIdentifier::new(1),
                    next: None,
                    gap: Gap { token: "g2".to_owned() },
                },
                Update::NewItemsChunk {
                    previous: Some(ChunkIdentifier::new(1)),
                    new: ChunkIdentifier::new(0),
                    next: None,
                },
                Update::PushItems {
                    at: Position::new(ChunkIdentifier::new(0), 0),
                    items: vec![image(f, event_id!("$img3")), text(f, event_id!("$txt3"))],
                },
            ],
        )
        .await
        .unwrap();
}

async fn client_with_store(server: &MatrixMockServer, store: Arc<MemoryStore>) -> Client {
    server
        .client_builder()
        .on_builder(|builder| {
            builder.store_config(
                StoreConfig::new(CrossProcessLockConfig::multi_process("hodor"))
                    .event_cache_store(store),
            )
        })
        .build()
        .await
}

async fn room_caches(
    client: &Client,
    room_id: &RoomId,
) -> (RoomEventCache, MessageTypesEventCache) {
    let event_cache = client.event_cache();
    event_cache.subscribe().unwrap();

    client.base_client().get_or_create_room(room_id, RoomState::Joined);

    let (room_event_cache, _) = event_cache.room(room_id).await.unwrap();
    let (view, _) = event_cache.message_types(room_id, vec![IMAGE.to_owned()]).await.unwrap();

    (room_event_cache, view)
}

#[async_test]
async fn test_seed_orders_events_and_gaps_by_the_chunk_links() {
    let room_id = room_id!("!galette:saucisse.bzh");
    let f = EventFactory::new().room(room_id);

    let store = Arc::new(MemoryStore::new());
    prefill_store(&store, room_id, &f).await;

    let server = MatrixMockServer::new().await;
    let client = client_with_store(&server, store).await;
    let (_room_event_cache, view) = room_caches(&client, room_id).await;

    let (events, gaps, _) = view.subscribe().await;

    // All the images, in timeline order, without loading anything in memory
    // (the room event cache only ever loaded its last chunk).
    assert_eq!(ids(&events), [event_id!("$img1"), event_id!("$img2"), event_id!("$img3")]);

    // Both gaps, anchored to the first image after them.
    assert_eq!(gaps.len(), 2);
    assert_eq!(gaps[0].prev_token, "g1");
    assert_eq!(gaps[0].following_event_id.as_deref(), Some(event_id!("$img2")));
    assert_eq!(gaps[1].prev_token, "g2");
    assert_eq!(gaps[1].following_event_id.as_deref(), Some(event_id!("$img3")));

    // Everything is exposed already: nothing to paginate.
    assert!(view.paginate_backwards(10).await.unwrap());
}

#[async_test]
async fn test_paginate_backwards_exposes_older_pages() {
    let room_id = room_id!("!galette:saucisse.bzh");
    let f = EventFactory::new().room(room_id);

    // 60 images in one chunk, more than the initial page (50).
    let store = Arc::new(MemoryStore::new());
    let images = (0..60)
        .map(|i| image(&f, &EventId::parse(format!("$img{i}")).unwrap()))
        .collect::<Vec<_>>();
    store
        .handle_linked_chunk_updates(
            LinkedChunkId::Room(room_id),
            vec![
                Update::NewGapChunk {
                    previous: None,
                    new: ChunkIdentifier::new(0),
                    next: None,
                    gap: Gap { token: "leading".to_owned() },
                },
                Update::NewItemsChunk {
                    previous: Some(ChunkIdentifier::new(0)),
                    new: ChunkIdentifier::new(1),
                    next: None,
                },
                Update::PushItems {
                    at: Position::new(ChunkIdentifier::new(1), 0),
                    items: images.clone(),
                },
            ],
        )
        .await
        .unwrap();

    let server = MatrixMockServer::new().await;
    let client = client_with_store(&server, store).await;
    let (_room_event_cache, view) = room_caches(&client, room_id).await;

    let (events, gaps, mut updates) = view.subscribe().await;

    // The newest 50 are exposed; the leading gap isn't yet (it's older than
    // the held-back events).
    assert_eq!(events.len(), 50);
    assert_eq!(ids(&events), ids(&images[10..]));
    assert!(gaps.is_empty());

    // Expose 5 more: not at the start yet.
    assert!(!view.paginate_backwards(5).await.unwrap());

    assert_let_timeout!(
        Ok(MessageTypesCacheUpdate { diffs, origin: EventsOrigin::Cache, gaps }) = updates.recv()
    );
    assert_eq!(diffs.len(), 5);
    // Prepended newest-first: $img9 first, …, $img5 last, so that they end up
    // in order.
    for (diff, expected) in diffs.iter().zip(images[5..10].iter().rev()) {
        assert_matches!(diff, VectorDiff::Insert { index: 0, value } => {
            assert_eq!(value.event_id(), expected.event_id());
        });
    }
    assert!(gaps.is_empty());

    // Expose the rest: hits the start, and the leading gap comes along.
    assert!(view.paginate_backwards(100).await.unwrap());

    assert_let_timeout!(Ok(MessageTypesCacheUpdate { diffs, gaps, .. }) = updates.recv());
    assert_eq!(diffs.len(), 5);
    assert_eq!(gaps.len(), 1);
    assert_eq!(gaps[0].prev_token, "leading");
    assert_eq!(gaps[0].following_event_id.as_deref(), Some(event_id!("$img0")));

    let (events, _) = view.events_and_gaps().await;
    assert_eq!(ids(&events), ids(&images));

    // Nothing more to expose: no update.
    assert!(view.paginate_backwards(1).await.unwrap());
    assert!(updates.recv().now_or_never().is_none());
}

#[async_test]
async fn test_sync_appends_matching_events_and_trailing_gaps() {
    let room_id = room_id!("!galette:saucisse.bzh");
    let f = EventFactory::new().room(room_id);

    let store = Arc::new(MemoryStore::new());
    prefill_store(&store, room_id, &f).await;

    let server = MatrixMockServer::new().await;
    let client = client_with_store(&server, store).await;
    let (_room_event_cache, view) = room_caches(&client, room_id).await;

    let (_, _, mut updates) = view.subscribe().await;

    // A sync with a text and an image: only the image shows up, at the end.
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id)
                .add_timeline_event(text_builder(&f, event_id!("$txt4")))
                .add_timeline_event(image_builder(&f, event_id!("$img4"))),
        )
        .await;

    assert_let_timeout!(
        Ok(MessageTypesCacheUpdate { diffs, origin: EventsOrigin::Sync, gaps }) = updates.recv()
    );
    assert_eq!(diffs.len(), 1);
    assert_matches!(&diffs[0], VectorDiff::Insert { index: 3, value } => {
        assert_eq!(value.event_id().as_deref(), Some(event_id!("$img4")));
    });
    assert_eq!(gaps.len(), 2);

    // A limited sync with only text: no event diff, but a new gap at the
    // newest end, with nothing known after it, so it renders at the end.
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id)
                .set_timeline_limited()
                .set_timeline_prev_batch("g3")
                .add_timeline_event(text_builder(&f, event_id!("$txt5"))),
        )
        .await;

    assert_let_timeout!(Ok(MessageTypesCacheUpdate { diffs, gaps, .. }) = updates.recv());
    assert!(diffs.is_empty());
    assert_eq!(gaps.len(), 3);
    assert_eq!(gaps[2].prev_token, "g3");
    assert_eq!(gaps[2].following_event_id, None);
}

#[async_test]
async fn test_resolving_a_gap_inserts_the_matching_events_in_place() {
    let room_id = room_id!("!galette:saucisse.bzh");
    let f = EventFactory::new().room(room_id);

    let server = MatrixMockServer::new().await;

    let store = Arc::new(MemoryStore::new());
    prefill_store(&store, room_id, &f).await;

    let client = client_with_store(&server, store).await;
    let (room_event_cache, view) = room_caches(&client, room_id).await;

    let (events, gaps, mut updates) = view.subscribe().await;
    assert_eq!(events.len(), 3);
    assert_eq!(gaps.len(), 2);

    // Resolving "g1" (the older gap) yields an image and a text, and is
    // fully resolved (no new token).
    server
        .mock_room_messages()
        .match_from("g1")
        .ok(RoomMessagesResponseTemplate::default().events(vec![
            // Reverse topological order.
            image_builder(&f, event_id!("$img1b")).into_raw_timeline(),
            text_builder(&f, event_id!("$txt1a")).into_raw_timeline(),
        ]))
        .mock_once()
        .mount()
        .await;

    // The gap isn't in memory (only the last chunk is loaded); the view
    // loads the room's storage down to it first.
    assert!(view.resolve_gap("g1".to_owned(), 10).await.unwrap());

    // The storage loads reach the view too (as store-less no-ops: they
    // don't produce store updates), then the resolution: the image lands
    // between $img1 and $img2, and "g1" is gone.
    let mut saw_resolution = false;
    for _ in 0..8 {
        assert_let_timeout!(Ok(MessageTypesCacheUpdate { diffs, gaps, .. }) = updates.recv());
        if let Some(diff) = diffs.first() {
            assert_matches!(diff, VectorDiff::Insert { index: 1, value } => {
                assert_eq!(value.event_id().as_deref(), Some(event_id!("$img1b")));
            });
            assert_eq!(gaps.len(), 1);
            assert_eq!(gaps[0].prev_token, "g2");
            saw_resolution = true;
            break;
        }
    }
    assert!(saw_resolution, "the gap resolution never reached the view");

    let (events, gaps) = view.events_and_gaps().await;
    assert_eq!(
        ids(&events),
        [event_id!("$img1"), event_id!("$img1b"), event_id!("$img2"), event_id!("$img3")]
    );
    assert_eq!(gaps.len(), 1);
    assert_eq!(gaps[0].prev_token, "g2");
    assert_eq!(gaps[0].following_event_id.as_deref(), Some(event_id!("$img3")));

    // The room event cache agrees, from its own (now fully loaded) linked
    // chunk.
    let room_gaps = room_event_cache.timeline_gaps().await.unwrap();
    assert_eq!(room_gaps.len(), 1);
    assert_eq!(room_gaps[0].prev_token, "g2");
}

#[async_test]
async fn test_redaction_removes_the_event() {
    let room_id = room_id!("!galette:saucisse.bzh");
    let f = EventFactory::new().room(room_id);

    let store = Arc::new(MemoryStore::new());
    prefill_store(&store, room_id, &f).await;

    let server = MatrixMockServer::new().await;
    let client = client_with_store(&server, store).await;
    let (_room_event_cache, view) = room_caches(&client, room_id).await;

    let (events, _, mut updates) = view.subscribe().await;
    assert_eq!(events.len(), 3);

    // Redact $img3 (in the last chunk, which is loaded in memory, so the
    // redaction applies to it).
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id).add_timeline_event(
                f.redaction(event_id!("$img3")).sender(*ALICE).event_id(event_id!("$redaction")),
            ),
        )
        .await;

    // The redaction event itself is pushed first (no matching event: an
    // update with no diff), then the target is replaced by its redacted form.
    let mut saw_removal = false;
    for _ in 0..4 {
        assert_let_timeout!(Ok(MessageTypesCacheUpdate { diffs, .. }) = updates.recv());
        if !diffs.is_empty() {
            assert_matches!(diffs.as_slice(), [VectorDiff::Remove { index: 2 }]);
            saw_removal = true;
            break;
        }
    }
    assert!(saw_removal, "the redaction never reached the view");

    let (events, _) = view.events_and_gaps().await;
    assert_eq!(ids(&events), [event_id!("$img1"), event_id!("$img2")]);
}

#[async_test]
async fn test_clearing_the_cache_empties_the_view() {
    let room_id = room_id!("!galette:saucisse.bzh");
    let f = EventFactory::new().room(room_id);

    let store = Arc::new(MemoryStore::new());
    prefill_store(&store, room_id, &f).await;

    let server = MatrixMockServer::new().await;
    let client = client_with_store(&server, store).await;
    let (_room_event_cache, view) = room_caches(&client, room_id).await;

    let (events, _, mut updates) = view.subscribe().await;
    assert_eq!(events.len(), 3);

    client.event_cache().clear_all_rooms().await.unwrap();

    assert_let_timeout!(Ok(MessageTypesCacheUpdate { diffs, gaps, .. }) = updates.recv());
    assert_matches!(diffs.as_slice(), [VectorDiff::Clear]);
    assert!(gaps.is_empty());

    let (events, gaps) = view.events_and_gaps().await;
    assert!(events.is_empty());
    assert!(gaps.is_empty());
    assert!(view.paginate_backwards(10).await.unwrap());
}
