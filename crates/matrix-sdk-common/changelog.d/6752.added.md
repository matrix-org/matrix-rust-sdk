`linked_chunk::UpdatesSubscriber` is now available to everyone, not only for
internal testing purposes.

```rust
let linked_chunk = LinkedChunk::new_with_update_history();
let mut updates_subscriber = linked_chunk.updates().unwrap().subscribe();

// `UpdatesSubscriber` implements `Stream`.
// Let's wait on a next value to come…
use futures_util::stream::StreamExt;

while let Some(next_update) = updates_subscriber.next().await {
    // Do something!
}
```
