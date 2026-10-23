//! How much memory the search index takes once a global search opened it.
//!
//! This isn't a timing benchmark: it counts the bytes allocated on the heap
//! with a counting global allocator, and prints a table. Run it with
//! `cargo bench --features search --bench search_memory`.

use std::{
    alloc::{GlobalAlloc, Layout, System},
    path::Path,
    sync::atomic::{AtomicIsize, Ordering},
    time::{Duration, Instant},
};

use matrix_sdk::{
    Client, search_index::SearchIndexStoreKind, sleep::sleep, stream::StreamExt as _,
    test_utils::mocks::MatrixMockServer,
};
use matrix_sdk_test::{JoinedRoomBuilder, event_factory::EventFactory};
use ruma::{OwnedRoomId, owned_user_id};
use tempfile::TempDir;
use tokio::runtime::Builder;

/// The number of bytes currently allocated on the heap.
static ALLOCATED: AtomicIsize = AtomicIsize::new(0);
/// The highest value of [`ALLOCATED`] since it was last reset.
static PEAK: AtomicIsize = AtomicIsize::new(0);

struct CountingAllocator;

impl CountingAllocator {
    fn add(bytes: isize) {
        let allocated = ALLOCATED.fetch_add(bytes, Ordering::Relaxed) + bytes;
        PEAK.fetch_max(allocated, Ordering::Relaxed);
    }
}

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        Self::add(layout.size() as isize);
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        Self::add(-(layout.size() as isize));
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        Self::add(new_size as isize - layout.size() as isize);
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: CountingAllocator = CountingAllocator;

const NUMBER_OF_ROOMS: usize = 100;
const MESSAGES_PER_ROOM: usize = 500;
const KEY: &str = "key";

const WORDS: &[&str] = &[
    "lunch", "tomorrow", "meeting", "deploy", "release", "coffee", "weekend", "train", "review",
    "budget", "holiday", "concert", "bug", "fix", "design", "draft", "call", "dinner", "plan",
];

fn room_ids() -> Vec<OwnedRoomId> {
    (0..NUMBER_OF_ROOMS)
        .map(|nth| format!("!room{nth:04}:example.com").try_into().unwrap())
        .collect()
}

/// A message body made of a few words, different for every `nth`.
fn body(nth: usize) -> String {
    (0..12).map(|i| WORDS[(nth * 7 + i * 3) % WORDS.len()]).collect::<Vec<_>>().join(" ")
}

/// A client using a `kind` search index in `dir`, that joined `rooms`. When
/// `with_messages` is set, each room gets `MESSAGES_PER_ROOM` messages, the
/// first one matching the search query.
async fn client(
    server: &MatrixMockServer,
    kind: SearchIndexStoreKind,
    rooms: &[OwnedRoomId],
    with_messages: bool,
) -> Client {
    let client = server
        .client_builder()
        .on_builder(|builder| builder.search_index_store(kind))
        .build()
        .await;

    let sender = owned_user_id!("@alice:example.com");

    server
        .mock_sync()
        .ok_and_run(&client, |builder| {
            for room_id in rooms {
                let mut room = JoinedRoomBuilder::new(room_id);

                if with_messages {
                    let f = EventFactory::new().room(room_id).sender(&sender);
                    room = room.add_timeline_event(f.text_msg("good morning").server_ts(1_000));

                    for nth in 1..MESSAGES_PER_ROOM {
                        room = room.add_timeline_event(
                            f.text_msg(body(nth)).server_ts(1_000 + nth as u64),
                        );
                    }
                }

                builder.add_joined_room(room);
            }
        })
        .await;

    client
}

/// Run a global search, the way the search service does it, and count the
/// results.
async fn global_search(client: &Client) -> usize {
    let mut count = 0;
    let mut stream = Box::pin(client.search_messages("morning".to_owned()).build());
    while let Some(page) = stream.next().await {
        count += page.unwrap().len();
    }
    count
}

/// The total size of the files in `dir`.
fn size_on_disk(dir: &Path) -> u64 {
    std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| {
            let entry = entry.unwrap();
            let metadata = entry.metadata().unwrap();
            if metadata.is_dir() { size_on_disk(&entry.path()) } else { metadata.len() }
        })
        .sum()
}

fn mb(bytes: impl Into<f64>) -> String {
    format!("{:.1} MB", bytes.into() / 1_000_000.0)
}

fn main() {
    let runtime = Builder::new_multi_thread().enable_all().build().expect("Can't create runtime");
    runtime.block_on(run());
}

async fn run() {
    let server = MatrixMockServer::new().await;
    let rooms = room_ids();

    println!(
        "# Search index memory ({NUMBER_OF_ROOMS} rooms, {MESSAGES_PER_ROOM} messages per room)"
    );
    println!();
    println!(
        "| Index | On disk | Held after a global search | Peak during the search | First search | \
         Next search |"
    );
    println!("|---|---:|---:|---:|---:|---:|");

    let kinds: [(&str, fn(&Path) -> SearchIndexStoreKind); 2] = [
        ("encrypted", |dir| {
            SearchIndexStoreKind::EncryptedDirectory(dir.to_owned(), KEY.to_owned())
        }),
        ("unencrypted", |dir| SearchIndexStoreKind::UnencryptedDirectory(dir.to_owned())),
    ];

    for (name, kind) in kinds {
        let dir = TempDir::new().unwrap();

        // Index every room, then drop the client so the next one opens the
        // indexes from disk.
        {
            let client = client(&server, kind(dir.path()), &rooms, true).await;
            while global_search(&client).await < NUMBER_OF_ROOMS {
                sleep(Duration::from_millis(500)).await;
            }
        }
        sleep(Duration::from_secs(1)).await;

        let on_disk = size_on_disk(dir.path());

        // A fresh client, whose room indexes aren't open yet.
        let client = client(&server, kind(dir.path()), &rooms, false).await;
        sleep(Duration::from_secs(1)).await;

        let before = ALLOCATED.load(Ordering::Relaxed);
        PEAK.store(before, Ordering::Relaxed);

        let start = Instant::now();
        assert_eq!(global_search(&client).await, NUMBER_OF_ROOMS);
        let first_search = start.elapsed();

        // Let anything temporary go away.
        sleep(Duration::from_secs(1)).await;

        let held = ALLOCATED.load(Ordering::Relaxed) - before;
        let peak = PEAK.load(Ordering::Relaxed) - before;

        // The room indexes are open now: time a search on its own.
        let start = Instant::now();
        assert_eq!(global_search(&client).await, NUMBER_OF_ROOMS);
        let next_search = start.elapsed();

        println!(
            "| {name} | {} | {} | {} | {first_search:.0?} | {next_search:.0?} |",
            mb(on_disk as f64),
            mb(held as f64),
            mb(peak as f64)
        );

        drop(client);
    }
}
