# matrix-sdk-search

This crate implements a searchable index for messages in a Matrix client.

## Usage

The recommended way to use this crate is to include `matrix-sdk` as a dependency 
in your `Cargo.toml` file with the `experimental-search` feature flag turned on.

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

