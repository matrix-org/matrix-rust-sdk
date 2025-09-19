# matrix-sdk-search

This crate implements a searchable index for messages in a Matrix client.

## Usage

The recommended way to use this crate is to include `matrix-sdk` as a dependency 
in your `Cargo.toml` file with the `experimental-search` feature flag turned on.

Instantiation is done by the client builder using `matrix_sdk::client::builder::ClientBuilder::search_index_store()`.

```rust
use std::path::Path;
use matrix_sdk::{
    Client, ClientBuildError, SqliteCryptoStore, SqliteEventCacheStore, SqliteStateStore,  
    config::StoreConfig,
    search_index::SearchIndexStoreKind,
};

async fn make_client(
    name: &str, 
    session_path: &Path, 
    server_name: &str
) -> Result<Client, ClientBuildError> {
    Client::builder()
        .store_config(
            StoreConfig::new(name.to_owned())
                .crypto_store(SqliteCryptoStore::open(session_path.join("crypto"), None).await?)
                .state_store(SqliteStateStore::open(session_path.join("state"), None).await?)
                .event_cache_store(
                    SqliteEventCacheStore::open(session_path.join("cache"), None).await?,
                ),
        )
        .server_name_or_homeserver_url(server_name)
        .search_index_store(SearchIndexStoreKind::UnencryptedDirectory(session_path.join("index")))
        .build()
        .await
}

```

And searching is done via `matrix_sdk::room::Room::search()`.

```rust
use ruma::OwnedEventId;
use matrix_sdk::room::Room;
use matrix_sdk_search::error::IndexError;

async fn search_in_room(room: Room, query: &str) -> Result<Vec<OwnedEventId>, IndexError> {
    // Returns at most 100 results with no pagination offset.
    room.search(query, 100, None).await
}
```

## Stand-alone usage

Constructing a `matrix_sdk_search::index::RoomIndex` is done with the `matrix_sdk_search::index::builder::RoomIndexBuilder`.

```rust
use std::path::PathBuf;
use matrix_sdk_search::{
    error::IndexError,
    index::{
        RoomIndex,
        builder::RoomIndexBuilder,
    },
};
use ruma::RoomId;

fn create_index(path: PathBuf, room_id: &RoomId) -> Result<RoomIndex, IndexError> {
    RoomIndexBuilder::new_on_disk(path, room_id).unencrypted().build()
}
```

The search crate accepts index operations through `matrix_sdk_search::index::RoomIndex::execute()`
which takes a `matrix_sdk_search::index::RoomIndexOperation`. 

```rust
use matrix_sdk_search::index::{
    RoomIndex, RoomIndexOperation,
    builder::RoomIndexBuilder
};
use ruma::events::room::message::OriginalSyncRoomMessageEvent;

async fn add_event(index: &mut RoomIndex, event: OriginalSyncRoomMessageEvent) {
    index.execute(RoomIndexOperation::Add(event));
}
```

Some method(s) for creating these operations will need to be implemented. There is an example of
handling `ruma::events::AnySyncTimelineEvents` in `matrix-sdk::search_index`.

