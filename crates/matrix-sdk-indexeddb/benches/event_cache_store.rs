#![cfg(target_family = "wasm")]

use std::{
    hint::black_box,
    sync::{Arc, LazyLock},
    time::Duration,
};

use matrix_sdk_base::{
    event_cache::{
        Event, Gap,
        store::{
            EventCacheStore, EventCacheStoreError, integration_tests::make_test_event_with_event_id,
        },
    },
    linked_chunk::{ChunkIdentifier, LinkedChunkId, Position, Update},
};
use matrix_sdk_indexeddb::IndexeddbEventCacheStore;
use matrix_sdk_store_encryption::StoreCipher;
use ruma::{EventId, RoomId, event_id, room_id};
use uuid::Uuid;
use wasm_bindgen_test::{Criterion, Instant, wasm_bindgen_bench, wasm_bindgen_test_configure};

wasm_bindgen_test_configure!(run_in_browser);

/// Opens a connection to a new IndexedDB database, where the name of the
/// database is randomly generated.
async fn get_event_cache_store() -> Result<IndexeddbEventCacheStore, EventCacheStoreError> {
    let name = format!("bench-event-cache-store-{}", Uuid::new_v4().as_hyphenated());
    Ok(IndexeddbEventCacheStore::builder()
        .database_name(name)
        .store_cipher(Arc::new(StoreCipher::new().expect("store cipher")))
        .build()
        .await?)
}

/// The default [`ChunkIdentifier`] used to apply [`Update::NewItemsChunk`] in
/// [`LinkedChunkUpdates::init`].
pub static DEFAULT_INITIAL_CHUNK_IDENTIFIER: LazyLock<ChunkIdentifier> =
    LazyLock::new(|| ChunkIdentifier::new(0));

/// The default [`Position`] used to apply [`Update::PushItems`] in
/// [`LinkedChunkUpdates::init_and_push_events`]. Constructed with
/// [`DEFAULT_INITIAL_CHUNK_IDENTIFIER`].
pub static DEFAULT_INITIAL_POSITION: LazyLock<Position> =
    LazyLock::new(|| Position::new(*DEFAULT_INITIAL_CHUNK_IDENTIFIER, 0));

/// The default [`RoomId`] used in benchmarks.
pub static DEFAULT_ROOM_ID: LazyLock<&RoomId> = LazyLock::new(|| room_id!("!room"));

/// The default content of an [`Event`] used in benchmarks.
pub static DEFAULT_EVENT_CONTENT: LazyLock<String> = LazyLock::new(|| "a".repeat(1000));

/// The default [`EventId`] used in benchmarks.
pub static DEFAULT_EVENT_ID: LazyLock<&EventId> = LazyLock::new(|| event_id!("$event"));

/// The default [`Event`] used in benchmarks. Constructed with
/// [`DEFAULT_ROOM_ID`], [`DEFAULT_EVENT_CONTENT`], and [`DEFAULT_EVENT_ID`].
pub static DEFAULT_EVENT: LazyLock<Event> = LazyLock::new(|| {
    make_test_event_with_event_id(&DEFAULT_ROOM_ID, &DEFAULT_EVENT_CONTENT, Some(*DEFAULT_EVENT_ID))
});

/// The default [`LinkedChunkId::Room`] used in benchmarks. Constructed with
/// [`DEFAULT_ROOM_ID`].
pub static DEFAULT_LINKED_CHUNK_ID_ROOM: LazyLock<LinkedChunkId<'_>> =
    LazyLock::new(|| LinkedChunkId::Room(&DEFAULT_ROOM_ID));

/// The default [`EventId`] used as a thread root in
/// [`DEFAULT_LINKED_CHUNK_ID_THREAD`].
pub static DEFAULT_LINKED_CHUNK_ID_THREAD_EVENT_ID: LazyLock<&EventId> =
    LazyLock::new(|| event_id!("$thread"));

/// The default [`LinkedChunkId::Thread`] used in benchmarks. Constructed with
/// [`DEFAULT_ROOM_ID`] and [`DEFAULT_LINKED_CHUNK_ID_THREAD_EVENT_ID`].
pub static DEFAULT_LINKED_CHUNK_ID_THREAD: LazyLock<LinkedChunkId<'_>> = LazyLock::new(|| {
    LinkedChunkId::Thread(&DEFAULT_ROOM_ID, &DEFAULT_LINKED_CHUNK_ID_THREAD_EVENT_ID)
});

/// The default [`LinkedChunkId::PinnedEvents`] used in benchmarks. Constructed
/// with [`DEFAULT_ROOM_ID`].
pub static DEFAULT_LINKED_CHUNK_ID_PINNED_EVENTS: LazyLock<LinkedChunkId<'_>> =
    LazyLock::new(|| LinkedChunkId::PinnedEvents(&DEFAULT_ROOM_ID));

/// The default [`EventId`] used as the focus event in
/// [`DEFAULT_LINKED_CHUNK_ID_EVENT_FOCUSED`].
pub static DEFAULT_LINKED_CHUNK_ID_EVENT_FOCUSED_EVENT_ID: LazyLock<&EventId> =
    LazyLock::new(|| event_id!("$focus"));

/// The default [`LinkedChunkId::EventFocused`] used in benchmarks. Constructed
/// with [`DEFAULT_ROOM_ID`] and
/// [`DEFAULT_LINKED_CHUNK_ID_EVENT_FOCUSED_EVENT_ID`].
pub static DEFAULT_LINKED_CHUNK_ID_EVENT_FOCUSED: LazyLock<LinkedChunkId<'_>> =
    LazyLock::new(|| {
        LinkedChunkId::EventFocused(
            &DEFAULT_ROOM_ID,
            &DEFAULT_LINKED_CHUNK_ID_EVENT_FOCUSED_EVENT_ID,
        )
    });

/// The default set of [`LinkedChunkId`]s used in benchmarks. Constructed from
/// [`DEFAULT_LINKED_CHUNK_ID_ROOM`], [`DEFAULT_LINKED_CHUNK_ID_THREAD`],
/// [`DEFAULT_LINKED_CHUNK_ID_PINNED_EVENT`], and
/// [`DEFAULT_LINKED_CHUNK_ID_EVENT_FOCUSED`].
pub static DEFAULT_LINKED_CHUNK_IDS: LazyLock<Vec<LinkedChunkId<'_>>> = LazyLock::new(|| {
    vec![
        *DEFAULT_LINKED_CHUNK_ID_ROOM,
        *DEFAULT_LINKED_CHUNK_ID_THREAD,
        *DEFAULT_LINKED_CHUNK_ID_PINNED_EVENTS,
        *DEFAULT_LINKED_CHUNK_ID_EVENT_FOCUSED,
    ]
});

/// A structure encapsulating a set of [`Update`]s which should be applied to a
/// given [`LinkedChunkId`].
#[derive(Debug, Clone)]
pub struct LinkedChunkUpdates<'a> {
    /// The [`LinkedChunkId`] to which the [`Self::updates`] should be applied.
    pub linked_chunk_id: LinkedChunkId<'a>,
    /// The [`Update`]s which should be applied to [`Self::linked_chunk_id`].
    pub updates: Vec<Update<Event, Gap>>,
}

impl<'a> LinkedChunkUpdates<'a> {
    /// Creates a [`Vec`] of [`LinkedChunkUpdates`] where each
    /// [`LinkedChunkUpdates`] corresponds to one of the provided
    /// [`LinkedChunkId`]s and contains an [`Update::NewItemsChunk`].
    ///
    /// This is useful for creating a
    /// [`Chunk`](matrix_sdk_common::linked_chunk::Chunk`) in a set of
    /// [`LinkedChunk`](matrix_sdk_common::linked_chunk::LinkedChunk)s.
    pub fn new_items_chunk(
        linked_chunk_ids: &[LinkedChunkId<'a>],
        previous: Option<ChunkIdentifier>,
        new: ChunkIdentifier,
        next: Option<ChunkIdentifier>,
    ) -> Vec<Self> {
        linked_chunk_ids
            .iter()
            .cloned()
            .map(|linked_chunk_id| Self {
                linked_chunk_id,
                updates: vec![Update::NewItemsChunk { previous, new, next }],
            })
            .collect()
    }

    /// Creates a [`Vec`] of [`LinkedChunkUpdates`] where each
    /// [`LinkedChunkUpdates`] corresponds to one of the provided
    /// [`LinkedChunkId`]s and contains an [`Update::PushItems`].
    ///
    /// This is useful for pushing [`Event`]s to a set of
    /// [`LinkedChunk`](matrix_sdk_common::linked_chunk::LinkedChunk)s.
    pub fn push_events(
        linked_chunk_ids: &[LinkedChunkId<'a>],
        at: Position,
        events: &[Event],
    ) -> Vec<Self> {
        linked_chunk_ids
            .iter()
            .cloned()
            .map(|linked_chunk_id| Self {
                linked_chunk_id,
                updates: vec![Update::PushItems { at, items: events.into() }],
            })
            .collect()
    }

    /// Creates a [`Vec`] of [`LinkedChunkUpdates`] where each
    /// [`LinkedChunkUpdates`] corresponds to one of the provided
    /// [`LinkedChunkId`]s and contains an [`Update::ReplaceItem`].
    ///
    /// This is useful for replacing an [`Event`] in a set of
    /// [`LinkedChunk`](matrix_sdk_common::linked_chunk::LinkedChunk)s.
    pub fn replace_event(
        linked_chunk_ids: &[LinkedChunkId<'a>],
        at: Position,
        event: &Event,
    ) -> Vec<Self> {
        linked_chunk_ids
            .iter()
            .cloned()
            .map(|linked_chunk_id| Self {
                linked_chunk_id,
                updates: vec![Update::ReplaceItem { at, item: event.clone() }],
            })
            .collect()
    }

    /// Creates a [`Vec`] of [`LinkedChunkUpdates`] where each
    /// [`LinkedChunkUpdates`] corresponds to one of the provided
    /// [`LinkedChunkId`]s and contains an [`Update::NewItemsChunk`] constructed
    /// with [`DEFAULT_INITIAL_CHUNK_IDENTIFIER`].
    ///
    /// This is useful for initializing a set of
    /// [`LinkedChunk`](matrix_sdk_common::linked_chunk::LinkedChunk)s with an
    /// initial [`Chunk`](matrix_sdk_common::linked_chunk::Chunk).
    pub fn init(linked_chunk_ids: &[LinkedChunkId<'a>]) -> Vec<Self> {
        Self::new_items_chunk(linked_chunk_ids, None, *DEFAULT_INITIAL_CHUNK_IDENTIFIER, None)
    }

    /// Creates a [`Vec`] of [`LinkedChunkUpdates`] where each
    /// [`LinkedChunkUpdates`] corresponds to one of the provided
    /// [`LinkedChunkId`]s and contains an [`Update::NewItemsChunk`] constructed
    /// with [`DEFAULT_INITIAL_CHUNK_IDENTIFIER`] and an [`Update::PushItems`]
    /// constructed with [`DEFAULT_INITIAL_POSITION`].
    ///
    /// This is useful for initializing a set of
    /// [`LinkedChunk`](matrix_sdk_common::linked_chunk::LinkedChunk)s with an
    /// initial [`Chunk`](matrix_sdk_common::linked_chunk::Chunk) and a set of
    /// [`Event`]s.
    pub fn init_and_push_events(
        linked_chunk_ids: &[LinkedChunkId<'a>],
        events: &[Event],
    ) -> Vec<Self> {
        let mut updates = Self::init(linked_chunk_ids);
        updates.append(&mut Self::push_events(linked_chunk_ids, *DEFAULT_INITIAL_POSITION, events));
        updates
    }

    /// Creates a [`Vec`] of [`LinkedChunkUpdates`] where each
    /// [`LinkedChunkUpdates`] corresponds to one of the provided
    /// [`LinkedChunkId`]s and contains an [`Update::Clear`].
    ///
    /// This is useful for clearing the contents of a set of
    /// [`LinkedChunk`](matrix_sdk_common::linked_chunk::LinkedChunk)s.
    pub fn clear(linked_chunk_ids: &[LinkedChunkId<'a>]) -> Vec<Self> {
        linked_chunk_ids
            .iter()
            .cloned()
            .map(|linked_chunk_id| Self { linked_chunk_id, updates: vec![Update::Clear] })
            .collect()
    }
}

/// A structure representing the benchmarking lifecycle when calling
/// [`EventCacheStore::handle_linked_chunk_updates`]. It contains a set of
/// [`LinkedChunkUpdates`] which are to be applied before timing starts, during
/// the timing process, and after timing is over.
#[derive(Debug, Clone)]
pub struct IterLinkedChunkUpdates<'a> {
    /// The [`LinkedChunkUpdates`] to apply before timing begins - i.e., the
    /// [`Update`]s which are used to initialize the database before
    /// benchmarking.
    pub before: Vec<LinkedChunkUpdates<'a>>,
    /// The [`LinkedChunkUpdates`] to apply during timing - i.e., the
    /// [`Update`]s which are being benchmarked.
    pub during: Vec<LinkedChunkUpdates<'a>>,
    /// The [`LinkedChunkUpdates`] to apply after timing ends - i.e., the
    /// [`Update`]s which are used to clean up the database after benchmarking.
    pub after: Vec<LinkedChunkUpdates<'a>>,
}

/// A function which can be used in the context of
/// [`Bencher::iter_custom_future`][1] to set up a database, time a set of
/// [`Update`]s, and then clean up the database using the provided [`Update`]s
/// in [`IterLinkedChunkUpdates::before`], [`IterLinkedChunkUpdates::during`],
/// and [`IterLinkedChunkUpdates::after`], respectively.
///
/// Note that `iters` represents the number of times the entire cycle is
/// executed and that the final time returned reflects the sum of all executions
/// of the [`Update`]s defined in [`IterLinkedChunkUpdates::during`].
/// Furthermore, timing is based on wall time elapsed while
/// [`IterLinkedChunkUpdates::during`] is executing.
///
/// [1]: wasm_bindgen_test::__rt::criterion::Bencher::iter_custom_future
pub async fn iter_linked_chunk_updates(
    iters: u64,
    updates: IterLinkedChunkUpdates<'_>,
) -> Result<Duration, EventCacheStoreError> {
    let event_cache_store = get_event_cache_store().await.unwrap();

    let mut duration = Duration::from_secs(0);
    for _ in 0..iters {
        for update in updates.before.clone() {
            event_cache_store
                .handle_linked_chunk_updates(update.linked_chunk_id, update.updates)
                .await?;
        }

        for update in updates.during.clone() {
            let start = Instant::now();
            black_box(
                event_cache_store
                    .handle_linked_chunk_updates(update.linked_chunk_id, update.updates)
                    .await?,
            );
            duration += start.elapsed();
        }

        for update in updates.after.clone() {
            event_cache_store
                .handle_linked_chunk_updates(update.linked_chunk_id, update.updates)
                .await?;
        }
    }
    Ok(duration)
}

/// Function for benchmarking [`Update::PushItems`].
///
/// This function initializes the database with an empty
/// [`Update::NewItemsChunk`], records the wall time that elapses when executing
/// [`Update::PushItems`] with a single [`Event`], and then clears the
/// database with [`Update::Clear`].
///
/// Furthermore, it runs this operation in multiple scenarios. In each scenario
/// a copy of the [`Event`] already exists in some number of other
/// [`LinkedChunk`][1]s in the [`Room`][2]. This helps to gauge the performance
/// penalty of a de-normalized database where the same [`Event`] is duplicated
/// for each [`LinkedChunk`][1] in which it exists and must, therefore, be
/// updated in multiple places.
///
/// [1]: matrix_sdk_common::linked_chunk::LinkedChunk
/// [2]: matrix_sdk_base::Room
#[wasm_bindgen_bench]
async fn linked_chunk_updates_push_items(c: &mut Criterion) {
    *c = std::mem::take(c).measurement_time(Duration::from_secs(15)).sample_size(500);
    for index in 1..=DEFAULT_LINKED_CHUNK_IDS.len() {
        let description =
            format!("Update::PushItems [event exists in {} linked chunks]", index - 1);
        c.bench_async_function(&description, move |b| {
            Box::pin(b.iter_custom_future(move |iters| async move {
                let linked_chunk_id = DEFAULT_LINKED_CHUNK_IDS[0];
                let events = &[DEFAULT_EVENT.clone()];

                iter_linked_chunk_updates(
                    iters,
                    IterLinkedChunkUpdates {
                        before: vec![
                            LinkedChunkUpdates::init(&[linked_chunk_id]),
                            LinkedChunkUpdates::init_and_push_events(
                                &DEFAULT_LINKED_CHUNK_IDS[1..index],
                                events,
                            ),
                        ]
                        .concat(),
                        during: LinkedChunkUpdates::push_events(
                            &[linked_chunk_id],
                            *DEFAULT_INITIAL_POSITION,
                            events,
                        ),
                        after: LinkedChunkUpdates::clear(&DEFAULT_LINKED_CHUNK_IDS),
                    },
                )
                .await
                .unwrap()
            }))
        })
        .await;
    }
}

/// Function for benchmarking [`Update::ReplaceItem`].
///
/// This function initializes the database with an [`Update::NewItemsChunk`] and
/// [`Update::PushItems`] with a single [`Event`], records the wall time that
/// elapses when executing [`Update::ReplaceItem`] at the position of the
/// previously added [`Event`], and then clears the database with
/// [`Update::Clear`].
///
/// Furthermore, it runs this operation in multiple scenarios. In each scenario
/// a copy of the [`Event`] is added to some number of other [`LinkedChunk`][1]s
/// in the [`Room`][2]. This helps to gauge the performance penalty of a
/// de-normalized database where the same [`Event`] is duplicated
/// for each [`LinkedChunk`][1] in which it exists and must, therefore, be
/// updated in multiple places.
///
/// [1]: matrix_sdk_common::linked_chunk::LinkedChunk
/// [2]: matrix_sdk_base::Room
#[wasm_bindgen_bench]
async fn linked_chunk_updates_replace_item(c: &mut Criterion) {
    *c = std::mem::take(c).measurement_time(Duration::from_secs(15)).sample_size(500);
    for index in 1..=DEFAULT_LINKED_CHUNK_IDS.len() {
        let description = format!("Update::ReplaceItem [event exists in {} linked chunks]", index);
        c.bench_async_function(&description, move |b| {
            Box::pin(b.iter_custom_future(move |iters| async move {
                iter_linked_chunk_updates(
                    iters,
                    IterLinkedChunkUpdates {
                        before: LinkedChunkUpdates::init_and_push_events(
                            &DEFAULT_LINKED_CHUNK_IDS[0..index],
                            &[DEFAULT_EVENT.clone()],
                        ),
                        during: LinkedChunkUpdates::replace_event(
                            &[DEFAULT_LINKED_CHUNK_IDS[0]],
                            *DEFAULT_INITIAL_POSITION,
                            &DEFAULT_EVENT,
                        ),
                        after: LinkedChunkUpdates::clear(&DEFAULT_LINKED_CHUNK_IDS),
                    },
                )
                .await
                .unwrap()
            }))
        })
        .await;
    }
}
